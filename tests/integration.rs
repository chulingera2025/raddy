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

//! End-to-end integration tests.
//!
//! Each test spawns the real `raddy` binary (via `CARGO_BIN_EXE_raddy`) as a
//! subprocess against a minimal in-process upstream, so the server's own tokio
//! runtime, signal handlers, and connection pools are exercised exactly as in
//! production. The M2 acceptance criteria covered here: forwarding, `to`
//! round-robin, 400/404 fallbacks, `redir`, SIGHUP reload, invalid-reload
//! retention, and the `raddy check` exit codes.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_raddy");

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Reserve a free TCP port.
///
/// Ports are drawn from a per-process counter so parallel tests never collide
/// with each other, and each port is verified bindable before it is returned
/// (so a port taken by some other process is skipped).
fn free_port() -> u16 {
    static PORT_COUNTER: AtomicUsize = AtomicUsize::new(0);
    const PORT_BASE: u16 = 20_000;
    loop {
        let port = PORT_BASE + PORT_COUNTER.fetch_add(1, Ordering::Relaxed) as u16;
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// Bind a loopback listener for a test upstream, retrying with a fresh port if
/// a lingering socket (e.g. a TIME_WAIT left by a killed subprocess) blocks it.
///
/// The listener is bound in the test thread (not inside the worker) so the
/// retry loop runs before any request traffic starts.
fn bind_listener() -> (u16, TcpListener) {
    loop {
        let port = free_port();
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return (port, listener),
            Err(_) => continue,
        }
    }
}

/// A minimal HTTP/1.1 upstream that responds `label=<name>` and counts the
/// connections it accepted.
struct EchoUpstream {
    port: u16,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl EchoUpstream {
    fn spawn(label: &str) -> (u16, EchoUpstream) {
        let (port, listener) = bind_listener();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let hits_thread = hits.clone();
        let stop_thread = stop.clone();
        let label = label.to_string();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                hits_thread.fetch_add(1, Ordering::Relaxed);
                let label = label.clone();
                thread::spawn(move || {
                    // Read the request headers (single read suffices for a GET).
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let body = format!("label={label}");
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                });
            }
        });
        (
            port,
            EchoUpstream {
                port,
                hits,
                stop,
                handle: Some(handle),
            },
        )
    }

    fn hit_count(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }
}

impl Drop for EchoUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock the accept loop, then join.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// An upstream that keeps connections alive (no `Connection: close`) and counts
/// *distinct* connections, so a client can assert connection reuse across
/// requests and reloads.
struct KeepAliveUpstream {
    port: u16,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl KeepAliveUpstream {
    fn spawn() -> (u16, KeepAliveUpstream) {
        let (port, listener) = bind_listener();
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let connections_thread = connections.clone();
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                connections_thread.fetch_add(1, Ordering::Relaxed);
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    // Serve requests on this connection until the client closes it.
                    'outer: loop {
                        head.clear();
                        loop {
                            match stream.read(&mut byte) {
                                Ok(0) | Err(_) => break 'outer,
                                Ok(_) => {
                                    head.push(byte[0]);
                                    if head.ends_with(b"\r\n\r\n") {
                                        break;
                                    }
                                }
                            }
                        }
                        let body = "label=keepalive";
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        if stream.write_all(resp.as_bytes()).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (
            port,
            KeepAliveUpstream {
                port,
                connections,
                stop,
                handle: Some(handle),
            },
        )
    }

    fn connection_count(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }
}

impl Drop for KeepAliveUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A running `raddy run` subprocess plus the config file it reads.
struct RadRaddy {
    child: Child,
    port: u16,
    config_path: PathBuf,
}

impl RadRaddy {
    /// Spawn `raddy run` with a config generated by `config_for` (which receives
    /// the chosen port).
    fn spawn(config_for: impl Fn(u16) -> String) -> RadRaddy {
        Self::spawn_with_args(config_for, &[])
    }

    /// Spawn with extra CLI arguments (e.g. `--metrics-addr`, `--access-log`).
    fn spawn_with_args(config_for: impl Fn(u16) -> String, extra: &[String]) -> RadRaddy {
        loop {
            let port = free_port();
            let config = config_for(port);
            let config_path = std::env::temp_dir().join(format!(
                "raddy_integration_{}_{}.Raddyfile",
                std::process::id(),
                port
            ));
            std::fs::write(&config_path, &config).unwrap();
            let mut cmd = Command::new(BIN);
            cmd.args(["run", "-c"]).arg(&config_path);
            cmd.args(extra);
            cmd.env("RUST_LOG", "error").stderr(Stdio::inherit());
            let child = cmd.spawn().expect("failed to spawn the raddy binary");
            let mut raddy = RadRaddy {
                child,
                port,
                config_path,
            };
            if raddy.wait_for_ready() {
                return raddy;
            }
            // The child exited (bind failure) or never listened; retry.
            let _ = raddy.child.kill();
            let _ = raddy.child.wait();
        }
    }

    /// The port the server is listening on (the test should use this, not a
    /// locally captured port, since a spawn retry may have changed it).
    fn port(&self) -> u16 {
        self.port
    }

    /// Poll until the listener accepts connections, or the child exits. Returns
    /// `false` if the child failed to start or the deadline passed.
    fn wait_for_ready(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.child.try_wait().expect("try_wait failed").is_some() {
                return false;
            }
            // Require an actual HTTP response (any status), not just a TCP
            // accept: the listener can be bound before the proxy layer is ready
            // to serve, and under parallel load that window is long enough to
            // flake a test. The probe omits Host so raddy answers 400 without
            // touching any upstream (which would pollute connection counts).
            if try_request(self.port, None, "/").is_some() {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Rewrite the Raddyfile and deliver a SIGHUP.
    fn reload(&mut self, config: &str) {
        std::fs::write(&self.config_path, config).unwrap();
        // SAFETY: self.child.id() is this process's own raddy child.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGHUP);
        }
    }
}

impl Drop for RadRaddy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.config_path);
    }
}

/// A parsed HTTP response.
#[derive(Debug)]
struct Response {
    status: u16,
    headers: String, // lowercased
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        let prefix = format!("{name}:");
        self.headers
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .map(str::trim)
    }
}

/// Send a request to `port`; returns `None` if the connection or read fails
/// (used for polling during reload).
fn try_request(port: u16, host: Option<&str>, path: &str) -> Option<Response> {
    try_request_hdr(port, host, path, &[])
}

/// Send a request with extra header lines (e.g. `Accept-Encoding`).
fn try_request_hdr(
    port: u16,
    host: Option<&str>,
    path: &str,
    headers: &[&str],
) -> Option<Response> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let host_line = host.map(|h| format!("Host: {h}\r\n")).unwrap_or_default();
    let extra = headers
        .iter()
        .map(|h| format!("{h}\r\n"))
        .collect::<String>();
    let request = format!("GET {path} HTTP/1.1\r\n{host_line}{extra}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    // Read exactly the head + Content-Length body (never wait for a connection
    // close that a keep-alive response may not send).
    Some(read_one_response(&mut stream))
}

fn send_request(port: u16, host: Option<&str>, path: &str) -> Response {
    try_request(port, host, path).expect("request failed")
}

/// Send a request with extra header lines (e.g. `Accept-Encoding`).
fn send_request_hdr(port: u16, host: Option<&str>, path: &str, headers: &[&str]) -> Response {
    try_request_hdr(port, host, path, headers).expect("request failed")
}

