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

//! Load-balancing pool backed by pingora's `LoadBalancer` (M9).
//!
//! The pool replaces the v0.1 round-robin counter and follows ADR-011:
//! selection state and health are process-lifetime and live outside the swapped
//! snapshot. A balancer is keyed by (site, terminal) and rebuilt only when its
//! spec (upstream list, policy, health-check parameters) changes — a reload that
//! leaves the spec unchanged keeps the balancer and its health state.
//!
//! Active health checks run on a dedicated thread with its own tokio runtime, so
//! balancers can be created/replaced on reload without touching the server's
//! fixed service set.

use crate::config::ast::{HealthCheckSpec, LbPolicy, SiteKey, TerminalKind, UpstreamPeer};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora::lb::health_check;
use pingora::lb::selection::{BackendIter, BackendSelection, Consistent, Random, RoundRobin};
use pingora::lb::LoadBalancer;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The health-check runner's wake-up period. Smaller than any realistic
/// configured `interval` (even the test's `200ms`), so probes land within one
/// tick of their deadline.
const RUNNER_TICK: Duration = Duration::from_millis(100);

/// A type-erased load balancer: backend selection plus optional health checking.
#[async_trait]
pub trait BackendSelector: Send + Sync {
    /// Pick a backend for `key` (the client IP for `ip_hash`; ignored by the
    /// other policies). Returns the index into the spec's `upstreams` so the
    /// caller can recover the full peer (address + TLS scheme + host), or `None`
    /// when every backend is unhealthy.
    fn select(&self, key: &[u8]) -> Option<usize>;
    /// The health-check probe interval, if a health check is configured.
    fn probe_interval(&self) -> Option<Duration>;
    /// Run one round of health checks (no-op when none is configured).
    async fn probe(&self);
}

/// The specification that determines when a balancer must be rebuilt (ADR-011).
/// Carries the resolved upstream peers (address + TLS scheme + original host),
/// so changing any of them rebuilds the balancer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LbSpec {
    pub upstreams: Vec<UpstreamPeer>,
    pub policy: LbPolicy,
    pub health_check: Option<HealthCheckSpec>,
}

/// Process-lifetime pool of per-(site, terminal) load balancers.
pub struct LoadBalancerPool {
    entries: ArcSwap<BalancerSnapshot>,
    update_lock: Mutex<()>,
    probe_times: Mutex<HashMap<(SiteKey, usize), Instant>>,
}

#[derive(Clone, Default)]
struct BalancerSnapshot {
    entries: HashMap<SiteKey, HashMap<usize, Arc<PoolEntry>>>,
}

struct PoolEntry {
    spec: LbSpec,
    balancer: Arc<dyn BackendSelector>,
}

impl Default for LoadBalancerPool {
    fn default() -> Self {
        Self {
            entries: ArcSwap::from_pointee(BalancerSnapshot::default()),
            update_lock: Mutex::new(()),
            probe_times: Mutex::new(HashMap::new()),
        }
    }
}

impl LoadBalancerPool {
    /// Create an empty pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or build) the balancer for a terminal, matching `spec`. Rebuilds
    /// only when the spec changed, so a reload that does not alter a terminal's
    /// upstreams/policy/health check keeps its balancer and health state.
    pub fn balancer_for(
        &self,
        site_key: &SiteKey,
        terminal_index: usize,
        spec: &LbSpec,
    ) -> Arc<dyn BackendSelector> {
        self.balancer_for_parts(
            site_key,
            terminal_index,
            &spec.upstreams,
            spec.policy,
            spec.health_check,
        )
    }

