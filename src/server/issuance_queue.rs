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

//! Bounded, per-host-coordinated ACME issuance queue (B3a).
//!
//! Replaces the v0.1 unbounded `std::sync::mpsc::channel`: producers (startup,
//! the on-demand SNI-miss path, the renewal scheduler) call [`AcmeQueue::enqueue`],
//! which never blocks the request thread; the single issuance worker drains the
//! queue serially and reports each attempt's outcome through
//! [`AcmeQueue::complete`].
//!
//! Every resource is bounded:
//! - the pending queue holds at most `capacity` requests;
//! - the per-host state table holds at most `max_hosts` hosts (derived at
//!   startup from the authorized configured hosts, so it cannot grow with
//!   unconfigured SNI misses);
//! - a failed host is cooled down for a fixed backoff before it can be requeued,
//!   so a host that ACME rejects is not re-enqueued on every SNI miss.
//!
//! A host may have at most one pending or in-flight request. A `Renew` that
//! arrives while a `New` is pending or in flight is remembered (`force_owed`)
//! and drained as a forced attempt once the current attempt finishes — with the
//! cooldown honored on failure — so a renewal is never permanently downgraded
//! to a no-op `New`. A `Renew` that arrives while a `Renew` is already pending
//! or in flight is covered by that live attempt and is dropped, so a renewal
//! that outlasts the scheduler's scan interval cannot accumulate one follow-up
//! attempt per tick. Idle hosts whose failure cooldown has fully elapsed are
//! reclaimed when a new host is enqueued, so a config that churns through failed
//! hosts cannot pin the bounded table full and starve a newly configured host.
//! A full queue rolls back cleanly (no "ghost" pending host) and is observable
//! through [`EnqueueOutcome::QueueFull`].

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long a failed issuance keeps its host out of the queue.
const COOLDOWN: Duration = Duration::from_secs(600);
/// Upper bound on one worker wait. The worker re-scans at least this often, so
/// a host whose cooldown expires (or an owed forced attempt) is retried
/// promptly even when no producer enqueues anything new.
const MAX_WAIT: Duration = Duration::from_secs(30);

/// The kind of issuance requested for a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// Issue only if the host has no certificate yet (startup + on-demand).
    New,
    /// Re-issue even though a (soon-to-expire) certificate exists (renewal).
    Renew,
}

impl RequestKind {
    /// Whether an attempt of this kind replaces an existing certificate.
    pub fn force(self) -> bool {
        matches!(self, RequestKind::Renew)
    }
}

/// What happened when a producer asked to enqueue a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// The request was queued and will be attempted.
    Queued,
    /// A live request (or an owed forced renewal) already covers this one;
    /// nothing new queued and no follow-up is owed.
    Duplicate,
    /// A `Renew` upgraded an in-line `New`; a forced attempt is owed later.
    UpgradeForced,
    /// The host is cooling down after a failed attempt; nothing queued.
    InCooldown,
    /// The queue (or host table) is full; nothing queued and no state left behind.
    QueueFull,
}

/// One queued work item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedRequest {
    pub(crate) host: String,
    pub(crate) kind: RequestKind,
}

/// Per-host issuance state.
#[derive(Debug, Default)]
struct HostState {
    /// A request is queued but not yet started.
    pending: bool,
    /// A request is being processed right now.
    in_flight: bool,
    /// The kind of the live pending/in-flight request, if any. `Some` exactly
    /// while `pending || in_flight`, so an incoming `Renew` can tell whether the
    /// active attempt already is a renewal (covered) or a `New` (upgradeable).
    current_kind: Option<RequestKind>,
    /// A forced (renewal) attempt is owed after the current one finishes.
    force_owed: bool,
    /// No new requests before this time (after a failed attempt).
    cooldown_until: Option<Instant>,
}

/// The pure state machine, kept separate from the blocking machinery so it can
/// be unit-tested deterministically with injected clocks.
#[derive(Debug)]
struct Inner {
    queue: VecDeque<QueuedRequest>,
    states: HashMap<String, HostState>,
    capacity: usize,
    max_hosts: usize,
    cooldown: Duration,
}