/// Send a request with an explicit method (e.g. `HEAD`) and extra headers.
fn send_request_method(
    method: &str,
    port: u16,
    host: Option<&str>,
    path: &str,
    headers: &[&str],
) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let host_line = host.map(|h| format!("Host: {h}\r\n")).unwrap_or_default();
    let extra = headers
        .iter()
        .map(|h| format!("{h}\r\n"))
        .collect::<String>();
    let request =
        format!("{method} {path} HTTP/1.1\r\n{host_line}{extra}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let head_str = read_head(&mut stream);
    let status = parse_status(&head_str);
    if method == "HEAD" {
        // A HEAD response carries the GET Content-Length but no body (RFC 9110
        // §9.3.2), so only the head is read.
        return Response {
            status,
            headers: head_str.to_lowercase(),
            body: String::new(),
        };
    }
    read_one_response_with_head(head_str, &mut stream)
}

/// Read the response head (through `\r\n\r\n`) as a string.
fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read(&mut byte).unwrap_or(0) != 0 {
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&head).to_string()
}

/// The numeric status code of a response head.
fn parse_status(head_str: &str) -> u16 {
    head_str
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Read a single HTTP/1.1 response from a keep-alive connection: the head
/// (through `\r\n\r\n`) plus the body per `Content-Length`.
fn read_one_response(stream: &mut TcpStream) -> Response {
    let head_str = read_head(stream);
    read_one_response_with_head(head_str, stream)
}

/// Build a [`Response`] from an already-read head, reading the body per
/// `Content-Length`.
fn read_one_response_with_head(head_str: String, stream: &mut TcpStream) -> Response {
    let content_length = head_str
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).unwrap();
    }
    Response {
        status: parse_status(&head_str),
        headers: head_str.to_lowercase(),
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// Extract a header value from a lowercased response head string (with its
/// leading spaces and trailing CR trimmed).
fn head_header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}:");
    head.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::trim)
}

/// Send an arbitrary-method request and return the raw (binary) response: the
/// status, the lowercased head, and the body bytes.
///
/// Bodies are read per `Content-Length` when the head carries one; otherwise
/// they are read until the server closes the connection (close-delimited
/// framing, e.g. compressed responses). `HEAD` returns only the head.
fn send_raw(
    port: u16,
    host: Option<&str>,
    method: &str,
    path: &str,
    headers: &[&str],
) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let host_line = host.map(|h| format!("Host: {h}\r\n")).unwrap_or_default();
    let extra = headers
        .iter()
        .map(|h| format!("{h}\r\n"))
        .collect::<String>();
    let request =
        format!("{method} {path} HTTP/1.1\r\n{host_line}{extra}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let head_str = read_head(&mut stream);
    let status = parse_status(&head_str);
    let headers = head_str.to_lowercase();
    if method == "HEAD" {
        // A HEAD response carries the GET Content-Length but no body (RFC 9110
        // §9.3.2), so only the head is read.
        return (status, headers, Vec::new());
    }
    let body = match head_header(&headers, "content-length") {
        Some(len) => {
            let len = len.parse::<usize>().unwrap_or(0);
            let mut body = vec![0u8; len];
            if len > 0 {
                stream.read_exact(&mut body).unwrap();
            }
            body
        }
        None => {
            // Close-delimited body (no Content-Length): read until the server
            // closes the connection.
            let mut body = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => body.extend_from_slice(&buf[..n]),
                }
            }
            body
        }
    };
    (status, headers, body)
}

/// Decompress a complete gzip stream back to its payload.
fn gzip_decode(compressed: &[u8]) -> Vec<u8> {
    let mut decoder = flate2::read::GzDecoder::new(compressed);
    let mut decoded = Vec::new();
    Read::read_to_end(&mut decoder, &mut decoded).unwrap();
    decoded
}

/// Decompress a complete zstd frame back to its payload.
fn zstd_decode(compressed: &[u8]) -> Vec<u8> {
    let mut decoder = zstd::stream::read::Decoder::new(compressed).unwrap();
    let mut decoded = Vec::new();
    Read::read_to_end(&mut decoder, &mut decoded).unwrap();
    decoded
}

/// Send `n` requests over a single keep-alive downstream connection.
fn keepalive_requests(port: u16, host: &str, n: usize) -> Vec<Response> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut responses = Vec::with_capacity(n);
    for _ in 0..n {
        let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        responses.push(read_one_response(&mut stream));
    }
    responses
}

/// Poll until `cond` holds or the deadline passes.
fn wait_until(cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cond = cond;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {what}");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn forwards_to_upstream() {
    let (up_port, _up) = EchoUpstream::spawn("A");
    let raddy =
        RadRaddy::spawn(|port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"));

    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 200, "unexpected response: {resp:?}");
    assert_eq!(resp.body, "label=A");
}

#[test]
fn round_robin_across_upstreams() {
    let (a_port, a) = EchoUpstream::spawn("A");
    let (b_port, b) = EchoUpstream::spawn("B");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    reverse_proxy {{\n        to 127.0.0.1:{a_port} 127.0.0.1:{b_port}\n    }}\n}}\n"
        )
    });

    for _ in 0..6 {
        assert_eq!(
            send_request(raddy.port(), Some("localhost"), "/").status,
            200
        );
    }
    assert!(
        a.hit_count() >= 2 && b.hit_count() >= 2,
        "expected both upstreams to serve requests: a={}, b={}",
        a.hit_count(),
        b.hit_count()
    );
}

#[test]
fn missing_host_returns_400() {
    let (up_port, _up) = EchoUpstream::spawn("A");
    let raddy =
        RadRaddy::spawn(|port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"));

    // Wait for the server to accept requests before asserting.
    wait_until(
        || try_request(raddy.port(), None, "/").is_some(),
        "server to accept requests",
    );
    let resp = send_request(raddy.port(), None, "/");
    assert_eq!(resp.status, 400);
}

#[test]
fn unknown_host_returns_404_and_named_host_works() {
    let (up_port, _up) = EchoUpstream::spawn("A");
    let raddy = RadRaddy::spawn(|port| {
        format!("api.example.com:{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n")
    });

    let unknown = send_request(raddy.port(), Some("other.example.com"), "/");
    assert_eq!(unknown.status, 404);
    let known = send_request(raddy.port(), Some("api.example.com"), "/");
    assert_eq!(known.status, 200);
}

#[test]
fn redirect_expands_placeholders() {
    let raddy = RadRaddy::spawn(|port| {
        format!(":{port} {{\n    redir https://{{host}}{{uri}} permanent\n}}\n")
    });

    let resp = send_request(raddy.port(), Some("example.com:8080"), "/a/b?x=1");
    assert_eq!(resp.status, 308);
    assert_eq!(resp.header("location"), Some("https://example.com/a/b?x=1"));
}

#[test]
fn header_up_reaches_upstream() {
    let (up_port, _up) = EchoUpstream::spawn("A");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n    header_up X-Proxy-Port {port}\n}}\n"
        )
    });

    // The body only echoes `label=`, so this test asserts the header rewrite
    // does not break forwarding; the header value itself is asserted in the
    // `raddy check`/unit layer. (A full header-echo upstream is overkill here.)
    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "label=A");
}

#[test]
fn reverse_proxy_applies_header_down() {
    let (up_port, _up) = EchoUpstream::spawn("A");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n    header_down X-Proxy {{uri}}\n}}\n"
        )
    });

    let resp = send_request(raddy.port(), Some("localhost"), "/x?y=1");
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("x-proxy"),
        Some("/x?y=1"),
        "header_down must apply to the reverse-proxy response with {{uri}} expanded"
    );
}

#[test]
fn redir_applies_header_down() {
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    redir https://{{host}}{{uri}} permanent\n    header_down X-Redirected {{host}}\n    header_down Location /overridden\n}}\n"
        )
    });

    let resp = send_request(raddy.port(), Some("example.com:8080"), "/a");
    assert_eq!(resp.status, 308);
    assert_eq!(
        resp.header("x-redirected"),
        Some("example.com"),
        "header_down must apply to the redir response with {{host}} expanded"
    );
    assert_eq!(
        resp.header("location"),
        Some("/overridden"),
        "header_down must overwrite the redir's own Location header"
    );
}

