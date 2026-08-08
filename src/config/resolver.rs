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

//! Bounded, timed upstream hostname resolution (B3a).
//!
//! The config plane resolves each configured upstream to concrete addresses at
//! build time (ADR-011). The standard library's `ToSocketAddrs` has no timeout
//! and cannot be cancelled, so it is moved onto a small fixed pool of resolver
//! threads and awaited with an explicit [`RESOLVE_TIMEOUT`]: `raddy check` and
//! SIGHUP reload report a diagnosable timeout error instead of blocking the
//! config plane forever.
//!
//! The pool is a process-lifetime singleton with [`RESOLVER_THREADS`] workers
//! and a bounded job queue. A lookup that hangs in `getaddrinfo` can strand at
//! most one worker thread; the bounded queue then fails fast with an overload
//! error, and reloads never leak an unbounded number of resolver threads.
//! Explicit IP literals bypass the pool entirely (no DNS, no threads).
//!
//! Blocking HTTP clients that must not hang on DNS (the Cloudflare DNS-01
//! provider) reuse the same pool through [`agent_resolver`], because their own
//! resolver cannot be interrupted by a request timeout.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// Per-lookup timeout: a hostname that does not resolve within this is an error
/// (returned to `raddy check`/reload), never a hang.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Number of resolver worker threads. Bounded so a hung `getaddrinfo` can
/// strand at most this many threads for the process lifetime.
const RESOLVER_THREADS: usize = 2;
/// Bounded queue of pending lookups. When every worker is stuck and the queue
/// is full, new lookups fail fast rather than growing memory.
const RESOLVER_QUEUE: usize = 8;

/// One resolution job: a closure plus a one-shot reply channel.
struct Job {
    op: Box<dyn FnOnce() -> io::Result<Vec<SocketAddr>> + Send>,
    reply: Sender<io::Result<Vec<SocketAddr>>>,
}

/// A bounded FIFO of pending lookups shared by the resolver worker threads.
///
/// Pushes are non-blocking (the job is returned untouched when the queue is
/// full); pops block until work arrives. The capacity bounds memory whether or
/// not a worker is stuck in a hung `getaddrinfo`.
struct JobQueue {
    jobs: Mutex<VecDeque<Job>>,
    capacity: usize,
    condvar: Condvar,
}

impl JobQueue {
    fn new(capacity: usize) -> Self {
        Self {
            jobs: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            condvar: Condvar::new(),
        }
    }

    /// Enqueue `job`, or return it when the queue is full. Never blocks.
    fn push(&self, job: Job) -> Result<(), Job> {
        let mut jobs = self.jobs.lock().expect("resolver queue lock poisoned");
        if jobs.len() >= self.capacity {
            return Err(job);
        }
        jobs.push_back(job);
        self.condvar.notify_one();
        Ok(())
    }

    /// Block until a job is available and pop it.
    fn pop(&self) -> Job {
        let mut jobs = self.jobs.lock().expect("resolver queue lock poisoned");
        loop {
            if let Some(job) = jobs.pop_front() {
                return job;
            }
            jobs = self
                .condvar
                .wait(jobs)
                .expect("resolver queue lock poisoned");
        }
    }
}

/// A small fixed pool of resolver threads.
pub(crate) struct Resolver {
    queue: Arc<JobQueue>,
    timeout: Duration,
}

impl Resolver {
    /// The process-lifetime singleton: a fixed pool with the production
    /// timeout. Spawned lazily on the first hostname lookup.
    pub(crate) fn global() -> &'static Resolver {
        static POOL: OnceLock<Resolver> = OnceLock::new();
        POOL.get_or_init(|| {
            Resolver::with_params(RESOLVER_THREADS, RESOLVER_QUEUE, RESOLVE_TIMEOUT)
        })
    }

    /// Construct a pool with the given worker count, queue capacity, and
    /// per-lookup timeout (used by tests with short timeouts and fake ops).
    fn with_params(threads: usize, queue_capacity: usize, timeout: Duration) -> Resolver {
        let queue = Arc::new(JobQueue::new(queue_capacity));
        for _ in 0..threads {
            let queue = queue.clone();
            std::thread::spawn(move || loop {
                let job = queue.pop();
                let result = (job.op)();
                // The caller may have timed out and dropped the receiver; that
                // is fine — nothing more to report.
                let _ = job.reply.send(result);
            });
        }
        Resolver { queue, timeout }
    }

    /// Submit `op` to the pool and wait for its result with the configured
    /// timeout. Returns a diagnosable error on timeout, overload, or failure.
    fn run(
        &self,
        op: impl FnOnce() -> io::Result<Vec<SocketAddr>> + Send + 'static,
    ) -> Result<Vec<SocketAddr>, String> {
        let (reply, rx) = channel();
        let job = Job {
            op: Box::new(op),
            reply,
        };
        self.queue
            .push(job)
            .map_err(|_| "resolver overloaded (too many concurrent lookups)".to_string())?;
        match rx.recv_timeout(self.timeout) {
            Ok(Ok(addrs)) => Ok(addrs),
            Ok(Err(e)) => Err(format!("lookup failed: {e}")),
            Err(RecvTimeoutError::Timeout) => {
                Err(format!("timed out after {}s", self.timeout.as_secs()))
            }
            Err(RecvTimeoutError::Disconnected) => Err("resolver worker stopped".to_string()),
        }
    }
}

