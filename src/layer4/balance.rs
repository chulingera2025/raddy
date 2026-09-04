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

//! Native upstream selection and health checking for layer-4 listeners.
//!
//! The L4 data path owns this rather than borrowing `pingora::lb`, so upstream
//! selection is a plain function over an atomic health flag — no external
//! balancer state and no allocation on the hot path.
//!
//! Selection never returns an unhealthy backend. When every backend is
//! unhealthy the caller gets `None` and refuses the connection, which the proxy
//! reports as `no_upstream` rather than relaying into a known-dead upstream.

use crate::config::ast::{LbPolicy, TcpHealthCheckSpec};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Virtual nodes per backend on the `ip_hash` ring.
///
/// `ip_hash` must be *consistent* hashing, not `hash % len`: with modulo, adding
/// or removing one backend remaps almost every client and breaks the session
/// stickiness the policy exists to provide. 160 virtual nodes per backend is the
/// usual ketama figure — enough that the key space stays evenly divided.
const VNODES_PER_BACKEND: usize = 160;

/// One upstream and its current health.
#[derive(Debug)]
struct Backend {
    addr: SocketAddr,
    /// Backends start healthy: a listener must serve traffic immediately, before
    /// the first probe has had a chance to run.
    healthy: AtomicBool,
    /// Consecutive probe failures since the last success, for flap damping.
    failures: AtomicUsize,
    /// Consecutive probe successes since the last failure.
    successes: AtomicUsize,
}

/// A point on the `ip_hash` ring: a hash and the backend index that owns it.
#[derive(Debug)]
struct RingNode {
    hash: u64,
    backend: usize,
}

/// Health-checked upstream selection for one `tcp` listener.
#[derive(Debug)]
pub struct Balancer {
    backends: Vec<Backend>,
    policy: LbPolicy,
    /// Sorted ring, empty unless the policy is `ip_hash`.
    ring: Vec<RingNode>,
    /// Cursor for `round_robin` and `random`; persists across selections.
    cursor: AtomicUsize,
    health_check: Option<TcpHealthCheckSpec>,
}

impl Balancer {
    /// Build a balancer over `upstreams`.
    ///
    /// `health_check` enables active TCP-connect probing; without it every
    /// backend stays healthy for the process lifetime.
    pub fn new(
        upstreams: &[SocketAddr],
        policy: LbPolicy,
        health_check: Option<TcpHealthCheckSpec>,
    ) -> Self {
        let backends: Vec<Backend> = upstreams
            .iter()
            .map(|addr| Backend {
                addr: *addr,
                healthy: AtomicBool::new(true),
                failures: AtomicUsize::new(0),
                successes: AtomicUsize::new(0),
            })
            .collect();
        let ring = if policy == LbPolicy::IpHash {
            build_ring(upstreams)
        } else {
            Vec::new()
        };
        Self {
            backends,
            policy,
            ring,
            cursor: AtomicUsize::new(0),
            health_check,
        }
    }

    /// Select a healthy upstream for `key` (the client IP bytes under
    /// `ip_hash`; ignored by the other policies).
    ///
    /// Returns `None` when every backend is unhealthy or the set is empty.
    pub fn select(&self, key: &[u8]) -> Option<SocketAddr> {
        if self.backends.is_empty() {
            return None;
        }
        match self.policy {
            LbPolicy::IpHash => self.select_hashed(key),
            LbPolicy::RoundRobin => {
                let start = self.cursor.fetch_add(1, Ordering::Relaxed);
                self.first_healthy_from(start)
            }
            LbPolicy::Random => {
                // A cheap LCG over the shared cursor: no RNG state per
                // selection, and the sequence is good enough for spreading load.
                let c = self.cursor.fetch_add(1, Ordering::Relaxed);
                let scrambled = c.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                self.first_healthy_from(scrambled)
            }
        }
    }

    /// Walk the backend list from `start`, returning the first healthy address.
    ///
    /// Scanning forward (rather than retrying a random pick) means an
    /// all-unhealthy set terminates after one pass instead of spinning.
    fn first_healthy_from(&self, start: usize) -> Option<SocketAddr> {
        let n = self.backends.len();
        (0..n).find_map(|offset| {
            let backend = &self.backends[(start.wrapping_add(offset)) % n];
            backend
                .healthy
                .load(Ordering::Relaxed)
                .then_some(backend.addr)
        })
    }