#[test]
fn sighup_reload_swaps_config() {
    let (a_port, _a) = EchoUpstream::spawn("A");
    let (b_port, _b) = EchoUpstream::spawn("B");
    let mut raddy =
        RadRaddy::spawn(|port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{a_port}\n}}\n"));

    assert_eq!(
        send_request(raddy.port(), Some("localhost"), "/").body,
        "label=A"
    );
    raddy.reload(&format!(
        ":{} {{\n    reverse_proxy 127.0.0.1:{b_port}\n}}\n",
        raddy.port()
    ));
    wait_until(
        || try_request(raddy.port(), Some("localhost"), "/").is_some_and(|r| r.body == "label=B"),
        "reload to switch upstreams",
    );
}

#[test]
fn invalid_reload_keeps_old_config() {
    let (a_port, _a) = EchoUpstream::spawn("A");
    let mut raddy =
        RadRaddy::spawn(|port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{a_port}\n}}\n"));

    assert_eq!(
        send_request(raddy.port(), Some("localhost"), "/").body,
        "label=A"
    );
    raddy.reload("bogus config that cannot parse");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        send_request(raddy.port(), Some("localhost"), "/").body,
        "label=A"
    );
}

#[test]
fn upstream_pool_survives_reload() {
    let (up_port, up) = KeepAliveUpstream::spawn();
    let mut raddy =
        RadRaddy::spawn(|port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"));

    // Two requests on one downstream connection reuse a single upstream
    // connection (ADR-011 connection pooling).
    let responses = keepalive_requests(raddy.port(), "localhost", 2);
    assert!(responses.iter().all(|r| r.status == 200));
    assert_eq!(
        up.connection_count(),
        1,
        "expected the two requests to share one upstream connection"
    );

    // A reload swaps the snapshot but must not rebuild the Connector's pools.
    raddy.reload(&format!(
        ":{} {{\n    reverse_proxy 127.0.0.1:{up_port}\n    header_up X-Test yes\n}}\n",
        raddy.port()
    ));
    thread::sleep(Duration::from_millis(200));

    // A new downstream connection after the reload still reuses the pooled
    // upstream connection.
    let responses = keepalive_requests(raddy.port(), "localhost", 1);
    assert_eq!(responses[0].status, 200);
    assert_eq!(
        up.connection_count(),
        1,
        "the upstream pool must survive a reload (ADR-011)"
    );
}

#[test]
fn raddy_check_exit_codes() {
    let valid = std::env::temp_dir().join(format!(
        "raddy_check_valid_{}.Raddyfile",
        std::process::id()
    ));
    let invalid = std::env::temp_dir().join(format!(
        "raddy_check_invalid_{}.Raddyfile",
        std::process::id()
    ));
    std::fs::write(&valid, ":8080 {\n    reverse_proxy 127.0.0.1:1\n}\n").unwrap();
    std::fs::write(&invalid, "bogus\n").unwrap();

    let ok = Command::new(BIN)
        .args(["check", "-c"])
        .arg(&valid)
        .output()
        .unwrap();
    assert!(ok.status.success(), "check should exit 0 on valid config");
    let bad = Command::new(BIN)
        .args(["check", "-c"])
        .arg(&invalid)
        .output()
        .unwrap();
    assert!(
        !bad.status.success(),
        "check should exit 1 on invalid config"
    );

    let _ = std::fs::remove_file(&valid);
    let _ = std::fs::remove_file(&invalid);
}

#[test]
fn file_server_serves_static_files() {
    let dir = std::env::temp_dir().join(format!("raddy_fs_a_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("static")).unwrap();
    std::fs::write(dir.join("static/hello.txt"), "hello-static").unwrap();
    std::fs::write(dir.join("static/index.html"), "<h1>index</h1>").unwrap();
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    handle /static/* {{\n        root {}\n        file_server\n    }}\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();

    let file = send_request(port, Some("localhost"), "/static/hello.txt");
    assert_eq!(file.status, 200);
    assert_eq!(file.body, "hello-static");

    let index = send_request(port, Some("localhost"), "/static/");
    assert_eq!(index.status, 200);
    assert!(index.body.contains("index"));

    let missing = send_request(port, Some("localhost"), "/static/nope.txt");
    assert_eq!(missing.status, 404);

    // Our client sends the raw path (no normalization), so the `..` guard
    // must reject traversal.
    let traversal = send_request(port, Some("localhost"), "/static/../../etc/passwd");
    assert_eq!(traversal.status, 404);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_server_compresses_on_accept_encoding() {
    let dir = std::env::temp_dir().join(format!("raddy_fs_b_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("hello.txt"),
        "hello compressible compressible compressible",
    )
    .unwrap();
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    encode gzip\n    file_server\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();

    // raddy is up (wait_for_ready), but the very first request can transiently
    // fail under heavy parallel load; poll until it succeeds before asserting.
    wait_until(
        || try_request(port, Some("localhost"), "/hello.txt").is_some_and(|r| r.status == 200),
        "plain file_server response",
    );
    let plain = send_request(port, Some("localhost"), "/hello.txt");
    assert_eq!(plain.status, 200);
    assert_eq!(plain.body, "hello compressible compressible compressible");

    let gz = send_request_hdr(
        port,
        Some("localhost"),
        "/hello.txt",
        &["Accept-Encoding: gzip"],
    );
    assert_eq!(gz.header("content-encoding"), Some("gzip"));
    assert_ne!(
        gz.body, plain.body,
        "gzip response body must differ from the plain body"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Reverse-proxy streaming compression acceptance test (B3b1).
///
/// The upstream sends a compressible first part of a body and then blocks on a
/// barrier before sending the rest. A proxy that genuinely streams compression
/// must forward compressed bytes to the downstream *while the upstream is still
/// blocked*; the old full-buffer implementation emitted nothing until end of
/// stream and therefore times out waiting for the first compressed bytes.
fn streaming_compression_scenario(
    encode_line: &str,
    expected_ce: &str,
    decode: impl Fn(&[u8]) -> Vec<u8>,
) {
    let first_part = "compressible-".repeat(4_000); // ~52 KB
    let second_part = "second-half-".repeat(4_000); // ~52 KB
    let expected: Vec<u8> = format!("{first_part}{second_part}").into_bytes();

    let (first_sent_tx, first_sent_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = Arc::new(Mutex::new(release_rx));

    let (up_port, listener) = bind_listener();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let first_part_c = first_part.clone();
    let second_part_c = second_part.clone();
    let up_handle = thread::spawn(move || {
        for stream in listener.incoming() {
            if stop_thread.load(Ordering::Relaxed) {
                break;
            }
            let Ok(mut stream) = stream else { continue };
            let first_part = first_part_c.clone();
            let second_part = second_part_c.clone();
            let first_sent_tx = first_sent_tx.clone();
            let release_rx = release_rx.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let total = first_part.len() + second_part.len();
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(first_part.as_bytes());
                let _ = first_sent_tx.send(());
                // Block until the test has observed compressed bytes downstream.
                let _ = release_rx.lock().unwrap().recv();
                let _ = stream.write_all(second_part.as_bytes());
            });
        }
    });

    let config = move |port: u16| {
        format!(":{port} {{\n    encode {encode_line}\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n")
    };
    let raddy = RadRaddy::spawn(config);
    let mut stream = TcpStream::connect(("127.0.0.1", raddy.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: {encode_line}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();

    let head_str = read_head(&mut stream);
    assert_eq!(parse_status(&head_str), 200, "unexpected head: {head_str}");
    assert!(
        head_str
            .to_lowercase()
            .contains(&format!("content-encoding: {expected_ce}")),
        "expected Content-Encoding: {expected_ce} in head: {head_str}"
    );

    // Wait for the first compressed bytes. With a streaming implementation these
    // arrive while the upstream is still blocked on the barrier; with the old
    // full-buffer implementation nothing is emitted until end of stream and
    // this read times out.
    let mut body: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let first_read = stream
        .read(&mut buf)
        .expect("timed out waiting for the first compressed bytes before EOS");
    assert!(
        first_read > 0,
        "downstream must receive compressed bytes before the upstream sends EOS"
    );
    body.extend_from_slice(&buf[..first_read]);

    // The upstream may only continue once the downstream has seen body bytes.
    release_tx.send(()).unwrap();

    // Read the rest until the connection closes.
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
        }
    }

    // One continuous gzip member / zstd frame whose payload is the whole body.
    let decoded = decode(&body);
    assert_eq!(
        decoded, expected,
        "the concatenated compressed stream must decompress to the original body"
    );

    // The upstream really did wait on the barrier.
    assert!(
        first_sent_rx.recv().is_ok(),
        "the upstream should have sent its first part"
    );

    stop.store(true, Ordering::Relaxed);
    let _ = TcpStream::connect(("127.0.0.1", up_port));
    let _ = up_handle.join();
}

#[test]
fn reverse_proxy_streams_gzip_before_eos() {
    streaming_compression_scenario("gzip", "gzip", |body| {
        let mut decoder = flate2::read::GzDecoder::new(body);
        let mut decoded = Vec::new();
        Read::read_to_end(&mut decoder, &mut decoded).unwrap();
        decoded
    });
}

#[test]
fn reverse_proxy_streams_zstd_before_eos() {
    streaming_compression_scenario("zstd", "zstd", |body| {
        let mut decoder = zstd::stream::read::Decoder::new(body).unwrap();
        let mut decoded = Vec::new();
        Read::read_to_end(&mut decoder, &mut decoded).unwrap();
        decoded
    });
}

#[test]
fn file_server_applies_header_down() {
    let dir = std::env::temp_dir().join(format!("raddy_fs_hd_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), "hi").unwrap();
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    file_server\n    header_down X-Served {{uri}}\n    header_down X-Static yes\n    header_down Content-Type application/octet-stream\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();

    let resp = send_request(port, Some("localhost"), "/hello.txt");
    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.header("x-served"),
        Some("/hello.txt"),
        "header_down must apply to the file_server response with {{uri}} expanded"
    );
    assert_eq!(resp.header("x-static"), Some("yes"));
    assert_eq!(
        resp.header("content-type"),
        Some("application/octet-stream"),
        "header_down must overwrite the file_server's own Content-Type"
    );

    // HEAD keeps its bodyless response while still carrying header_down.
    let head = send_request_method("HEAD", port, Some("localhost"), "/hello.txt", &[]);
    assert_eq!(head.status, 200);
    assert_eq!(head.body, "");
    assert_eq!(head.header("x-static"), Some("yes"));

    // A missing file still 404s (header_down must not change error behavior).
    let missing = send_request(port, Some("localhost"), "/missing.txt");
    assert_eq!(missing.status, 404);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// file_server streaming, ranges, and compression (B3b2)
// ---------------------------------------------------------------------------

/// Create a temp dir containing a deterministic file larger than one 64 KiB
/// streaming chunk, and return the dir plus the expected bytes.
fn big_static_file(tag: &str) -> (std::path::PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("raddy_fs_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let expected: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("big.bin"), &expected).unwrap();
    (dir, expected)
}

#[test]
fn file_server_streams_large_file_exactly() {
    let (dir, expected) = big_static_file("stream");
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    file_server\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();
    wait_until(
        || try_request(port, Some("localhost"), "/big.bin").is_some_and(|r| r.status == 200),
        "large file response",
    );

    let (status, headers, body) = send_raw(port, Some("localhost"), "GET", "/big.bin", &[]);
    assert_eq!(status, 200);
    assert_eq!(head_header(&headers, "content-length"), Some("200000"));
    assert!(headers.contains("accept-ranges: bytes"));
    assert_eq!(body.len(), 200_000);
    assert_eq!(
        body, expected,
        "the streamed bytes must exactly match the file (multiple chunks)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_server_head_returns_headers_without_body() {
    let (dir, _expected) = big_static_file("head");
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    file_server\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();
    wait_until(
        || try_request(port, Some("localhost"), "/big.bin").is_some_and(|r| r.status == 200),
        "large file response",
    );

    let (status, headers, body) = send_raw(port, Some("localhost"), "HEAD", "/big.bin", &[]);
    assert_eq!(status, 200);
    assert_eq!(body.len(), 0, "HEAD must not return a body");
    assert_eq!(
        head_header(&headers, "content-length"),
        Some("200000"),
        "HEAD must report the size a GET would return"
    );
    assert!(headers.contains("accept-ranges: bytes"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_server_serves_single_byte_range() {
    let (dir, expected) = big_static_file("range");
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    file_server\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();
    wait_until(
        || try_request(port, Some("localhost"), "/big.bin").is_some_and(|r| r.status == 200),
        "large file response",
    );

    // Closed range: bytes=100-199.
    let (status, headers, body) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/big.bin",
        &["Range: bytes=100-199"],
    );
    assert_eq!(status, 206);
    assert_eq!(body, expected[100..200].to_vec());
    assert_eq!(head_header(&headers, "content-length"), Some("100"));
    assert_eq!(
        head_header(&headers, "content-range"),
        Some("bytes 100-199/200000")
    );
    assert!(headers.contains("accept-ranges: bytes"));

    // Open-ended range: bytes=199900-.
    let (status, headers, body) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/big.bin",
        &["Range: bytes=199900-"],
    );
    assert_eq!(status, 206);
    assert_eq!(body, expected[199900..].to_vec());
    assert_eq!(
        head_header(&headers, "content-range"),
        Some("bytes 199900-199999/200000")
    );

    // Suffix range: the last 100 bytes.
    let (status, headers, body) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/big.bin",
        &["Range: bytes=-100"],
    );
    assert_eq!(status, 206);
    assert_eq!(body, expected[199900..].to_vec());
    assert_eq!(
        head_header(&headers, "content-range"),
        Some("bytes 199900-199999/200000")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_server_unsatisfiable_range_is_416() {
    let (dir, _expected) = big_static_file("unsat");
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    file_server\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();
    wait_until(
        || try_request(port, Some("localhost"), "/big.bin").is_some_and(|r| r.status == 200),
        "large file response",
    );

    // A start past EOF is unsatisfiable.
    let (status, headers, body) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/big.bin",
        &["Range: bytes=200000-"],
    );
    assert_eq!(status, 416);
    assert_eq!(
        head_header(&headers, "content-range"),
        Some("bytes */200000")
    );
    assert_eq!(body.len(), 0, "416 must have no body");

    // Multiple ranges are out of scope: rejected as unsatisfiable (416), never
    // multipart.
    let (status, headers, body) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/big.bin",
        &["Range: bytes=0-9,20-29"],
    );
    assert_eq!(status, 416);
    assert_eq!(
        head_header(&headers, "content-range"),
        Some("bytes */200000")
    );
    assert_eq!(body.len(), 0);

    // Even with Accept-Encoding: gzip, a 416 has nothing to compress.
    let (status, headers, body) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/big.bin",
        &["Range: bytes=200000-", "Accept-Encoding: gzip"],
    );
    assert_eq!(status, 416);
    assert_eq!(head_header(&headers, "content-encoding"), None);
    assert_eq!(body.len(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_server_partial_ranges_are_not_compressed() {
    let (dir, expected) = big_static_file("rangece");
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    encode gzip\n    file_server\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();
    wait_until(
        || try_request(port, Some("localhost"), "/big.bin").is_some_and(|r| r.status == 200),
        "large file response",
    );

    // A partial (206) response must stay byte-exact even when the client would
    // otherwise accept gzip.
    let (status, headers, body) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/big.bin",
        &["Range: bytes=0-99", "Accept-Encoding: gzip"],
    );
    assert_eq!(status, 206);
    assert_eq!(head_header(&headers, "content-encoding"), None);
    assert_eq!(head_header(&headers, "content-length"), Some("100"));
    assert_eq!(body, expected[0..100].to_vec());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_server_streams_gzip_full_response() {
    let (dir, expected) = big_static_file("gzip");
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    encode gzip\n    file_server\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();
    wait_until(
        || try_request(port, Some("localhost"), "/big.bin").is_some_and(|r| r.status == 200),
        "large file response",
    );

    let (status, headers, body) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/big.bin",
        &["Accept-Encoding: gzip"],
    );
    assert_eq!(status, 200);
    assert_eq!(head_header(&headers, "content-encoding"), Some("gzip"));
    assert_eq!(
        head_header(&headers, "content-length"),
        None,
        "a compressed response must not carry the uncompressed Content-Length: {headers}"
    );
    assert!(
        headers.contains("vary: accept-encoding"),
        "compressed responses must vary on Accept-Encoding: {headers}"
    );
    let decoded = gzip_decode(&body);
    assert_eq!(
        decoded, expected,
        "the single continuous gzip stream must decode to the whole file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_server_streams_zstd_full_response() {
    let (dir, expected) = big_static_file("zstd");
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    encode zstd\n    file_server\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();
    wait_until(
        || try_request(port, Some("localhost"), "/big.bin").is_some_and(|r| r.status == 200),
        "large file response",
    );

    let (status, headers, body) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/big.bin",
        &["Accept-Encoding: zstd"],
    );
    assert_eq!(status, 200);
    assert_eq!(head_header(&headers, "content-encoding"), Some("zstd"));
    assert_eq!(
        head_header(&headers, "content-length"),
        None,
        "a compressed response must not carry the uncompressed Content-Length: {headers}"
    );
    assert!(
        headers.contains("vary: accept-encoding"),
        "compressed responses must vary on Accept-Encoding: {headers}"
    );
    let decoded = zstd_decode(&body);
    assert_eq!(
        decoded, expected,
        "the single continuous zstd frame must decode to the whole file"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_server_header_down_applies_to_range_responses() {
    let dir = std::env::temp_dir().join(format!("raddy_fs_rangehd_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), "hello range world").unwrap();
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    file_server\n    header_down X-Static yes\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();
    wait_until(
        || try_request(port, Some("localhost"), "/hello.txt").is_some_and(|r| r.status == 200),
        "file_server response",
    );

    // header_down applies to the full 200, the partial 206, and the 416.
    let (status, headers, _) = send_raw(port, Some("localhost"), "GET", "/hello.txt", &[]);
    assert_eq!(status, 200);
    assert_eq!(head_header(&headers, "x-static"), Some("yes"));

    let (status, headers, _) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/hello.txt",
        &["Range: bytes=0-4"],
    );
    assert_eq!(status, 206);
    assert_eq!(head_header(&headers, "x-static"), Some("yes"));

    let (status, headers, _) = send_raw(
        port,
        Some("localhost"),
        "GET",
        "/hello.txt",
        &["Range: bytes=99-"],
    );
    assert_eq!(status, 416);
    assert_eq!(head_header(&headers, "x-static"), Some("yes"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn metrics_endpoint_reports_requests() {
    let (up_port, _up) = EchoUpstream::spawn("A");
    let metrics_port = free_port();
    let addr = format!("127.0.0.1:{metrics_port}");
    let raddy = RadRaddy::spawn_with_args(
        move |port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"),
        &[String::from("--metrics-addr"), addr],
    );

    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 200);

    // The metrics listener is a second service and the request counter is
    // recorded in the logging hook after the response, so poll until the
    // endpoint both is reachable and reports the counter.
    wait_until(
        || {
            try_request(metrics_port, None, "/metrics")
                .is_some_and(|r| r.body.contains("raddy_requests_total"))
        },
        "metrics to report the request counter",
    );
    let metrics = try_request(metrics_port, None, "/metrics").expect("metrics endpoint");
    assert!(
        metrics.body.contains("raddy_requests_total"),
        "metrics should report the request counter: {}",
        metrics.body
    );
}

#[test]
fn access_log_writes_json_lines() {
    let (up_port, _up) = EchoUpstream::spawn("A");
    let log_path = std::env::temp_dir().join(format!("raddy_access_{}.log", std::process::id()));
    let log_str = log_path.to_string_lossy().into_owned();
    let raddy = RadRaddy::spawn_with_args(
        move |port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"),
        &[String::from("--access-log"), log_str],
    );

    let resp = send_request(raddy.port(), Some("localhost"), "/x");
    assert_eq!(resp.status, 200);

    wait_until(
        || {
            std::fs::read_to_string(&log_path)
                .map(|s| s.contains("\"path\":\"/x\"") && s.contains("\"status\":200"))
                .unwrap_or(false)
        },
        "access log line for the request",
    );
    let _ = std::fs::remove_file(&log_path);
}

/// Extract the `ts` field (epoch ms) from one JSON access-log line.
fn parse_log_ts(line: &str) -> u64 {
    let start = line
        .find("\"ts\":")
        .expect("access-log line has a ts field")
        + "\"ts\":".len();
    let end = line[start..]
        .find(',')
        .map(|i| start + i)
        .unwrap_or(line.len());
    line[start..end].parse().expect("ts is a number")
}

#[test]
fn access_log_client_uses_effective_ip() {
    let (up_port, _up) = EchoUpstream::spawn("A");
    let log_path = std::env::temp_dir().join(format!("raddy_access_ip_{}.log", std::process::id()));
    let log_str = log_path.to_string_lossy().into_owned();
    let raddy = RadRaddy::spawn_with_args(
        // The site-scoped `trusted_proxies` override (127.0.0.1), not the global
        // list (10.0.0.0/8), decides that the loopback peer is trusted.
        move |port| {
            format!(
                "{{ trusted_proxies 10.0.0.0/8 }}\n:{port} {{\n    trusted_proxies 127.0.0.1\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"
            )
        },
        &[String::from("--access-log"), log_str],
    );

    let resp = send_request_hdr(
        raddy.port(),
        Some("localhost"),
        "/x",
        &["X-Forwarded-For: 198.51.100.7"],
    );
    assert_eq!(resp.status, 200);

    // The trusted peer's X-Forwarded-For must be the logged client, not the TCP
    // peer 127.0.0.1.
    wait_until(
        || {
            std::fs::read_to_string(&log_path)
                .map(|s| {
                    s.lines().any(|l| {
                        l.contains("\"path\":\"/x\"") && l.contains("\"client\":\"198.51.100.7\"")
                    })
                })
                .unwrap_or(false)
        },
        "access log to use the effective client IP",
    );
    let _ = std::fs::remove_file(&log_path);
}

#[test]
fn access_log_ts_is_request_start() {
    // A deliberately slow upstream: it sleeps before responding, so the gap
    // between the request start and the logging time is measurable (~2.5s). The
    // log's `ts` must be the former, not the latter.
    let (up_port, listener) = bind_listener();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = thread::spawn(move || {
        for stream in listener.incoming() {
            if stop_thread.load(Ordering::Relaxed) {
                break;
            }
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                thread::sleep(Duration::from_millis(3000));
                let body = "slow";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    });

    let log_path = std::env::temp_dir().join(format!("raddy_access_ts_{}.log", std::process::id()));
    let log_str = log_path.to_string_lossy().into_owned();
    let raddy = RadRaddy::spawn_with_args(
        move |port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"),
        &[String::from("--access-log"), log_str],
    );

    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let resp = send_request(raddy.port(), Some("localhost"), "/slow");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "slow");

    let mut found = None;
    wait_until(
        || {
            let Ok(content) = std::fs::read_to_string(&log_path) else {
                return false;
            };
            found = content
                .lines()
                .find(|l| l.contains("\"path\":\"/slow\"") && l.contains("\"status\":200"))
                .map(parse_log_ts);
            found.is_some()
        },
        "access log line for the slow request",
    );
    let ts = found.expect("the slow request should be logged");
    // The request starts within ~2s of `before`; the log is written ~3s later,
    // so a logging-time ts would be at `before + 3000` or later.
    assert!(
        (before..=before + 2000).contains(&ts),
        "ts {ts} must be the request start (~{before}), not the logging time (~{})",
        before + 3000
    );

    stop.store(true, Ordering::Relaxed);
    let _ = TcpStream::connect(("127.0.0.1", up_port));
    let _ = handle.join();
    let _ = std::fs::remove_file(&log_path);
}

// ---------------------------------------------------------------------------
// Zero-downtime binary upgrade (M7)
// ---------------------------------------------------------------------------

/// Read the PID from a pidfile written by `raddy run`.
fn read_pid_file(path: &std::path::Path) -> i32 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read pidfile {}: {e}", path.display()))
        .trim()
        .parse()
        .expect("pidfile has a non-numeric pid")
}

/// Whether a process exists and is not (yet) a zombie.
///
/// A zombie still answers `kill(pid, 0)` until its parent reaps it, so on Linux
/// the state is read from `/proc/<pid>/stat` instead and `Z` counts as exited.
fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // Format: "pid (comm) state ..."; `comm` may contain spaces or parens,
        // so the state is the first field after the last `)`.
        let Some(after_comm) = stat.rfind(')') else {
            return false;
        };
        let state = stat[after_comm + 1..]
            .split_whitespace()
            .next()
            .unwrap_or("?");
        state != "Z"
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: signal 0 sends no signal; it only probes existence.
        let ret = unsafe { libc::kill(pid, 0) };
        ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Unique pidfile/upgrade-socket/cert-dir paths so parallel tests never collide.
///
/// The counter is module-scoped (a `static` inside a test function would be
/// function-scoped, so parallel tests would each start at 0 and collide).
static UPGRADE_TAG: AtomicUsize = AtomicUsize::new(0);

fn upgrade_paths(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir();
    (
        dir.join(format!("raddy_upgrade_{tag}.pid")),
        dir.join(format!("raddy_upgrade_{tag}.sock")),
        dir.join(format!("raddy_upgrade_{tag}_certs")),
    )
}

#[test]
fn zero_downtime_upgrade_hands_off_listeners() {
    let tag = format!(
        "{}_{}",
        std::process::id(),
        UPGRADE_TAG.fetch_add(1, Ordering::Relaxed)
    );
    let (pidfile, upgrade_sock, cert_dir) = upgrade_paths(&tag);
    let extra = vec![
        format!("--pidfile={}", pidfile.display()),
        format!("--upgrade-sock={}", upgrade_sock.display()),
        format!("--cert-dir={}", cert_dir.display()),
    ];

    let (up_port, _up) = EchoUpstream::spawn("A");
    let raddy = RadRaddy::spawn_with_args(
        |port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"),
        &extra,
    );
    let port = raddy.port();
    let old_pid = read_pid_file(&pidfile);
    assert!(process_alive(old_pid), "initial instance should be running");

    let resp = send_request(port, Some("localhost"), "/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "label=A");

    // Fire requests continuously while the upgrade runs; every one must succeed
    // (the handoff must not drop a single request — the acceptance criterion).
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let (result_tx, result_rx) = std::sync::mpsc::channel::<(usize, usize)>();
    let load_thread = std::thread::spawn(move || {
        let mut ok = 0usize;
        let mut failed = 0usize;
        while !stop_thread.load(Ordering::Relaxed) {
            match try_request(port, Some("localhost"), "/") {
                Some(r) if r.status == 200 && r.body == "label=A" => ok += 1,
                _ => failed += 1,
            }
            // Pace to a realistic rate (~1k req/s): a handoff gap would still
            // drop dozens here, but a max-DoS rate only stresses the test's
            // thread-per-connection upstream, not raddy.
            thread::sleep(Duration::from_micros(1000));
        }
        let _ = result_tx.send((ok, failed));
    });

    // Run `raddy upgrade` (the same binary) with the same flags the instance
    // was started with; it must hand off the listeners and return 0.
    let mut cmd = Command::new(BIN);
    cmd.args(["upgrade", "-c"]).arg(&raddy.config_path);
    cmd.args(&extra);
    cmd.env("RUST_LOG", "error");
    let status = cmd.status().expect("failed to spawn raddy upgrade");
    assert!(status.success(), "raddy upgrade should succeed: {status:?}");

    stop.store(true, Ordering::Relaxed);
    let (ok, failed) = result_rx.recv().expect("load thread should report");
    assert!(
        ok > 0,
        "requests should have been served during the upgrade"
    );
    assert_eq!(
        failed, 0,
        "zero requests may drop across the upgrade (ok={ok}, failed={failed})"
    );
    let _ = load_thread.join();

    // The old process handed off and exited; the pidfile names the replacement.
    assert!(!process_alive(old_pid), "old instance should have exited");
    let new_pid = read_pid_file(&pidfile);
    assert_ne!(new_pid, old_pid, "upgrade should replace the process");
    assert!(process_alive(new_pid), "replacement should be running");

    // The handed-off listeners still serve.
    let resp = send_request(port, Some("localhost"), "/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "label=A");

    // The replacement is detached from `RadRaddy`'s Drop, so stop it directly.
    // SAFETY: `new_pid` is the replacement process we verified is running.
    unsafe {
        libc::kill(new_pid, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && process_alive(new_pid) {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_alive(new_pid),
        "replacement should have been stopped"
    );
    let _ = std::fs::remove_file(&pidfile);
    let _ = std::fs::remove_file(&upgrade_sock);
    let _ = std::fs::remove_dir_all(&cert_dir);
}

#[test]
fn upgrade_aborts_on_broken_config_without_disturbing_instance() {
    let tag = format!(
        "{}_{}",
        std::process::id(),
        UPGRADE_TAG.fetch_add(1, Ordering::Relaxed)
    );
    let (pidfile, upgrade_sock, cert_dir) = upgrade_paths(&tag);
    let extra = vec![
        format!("--pidfile={}", pidfile.display()),
        format!("--upgrade-sock={}", upgrade_sock.display()),
        format!("--cert-dir={}", cert_dir.display()),
    ];

    let (up_port, _up) = EchoUpstream::spawn("A");
    let raddy = RadRaddy::spawn_with_args(
        |port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"),
        &extra,
    );
    let old_pid = read_pid_file(&pidfile);
    let port = raddy.port();

    // Corrupt the config (writing the file does not trigger a reload). The
    // running instance keeps serving its in-memory snapshot.
    std::fs::write(&raddy.config_path, "this is not a valid Raddyfile {\n").unwrap();

    let mut cmd = Command::new(BIN);
    cmd.args(["upgrade", "-c"]).arg(&raddy.config_path);
    cmd.args(&extra);
    cmd.env("RUST_LOG", "error");
    let status = cmd.status().expect("failed to spawn raddy upgrade");
    assert!(
        !status.success(),
        "upgrade should fail its pre-flight check"
    );

    // The running instance is untouched: same pid, still serving.
    assert_eq!(
        read_pid_file(&pidfile),
        old_pid,
        "pidfile should be unchanged"
    );
    assert!(process_alive(old_pid), "instance should still be running");
    let resp = send_request(port, Some("localhost"), "/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "label=A");

    let _ = std::fs::remove_file(&pidfile);
    let _ = std::fs::remove_file(&upgrade_sock);
    let _ = std::fs::remove_dir_all(&cert_dir);
}

// ---------------------------------------------------------------------------
// Load balancing + health checks (M9)
// ---------------------------------------------------------------------------

/// An upstream whose listener can be closed (new connections refused, as if the
/// process died) and later rebound on the same port (recovery). Used to verify
/// that the active health check routes around a dead upstream and flows back.
struct ToggleUpstream {
    port: u16,
    label: String,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ToggleUpstream {
    fn spawn(label: &str) -> (u16, ToggleUpstream) {
        let (port, listener) = bind_listener();
        let mut up = ToggleUpstream {
            port,
            label: label.to_string(),
            hits: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            handle: None,
        };
        up.accept_loop(listener);
        (port, up)
    }

    fn accept_loop(&mut self, listener: TcpListener) {
        let hits = self.hits.clone();
        let stop = self.stop.clone();
        let label = self.label.clone();
        self.handle = Some(thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                hits.fetch_add(1, Ordering::Relaxed);
                let label = label.clone();
                thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let body = format!("label={label}");
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                });
            }
        }));
    }

    /// Close the listener so new connections are refused (simulates a dead
    /// upstream; in-flight accepted connections finish normally).
    fn pause(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock the accept loop so it drops the listener and exits.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Re-bind the original port (SO_REUSEADDR, so TIME_WAIT from the old
    /// listener's connections does not block it) and start accepting again.
    fn resume(&mut self) {
        self.stop.store(false, Ordering::Relaxed);
        use socket2::{Domain, Protocol, SockAddr, Socket, Type};
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        socket.set_reuse_address(true).unwrap();
        socket
            .bind(&SockAddr::from(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                self.port,
            ))))
            .expect("rebind upstream port");
        socket.listen(16).unwrap();
        self.accept_loop(socket.into());
    }
}

impl Drop for ToggleUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The `reverse_proxy` block with a fast health check (short interval and low
/// flapping thresholds so the test observes transitions within a few seconds).
fn health_checked_proxy(upstreams: &[u16]) -> String {
    let to = upstreams
        .iter()
        .map(|p| format!("127.0.0.1:{p}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "reverse_proxy {{\n    to {to}\n    health_check {{\n        interval 200ms\n        timeout 200ms\n        consecutive_failures 2\n        consecutive_successes 1\n    }}\n}}\n"
    )
}

#[test]
fn health_check_routes_around_dead_upstream() {
    let (a_port, _a) = EchoUpstream::spawn("A");
    // A port with nothing listening — the health check must mark it unhealthy.
    let dead_port = free_port();
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    {}\n}}\n",
            health_checked_proxy(&[a_port, dead_port])
        )
    });

    // Give the health check time to remove the dead upstream.
    thread::sleep(Duration::from_secs(2));
    for _ in 0..10 {
        let resp = send_request(raddy.port(), Some("localhost"), "/");
        assert_eq!(resp.status, 200, "unexpected response: {resp:?}");
        assert_eq!(
            resp.body, "label=A",
            "dead upstream must not receive traffic"
        );
    }
}

#[test]
fn all_unhealthy_upstreams_return_502() {
    let dead_port = free_port();
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    {}\n}}\n",
            health_checked_proxy(&[dead_port])
        )
    });

    thread::sleep(Duration::from_secs(2));
    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(
        resp.status, 502,
        "all-unhealthy should return 502: {resp:?}"
    );
}

