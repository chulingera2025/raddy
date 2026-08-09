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
//! bucket.
//!
//! # Hard bounds and eviction
//!
//! The store is sharded into [`SHARDS`] independent [`Mutex`]es so a flood of
//! distinct client IPs never contends on one global lock. Each shard holds a
//! [`HashMap`] plus a bounded FIFO recency queue; both are capped at
//! [`PER_SHARD_CAP`] buckets, so total memory is bounded by
//! `SHARDS * PER_SHARD_CAP` (≈ [`MAX_BUCKETS`]) no matter how many distinct IPs
//! arrive. Eviction is second-chance (CLOCK) with a fixed scan bound
//! [`EVICT_SCAN`]: a single eviction examines at most `EVICT_SCAN` candidates
//! and never sorts the table, so per-request work is O(1) amortized and
//! O(EVICT_SCAN) worst case — a remote attacker cannot amplify a full-table
//! sort the way the old single-map LRU sweep could.

use crate::config::ast::{RateLimitKey, RateSpec, SiteKey};
use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

/// Number of shards. Fixed at 64: large enough that a shard lock is rarely
/// contended, small enough that per-shard bookkeeping stays cheap.
const SHARDS: usize = 64;
/// Target cap on stored buckets across all shards (kept from the v0.1 design).
const MAX_BUCKETS: usize = 100_000;
/// Hard per-shard cap; `SHARDS * PER_SHARD_CAP <= MAX_BUCKETS`.
const PER_SHARD_CAP: usize = MAX_BUCKETS / SHARDS;
/// Bound on how many recency-queue entries a single eviction may examine.
/// Second-chance passes over used buckets once each; the fixed bound keeps a
/// single eviction O(EVICT_SCAN) even when every bucket was recently used.
const EVICT_SCAN: usize = 64;

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
    /// Second-chance bit: set on a hit, cleared when the bucket is granted a
    /// reprieve during eviction. Lets a recently-active bucket survive one
    /// pass, approximating LRU without touching the recency queue on hits.
    used: bool,
}

/// One shard: the bucket map plus a FIFO recency queue of its keys.
///
/// The invariant `recency` holds exactly the keys of `map`, in a stable order
/// (new keys appended, evicted keys popped). On a hit the bucket is only marked
/// `used`; it is moved to the back of `recency` by the next eviction scan, so
/// the queue is never touched on the hot path.
#[derive(Debug, Default)]
struct Shard {
    map: HashMap<BucketKey, Bucket>,
    recency: VecDeque<BucketKey>,
}

/// Process-lifetime, single-node token-bucket rate limiter (spec §5.2).
pub struct RateLimiter {
    /// Per-process random hash seed, so an attacker cannot precompute which
    /// shard their flood lands in (each shard only ever evicts its own keys).
    state: RandomState,
    shards: [Mutex<Shard>; SHARDS],
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show only the total bucket count; the per-shard internals are not
        // meaningful to a caller and would require locking every shard.
        let total: usize = self
            .shards
            .iter()
            .map(|s| s.lock().expect("rate limiter lock poisoned").map.len())
            .sum();
        f.debug_struct("RateLimiter")
            .field("buckets", &total)
            .finish()
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self {
            state: RandomState::new(),
            shards: std::array::from_fn(|_| Mutex::new(Shard::default())),
        }
    }
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
        let mut shard = self.shards[self.shard_index(&key)]
            .lock()
            .expect("rate limiter lock poisoned");
        let now = Instant::now();
        if let Some(bucket) = shard.map.get_mut(&key) {
            // Hit: refill continuously from the last touch, capped at the burst,
            // and mark the bucket for the next eviction reprieve.
            bucket.used = true;
            bucket.tokens = refill(bucket.tokens, bucket.last, now, spec);
            bucket.last = Some(now);
            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                true
            } else {
                false
            }
        } else {
            // Miss: make room when this shard is at its hard cap, then start a
            // fresh full bucket (a new client may use its whole burst at once).
            if shard.map.len() >= PER_SHARD_CAP {
                evict_one(&mut shard);
            }
            // Record the key in the recency queue first, then insert the bucket,
            // so the two field borrows never overlap.
            shard.recency.push_back(key.clone());
            let bucket = shard.map.entry(key).or_default();
            bucket.tokens = refill(0.0, None, now, spec);
            bucket.last = Some(now);
            // A fresh bucket holds at least one token (burst >= 1 by the
            // parser), so this request always passes.
            debug_assert!(bucket.tokens >= 1.0);
            bucket.tokens -= 1.0;
            true
        }
    }

    /// The shard a key routes to. Seeded per-process, so the mapping is not
    /// predictable to a remote attacker.
    fn shard_index(&self, key: &BucketKey) -> usize {
        (self.state.hash_one(key) as usize) % SHARDS
    }
}