impl Inner {
    fn new(capacity: usize, max_hosts: usize, cooldown: Duration) -> Self {
        Self {
            queue: VecDeque::new(),
            states: HashMap::new(),
            capacity,
            max_hosts,
            cooldown,
        }
    }

    /// Try to queue `kind` for `host` at `now`. Never blocks and never leaves a
    /// "ghost" state entry behind on failure.
    fn enqueue(&mut self, host: &str, kind: RequestKind, now: Instant) -> EnqueueOutcome {
        let is_new_host = !self.states.contains_key(host);
        if is_new_host {
            // Reclaim slots held by idle hosts whose cooldown has fully elapsed
            // before enforcing the host-table bound, so a config that churns
            // through failed hosts cannot starve a newly configured host. Hosts
            // that are pending, in flight, owed a forced renewal, or still in
            // their backoff are never reclaimed.
            self.reclaim_expired_idle(now);
            if self.states.len() >= self.max_hosts {
                return EnqueueOutcome::QueueFull;
            }
        }
        let state = self.states.entry(host.to_string()).or_default();

        // A host with a live attempt can only merge into that attempt. The
        // live attempt's kind decides: a New is always redundant; a Renew is
        // covered (Duplicate) when the live attempt already is a Renew or a
        // prior Renew already upgraded this New, and otherwise marks exactly
        // one owed forced attempt.
        if state.pending || state.in_flight {
            return match (kind, state.current_kind) {
                (RequestKind::New, _) => EnqueueOutcome::Duplicate,
                (RequestKind::Renew, Some(RequestKind::New)) if !state.force_owed => {
                    state.force_owed = true;
                    EnqueueOutcome::UpgradeForced
                }
                _ => EnqueueOutcome::Duplicate,
            };
        }
        // A cooling-down host is rejected until its cooldown expires. Renewals
        // are retried by the periodic scheduler scan, so rejecting during the
        // backoff does not lose the renewal forever.
        if state.cooldown_until.is_some_and(|until| now < until) {
            return EnqueueOutcome::InCooldown;
        }
        // A forced attempt is already owed; it will issue the certificate, so
        // both a New and a Renew are redundant.
        if state.force_owed {
            return EnqueueOutcome::Duplicate;
        }
        // Fresh slot: respect the queue capacity, rolling back the state entry
        // inserted above so a full queue leaves no ghost pending host.
        if self.queue.len() >= self.capacity {
            if is_new_host {
                self.states.remove(host);
            }
            return EnqueueOutcome::QueueFull;
        }
        self.queue.push_back(QueuedRequest {
            host: host.to_string(),
            kind,
        });
        state.pending = true;
        state.current_kind = Some(kind);
        EnqueueOutcome::Queued
    }

    /// Remove table entries for hosts that are neither pending, nor in flight,
    /// nor owed a forced attempt, and whose failure cooldown has fully elapsed.
    fn reclaim_expired_idle(&mut self, now: Instant) {
        self.states.retain(|_, state| {
            state.pending
                || state.in_flight
                || state.force_owed
                || state.cooldown_until.is_some_and(|until| now < until)
        });
    }

    /// Pop the next queued request and mark it in flight. `None` if empty.
    fn pop(&mut self) -> Option<QueuedRequest> {
        let request = self.queue.pop_front()?;
        if let Some(state) = self.states.get_mut(&request.host) {
            state.pending = false;
            state.in_flight = true;
            state.current_kind = Some(request.kind);
        }
        Some(request)
    }