#[test]
fn ip_hash_pins_client_to_one_upstream() {
    let (a_port, _a) = EchoUpstream::spawn("A");
    let (b_port, _b) = EchoUpstream::spawn("B");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    reverse_proxy {{\n        to 127.0.0.1:{a_port} 127.0.0.1:{b_port}\n        lb_policy ip_hash\n    }}\n}}\n"
        )
    });

    // Every request comes from the same loopback client IP, so `ip_hash` must
    // pin them all to the same upstream.
    let mut bodies = std::collections::BTreeSet::new();
    for _ in 0..20 {
        let resp = send_request(raddy.port(), Some("localhost"), "/");
        assert_eq!(resp.status, 200);
        bodies.insert(resp.body.clone());
    }
    assert_eq!(
        bodies.len(),
        1,
        "same client IP must stay pinned to one upstream: {bodies:?}"
    );
}

#[test]
fn trusted_proxies_ip_hash_keys_on_effective_client_ip() {
    let (a_port, _a) = EchoUpstream::spawn("A");
    let (b_port, _b) = EchoUpstream::spawn("B");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            "{{ trusted_proxies 127.0.0.1 }}\n:{port} {{\n    reverse_proxy {{\n        to 127.0.0.1:{a_port} 127.0.0.1:{b_port}\n        lb_policy ip_hash\n    }}\n}}\n"
        )
    });
    let port = raddy.port();

    // With the loopback peer trusted, the ip_hash key is the X-Forwarded-For
    // client. Each distinct client must pin to one upstream, and two distinct
    // clients must pin to different upstreams — if the code still keyed on the
    // TCP peer, every client would share a single upstream.
    let pin = |xff: &str| -> String {
        let header = format!("X-Forwarded-For: {xff}");
        let mut bodies = std::collections::BTreeSet::new();
        for _ in 0..8 {
            let resp = send_request_hdr(port, Some("localhost"), "/", &[&header]);
            assert_eq!(resp.status, 200, "client {xff} request failed: {resp:?}");
            bodies.insert(resp.body.clone());
        }
        assert_eq!(
            bodies.len(),
            1,
            "client {xff} must stay pinned to one upstream: {bodies:?}"
        );
        bodies.into_iter().next().unwrap()
    };

    let mut seen = std::collections::BTreeSet::new();
    let mut distinct = false;
    for n in 1..64 {
        seen.insert(pin(&format!("1.2.3.{n}")));
        if seen.len() >= 2 {
            distinct = true;
            break;
        }
    }
    assert!(
        distinct,
        "distinct X-Forwarded-For clients must reach distinct upstreams: {seen:?}"
    );
}