/// Resolve `host:port` to all of its socket addresses.
///
/// An explicit IP literal is returned directly (no DNS, no pool), preserving
/// the v0.1 behavior exactly. A hostname goes through the bounded shared pool
/// and is subject to [`RESOLVE_TIMEOUT`].
pub(crate) fn resolve_host(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let host_owned = host.to_string();
    Resolver::global()
        .run(move || {
            (host_owned.as_str(), port)
                .to_socket_addrs()
                .map(|addrs| addrs.collect())
        })
        .map_err(|e| format!("failed to resolve upstream {host}:{port}: {e}"))
}

/// Split a blocking HTTP client's `netloc` (`host:port`) into `(host, port)`.
///
/// ureq passes the authority of the request URL to its custom resolver:
/// `example.com:443`, `127.0.0.1:8080`, or `[::1]:8443` for a bracketed IPv6
/// literal. Returns an `io::Error` on malformed input so the caller surfaces it
/// as a DNS/parse failure rather than panicking.
pub(crate) fn parse_netloc(netloc: &str) -> io::Result<(String, u16)> {
    let (host, port) = if let Some(rest) = netloc.strip_prefix('[') {
        let (host, port) = rest.split_once(']').ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "unterminated IPv6 literal")
        })?;
        let port = port
            .strip_prefix(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing port"))?;
        (host, port)
    } else {
        netloc
            .rsplit_once(':')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing port"))?
    };
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty host"));
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid port"))?;
    Ok((host.to_string(), port))
}

