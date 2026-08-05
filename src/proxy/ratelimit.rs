// Copyright (c) 2026 chulingera2025
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Single-node token-bucket rate limiting (M10, spec §5.2).
//!
//! The [`RateLimiter`] is process-lifetime, so bucket state survives SIGHUP
//! reloads (ADR-011) exactly like the load-balancing pool. A bucket is keyed by
//! `(site, terminal, directive offset, client IP)` — each `rate_limit`
//! directive keeps its own counter, and distinct sites/terminals never share a
//! bucket. The map is bounded: past [`MAX_BUCKETS`], the least-recently-used
//! buckets are evicted so a flood of distinct client IPs cannot grow memory
//! without bound.

use crate::config::ast::{RateLimitKey, RateSpec, SiteKey};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

/// Cap on stored buckets; beyond it the least-recently-used buckets are evicted.
const MAX_BUCKETS: usize = 100_000;
/// Percentage of buckets evicted on a single overflow sweep. Evicting a
/// fraction in one pass amortizes the O(n) sweep to O(1) per evicted bucket.
const EVICT_PERCENT: usize = 10;

/// The identity of one rate-limit counter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BucketKey {
    site: SiteKey,
    terminal: usize,
    /// The position of the `rate_limit` modifier within its effective scope, so
    /// several `rate_limit` directives on the same terminal stay independent.
    directive: usize,
    ip: IpAddr,
}

/// One token bucket.
#[derive(Debug, Default)]
struct Bucket {
    /// Current tokens (capped at the spec's burst).
    tokens: f64,
    /// When the bucket was last touched, for refill and eviction.
    last: Option<Instant>,
}

/// Process-lifetime, single-node token-bucket rate limiter (spec §5.2).
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<BucketKey, Bucket>>,
}

impl RateLimiter {
    /// Create an empty rate limiter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether one request for `key` under `spec` may pass, consuming a
    /// token when it does. `directive` disambiguates multiple `rate_limit`
    /// directives within the same effective scope.
    ///
    /// Returns `true` when a token was available (the request proceeds), `false`
    /// when the bucket was empty (the caller should answer 429).
    pub fn allow(
        &self,
        site: &SiteKey,
        terminal: usize,
        directive: usize,
        ip: IpAddr,
        spec: &RateSpec,
    ) -> bool {
        debug_assert_eq!(spec.key, RateLimitKey::RemoteIp);
        let key = BucketKey {
            site: site.clone(),
            terminal,
            directive,
            ip,
        };
        let mut buckets = self.buckets.lock().expect("rate limiter lock poisoned");
        if !buckets.contains_key(&key) && buckets.len() >= MAX_BUCKETS {
            evict_least_recent(&mut buckets);
        }
        let now = Instant::now();
        let bucket = buckets.entry(key).or_default();
        // Refill continuously from the last touch, capped at the burst.
        bucket.tokens = match bucket.last {
            Some(last) => {
                let elapsed = now.duration_since(last).as_secs_f64();
                (bucket.tokens + elapsed * spec.tokens_per_second()).min(spec.burst as f64)
            }
            None => spec.burst as f64,
        };
        bucket.last = Some(now);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Evict the least-recently-used buckets until the map is back under
/// [`MAX_BUCKETS`], removing [`EVICT_PERCENT`]% in one sweep.
///
/// Every bucket inserted by [`RateLimiter::allow`] has a `last` timestamp, so
/// the sweep is total.
fn evict_least_recent(buckets: &mut HashMap<BucketKey, Bucket>) {
    let to_remove = buckets.len() * EVICT_PERCENT / 100;
    if to_remove == 0 {
        return;
    }
    // Sort by last access ascending and drop the oldest `to_remove` keys.
    let mut order: Vec<(BucketKey, Instant)> = buckets
        .iter()
        .filter_map(|(key, bucket)| bucket.last.map(|last| (key.clone(), last)))
        .collect();
    order.sort_by_key(|(_, last)| *last);
    let removed: std::collections::HashSet<BucketKey> = order
        .into_iter()
        .take(to_remove)
        .map(|(key, _)| key)
        .collect();
    buckets.retain(|key, _| !removed.contains(key));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ast::{RateUnit, SiteKey};

    fn spec(count: u64, burst: u64) -> RateSpec {
        RateSpec {
            key: RateLimitKey::RemoteIp,
            count,
            unit: RateUnit::Second,
            burst,
        }
    }

    /// A distinct loopback IPv4 per `n` (supports > 256 distinct addresses).
    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(127, 0, (n >> 8) as u8, n as u8))
    }