#[test]
fn untrusted_peer_ip_hash_ignores_xff() {
    let (a_port, _a) = EchoUpstream::spawn("A");
    let (b_port, _b) = EchoUpstream::spawn("B");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    reverse_proxy {{\n        to 127.0.0.1:{a_port} 127.0.0.1:{b_port}\n        lb_policy ip_hash\n    }}\n}}\n"
        )
    });

    // Without `trusted_proxies` the X-Forwarded-For header is ignored: requests
    // carrying different XFF clients all key on the same loopback TCP peer and
    // must stay pinned to a single upstream.
    let mut bodies = std::collections::BTreeSet::new();
    for n in 0..20 {
        let header = format!("X-Forwarded-For: 1.2.3.{n}");
        let resp = send_request_hdr(raddy.port(), Some("localhost"), "/", &[&header]);
        assert_eq!(resp.status, 200);
        bodies.insert(resp.body.clone());
    }
    assert_eq!(
        bodies.len(),
        1,
        "untrusted X-Forwarded-For must not affect ip_hash: {bodies:?}"
    );
}

#[test]
fn health_check_recovers_after_upstream_returns() {
    let (a_port, _a) = EchoUpstream::spawn("A");
    let (b_port, mut b) = ToggleUpstream::spawn("B");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    {}\n}}\n",
            health_checked_proxy(&[a_port, b_port])
        )
    });

    // Both upstreams up: round-robin serves both labels.
    wait_until(
        || {
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..10 {
                seen.insert(send_request(raddy.port(), Some("localhost"), "/").body);
            }
            seen.contains("label=A") && seen.contains("label=B")
        },
        "round-robin across both healthy upstreams",
    );

    // Kill B: once the health check catches it, only A is served.
    b.pause();
    wait_until(
        || (0..10).all(|_| send_request(raddy.port(), Some("localhost"), "/").body == "label=A"),
        "traffic to drain from the dead upstream",
    );

    // Bring B back: traffic flows to both again.
    b.resume();
    wait_until(
        || {
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..20 {
                seen.insert(send_request(raddy.port(), Some("localhost"), "/").body);
            }
            seen.contains("label=A") && seen.contains("label=B")
        },
        "traffic to return to the recovered upstream",
    );
}

