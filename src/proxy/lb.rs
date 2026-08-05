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

use crate::config::ast::{HealthCheckSpec, LbPolicy, SiteKey, TerminalKind};
use async_trait::async_trait;
use pingora::lb::health_check;
use pingora::lb::selection::{BackendIter, BackendSelection, Consistent, Random, RoundRobin};
use pingora::lb::LoadBalancer;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
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
    /// other policies). Returns `None` when every backend is unhealthy.
    fn select(&self, key: &[u8]) -> Option<SocketAddr>;
    /// The health-check probe interval, if a health check is configured.
    fn probe_interval(&self) -> Option<Duration>;
    /// Run one round of health checks (no-op when none is configured).
    async fn probe(&self);
}

/// The specification that determines when a balancer must be rebuilt (ADR-011).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LbSpec {
    pub upstreams: Vec<SocketAddr>,
    pub policy: LbPolicy,
    pub health_check: Option<HealthCheckSpec>,
}

/// Process-lifetime pool of per-(site, terminal) load balancers.
#[derive(Default)]
pub struct LoadBalancerPool {
    entries: Mutex<HashMap<(SiteKey, usize), PoolEntry>>,
}

struct PoolEntry {
    spec: LbSpec,
    balancer: Arc<dyn BackendSelector>,
    /// When this balancer was last probed (`None` = never, so due immediately).
    last_probe: Option<Instant>,
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
        spec: LbSpec,
    ) -> Arc<dyn BackendSelector> {
        let mut entries = self.entries.lock().expect("lb pool lock poisoned");
        let key = (site_key.clone(), terminal_index);
        if let Some(entry) = entries.get(&key) {
            if entry.spec == spec {
                return entry.balancer.clone();
            }
        }
        let balancer = build_balancer(&spec);
        entries.insert(
            key,
            PoolEntry {
                spec,
                balancer: balancer.clone(),
                last_probe: None,
            },
        );
        balancer
    }

    /// Pre-build a balancer for every reverse-proxy terminal in the snapshot,
    /// so health checks begin immediately at startup instead of only after the
    /// first request reaches that terminal.
    pub fn warm(&self, config: &crate::config::ast::CompiledConfig) {
        for site in &config.sites {
            for (index, terminal) in site.terminals.iter().enumerate() {
                let TerminalKind::ReverseProxy {
                    upstreams,
                    lb_policy,
                    health_check,
                } = &terminal.kind
                else {
                    continue;
                };
                self.balancer_for(
                    &site.key,
                    index,
                    LbSpec {
                        upstreams: upstreams.clone(),
                        policy: *lb_policy,
                        health_check: *health_check,
                    },
                );
            }
        }
    }

    /// Drop balancers for (site, terminal) pairs that no longer exist in the
    /// snapshot. Called after every reload so removing a site stops its health
    /// probes instead of probing a decommissioned upstream forever.
    pub fn reconcile(&self, config: &crate::config::ast::CompiledConfig) {
        let mut entries = self.entries.lock().expect("lb pool lock poisoned");
        entries.retain(|key, _| Self::live_keys(config).contains(key));
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
        let mut entries = self.entries.lock().expect("lb pool lock poisoned");
        entries
            .values_mut()
            .filter_map(|entry| {
                let interval = entry.balancer.probe_interval()?;
                let due = match entry.last_probe {
                    None => true,
                    Some(last) => now.duration_since(last) >= interval,
                };
                if due {
                    entry.last_probe = Some(now);
                    Some(entry.balancer.clone())
                } else {
                    None
                }
            })
            .collect()
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
    let addrs: Vec<String> = spec.upstreams.iter().map(|a| a.to_string()).collect();
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
    })
}

/// Concrete erased balancer for a given selection algorithm.
struct WrappedLb<S: BackendSelection> {
    inner: LoadBalancer<S>,
    interval: Option<Duration>,
}

#[async_trait]
impl<S> BackendSelector for WrappedLb<S>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    fn select(&self, key: &[u8]) -> Option<SocketAddr> {
        // All configured upstreams are IP addresses; a non-inet backend is
        // treated as unavailable.
        self.inner
            .select(key, 256)
            .and_then(|backend| backend.addr.as_inet().cloned())
    }

    fn probe_interval(&self) -> Option<Duration> {
        self.interval
    }

    async fn probe(&self) {
        self.inner.backends().run_health_check(true).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(policy: LbPolicy, upstreams: &[&str]) -> LbSpec {
        let upstreams = upstreams
            .iter()
            .map(|s| s.parse().expect("test upstream"))
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
        let first = pool.balancer_for(&key, 0, s.clone());
        let second = pool.balancer_for(&key, 0, s);
        assert!(
            Arc::ptr_eq(&first, &second),
            "unchanged spec must reuse the balancer"
        );
    }

    #[test]
    fn changed_upstreams_rebuilds() {
        let pool = LoadBalancerPool::new();
        let key = SiteKey::CatchAll { port: 8080 };
        let a = pool.balancer_for(&key, 0, spec(LbPolicy::RoundRobin, &["127.0.0.1:1"]));
        let b = pool.balancer_for(&key, 0, spec(LbPolicy::RoundRobin, &["127.0.0.1:2"]));
        assert!(
            !Arc::ptr_eq(&a, &b),
            "changed spec must rebuild the balancer"
        );
    }

    #[test]
    fn distinct_terminals_have_distinct_balancers() {
        let pool = LoadBalancerPool::new();
        let key = SiteKey::CatchAll { port: 8080 };
        let s = spec(LbPolicy::RoundRobin, &["127.0.0.1:1", "127.0.0.1:2"]);
        let a = pool.balancer_for(&key, 0, s.clone());
        let b = pool.balancer_for(&key, 1, s);
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
        let first = pool.balancer_for(&key, 0, s.clone());

        // Reconcile against a config with no sites: the entry must be pruned.
        let empty = CompiledConfig {
            global: GlobalConfig::default(),
            sites: vec![],
        };
        pool.reconcile(&empty);
        let rebuilt = pool.balancer_for(&key, 0, s);
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
            spec(LbPolicy::RoundRobin, &["127.0.0.1:1", "127.0.0.1:2"]),
        );
        let a = balancer.select(b"").unwrap();
        let b = balancer.select(b"").unwrap();
        assert_ne!(a, b);
        assert_eq!(balancer.select(b"").unwrap(), a, "third pick wraps back");
        // No health check configured → no probing.
        assert_eq!(balancer.probe_interval(), None);
    }
}