/// A bounded DNS resolver for blocking HTTP clients (ureq).
///
/// ureq's built-in DNS lookup cannot be interrupted by its request timeouts, so
/// a hung `getaddrinfo` would block the caller thread indefinitely. This adapter
/// parses ureq's `netloc` and delegates to the fixed [`Resolver`] pool instead:
/// a hung lookup can strand only the fixed workers, and the caller fails with
/// the pool's [`RESOLVE_TIMEOUT`]. IP literals are returned directly (no DNS).
pub(crate) fn agent_resolver(netloc: &str) -> io::Result<Vec<SocketAddr>> {
    let (host, port) = parse_netloc(netloc)?;
    resolve_host(&host, port).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn addr(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port)
    }

    #[test]
    fn explicit_ip_literal_bypasses_the_pool() {
        // An IP literal must not spawn threads or touch the pool.
        assert_eq!(
            resolve_host("127.0.0.1", 8080).unwrap(),
            vec![addr(127, 0, 0, 1, 8080)]
        );
    }

    #[test]
    fn resolver_returns_all_results_from_the_op() {
        let resolver = Resolver::with_params(1, 4, Duration::from_secs(5));
        let got = resolver
            .run(|| Ok(vec![addr(10, 0, 0, 1, 80), addr(10, 0, 0, 2, 80)]))
            .unwrap();
        assert_eq!(got, vec![addr(10, 0, 0, 1, 80), addr(10, 0, 0, 2, 80)]);
    }

    #[test]
    fn resolver_reports_op_failure() {
        let resolver = Resolver::with_params(1, 4, Duration::from_secs(5));
        let err = resolver
            .run(|| Err(io::Error::new(io::ErrorKind::NotFound, "no such host")))
            .unwrap_err();
        assert!(err.contains("lookup failed"), "got: {err}");
    }

    #[test]
    fn resolver_times_out_a_hanging_lookup() {
        // The op sleeps far longer than the pool's timeout; the caller must get
        // a diagnosable timeout error instead of blocking indefinitely.
        let resolver = Resolver::with_params(1, 4, Duration::from_millis(30));
        let err = resolver
            .run(|| {
                std::thread::sleep(Duration::from_millis(300));
                Ok(vec![addr(10, 0, 0, 1, 80)])
            })
            .unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
    }

    #[test]
    fn resolver_overload_fails_fast() {
        // One worker blocked on a long op, queue of 1: the second queued job
        // fills the queue and the third fails immediately without blocking.
        let resolver = Resolver::with_params(1, 1, Duration::from_millis(30));
        let blocker = resolver.run(|| {
            std::thread::sleep(Duration::from_millis(200));
            Ok(vec![addr(10, 0, 0, 1, 80)])
        });
        let queued = resolver.run(|| Ok(vec![addr(10, 0, 0, 2, 80)]));
        let overload = resolver.run(|| Ok(vec![addr(10, 0, 0, 3, 80)]));
        // The third lookup must not block; the first two may succeed or time
        // out depending on timing, but the overload one reports immediately.
        let overload_err = overload.unwrap_err();
        assert!(
            overload_err.contains("overloaded"),
            "expected an overload error, got: {overload_err}"
        );
        let _ = blocker;
        let _ = queued;
    }

    #[test]
    fn ipv6_literal_resolves_directly() {
        // An explicit IPv6 literal must bypass the pool exactly like IPv4
        // (no DNS, no threads).
        assert_eq!(
            resolve_host("::1", 8443).unwrap(),
            vec![SocketAddr::new(
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
                8443
            )]
        );
    }

    #[test]
    fn resolver_recovers_after_caller_times_out() {
        // A hung `getaddrinfo` strands one worker; once the underlying op
        // finishes, the worker must return to service. A timed-out caller thus
        // never permanently poisons the pool — threads stay fixed and the queue
        // stays bounded no matter how many lookups time out.
        let (release_tx, release_rx) = channel();
        let resolver = Resolver::with_params(1, 4, Duration::from_millis(30));
        let first = resolver.run(move || {
            let _ = release_rx.recv(); // stand in for a hung getaddrinfo
            Ok(vec![addr(10, 0, 0, 1, 80)])
        });
        assert!(
            first.unwrap_err().contains("timed out"),
            "the blocked lookup must time out"
        );
        // Release the stranded worker; the next lookup must now succeed.
        release_tx.send(()).unwrap();
        let second = resolver.run(|| Ok(vec![addr(10, 0, 0, 2, 80)])).unwrap();
        assert_eq!(second, vec![addr(10, 0, 0, 2, 80)]);
    }

    #[test]
    fn empty_result_is_a_valid_resolver_output() {
        // An empty result set is returned as-is; the caller (`resolve_upstreams`)
        // decides that "no address resolved" is a configuration error. This keeps
        // the resolver itself dumb and the policy in the config plane.
        let resolver = Resolver::with_params(1, 4, Duration::from_secs(5));
        let got = resolver.run(|| Ok(vec![])).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn parse_netloc_hostname_and_ipv4() {
        assert_eq!(
            parse_netloc("example.com:443").unwrap(),
            ("example.com".to_string(), 443)
        );
        assert_eq!(
            parse_netloc("127.0.0.1:8080").unwrap(),
            ("127.0.0.1".to_string(), 8080)
        );
    }

    #[test]
    fn parse_netloc_bracketed_ipv6() {
        assert_eq!(
            parse_netloc("[::1]:8443").unwrap(),
            ("::1".to_string(), 8443)
        );
        assert_eq!(
            parse_netloc("[2001:db8::1]:53").unwrap(),
            ("2001:db8::1".to_string(), 53)
        );
    }

    #[test]
    fn parse_netloc_rejects_malformed_input() {
        assert!(parse_netloc("example.com").is_err(), "missing port");
        assert!(parse_netloc("example.com:notaport").is_err(), "bad port");
        assert!(parse_netloc("[::1]").is_err(), "missing port after IPv6");
        assert!(parse_netloc("[::1").is_err(), "unterminated IPv6");
        assert!(parse_netloc(":443").is_err(), "empty host");
        assert!(
            parse_netloc("[::1]:0").is_ok(),
            "port 0 is technically valid"
        );
    }

    #[test]
    fn agent_resolver_resolves_ip_literals_without_dns() {
        // IP literals (plain, bracketed, and unbracketed IPv6) must be returned
        // directly through the adapter, never touching the pool or a DNS lookup.
        assert_eq!(
            agent_resolver("127.0.0.1:9000").unwrap(),
            vec![addr(127, 0, 0, 1, 9000)]
        );
        assert_eq!(
            agent_resolver("[::1]:9443").unwrap(),
            vec![SocketAddr::new(
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
                9443
            )]
        );
        // ureq's URL host strips IPv6 brackets, so the resolver also sees the
        // unbracketed `host:port` form.
        assert_eq!(
            agent_resolver("::1:9443").unwrap(),
            vec![SocketAddr::new(
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
                9443
            )]
        );
    }

    #[test]
    fn agent_resolver_rejects_malformed_netloc() {
        assert!(agent_resolver("no-port").is_err());
        assert!(agent_resolver("[::1]").is_err());
    }
}