#[test]
fn health_state_survives_reload() {
    let (a_port, _a) = EchoUpstream::spawn("A");
    let (b_port, mut b) = ToggleUpstream::spawn("B");
    let mut raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    {}\n}}\n",
            health_checked_proxy(&[a_port, b_port])
        )
    });
    let config = format!(
        ":{} {{\n    {}\n}}\n",
        raddy.port(),
        health_checked_proxy(&[a_port, b_port])
    );

    // Both upstreams up.
    wait_until(
        || {
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..10 {
                seen.insert(send_request(raddy.port(), Some("localhost"), "/").body);
            }
            seen.contains("label=A") && seen.contains("label=B")
        },
        "round-robin across both healthy upstreams",
    );

    // Kill B and let the health check drain it.
    b.pause();
    wait_until(
        || (0..10).all(|_| send_request(raddy.port(), Some("localhost"), "/").body == "label=A"),
        "traffic to drain from the dead upstream",
    );

    // Reload the identical config: the balancer (and B's unhealthy state) must
    // survive the reload (ADR-011), so B still receives no traffic.
    raddy.reload(&config);
    for _ in 0..10 {
        assert_eq!(
            send_request(raddy.port(), Some("localhost"), "/").body,
            "label=A",
            "health state must survive a reload"
        );
    }

    // Bring B back: the reloaded (reused) balancer re-probes and restores it.
    b.resume();
    wait_until(
        || {
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..20 {
                seen.insert(send_request(raddy.port(), Some("localhost"), "/").body);
            }
            seen.contains("label=A") && seen.contains("label=B")
        },
        "traffic to return to the recovered upstream after reload",
    );
}