    /// If some host is owed a forced attempt and is free to start one, take it
    /// and mark it in flight. Called when the queue is empty, so an owed renewal
    /// is honored without ever starving queued hosts.
    fn pop_due_forced(&mut self, now: Instant) -> Option<QueuedRequest> {
        let host = self.states.iter().find_map(|(host, state)| {
            (state.force_owed
                && !state.pending
                && !state.in_flight
                && state.cooldown_until.is_none_or(|until| now >= until))
            .then(|| host.clone())
        })?;
        let state = self.states.get_mut(&host).expect("just found");
        state.force_owed = false;
        state.in_flight = true;
        state.current_kind = Some(RequestKind::Renew);
        Some(QueuedRequest {
            host,
            kind: RequestKind::Renew,
        })
    }

    /// Finish an attempt for `host`. On success the host is cleared (and, when
    /// nothing is owed, removed from the table); a forced attempt owed while a
    /// `Renew` upgraded the in-line `New` is requeued, or left owed when the
    /// queue is full. On failure the host enters cooldown and `force_owed` is
    /// kept, so the owed renewal still runs after the backoff.
    fn complete(&mut self, host: &str, success: bool, now: Instant) {
        // Decide cleanup inside a block so the mutable borrow of `states` ends
        // before the (possibly) removing `remove` call below.
        let should_remove = {
            let Some(state) = self.states.get_mut(host) else {
                return;
            };
            let force_owed = state.force_owed;
            state.pending = false;
            state.in_flight = false;
            state.current_kind = None;
            if success {
                if force_owed && self.queue.len() < self.capacity {
                    self.queue.push_back(QueuedRequest {
                        host: host.to_string(),
                        kind: RequestKind::Renew,
                    });
                    state.pending = true;
                    state.force_owed = false;
                    state.current_kind = Some(RequestKind::Renew);
                }
                !force_owed
            } else {
                state.cooldown_until = Some(now + self.cooldown);
                // force_owed retained for the post-cooldown forced attempt.
                false
            }
        };
        if should_remove {
            self.states.remove(host);
        }
    }

    /// How long the worker may sleep before it must re-scan: until the next
    /// cooldown expiry, bounded by [`MAX_WAIT`].
    fn wait_duration(&self, now: Instant) -> Duration {
        let until_next = self
            .states
            .values()
            .filter_map(|state| state.cooldown_until)
            .filter(|until| *until > now)
            .map(|until| until.saturating_duration_since(now))
            .min();
        until_next.map(|d| d.min(MAX_WAIT)).unwrap_or(MAX_WAIT)
    }
}

/// The bounded queue plus the worker's blocking machinery.
///
/// Producers use [`AcmeQueue::enqueue`] (non-blocking); the worker thread owns
/// the private [`next_request`](Self::next_request)/[`complete`](Self::complete)
/// pair.
pub struct AcmeQueue {
    inner: Mutex<Inner>,
    condvar: Condvar,
}

impl AcmeQueue {
    /// Create a queue with `capacity` pending slots and at most `max_hosts`
    /// tracked hosts, using the production failure backoff.
    pub fn new(capacity: usize, max_hosts: usize) -> Arc<Self> {
        Arc::new(Self::with_cooldown(capacity, max_hosts, COOLDOWN))
    }