    fn key() -> SiteKey {
        SiteKey::CatchAll { port: 8080 }
    }

    #[test]
    fn allows_up_to_burst_then_denies() {
        let limiter = RateLimiter::new();
        let s = spec(3, 3);
        // The first `burst` requests pass instantly.
        for _ in 0..3 {
            assert!(limiter.allow(&key(), 0, 0, ip(1), &s));
        }
        // The next one is denied until a token refills.
        assert!(!limiter.allow(&key(), 0, 0, ip(1), &s));
        assert!(!limiter.allow(&key(), 0, 0, ip(1), &s));
    }

    #[test]
    fn refills_over_time() {
        let limiter = RateLimiter::new();
        // 100r/s, burst 100: one token refills every 10ms.
        let s = spec(100, 100);
        for _ in 0..100 {
            assert!(limiter.allow(&key(), 0, 0, ip(1), &s));
        }
        assert!(!limiter.allow(&key(), 0, 0, ip(1), &s));
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(
            limiter.allow(&key(), 0, 0, ip(1), &s),
            "a token should have refilled after 30ms"
        );
    }

    #[test]
    fn buckets_are_independent_per_ip() {
        let limiter = RateLimiter::new();
        let s = spec(1, 1);
        assert!(limiter.allow(&key(), 0, 0, ip(1), &s));
        assert!(!limiter.allow(&key(), 0, 0, ip(1), &s));
        // A different client IP gets a fresh bucket.
        assert!(limiter.allow(&key(), 0, 0, ip(2), &s));
    }

    #[test]
    fn buckets_are_independent_per_terminal_and_directive() {
        let limiter = RateLimiter::new();
        let s = spec(1, 1);
        assert!(limiter.allow(&key(), 0, 0, ip(1), &s));
        assert!(!limiter.allow(&key(), 0, 0, ip(1), &s));
        // A different terminal or a second rate_limit directive is a new counter.
        assert!(limiter.allow(&key(), 1, 0, ip(1), &s));
        assert!(limiter.allow(&key(), 0, 1, ip(1), &s));
    }

    #[test]
    fn overflow_evicts_least_recent() {
        // Fill a map with 1000 buckets whose last access times are spread out,
        // then evict: only the oldest EVICT_PERCENT% should be dropped.
        let mut buckets = HashMap::new();
        for i in 0..1000usize {
            buckets.insert(
                BucketKey {
                    site: key(),
                    terminal: 0,
                    directive: 0,
                    ip: ip(i as u32),
                },
                Bucket {
                    tokens: 1.0,
                    last: Some(Instant::now() - std::time::Duration::from_millis(i as u64)),
                },
            );
        }
        evict_least_recent(&mut buckets);
        assert_eq!(
            buckets.len(),
            1000 - 1000 * EVICT_PERCENT / 100,
            "the oldest EVICT_PERCENT% must be evicted"
        );
        // Bucket 0 is most recent (now - 0ms) → kept; bucket 999 is least
        // recent (now - 999ms) → evicted.
        assert!(buckets.contains_key(&BucketKey {
            site: key(),
            terminal: 0,
            directive: 0,
            ip: ip(0),
        }));
        assert!(!buckets.contains_key(&BucketKey {
            site: key(),
            terminal: 0,
            directive: 0,
            ip: ip(999),
        }));
    }
}