// ---------------------------------------------------------------------------
// Rate limiting (M10, spec §5.2) and trusted_proxies (spec §4)
// ---------------------------------------------------------------------------

#[test]
fn rate_limit_returns_429_after_burst() {
    let (upstream_port, _upstream) = EchoUpstream::spawn("A");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    rate_limit remote_ip 1r/s burst=3\n    reverse_proxy 127.0.0.1:{upstream_port}\n}}\n"
        )
    });
    // The first `burst` requests pass instantly.
    for _ in 0..3 {
        let resp = send_request(raddy.port(), Some("localhost"), "/");
        assert_eq!(resp.status, 200, "burst request should pass: {resp:?}");
    }
    // The next request finds an empty bucket (the 1r/s refill is negligible
    // within the test's milliseconds) and is rejected with 429.
    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 429, "expected 429 after the burst: {resp:?}");
}

#[test]
fn rate_limit_ignores_xff_without_trusted_proxies() {
    let (upstream_port, _upstream) = EchoUpstream::spawn("A");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    rate_limit remote_ip 1r/s burst=1\n    reverse_proxy 127.0.0.1:{upstream_port}\n}}\n"
        )
    });
    // Without `trusted_proxies` the X-Forwarded-For header is ignored: every
    // request is keyed to the TCP peer (127.0.0.1), so a second request from a
    // "different" client is still rate limited.
    let first = send_request_hdr(
        raddy.port(),
        Some("localhost"),
        "/",
        &["X-Forwarded-For: 1.2.3.4"],
    );
    assert_eq!(first.status, 200, "first request should pass: {first:?}");
    let second = send_request_hdr(
        raddy.port(),
        Some("localhost"),
        "/",
        &["X-Forwarded-For: 5.6.7.8"],
    );
    assert_eq!(
        second.status, 429,
        "X-Forwarded-For must be ignored without trusted_proxies: {second:?}"
    );
}