    /// Consistent-hash selection: find the first ring node at or after the key's
    /// hash, then walk the ring until a healthy backend is found.
    fn select_hashed(&self, key: &[u8]) -> Option<SocketAddr> {
        if self.ring.is_empty() {
            return self.first_healthy_from(0);
        }
        let hash = fnv1a(key);
        let start = self.ring.partition_point(|node| node.hash < hash) % self.ring.len();
        (0..self.ring.len()).find_map(|offset| {
            let node = &self.ring[(start + offset) % self.ring.len()];
            let backend = &self.backends[node.backend];
            backend
                .healthy
                .load(Ordering::Relaxed)
                .then_some(backend.addr)
        })
    }

    /// The configured probe interval, if active health checking is enabled.
    pub fn probe_interval(&self) -> Option<Duration> {
        self.health_check.as_ref().map(|hc| hc.interval)
    }

    /// Run one round of TCP-connect probes and apply the flap-damping
    /// thresholds.
    ///
    /// A backend is marked unhealthy only after `consecutive_failures` failed
    /// probes and healthy again only after `consecutive_successes` successful
    /// ones, so a single blip neither removes nor restores an upstream.
    pub async fn probe(&self) {
        let Some(hc) = &self.health_check else {
            return;
        };
        for backend in &self.backends {
            let reachable =
                tokio::time::timeout(hc.timeout, tokio::net::TcpStream::connect(backend.addr))
                    .await
                    .is_ok_and(|result| result.is_ok());
            if reachable {
                backend.failures.store(0, Ordering::Relaxed);
                let successes = backend.successes.fetch_add(1, Ordering::Relaxed) + 1;
                if successes >= hc.consecutive_successes
                    && !backend.healthy.swap(true, Ordering::Relaxed)
                {
                    tracing::info!("l4 upstream {} is healthy again", backend.addr);
                }
            } else {
                backend.successes.store(0, Ordering::Relaxed);
                let failures = backend.failures.fetch_add(1, Ordering::Relaxed) + 1;
                if failures >= hc.consecutive_failures
                    && backend.healthy.swap(false, Ordering::Relaxed)
                {
                    tracing::warn!("l4 upstream {} marked unhealthy", backend.addr);
                }
            }
        }
    }
}

/// Build the sorted `ip_hash` ring for `upstreams`.
fn build_ring(upstreams: &[SocketAddr]) -> Vec<RingNode> {
    let mut ring = Vec::with_capacity(upstreams.len() * VNODES_PER_BACKEND);
    for (index, addr) in upstreams.iter().enumerate() {
        for vnode in 0..VNODES_PER_BACKEND {
            ring.push(RingNode {
                hash: fnv1a(format!("{addr}#{vnode}").as_bytes()),
                backend: index,
            });
        }
    }
    ring.sort_unstable_by_key(|node| node.hash);
    ring
}