/// Continuous token refill: `tokens + elapsed * rate`, capped at the burst.
/// A bucket with no recorded touch starts full.
fn refill(tokens: f64, last: Option<Instant>, now: Instant, spec: &RateSpec) -> f64 {
    match last {
        Some(last) => {
            let elapsed = now.duration_since(last).as_secs_f64();
            (tokens + elapsed * spec.tokens_per_second()).min(spec.burst as f64)
        }
        None => spec.burst as f64,
    }
}

/// Evict one bucket via second-chance (CLOCK) with a bounded scan.
///
/// The front of the recency queue is examined up to [`EVICT_SCAN`] times:
/// a bucket marked `used` gets one reprieve (the bit is cleared and the key is
/// rotated to the back), the first unmarked bucket is removed. After
/// `EVICT_SCAN` reprieves the front bucket is evicted unconditionally, so a
/// single eviction is O(EVICT_SCAN) even when every bucket was recently used
/// and no full-table scan or sort is ever performed.
fn evict_one(shard: &mut Shard) {
    for _ in 0..EVICT_SCAN {
        let Some(key) = shard.recency.pop_front() else {
            return;
        };
        match shard.map.get_mut(&key) {
            Some(bucket) if bucket.used => {
                bucket.used = false;
                shard.recency.push_back(key);
            }
            Some(_) => {
                shard.map.remove(&key);
                return;
            }
            // A stale queue entry (key absent from the map) is dropped; the
            // invariant normally prevents this, so this is purely defensive.
            None => {}
        }
    }
    // Bounded scan exhausted: drop the front candidate regardless.
    if let Some(key) = shard.recency.pop_front() {
        shard.map.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ast::{RateUnit, SiteKey};
    use std::sync::Arc;

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

    /// A distinct IPv4 in the 10/8 test range per `n` (2^24 distinct values),
    /// for capacity floods that must exceed the whole sharded capacity.
    fn many_ip(n: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(
            10,
            (n >> 16) as u8,
            (n >> 8) as u8,
            n as u8,
        ))
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
    fn fresh_bucket_grants_its_burst_immediately() {
        let limiter = RateLimiter::new();
        // A new client may spend its whole burst at once (spec §5.2).
        let s = spec(5, 5);
        for _ in 0..5 {
            assert!(limiter.allow(&key(), 0, 0, ip(1), &s));
        }
        assert!(!limiter.allow(&key(), 0, 0, ip(1), &s));
    }

    #[test]
    fn shard_capacity_is_hard_bounded() {
        let limiter = RateLimiter::new();
        let s = spec(1, 1);
        // Flood far more distinct IPs than the total capacity: the store must
        // stay bounded per shard (structural invariant, no wall-clock timing).
        let flood = (SHARDS * PER_SHARD_CAP * 2) as u32;
        for i in 0..flood {
            limiter.allow(&key(), 0, 0, many_ip(i), &s);
        }
        let mut total = 0;
        for shard in &limiter.shards {
            let shard = shard.lock().expect("rate limiter lock poisoned");
            assert!(
                shard.map.len() <= PER_SHARD_CAP,
                "shard exceeded its hard cap: {}",
                shard.map.len()
            );
            assert_eq!(
                shard.map.len(),
                shard.recency.len(),
                "map and recency queue out of sync"
            );
            total += shard.map.len();
        }
        assert!(
            total <= SHARDS * PER_SHARD_CAP,
            "total buckets {total} exceed the global cap"
        );
    }

    #[test]
    fn second_chance_eviction_spares_recently_used_bucket() {
        let limiter = RateLimiter::new();
        let mut shard = limiter.shards[0]
            .lock()
            .expect("rate limiter lock poisoned");
        let now = Instant::now();
        // Fill the shard to its hard cap with untouched buckets.
        for i in 0..PER_SHARD_CAP as u32 {
            let bucket_key = BucketKey {
                site: key(),
                terminal: 0,
                directive: 0,
                ip: ip(i),
            };
            shard.map.insert(
                bucket_key.clone(),
                Bucket {
                    tokens: 1.0,
                    last: Some(now),
                    used: false,
                },
            );
            shard.recency.push_back(bucket_key);
        }
        // Mark the oldest bucket as recently used: it must survive the next
        // eviction (one reprieve) while a newer untouched bucket is evicted.
        let oldest = shard.recency.front().expect("filled queue").clone();
        shard.map.get_mut(&oldest).expect("oldest present").used = true;
        let len_before = shard.map.len();

        evict_one(&mut shard);

        assert_eq!(shard.map.len(), len_before - 1);
        assert!(
            shard.map.contains_key(&oldest),
            "a recently-used bucket must survive one eviction reprieve"
        );
        assert_eq!(shard.map.len(), shard.recency.len());
    }

    #[test]
    fn eviction_scan_is_bounded_when_every_bucket_used() {
        let limiter = RateLimiter::new();
        let mut shard = limiter.shards[0]
            .lock()
            .expect("rate limiter lock poisoned");
        let now = Instant::now();
        // Every bucket marked used: a naive scan would pass over all of them
        // before finding an eviction candidate. The bounded scan must still
        // evict exactly one and terminate after EVICT_SCAN reprieves.
        for i in 0..PER_SHARD_CAP as u32 {
            let bucket_key = BucketKey {
                site: key(),
                terminal: 0,
                directive: 0,
                ip: ip(i),
            };
            shard.map.insert(
                bucket_key.clone(),
                Bucket {
                    tokens: 1.0,
                    last: Some(now),
                    used: true,
                },
            );
            shard.recency.push_back(bucket_key);
        }
        let len_before = shard.map.len();

        evict_one(&mut shard);

        assert_eq!(
            shard.map.len(),
            len_before - 1,
            "bounded scan still evicts one"
        );
        assert_eq!(shard.map.len(), shard.recency.len());
    }

    #[test]
    fn bounded_scan_evicts_one_after_evict_scan_reprieves() {
        // Every bucket marked used: a full-table scan would have to pass over
        // all of them before finding an eviction candidate. The bounded scan
        // must stop after EVICT_SCAN reprieves and evict the next candidate
        // unconditionally — exactly one bucket gone, the first EVICT_SCAN
        // reprieved in order (O(EVICT_SCAN), never a full-table scan).
        let limiter = RateLimiter::new();
        let mut shard = limiter.shards[0]
            .lock()
            .expect("rate limiter lock poisoned");
        let now = Instant::now();
        let n = EVICT_SCAN + 1;
        for i in 0..n as u32 {
            let bucket_key = BucketKey {
                site: key(),
                terminal: 0,
                directive: 0,
                ip: ip(i),
            };
            shard.map.insert(
                bucket_key.clone(),
                Bucket {
                    tokens: 1.0,
                    last: Some(now),
                    used: true,
                },
            );
            shard.recency.push_back(bucket_key);
        }

        evict_one(&mut shard);

        assert_eq!(
            shard.map.len(),
            EVICT_SCAN,
            "exactly one bucket evicted after EVICT_SCAN reprieves"
        );
        assert_eq!(shard.map.len(), shard.recency.len());
        // The (EVICT_SCAN+1)-th bucket was still marked used, yet the bounded
        // scan's fallback evicted it rather than scanning further.
        let evicted = BucketKey {
            site: key(),
            terminal: 0,
            directive: 0,
            ip: ip(EVICT_SCAN as u32),
        };
        assert!(!shard.map.contains_key(&evicted));
        // The reprieved buckets rotate to the back in their original order.
        assert_eq!(shard.recency.front().map(|k| k.ip), Some(ip(0)));
    }

    #[test]
    fn concurrent_allows_are_safe_and_bounded() {
        let limiter = Arc::new(RateLimiter::new());
        let s = spec(10, 10);
        std::thread::scope(|scope| {
            for t in 0..8u32 {
                let limiter = limiter.clone();
                scope.spawn(move || {
                    let base = t * 10_000;
                    for i in 0..10_000u32 {
                        let _ = limiter.allow(&key(), 0, 0, many_ip(base + i), &s);
                    }
                });
            }
        });
        // Structural invariant after the concurrent storm: no shard overflowed
        // and every map/queue pair stayed consistent.
        let mut total = 0;
        for shard in &limiter.shards {
            let shard = shard.lock().expect("rate limiter lock poisoned");
            assert!(shard.map.len() <= PER_SHARD_CAP);
            assert_eq!(shard.map.len(), shard.recency.len());
            total += shard.map.len();
        }
        assert!(total <= SHARDS * PER_SHARD_CAP);
    }
}