    /// Get or build a balancer from compiled terminal fields.
    ///
    /// The common request path passes borrowed upstream metadata from the
    /// immutable config snapshot. The `LbSpec` allocation is therefore limited
    /// to a cache miss or a configuration change.
    ///
    /// # Parameters
    ///
    /// * `site_key` identifies the configured site.
    /// * `terminal_index` identifies the reverse-proxy terminal within it.
    /// * `upstreams` contains the resolved upstream peers.
    /// * `policy` selects the backend selection algorithm.
    /// * `health_check` describes optional active health checking.
    ///
    /// # Returns
    ///
    /// The process-lifetime balancer for this site and terminal.
    pub fn balancer_for_parts(
        &self,
        site_key: &SiteKey,
        terminal_index: usize,
        upstreams: &[UpstreamPeer],
        policy: LbPolicy,
        health_check: Option<HealthCheckSpec>,
    ) -> Arc<dyn BackendSelector> {
        if let Some(balancer) =
            self.matching_balancer(site_key, terminal_index, upstreams, policy, health_check)
        {
            return balancer;
        }

        // Rebuilds are rare; serialize only this slow path. Request handling
        // uses `balancer_for_request`, which never enters this writer path.
        let _update = self
            .update_lock
            .lock()
            .expect("lb pool update lock poisoned");
        if let Some(balancer) =
            self.matching_balancer(site_key, terminal_index, upstreams, policy, health_check)
        {
            return balancer;
        }
        let spec = LbSpec {
            upstreams: upstreams.to_vec(),
            policy,
            health_check,
        };
        let balancer = build_balancer(&spec);
        let current = self.entries.load();
        let mut next = (**current).clone();
        next.entries.entry(site_key.clone()).or_default().insert(
            terminal_index,
            Arc::new(PoolEntry {
                spec,
                balancer: balancer.clone(),
            }),
        );
        self.entries.store(Arc::new(next));
        self.probe_times
            .lock()
            .expect("lb probe lock poisoned")
            .remove(&(site_key.clone(), terminal_index));
        balancer
    }

    /// Return a balancer for an in-flight request without mutating the pool.
    ///
    /// The request already owns an immutable config snapshot. If a reload has
    /// published a different entry for the same site and terminal, building an
    /// ephemeral balancer preserves that request's snapshot without replacing
    /// the new entry in the process-lifetime pool.
    ///
    /// # Parameters
    ///
    /// * `site_key` identifies the configured site.
    /// * `terminal_index` identifies the reverse-proxy terminal within it.
    /// * `upstreams` contains the resolved upstream peers.
    /// * `policy` selects the backend selection algorithm.
    /// * `health_check` describes optional active health checking.
    ///
    /// # Returns
    ///
    /// The matching cached balancer or a request-local fallback.
    pub(crate) fn balancer_for_request(
        &self,
        site_key: &SiteKey,
        terminal_index: usize,
        upstreams: &[UpstreamPeer],
        policy: LbPolicy,
        health_check: Option<HealthCheckSpec>,
    ) -> Arc<dyn BackendSelector> {
        if let Some(balancer) =
            self.matching_balancer(site_key, terminal_index, upstreams, policy, health_check)
        {
            return balancer;
        }
        let spec = LbSpec {
            upstreams: upstreams.to_vec(),
            policy,
            health_check,
        };
        build_balancer(&spec)
    }

    /// Look up a balancer without taking a writer lock.
    fn matching_balancer(
        &self,
        site_key: &SiteKey,
        terminal_index: usize,
        upstreams: &[UpstreamPeer],
        policy: LbPolicy,
        health_check: Option<HealthCheckSpec>,
    ) -> Option<Arc<dyn BackendSelector>> {
        let entries = self.entries.load();
        let entry = entries
            .entries
            .get(site_key)
            .and_then(|terminals| terminals.get(&terminal_index))?;
        if entry.spec.upstreams.as_slice() == upstreams
            && entry.spec.policy == policy
            && entry.spec.health_check == health_check
        {
            Some(entry.balancer.clone())
        } else {
            None
        }
    }

    /// Replace the pool snapshot with balancers for all reverse-proxy
    /// terminals in a compiled config.
    pub fn warm(&self, config: &crate::config::ast::CompiledConfig) {
        for site in &config.sites {
            for (index, terminal) in site.terminals.iter().enumerate() {
                let TerminalKind::ReverseProxy {
                    upstreams,
                    lb_policy,
                    health_check,
                    ..
                } = &terminal.kind
                else {
                    continue;
                };
                self.balancer_for_parts(&site.key, index, upstreams, *lb_policy, *health_check);
            }
        }
    }

    /// Drop balancers for (site, terminal) pairs that no longer exist in the
    /// snapshot. Called after every reload so removing a site stops its health
    /// probes instead of probing a decommissioned upstream forever.
    pub fn reconcile(&self, config: &crate::config::ast::CompiledConfig) {
        let _update = self
            .update_lock
            .lock()
            .expect("lb pool update lock poisoned");
        let live = Self::live_keys(config);
        let current = self.entries.load();
        let mut next = (**current).clone();
        next.entries.retain(|site_key, terminals| {
            terminals
                .retain(|terminal_index, _| live.contains(&(site_key.clone(), *terminal_index)));
            !terminals.is_empty()
        });
        self.entries.store(Arc::new(next));
        self.probe_times
            .lock()
            .expect("lb probe lock poisoned")
            .retain(|key, _| live.contains(key));
    }