    /// Variant with an explicit failure backoff (used by deterministic tests).
    pub fn with_cooldown(capacity: usize, max_hosts: usize, cooldown: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner::new(capacity, max_hosts, cooldown)),
            condvar: Condvar::new(),
        }
    }

    /// Queue a request for `host`. Never blocks the caller (a request thread,
    /// the startup path, or the renewal scheduler) and never leaves a ghost
    /// pending entry behind; a full queue is reported through the outcome.
    pub fn enqueue(&self, host: &str, kind: RequestKind) -> EnqueueOutcome {
        let mut inner = self.inner.lock().expect("issuance queue lock poisoned");
        let outcome = inner.enqueue(host, kind, Instant::now());
        if outcome == EnqueueOutcome::Queued {
            self.condvar.notify_one();
        }
        outcome
    }

    /// Block until a request is ready, then return it marked in flight. The
    /// worker owns the single call site.
    pub(crate) fn next_request(&self) -> QueuedRequest {
        let mut inner = self.inner.lock().expect("issuance queue lock poisoned");
        loop {
            let now = Instant::now();
            if let Some(request) = inner.pop() {
                return request;
            }
            if let Some(request) = inner.pop_due_forced(now) {
                return request;
            }
            let wait = inner.wait_duration(now);
            let (guard, _) = self
                .condvar
                .wait_timeout(inner, wait)
                .expect("issuance queue lock poisoned");
            inner = guard;
        }
    }

    /// Report an attempt's outcome to the state machine and wake anyone waiting
    /// (a freed queue slot, an expired cooldown, an owed forced attempt).
    pub(crate) fn complete(&self, host: &str, success: bool) {
        let mut inner = self.inner.lock().expect("issuance queue lock poisoned");
        inner.complete(host, success, Instant::now());
        self.condvar.notify_all();
    }

    /// Number of tracked hosts (test hook).
    #[cfg(test)]
    fn host_count(&self) -> usize {
        self.inner
            .lock()
            .expect("issuance queue lock poisoned")
            .states
            .len()
    }

    /// Number of pending requests (test hook).
    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.inner
            .lock()
            .expect("issuance queue lock poisoned")
            .queue
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inner(capacity: usize, max_hosts: usize) -> Inner {
        Inner::new(capacity, max_hosts, COOLDOWN)
    }

    #[test]
    fn duplicate_new_is_merged() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        assert_eq!(
            sm.enqueue("a.test", RequestKind::New, t),
            EnqueueOutcome::Queued
        );
        assert_eq!(
            sm.enqueue("a.test", RequestKind::New, t),
            EnqueueOutcome::Duplicate
        );
        assert_eq!(
            sm.queue.len(),
            1,
            "a second New must not enqueue a second slot"
        );
    }

    #[test]
    fn renew_upgrades_pending_new() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        assert_eq!(
            sm.enqueue("a.test", RequestKind::Renew, t),
            EnqueueOutcome::UpgradeForced
        );
        assert!(
            sm.states["a.test"].force_owed,
            "the Renew must be remembered as an owed forced attempt"
        );
        assert_eq!(sm.queue.len(), 1, "no extra slot is used by the upgrade");
    }

    #[test]
    fn renew_upgrades_pending_new_only_once() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        assert_eq!(
            sm.enqueue("a.test", RequestKind::Renew, t),
            EnqueueOutcome::UpgradeForced
        );
        assert_eq!(
            sm.enqueue("a.test", RequestKind::Renew, t),
            EnqueueOutcome::Duplicate,
            "a second Renew over the same active New is already covered"
        );
        assert!(sm.states["a.test"].force_owed);
        assert_eq!(sm.queue.len(), 1, "only one follow-up is ever owed");
    }

    #[test]
    fn renew_during_pending_renew_is_duplicate() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        assert_eq!(
            sm.enqueue("a.test", RequestKind::Renew, t),
            EnqueueOutcome::Queued
        );
        assert_eq!(
            sm.enqueue("a.test", RequestKind::Renew, t),
            EnqueueOutcome::Duplicate,
            "a Renew while a Renew is pending is covered by that attempt"
        );
        assert_eq!(sm.queue.len(), 1, "no follow-up slot is added");
        let request = sm.pop().unwrap();
        assert_eq!(request.kind, RequestKind::Renew);
        sm.complete("a.test", true, t);
        assert_eq!(
            sm.pop(),
            None,
            "a duplicated Renew must not spawn a follow-up attempt"
        );
        assert!(
            !sm.states.contains_key("a.test"),
            "a Renew that was covered leaves no state behind"
        );
    }

    #[test]
    fn renew_during_in_flight_renew_is_duplicate() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::Renew, t);
        let request = sm.pop().unwrap();
        assert_eq!(request.kind, RequestKind::Renew);
        assert_eq!(
            sm.enqueue("a.test", RequestKind::Renew, t),
            EnqueueOutcome::Duplicate,
            "a Renew while a Renew is in flight is covered by that attempt"
        );
        sm.complete("a.test", true, t);
        assert_eq!(sm.pop(), None, "no follow-up is spawned");
        assert!(!sm.states.contains_key("a.test"));
    }

    #[test]
    fn renew_during_owed_forced_is_duplicate() {
        // A forced renewal is already owed (New upgraded, completed while the
        // queue was full so the owed attempt stays not-yet-started): a further
        // Renew is covered by that owed attempt, not a second upgrade.
        let mut sm = inner(1, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        sm.enqueue("a.test", RequestKind::Renew, t);
        let _first = sm.pop().unwrap();
        sm.enqueue("b.test", RequestKind::New, t); // fills the queue
        sm.complete("a.test", true, t); // queue full -> force_owed retained
        assert!(sm.states["a.test"].force_owed);
        assert_eq!(
            sm.enqueue("a.test", RequestKind::Renew, t),
            EnqueueOutcome::Duplicate,
            "the owed forced attempt already covers the renewal"
        );
        assert_eq!(
            sm.pop_due_forced(t).unwrap().kind,
            RequestKind::Renew,
            "exactly one owed forced attempt drains"
        );
    }

    #[test]
    fn renew_upgrade_produces_forced_attempt_after_success() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        sm.enqueue("a.test", RequestKind::Renew, t);

        let first = sm.pop().expect("the New is queued first");
        assert_eq!(first.kind, RequestKind::New);
        sm.complete("a.test", true, t);

        let follow_up = sm.pop().expect("the owed forced attempt is queued");
        assert_eq!(follow_up.host, "a.test");
        assert_eq!(
            follow_up.kind,
            RequestKind::Renew,
            "must be a forced renewal"
        );
        assert!(follow_up.kind.force());
    }

    #[test]
    fn renew_upgrade_produces_forced_attempt_after_cooldown() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        sm.enqueue("a.test", RequestKind::Renew, t);
        let _first = sm.pop().unwrap();
        sm.complete("a.test", false, t); // failure: cooldown + force_owed kept

        // While cooling down nothing drains, including the owed forced attempt.
        assert_eq!(sm.pop(), None);
        assert_eq!(sm.pop_due_forced(t), None);

        // After the backoff the owed forced renewal runs.
        let after = t + Duration::from_secs(601);
        let owed = sm
            .pop_due_forced(after)
            .expect("owed forced attempt drains");
        assert_eq!(owed.host, "a.test");
        assert_eq!(owed.kind, RequestKind::Renew);
    }

    #[test]
    fn failure_enters_cooldown_and_other_hosts_continue() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        let _req = sm.pop().unwrap();
        sm.complete("a.test", false, t);

        assert_eq!(
            sm.enqueue("a.test", RequestKind::New, t),
            EnqueueOutcome::InCooldown
        );
        assert_eq!(
            sm.enqueue("a.test", RequestKind::Renew, t),
            EnqueueOutcome::InCooldown
        );
        // A different host is unaffected.
        assert_eq!(
            sm.enqueue("b.test", RequestKind::New, t),
            EnqueueOutcome::Queued
        );
        let req_b = sm.pop().unwrap();
        assert_eq!(req_b.host, "b.test");
        sm.complete("b.test", true, t);
        assert_eq!(
            sm.enqueue("c.test", RequestKind::New, t),
            EnqueueOutcome::Queued
        );
    }

    #[test]
    fn cooldown_expiry_allows_retry() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        let _req = sm.pop().unwrap();
        sm.complete("a.test", false, t);
        assert_eq!(
            sm.enqueue("a.test", RequestKind::New, t),
            EnqueueOutcome::InCooldown
        );

        let after = t + Duration::from_secs(601);
        assert_eq!(
            sm.enqueue("a.test", RequestKind::New, after),
            EnqueueOutcome::Queued
        );
    }

    #[test]
    fn queue_full_rolls_back_without_ghost_pending() {
        let mut sm = inner(1, 4);
        let t = Instant::now();
        assert_eq!(
            sm.enqueue("a.test", RequestKind::New, t),
            EnqueueOutcome::Queued
        );
        // Queue is at capacity: b must not be accepted...
        assert_eq!(
            sm.enqueue("b.test", RequestKind::New, t),
            EnqueueOutcome::QueueFull
        );
        // ...and must leave no ghost state behind.
        assert!(
            !sm.states.contains_key("b.test"),
            "a rejected host must not remain in the state table"
        );
        assert_eq!(sm.states.len(), 1);
    }

    #[test]
    fn queue_full_keeps_owed_forced_attempt() {
        let mut sm = inner(1, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        sm.enqueue("a.test", RequestKind::Renew, t);
        let _first = sm.pop().unwrap(); // queue now empty
                                        // Another host fills the queue before a's attempt completes.
        assert_eq!(
            sm.enqueue("b.test", RequestKind::New, t),
            EnqueueOutcome::Queued
        );
        sm.complete("a.test", true, t);

        // The owed forced attempt is kept (queue full), not dropped.
        assert!(sm.states["a.test"].force_owed);
        let req_b = sm.pop().unwrap();
        assert_eq!(req_b.host, "b.test");
        let owed = sm.pop_due_forced(t).unwrap();
        assert_eq!(owed.host, "a.test");
        assert_eq!(owed.kind, RequestKind::Renew);
    }

    #[test]
    fn success_clears_state() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        let _req = sm.pop().unwrap();
        sm.complete("a.test", true, t);

        assert!(
            !sm.states.contains_key("a.test"),
            "a successful host with nothing owed is reclaimed"
        );
        // A later New is a fresh, accepted request.
        assert_eq!(
            sm.enqueue("a.test", RequestKind::New, t),
            EnqueueOutcome::Queued
        );
    }

    #[test]
    fn host_limit_is_enforced() {
        let mut sm = inner(4, 2);
        let t = Instant::now();
        assert_eq!(
            sm.enqueue("a.test", RequestKind::New, t),
            EnqueueOutcome::Queued
        );
        assert_eq!(
            sm.enqueue("b.test", RequestKind::New, t),
            EnqueueOutcome::Queued
        );
        assert_eq!(
            sm.enqueue("c.test", RequestKind::New, t),
            EnqueueOutcome::QueueFull
        );
        assert!(!sm.states.contains_key("c.test"));
    }

    #[test]
    fn expired_idle_hosts_reclaimed_before_host_limit() {
        let mut sm = inner(4, 2);
        let t = Instant::now();
        // Two failed hosts sit idle in cooldown, filling the host table.
        for host in ["a.test", "b.test"] {
            assert_eq!(
                sm.enqueue(host, RequestKind::New, t),
                EnqueueOutcome::Queued
            );
            let _req = sm.pop().unwrap();
            sm.complete(host, false, t);
        }
        assert_eq!(sm.states.len(), 2);

        let after = t + Duration::from_secs(601); // both cooldowns expired
        assert_eq!(
            sm.enqueue("c.test", RequestKind::New, after),
            EnqueueOutcome::Queued,
            "expired idle hosts must yield their slots to a new host"
        );
        assert!(!sm.states.contains_key("a.test"));
        assert!(!sm.states.contains_key("b.test"));
        assert!(sm.states.contains_key("c.test"));
    }

    #[test]
    fn unexpired_idle_hosts_not_reclaimed() {
        let mut sm = inner(4, 2);
        let t = Instant::now();
        for host in ["a.test", "b.test"] {
            sm.enqueue(host, RequestKind::New, t);
            let _req = sm.pop().unwrap();
            sm.complete(host, false, t);
        }
        // Still inside the backoff window: neither host may be reclaimed, so
        // the new host is still rejected by the host-table bound.
        let soon = t + Duration::from_secs(100);
        assert_eq!(
            sm.enqueue("c.test", RequestKind::New, soon),
            EnqueueOutcome::QueueFull
        );
        assert!(sm.states.contains_key("a.test"));
        assert!(sm.states.contains_key("b.test"));
        assert!(!sm.states.contains_key("c.test"));
    }

    #[test]
    fn force_owed_host_not_reclaimed() {
        let mut sm = inner(1, 2);
        let t = Instant::now();
        // Build an idle host with a retained owed forced renewal: New upgraded
        // by Renew, completed successfully while the queue was full.
        sm.enqueue("a.test", RequestKind::New, t);
        sm.enqueue("a.test", RequestKind::Renew, t);
        let _req = sm.pop().unwrap();
        sm.enqueue("b.test", RequestKind::New, t); // fills the queue
        sm.complete("a.test", true, t); // queue full -> force_owed retained
        assert!(
            sm.states["a.test"].force_owed
                && !sm.states["a.test"].pending
                && !sm.states["a.test"].in_flight
        );
        // Fail b so it becomes a reclaimable expired-idle host next to a.
        let _req_b = sm.pop().unwrap();
        sm.complete("b.test", false, t);

        let after = t + Duration::from_secs(601); // b's cooldown expired
        assert_eq!(
            sm.enqueue("c.test", RequestKind::New, after),
            EnqueueOutcome::Queued
        );
        assert!(
            sm.states.contains_key("a.test"),
            "a host owed a forced renewal must never be reclaimed"
        );
        assert!(sm.states["a.test"].force_owed);
        assert!(
            !sm.states.contains_key("b.test"),
            "the expired idle host is reclaimed"
        );
        assert!(sm.states.contains_key("c.test"));
    }

    #[test]
    fn acme_queue_enqueue_tracks_state() {
        // Exercise the public wrapper (Mutex + Condvar), not just the pure
        // state machine: duplicate merging and state accounting must hold.
        let queue = AcmeQueue::with_cooldown(2, 4, COOLDOWN);
        assert_eq!(
            queue.enqueue("a.test", RequestKind::New),
            EnqueueOutcome::Queued
        );
        assert_eq!(
            queue.enqueue("a.test", RequestKind::New),
            EnqueueOutcome::Duplicate
        );
        assert_eq!(queue.pending_len(), 1);
        assert_eq!(queue.host_count(), 1);
    }

    #[test]
    fn acme_queue_next_request_marks_in_flight() {
        let queue = AcmeQueue::with_cooldown(2, 4, COOLDOWN);
        queue.enqueue("a.test", RequestKind::New);
        let request = queue.next_request();
        assert_eq!(request.host, "a.test");
        assert_eq!(request.kind, RequestKind::New);
        // In flight: a New merges, a Renew upgrades to an owed forced attempt.
        assert_eq!(
            queue.enqueue("a.test", RequestKind::New),
            EnqueueOutcome::Duplicate
        );
        assert_eq!(
            queue.enqueue("a.test", RequestKind::Renew),
            EnqueueOutcome::UpgradeForced
        );
        queue.complete("a.test", true);
        assert_eq!(
            queue.pending_len(),
            1,
            "the owed forced renewal is requeued"
        );
    }

    #[test]
    fn wait_duration_tracks_cooldown_expiry() {
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        let _req = sm.pop().unwrap();
        sm.complete("a.test", false, t);

        // The worker may sleep only until the cooldown expires (bounded).
        assert_eq!(sm.wait_duration(t), MAX_WAIT);
        let after = t + Duration::from_secs(601);
        assert_eq!(
            sm.wait_duration(after),
            MAX_WAIT,
            "no cooldown left to wait on"
        );
    }

    #[test]
    fn fresh_renew_is_queued_directly() {
        // A Renew for a host with no live attempt is an ordinary queue item, not
        // a forced upgrade: the renewal path bypasses the has-certificate check
        // entirely (the worker's `issue_for(force=true)`).
        let mut sm = inner(4, 4);
        let t = Instant::now();
        assert_eq!(
            sm.enqueue("a.test", RequestKind::Renew, t),
            EnqueueOutcome::Queued
        );
        let request = sm.pop().unwrap();
        assert_eq!(request.host, "a.test");
        assert_eq!(request.kind, RequestKind::Renew);
        assert!(request.kind.force());
    }

    #[test]
    fn new_during_owed_forced_is_duplicate() {
        // While a forced renewal is owed (but not yet started — the queue was
        // full so `complete` kept it owed instead of requeueing), a New is
        // redundant: the owed forced attempt will issue the certificate anyway.
        let mut sm = inner(1, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        sm.enqueue("a.test", RequestKind::Renew, t);
        let _first = sm.pop().unwrap();
        sm.enqueue("b.test", RequestKind::New, t); // fills the queue
        sm.complete("a.test", true, t); // queue full -> force_owed retained
        assert!(
            sm.states["a.test"].force_owed && !sm.states["a.test"].pending,
            "the forced attempt must be owed but not pending"
        );
        assert_eq!(
            sm.enqueue("a.test", RequestKind::New, t),
            EnqueueOutcome::Duplicate,
            "a New is redundant while a forced renewal is owed"
        );
    }

    #[test]
    fn complete_unknown_host_is_noop() {
        // The worker only ever completes hosts it popped, but a stale completion
        // must not panic or create state (defensive).
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.complete("never-queued.test", true, t);
        sm.complete("never-queued.test", false, t);
        assert_eq!(sm.states.len(), 0);
        assert_eq!(sm.queue.len(), 0);
    }

    #[test]
    fn pop_prefers_queued_over_due_forced() {
        // The worker drains the FIFO before honoring an owed forced attempt, so
        // a queued host is never starved behind a renewal that is already owed.
        let mut sm = inner(4, 4);
        let t = Instant::now();
        sm.enqueue("a.test", RequestKind::New, t);
        sm.enqueue("a.test", RequestKind::Renew, t);
        let _first = sm.pop().unwrap(); // New(a) in flight
        sm.complete("a.test", false, t); // failure: cooldown + force_owed kept
        let after = t + Duration::from_secs(601); // cooldown expired
        sm.enqueue("b.test", RequestKind::New, after);

        // Worker behavior at `after`: `pop()` first, `pop_due_forced()` only
        // once the queue is empty.
        let queued = sm.pop().unwrap();
        assert_eq!(queued.host, "b.test");
        let owed = sm.pop_due_forced(after).unwrap();
        assert_eq!(owed.host, "a.test");
        assert_eq!(owed.kind, RequestKind::Renew);
    }

    #[test]
    fn next_request_wakes_when_producer_enqueues() {
        // A producer's enqueue must wake a parked worker (the startup batch and
        // the on-demand SNI path racing the worker's sleep). Deterministic: the
        // worker either sees the queue before parking or is woken by the notify.
        let queue = Arc::new(AcmeQueue::with_cooldown(4, 4, COOLDOWN));
        let worker = queue.clone();
        let handle = std::thread::spawn(move || worker.next_request());
        assert_eq!(
            queue.enqueue("a.test", RequestKind::New),
            EnqueueOutcome::Queued
        );
        let request = handle.join().unwrap();
        assert_eq!(request.host, "a.test");
        assert_eq!(request.kind, RequestKind::New);
    }

    #[test]
    fn complete_wakes_worker_for_owed_forced_attempt() {
        // Completing an in-flight attempt that left an owed forced renewal must
        // wake a parked worker, so the follow-up runs without waiting out the
        // worker's re-scan.
        let queue = Arc::new(AcmeQueue::with_cooldown(4, 4, COOLDOWN));
        queue.enqueue("a.test", RequestKind::New);
        queue.enqueue("a.test", RequestKind::Renew);
        let first = queue.next_request();
        assert_eq!(first.kind, RequestKind::New);

        let worker = queue.clone();
        let handle = std::thread::spawn(move || worker.next_request());
        queue.complete("a.test", true); // requeues the owed Renew + notifies
        let second = handle.join().unwrap();
        assert_eq!(second.host, "a.test");
        assert_eq!(second.kind, RequestKind::Renew);
    }
}