/// Hash used for both ring positions and lookup keys.
///
/// FNV-1a alone is **not** usable here. Its avalanche in the high bits is weak
/// for inputs that share a long prefix, and `ip_hash` keys are exactly that —
/// `198.51.100.7`, `198.51.100.8`, … Measured directly, plain FNV-1a packed 400
/// such keys into 16% of the 64-bit space and handed every one of them to a
/// single backend. The murmur3 finalizer below costs three shifts and two
/// multiplies and restores full avalanche, which spreads the same keys evenly
/// and keeps re-mapping near the consistent-hashing ideal when the backend set
/// changes.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // murmur3 fmix64
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(n: u16) -> Vec<SocketAddr> {
        (0..n)
            .map(|i| SocketAddr::from(([127, 0, 0, 1], 9000 + i)))
            .collect()
    }

    #[test]
    fn round_robin_cycles_through_every_backend() {
        let lb = Balancer::new(&addrs(3), LbPolicy::RoundRobin, None);
        let picked: Vec<SocketAddr> = (0..6).filter_map(|_| lb.select(b"")).collect();
        assert_eq!(picked.len(), 6);
        // Three distinct backends, each used twice over six selections.
        let mut unique = picked.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn ip_hash_is_stable_for_the_same_key() {
        let lb = Balancer::new(&addrs(4), LbPolicy::IpHash, None);
        let first = lb.select(b"203.0.113.7").expect("a backend");
        for _ in 0..10 {
            assert_eq!(lb.select(b"203.0.113.7"), Some(first));
        }
    }

    #[test]
    fn ip_hash_spreads_prefix_sharing_client_ips_evenly() {
        // The regression this guards: real `ip_hash` keys are client IPs from
        // the same subnet, so they share a long prefix. Without an avalanche
        // finalizer the hashes clustered and one backend received *every* key.
        let lb = Balancer::new(&addrs(3), LbPolicy::IpHash, None);
        let mut counts = [0usize; 3];
        for i in 0..600 {
            let key = format!("198.51.100.{i}");
            let picked = lb.select(key.as_bytes()).expect("a backend");
            let index = lb
                .backends
                .iter()
                .position(|b| b.addr == picked)
                .expect("selected backend is in the set");
            counts[index] += 1;
        }
        // A fair split is 200 each; require every backend to carry a real share
        // rather than asserting an exact ratio.
        for (index, count) in counts.iter().enumerate() {
            assert!(
                *count > 600 / 6,
                "backend {index} received only {count}/600 keys: {counts:?}"
            );
        }
    }

    #[test]
    fn ip_hash_keeps_most_keys_when_a_backend_is_added() {
        // The point of consistent hashing: growing the pool must not reshuffle
        // every client. `hash % len` would remap ~3/4 of keys here.
        let keys: Vec<String> = (0..400).map(|i| format!("198.51.100.{i}")).collect();
        let before = Balancer::new(&addrs(3), LbPolicy::IpHash, None);
        let after = Balancer::new(&addrs(4), LbPolicy::IpHash, None);
        let moved = keys
            .iter()
            .filter(|key| before.select(key.as_bytes()) != after.select(key.as_bytes()))
            .count();
        // Ideal is 1/4; allow slack for ring granularity, but nothing close to
        // the ~75% a modulo scheme would produce.
        assert!(
            moved < keys.len() / 2,
            "consistent hashing remapped {moved}/{} keys",
            keys.len()
        );
    }

    #[test]
    fn selection_skips_unhealthy_backends() {
        let lb = Balancer::new(&addrs(3), LbPolicy::RoundRobin, None);
        lb.backends[0].healthy.store(false, Ordering::Relaxed);
        lb.backends[2].healthy.store(false, Ordering::Relaxed);
        for _ in 0..10 {
            assert_eq!(lb.select(b""), Some(lb.backends[1].addr));
        }
    }

    #[test]
    fn ip_hash_falls_through_to_a_healthy_backend() {
        let lb = Balancer::new(&addrs(3), LbPolicy::IpHash, None);
        let first = lb.select(b"key").expect("a backend");
        let index = lb
            .backends
            .iter()
            .position(|b| b.addr == first)
            .expect("selected backend is in the set");
        lb.backends[index].healthy.store(false, Ordering::Relaxed);
        let second = lb.select(b"key").expect("a healthy backend");
        assert_ne!(second, first);
    }

    #[test]
    fn all_unhealthy_yields_none_instead_of_spinning() {
        let lb = Balancer::new(&addrs(3), LbPolicy::RoundRobin, None);
        for backend in &lb.backends {
            backend.healthy.store(false, Ordering::Relaxed);
        }
        assert_eq!(lb.select(b""), None);

        let lb = Balancer::new(&addrs(3), LbPolicy::IpHash, None);
        for backend in &lb.backends {
            backend.healthy.store(false, Ordering::Relaxed);
        }
        assert_eq!(lb.select(b"key"), None);
    }

    #[test]
    fn empty_upstream_set_yields_none() {
        let lb = Balancer::new(&[], LbPolicy::RoundRobin, None);
        assert_eq!(lb.select(b""), None);
    }

    #[tokio::test]
    async fn probe_marks_a_dead_backend_unhealthy_after_the_threshold() {
        // Port 1 on loopback is reliably closed, so the connect fails fast.
        let dead = SocketAddr::from(([127, 0, 0, 1], 1));
        let lb = Balancer::new(
            &[dead],
            LbPolicy::RoundRobin,
            Some(TcpHealthCheckSpec {
                interval: Duration::from_millis(10),
                timeout: Duration::from_millis(200),
                consecutive_failures: 2,
                consecutive_successes: 1,
            }),
        );
        lb.probe().await;
        // One failure is below the threshold: still healthy, no flapping.
        assert_eq!(lb.select(b""), Some(dead));
        lb.probe().await;
        assert_eq!(lb.select(b""), None);
    }

    #[tokio::test]
    async fn probe_restores_a_reachable_backend() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe target");
        let addr = listener.local_addr().expect("probe address");
        let lb = Balancer::new(
            &[addr],
            LbPolicy::RoundRobin,
            Some(TcpHealthCheckSpec {
                interval: Duration::from_millis(10),
                timeout: Duration::from_millis(200),
                consecutive_failures: 1,
                consecutive_successes: 2,
            }),
        );
        lb.backends[0].healthy.store(false, Ordering::Relaxed);
        lb.probe().await;
        // One success is below the restore threshold.
        assert_eq!(lb.select(b""), None);
        lb.probe().await;
        assert_eq!(lb.select(b""), Some(addr));
    }
}