    /// The (site, terminal) keys that currently hold a reverse-proxy terminal.
    fn live_keys(config: &crate::config::ast::CompiledConfig) -> HashSet<(SiteKey, usize)> {
        config
            .sites
            .iter()
            .flat_map(|site| {
                site.terminals
                    .iter()
                    .enumerate()
                    .filter_map(|(index, terminal)| {
                        matches!(&terminal.kind, TerminalKind::ReverseProxy { .. })
                            .then_some((site.key.clone(), index))
                    })
            })
            .collect()
    }

    /// The balancers due for a health-check probe at `now`, resetting their
    /// probe timestamps. Called by the health-check runner thread.
    fn probe_due(&self, now: Instant) -> Vec<Arc<dyn BackendSelector>> {
        let _update = self
            .update_lock
            .lock()
            .expect("lb pool update lock poisoned");
        let entries = self.entries.load();
        let mut probe_times = self.probe_times.lock().expect("lb probe lock poisoned");
        let mut due = Vec::new();
        for (site_key, terminals) in &entries.entries {
            for (terminal_index, entry) in terminals {
                let Some(interval) = entry.balancer.probe_interval() else {
                    continue;
                };
                let key = (site_key.clone(), *terminal_index);
                let is_due = probe_times
                    .get(&key)
                    .is_none_or(|last| now.duration_since(*last) >= interval);
                if is_due {
                    probe_times.insert(key, now);
                    due.push(entry.balancer.clone());
                }
            }
        }
        due
    }
}

/// Spawn the health-check runner thread. It probes every balancer's backends at
/// that balancer's configured interval, on its own tokio runtime so the pool can
/// change across reloads without touching the server's services.
pub fn spawn_health_check_runner(pool: Arc<LoadBalancerPool>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build health-check runtime");
        loop {
            for balancer in pool.probe_due(Instant::now()) {
                rt.block_on(balancer.probe());
            }
            std::thread::sleep(RUNNER_TICK);
        }
    });
}

/// Build a type-erased balancer from a spec, attaching the health check if any.
fn build_balancer(spec: &LbSpec) -> Arc<dyn BackendSelector> {
    // The upstreams were resolved by the validator, so they parse back as
    // `SocketAddr`; a failure here is a programming error.
    let addrs: Vec<String> = spec.upstreams.iter().map(|p| p.addr.to_string()).collect();
    match spec.policy {
        LbPolicy::RoundRobin => wrap(
            LoadBalancer::<RoundRobin>::try_from_iter(addrs).expect("resolved upstreams"),
            spec,
        ),
        LbPolicy::Random => wrap(
            LoadBalancer::<Random>::try_from_iter(addrs).expect("resolved upstreams"),
            spec,
        ),
        LbPolicy::IpHash => wrap(
            LoadBalancer::<Consistent>::try_from_iter(addrs).expect("resolved upstreams"),
            spec,
        ),
    }
}

/// Attach the configured TCP health check and wrap into the erased trait.
fn wrap<S>(mut lb: LoadBalancer<S>, spec: &LbSpec) -> Arc<dyn BackendSelector>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    let interval = spec.health_check.map(|hc| hc.interval);
    let mut indices_by_addr: HashMap<std::net::SocketAddr, Vec<usize>> = HashMap::new();
    for (index, peer) in spec.upstreams.iter().enumerate() {
        indices_by_addr.entry(peer.addr).or_default().push(index);
    }
    if let Some(hc) = spec.health_check {
        let mut check = health_check::TcpHealthCheck::new();
        check.consecutive_failure = hc.consecutive_failures;
        check.consecutive_success = hc.consecutive_successes;
        check.peer_template.options.connection_timeout = Some(hc.timeout);
        lb.set_health_check(check);
    }
    Arc::new(WrappedLb {
        inner: lb,
        interval,
        indices_by_addr,
        rotation: AtomicUsize::new(0),
    })
}

/// Concrete erased balancer for a given selection algorithm.
struct WrappedLb<S: BackendSelection> {
    inner: LoadBalancer<S>,
    interval: Option<Duration>,
    /// Mapping from Pingora's selected address back to all configured peer
    /// indices that share it. Built once when the balancer is created.
    indices_by_addr: HashMap<std::net::SocketAddr, Vec<usize>>,
    rotation: AtomicUsize,
}