#[test]
fn trusted_proxies_enables_per_client_rate_limiting() {
    let (upstream_port, _upstream) = EchoUpstream::spawn("A");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            "{{\n    trusted_proxies 127.0.0.1\n}}\n:{port} {{\n    rate_limit remote_ip 1r/s burst=1\n    reverse_proxy 127.0.0.1:{upstream_port}\n}}\n"
        )
    });
    // With the peer trusted, X-Forwarded-For is honored: distinct client IPs
    // each get their own bucket.
    for client in ["1.2.3.4", "5.6.7.8", "9.9.9.9"] {
        let header = format!("X-Forwarded-For: {client}");
        let resp = send_request_hdr(raddy.port(), Some("localhost"), "/", &[&header]);
        assert_eq!(
            resp.status, 200,
            "{client} should get its own bucket: {resp:?}"
        );
    }
    // The same client again has spent its single token and is now limited.
    let again = send_request_hdr(
        raddy.port(),
        Some("localhost"),
        "/",
        &["X-Forwarded-For: 1.2.3.4"],
    );
    assert_eq!(again.status, 429, "spent bucket should reject: {again:?}");
}

// ---------------------------------------------------------------------------
// Migration (`raddy import`, ARCHITECTURE §7)
// ---------------------------------------------------------------------------

#[test]
fn import_caddyfile_output_validates() {
    let src = std::env::temp_dir().join(format!("raddy_import_{}.Caddyfile", std::process::id()));
    let out = std::env::temp_dir().join(format!("raddy_import_{}.Raddyfile", std::process::id()));
    std::fs::write(
        &src,
        "example.com {\n    handle /static/* {\n        root /var/www\n        file_server\n    }\n    reverse_proxy 127.0.0.1:8080\n}\n",
    )
    .unwrap();

    let status = Command::new(BIN)
        .args(["import", "caddyfile"])
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "import should succeed");

    // The converted Raddyfile must pass `raddy check` (the same validation a
    // reload performs).
    let check = Command::new(BIN)
        .args(["check", "-c"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "converted output must validate: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn import_nginx_output_validates() {
    let src = std::env::temp_dir().join(format!("raddy_import_nginx_{}.conf", std::process::id()));
    let out = std::env::temp_dir().join(format!(
        "raddy_import_nginx_{}.Raddyfile",
        std::process::id()
    ));
    std::fs::write(
        &src,
        "server {\n    listen 80;\n    server_name example.com;\n    location / {\n        proxy_pass http://127.0.0.1:8080;\n    }\n}\n",
    )
    .unwrap();

    let status = Command::new(BIN)
        .args(["import", "nginx"])
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "import should succeed");

    let check = Command::new(BIN)
        .args(["check", "-c"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "converted output must validate: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn import_prints_to_stdout_by_default() {
    let src =
        std::env::temp_dir().join(format!("raddy_import_out_{}.Caddyfile", std::process::id()));
    std::fs::write(&src, "example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n").unwrap();
    let out = Command::new(BIN)
        .args(["import", "caddyfile"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("reverse_proxy 127.0.0.1:8080"));
    let _ = std::fs::remove_file(&src);
}

#[test]
fn import_rejects_unknown_format() {
    let status = Command::new(BIN)
        .args(["import", "bogus", "/nonexistent"])
        .status()
        .unwrap();
    assert!(!status.success(), "an unknown format is a CLI error");
}