#[async_trait]
impl<S> BackendSelector for WrappedLb<S>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    fn select(&self, key: &[u8]) -> Option<usize> {
        // All configured upstreams are IP addresses; a non-inet backend is
        // treated as unavailable.
        let addr = self.inner.select(key, 256)?.addr.as_inet().cloned()?;
        let indices = self.indices_by_addr.get(&addr)?;
        match indices.len() {
            1 => indices.first().copied(),
            // Several peers share the address with different TLS identities
            // (P2): the pick must be distributed among them. Round-robin and
            // random rotate globally (empty key); `ip_hash` must pin the same
            // client to the same peer, so its (non-empty) client key decides.
            _ => {
                let selected = if key.is_empty() {
                    self.rotation.fetch_add(1, Ordering::Relaxed) % indices.len()
                } else {
                    stable_hash(key) as usize % indices.len()
                };
                indices.get(selected).copied()
            }
        }
    }

    fn probe_interval(&self) -> Option<Duration> {
        self.interval
    }

    async fn probe(&self) {
        self.inner.backends().run_health_check(true).await;
    }
}

/// A small deterministic (FNV-1a) hash of `bytes`. Used to pin `ip_hash`
/// clients to one peer among several that share an address (P2): the mapping
/// must be stable across restarts, so the process-random `DefaultHasher` is not
/// used here.
fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(policy: LbPolicy, upstreams: &[&str]) -> LbSpec {
        let upstreams = upstreams
            .iter()
            .map(|s| UpstreamPeer {
                addr: s.parse().expect("test upstream"),
                tls: false,
                http_version: crate::config::ast::UpstreamHttpVersion::Auto,
                host: String::new(),
            })
            .collect();
        LbSpec {
            upstreams,
            policy,
            health_check: None,
        }
    }

    #[test]
    fn same_spec_reuses_balancer() {
        let pool = LoadBalancerPool::new();
        let key = SiteKey::CatchAll { port: 8080 };
        let s = spec(LbPolicy::RoundRobin, &["127.0.0.1:1", "127.0.0.1:2"]);
        let first = pool.balancer_for(&key, 0, &s);
        let second = pool.balancer_for(&key, 0, &s);
        assert!(
            Arc::ptr_eq(&first, &second),
            "unchanged spec must reuse the balancer"
        );
    }

    #[test]
    fn changed_upstreams_rebuilds() {
        let pool = LoadBalancerPool::new();
        let key = SiteKey::CatchAll { port: 8080 };
        let a_spec = spec(LbPolicy::RoundRobin, &["127.0.0.1:1"]);
        let b_spec = spec(LbPolicy::RoundRobin, &["127.0.0.1:2"]);
        let a = pool.balancer_for(&key, 0, &a_spec);
        let b = pool.balancer_for(&key, 0, &b_spec);
        assert!(
            !Arc::ptr_eq(&a, &b),
            "changed spec must rebuild the balancer"
        );
    }

    #[test]
    fn request_lookup_does_not_replace_newer_cached_entry() {
        let pool = LoadBalancerPool::new();
        let key = SiteKey::CatchAll { port: 8080 };
        let current_spec = spec(LbPolicy::RoundRobin, &["127.0.0.1:1"]);
        let old_spec = spec(LbPolicy::RoundRobin, &["127.0.0.1:2"]);
        let current = pool.balancer_for(&key, 0, &current_spec);

        // An in-flight request may still carry an older snapshot after reload.
        // It receives a private fallback, while the current pool entry remains
        // the one warmed for new requests.
        let old = pool.balancer_for_request(
            &key,
            0,
            &old_spec.upstreams,
            old_spec.policy,
            old_spec.health_check,
        );
        let after = pool.balancer_for(&key, 0, &current_spec);
        assert!(!Arc::ptr_eq(&current, &old));
        assert!(Arc::ptr_eq(&current, &after));
    }

    #[test]
    fn distinct_terminals_have_distinct_balancers() {
        let pool = LoadBalancerPool::new();
        let key = SiteKey::CatchAll { port: 8080 };
        let s = spec(LbPolicy::RoundRobin, &["127.0.0.1:1", "127.0.0.1:2"]);
        let a = pool.balancer_for(&key, 0, &s);
        let b = pool.balancer_for(&key, 1, &s);
        assert!(
            !Arc::ptr_eq(&a, &b),
            "different terminals must not share a balancer"
        );
    }

    #[test]
    fn reconcile_prunes_removed_site() {
        use crate::config::ast::{CompiledConfig, GlobalConfig};

        let pool = LoadBalancerPool::new();
        let key = SiteKey::CatchAll { port: 8080 };
        let s = spec(LbPolicy::RoundRobin, &["127.0.0.1:1"]);
        let first = pool.balancer_for(&key, 0, &s);

        // Reconcile against a config with no sites: the entry must be pruned.
        let empty = CompiledConfig {
            global: GlobalConfig::default(),
            sites: vec![],
            layer4: vec![],
        };
        pool.reconcile(&empty);
        let rebuilt = pool.balancer_for(&key, 0, &s);
        assert!(
            !Arc::ptr_eq(&first, &rebuilt),
            "a pruned entry must not be reused"
        );
    }

    #[tokio::test]
    async fn round_robin_rotates_and_skips_unhealthy() {
        let pool = LoadBalancerPool::new();
        let key = SiteKey::CatchAll { port: 8080 };
        // 127.0.0.1:1 and :2 have nothing listening, but with no health check
        // every backend is "ready", so selection still round-robins.
        let balancer = pool.balancer_for(
            &key,
            0,
            &spec(LbPolicy::RoundRobin, &["127.0.0.1:1", "127.0.0.1:2"]),
        );
        let a = balancer.select(b"").unwrap();
        let b = balancer.select(b"").unwrap();
        assert_ne!(a, b);
        assert_eq!(balancer.select(b"").unwrap(), a, "third pick wraps back");
        // No health check configured → no probing.
        assert_eq!(balancer.probe_interval(), None);
    }

    #[test]
    fn same_addr_peers_with_distinct_tls_rotate() {
        let pool = LoadBalancerPool::new();
        let key = SiteKey::CatchAll { port: 8080 };
        // Two TLS peers sharing an address but carrying different hostnames must
        // both be reachable (P2): selection alternates between their indices so
        // the caller builds the correct SNI for each.
        let mut spec = spec(LbPolicy::RoundRobin, &["10.0.0.1:443"]);
        spec.upstreams[0].tls = true;
        spec.upstreams[0].host = "a.example.com".into();
        spec.upstreams.push(UpstreamPeer {
            addr: "10.0.0.1:443".parse().unwrap(),
            tls: true,
            http_version: crate::config::ast::UpstreamHttpVersion::Auto,
            host: "b.example.com".into(),
        });
        let balancer = pool.balancer_for(&key, 0, &spec);
        let a = balancer.select(b"").unwrap();
        let b = balancer.select(b"").unwrap();
        assert_ne!(
            a, b,
            "same-address peers with distinct TLS identity must alternate"
        );
        assert!(
            a < 2 && b < 2,
            "selection must be an index into the two upstreams (got {a}, {b})"
        );
    }

    #[test]
    fn ip_hash_pins_same_client_across_same_addr_tls_peers() {
        // A4: two TLS peers sharing one address (P2). `ip_hash` must stick the
        // same client to the same peer — the old global rotation made successive
        // requests from one client alternate SNI identities.
        let pool = LoadBalancerPool::new();
        let key = SiteKey::CatchAll { port: 8080 };
        let mut spec = spec(LbPolicy::IpHash, &["10.0.0.1:443"]);
        spec.upstreams[0].tls = true;
        spec.upstreams[0].host = "a.example.com".into();
        spec.upstreams.push(UpstreamPeer {
            addr: "10.0.0.1:443".parse().unwrap(),
            tls: true,
            http_version: crate::config::ast::UpstreamHttpVersion::Auto,
            host: "b.example.com".into(),
        });
        let balancer = pool.balancer_for(&key, 0, &spec);

        // The same client key always selects the same index; distinct clients
        // are distributed (with two peers, some must differ).
        let client_a = b"203.0.113.9".as_slice();
        let pick = balancer.select(client_a).unwrap();
        for _ in 0..10 {
            assert_eq!(
                balancer.select(client_a).unwrap(),
                pick,
                "ip_hash must not rotate same-address TLS peers for one client"
            );
        }
        let mut distinct = false;
        for n in 0..16u32 {
            if balancer
                .select(format!("203.0.113.{n}").as_bytes())
                .unwrap()
                != pick
            {
                distinct = true;
                break;
            }
        }
        assert!(distinct, "distinct clients should reach both TLS peers");
    }
}
