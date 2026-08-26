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
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

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

/// A minimal HTTP/1.1 upstream bound to IPv6 loopback.
struct Ipv6EchoUpstream {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Ipv6EchoUpstream {
    fn spawn(label: &str) -> (u16, Ipv6EchoUpstream) {
        let listener = TcpListener::bind("[::1]:0").expect("bind IPv6 upstream");
        let port = listener.local_addr().expect("IPv6 upstream address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let label = label.to_string();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let label = label.clone();
                thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let body = format!("label={label}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                });
            }
        });
        (
            port,
            Ipv6EchoUpstream {
                port,
                stop,
                handle: Some(handle),
            },
        )
    }
}

impl Drop for Ipv6EchoUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("::1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A minimal HTTPS upstream (spec §5.4): accepts TCP connections, wraps them in
/// TLS with a self-signed certificate for `localhost`, and answers
/// `label=secure`. The certificate is deliberately untrusted by system roots,
/// so a proxy that verifies upstream certs must be configured with
/// `tls_skip_verify` (or the operator must trust the test CA).
struct TlsEchoUpstream {
    port: u16,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TlsEchoUpstream {
    fn spawn() -> (u16, TlsEchoUpstream) {
        use openssl::ssl::{SslAcceptor, SslMethod};
        let (port, listener) = bind_listener();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_pem = cert.cert.pem();
        let key_pem = cert.signing_key.serialize_pem();
        let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        let x509 = openssl::x509::X509::from_pem(cert_pem.as_bytes()).unwrap();
        let pkey = openssl::pkey::PKey::private_key_from_pem(key_pem.as_bytes()).unwrap();
        builder.set_certificate(&x509).unwrap();
        builder.set_private_key(&pkey).unwrap();
        let acceptor = builder.build();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let hits_thread = hits.clone();
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                hits_thread.fetch_add(1, Ordering::Relaxed);
                let acceptor = acceptor.clone();
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let Ok(mut tls) = acceptor.accept(stream) else {
                        return;
                    };
                    let mut buf = [0u8; 4096];
                    let _ = tls.read(&mut buf);
                    let body = "label=secure";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = tls.write_all(resp.as_bytes());
                });
            }
        });
        (
            port,
            TlsEchoUpstream {
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

impl Drop for TlsEchoUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A minimal protocol-upgrade upstream (spec §5.5): answers any request that
/// asks for an upgrade with `101 Switching Protocols`, then echoes raw bytes
/// back so the tunnel can be verified end to end.
struct UpgradeEchoUpstream {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl UpgradeEchoUpstream {
    fn spawn() -> (u16, UpgradeEchoUpstream) {
        let (port, listener) = bind_listener();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    // Read the request head (single read suffices for a GET).
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while head.len() < 8192 {
                        match stream.read(&mut byte) {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {
                                head.push(byte[0]);
                                if head.ends_with(b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    if !head
                        .windows(b"Upgrade".len())
                        .any(|w| w.eq_ignore_ascii_case(b"Upgrade"))
                    {
                        return;
                    }
                    let resp = "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
                    if stream.write_all(resp.as_bytes()).is_err() {
                        return;
                    }
                    // Echo raw bytes until either side closes the tunnel.
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if stream.write_all(&buf[..n]).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        (
            port,
            UpgradeEchoUpstream {
                port,
                stop,
                handle: Some(handle),
            },
        )
    }
}

impl Drop for UpgradeEchoUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A raw TCP echo upstream (no HTTP framing): returns each received message
/// prefixed with `echo:`, and counts accepted connections. Used to verify the
/// layer-4 raw-TCP proxy (L4_PROXY_PLAN P0).
struct TcpEchoUpstream {
    port: u16,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TcpEchoUpstream {
    fn spawn() -> (u16, TcpEchoUpstream) {
        let (port, listener) = bind_listener();
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let hits_thread = hits.clone();
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                hits_thread.fetch_add(1, Ordering::Relaxed);
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if stream
                                    .write_all(b"echo:")
                                    .and_then(|()| stream.write_all(&buf[..n]))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        (
            port,
            TcpEchoUpstream {
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

impl Drop for TcpEchoUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A raw TCP upstream that waits for the client half-close before replying.
/// This verifies that the proxy propagates EOF in one direction while draining
/// a response in the other direction.
struct TcpHalfCloseUpstream {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TcpHalfCloseUpstream {
    fn spawn() -> (u16, TcpHalfCloseUpstream) {
        let (port, listener) = bind_listener();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => {
                            let _ = stream.write_all(b"after-half-close");
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
        });
        (
            port,
            TcpHalfCloseUpstream {
                port,
                stop,
                handle: Some(handle),
            },
        )
    }
}

impl Drop for TcpHalfCloseUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A cleartext HTTP/2 prior-knowledge upstream used to verify `h2c://` peers.
struct H2EchoUpstream {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

/// A TLS HTTP/2 upstream used to verify `h2://` peers, including ALPN
/// negotiation and the upstream TLS verification options.
struct H2TlsEchoUpstream {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl H2TlsEchoUpstream {
    fn spawn() -> (u16, H2TlsEchoUpstream) {
        use openssl::ssl::{AlpnError, Ssl, SslAcceptor, SslMethod};
        let (port, listener) = bind_listener();
        listener.set_nonblocking(true).unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_pem = cert.cert.pem();
        let key_pem = cert.signing_key.serialize_pem();
        let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        let x509 = openssl::x509::X509::from_pem(cert_pem.as_bytes()).unwrap();
        let pkey = openssl::pkey::PKey::private_key_from_pem(key_pem.as_bytes()).unwrap();
        builder.set_certificate(&x509).unwrap();
        builder.set_private_key(&pkey).unwrap();
        builder.set_alpn_select_callback(|_, client| {
            openssl::ssl::select_next_proto(b"\x02h2", client).ok_or(AlpnError::NOACK)
        });
        let acceptor = builder.build();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build TLS h2 upstream runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("convert TLS h2 upstream listener");
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(25)) => {
                            if stop_thread.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { continue };
                            let acceptor = acceptor.clone();
                            tokio::spawn(async move {
                                let ssl = Ssl::new(acceptor.context()).expect("create TLS state");
                                let mut tls = tokio_openssl::SslStream::new(ssl, stream)
                                    .expect("wrap TLS stream");
                                if std::pin::Pin::new(&mut tls).accept().await.is_err() {
                                    return;
                                }
                                let Ok(mut connection) = h2::server::handshake(tls).await else {
                                    return;
                                };
                                while let Some(Ok((request, mut respond))) = connection.accept().await {
                                    let _ = request;
                                    let response = http::Response::builder()
                                        .status(200)
                                        .header("content-length", "6")
                                        .body(())
                                        .expect("build TLS h2 response");
                                    let Ok(mut send) = respond.send_response(response, false) else {
                                        break;
                                    };
                                    if send
                                        .send_data(bytes::Bytes::from_static(b"h2-tls"), true)
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            });
                        }
                    }
                }
            });
        });
        (
            port,
            H2TlsEchoUpstream {
                port,
                stop,
                handle: Some(handle),
            },
        )
    }
}

impl Drop for H2TlsEchoUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl H2EchoUpstream {
    fn spawn() -> (u16, H2EchoUpstream) {
        let (port, listener) = bind_listener();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build h2 upstream runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("convert h2 upstream listener");
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(25)) => {
                            if stop_thread.load(Ordering::Relaxed) {
                                break;
                            }
                        }
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { continue };
                            tokio::spawn(async move {
                                let Ok(mut connection) = h2::server::handshake(stream).await else {
                                    return;
                                };
                                while let Some(Ok((request, mut respond))) = connection.accept().await {
                                    let _ = request;
                                    let response = http::Response::builder()
                                        .status(200)
                                        .header("content-length", "5")
                                        .body(())
                                        .expect("build h2 response");
                                    let Ok(mut send) = respond.send_response(response, false) else {
                                        break;
                                    };
                                    if send
                                        .send_data(bytes::Bytes::from_static(b"h2-ok"), true)
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            });
                        }
                    }
                }
            });
        });
        (
            port,
            H2EchoUpstream {
                port,
                stop,
                handle: Some(handle),
            },
        )
    }
}

impl Drop for H2EchoUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A raw UDP echo upstream: returns each received datagram prefixed with its
/// label (`<label>:<data>`), so tests can tell which upstream served a client.
struct UdpEchoUpstream {
    port: u16,
    ipv6: bool,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl UdpEchoUpstream {
    fn spawn(label: &str) -> (u16, UdpEchoUpstream) {
        Self::spawn_on(label, "127.0.0.1:0", false)
    }

    fn spawn_ipv6(label: &str) -> (u16, UdpEchoUpstream) {
        Self::spawn_on(label, "[::1]:0", true)
    }

    fn spawn_on(label: &str, bind: &str, ipv6: bool) -> (u16, UdpEchoUpstream) {
        let sock = std::net::UdpSocket::bind(bind).unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let port = sock.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let label = label.to_string();
        let handle = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                match sock.recv_from(&mut buf) {
                    Ok((n, src)) => {
                        let mut reply = label.as_bytes().to_vec();
                        reply.push(b':');
                        reply.extend_from_slice(&buf[..n]);
                        let _ = sock.send_to(&reply, src);
                    }
                    Err(_) => continue, // read timeout; re-check stop
                }
            }
        });
        (
            port,
            UdpEchoUpstream {
                port,
                ipv6,
                stop,
                handle: Some(handle),
            },
        )
    }
}

impl Drop for UdpEchoUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the blocking recv so the thread can check stop and exit.
        let wake_addr = if self.ipv6 {
            SocketAddr::new("::1".parse().unwrap(), self.port)
        } else {
            SocketAddr::new("127.0.0.1".parse().unwrap(), self.port)
        };
        let bind_addr = if self.ipv6 { "[::1]:0" } else { "127.0.0.1:0" };
        let _ = std::net::UdpSocket::bind(bind_addr).and_then(|s| s.send_to(b"x", wake_addr));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Send a datagram through a `udp` listener and return the reply (the upstream
/// echoes `<label>:<data>`).
fn udp_roundtrip(port: u16, msg: &str) -> Option<String> {
    let client = std::net::UdpSocket::bind("127.0.0.1:0").ok()?;
    client.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    client.send_to(msg.as_bytes(), ("127.0.0.1", port)).ok()?;
    let mut buf = [0u8; 256];
    let (n, _) = client.recv_from(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Send a datagram from a caller-owned UDP socket and read one reply.
fn udp_roundtrip_on(client: &UdpSocket, port: u16, msg: &str) -> Option<String> {
    client.send_to(msg.as_bytes(), ("127.0.0.1", port)).ok()?;
    let mut buf = [0u8; 256];
    let (n, _) = client.recv_from(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Send a datagram through an IPv6 UDP listener and return the reply.
fn udp_roundtrip_ipv6(port: u16, msg: &str) -> Option<String> {
    let client = UdpSocket::bind("[::1]:0").ok()?;
    client.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let target = SocketAddr::new("::1".parse().ok()?, port);
    client.send_to(msg.as_bytes(), target).ok()?;
    let mut buf = [0u8; 256];
    let (n, _) = client.recv_from(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Read a numeric Prometheus metric from a test server.
fn metric_value(port: u16, name: &str) -> Option<u64> {
    let response = try_request(port, None, "/metrics")?;
    response
        .body
        .lines()
        .find(|line| line.starts_with(name))
        .and_then(|line| line.split_whitespace().last())
        .and_then(|value| value.parse::<u64>().ok())
}

/// An upstream that answers `path=<request path>`, so tests can assert what
/// path a `handle_path` strip or `rewrite` produced (spec §5.9).
struct PathEchoUpstream {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl PathEchoUpstream {
    fn spawn() -> (u16, PathEchoUpstream) {
        let (port, listener) = bind_listener();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = [0u8; 8192];
                    if stream.read(&mut buf).is_err() {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf);
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let body = format!("path={path}");
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
            PathEchoUpstream {
                port,
                stop,
                handle: Some(handle),
            },
        )
    }
}

impl Drop for PathEchoUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A minimal auth upstream for `forward_auth` (spec §5.10): grants `/` with
/// `X-Auth-User`, denies `/deny` with 401.
struct AuthUpstream {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl AuthUpstream {
    fn spawn() -> (u16, AuthUpstream) {
        let (port, listener) = bind_listener();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = [0u8; 8192];
                    if stream.read(&mut buf).is_err() {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf);
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let granted = !path.starts_with("/deny");
                    let (status, reason, extra) = if granted {
                        (200, "OK", "X-Auth-User: alice\r\n")
                    } else {
                        (401, "Unauthorized", "")
                    };
                    let body = if granted { "granted" } else { "denied" };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                });
            }
        });
        (
            port,
            AuthUpstream {
                port,
                stop,
                handle: Some(handle),
            },
        )
    }
}

impl Drop for AuthUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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

/// How a spawned server's readiness is probed. TLS sites (spec §5.7) cannot be
/// reached with a plain-HTTP probe, and an mTLS `require` site rejects every
/// handshake — those need a TLS or TCP-only probe respectively.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadyProbe {
    Plain,
    Tls,
    Tcp,
    /// A UDP listener: the child is alive and a datagram send succeeds
    /// (UDP is connectionless; the actual relay is polled by the test).
    Udp,
    /// An IPv6 UDP listener, probed through the IPv6 loopback address.
    UdpV6,
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

    /// Spawn with a config plus extra environment variables for the child
    /// (used for `{$ENV}` expansion, spec §5.12).
    fn spawn_with_env(config_for: impl Fn(u16) -> String, env: &[(&str, &str)]) -> RadRaddy {
        Self::spawn_with_env_probe(config_for, env, ReadyProbe::Plain)
    }

    /// Spawn with extra CLI arguments (e.g. `--metrics-addr`, `--access-log`).
    fn spawn_with_args(config_for: impl Fn(u16) -> String, extra: &[String]) -> RadRaddy {
        Self::spawn_with_probe(config_for, extra, ReadyProbe::Plain)
    }

    /// Spawn a server whose site's port is served over TLS (a named site with a
    /// `tls` directive, spec §5.7); the readiness probe performs a TLS
    /// handshake.
    fn spawn_tls(config_for: impl Fn(u16) -> String) -> RadRaddy {
        Self::spawn_with_probe(config_for, &[], ReadyProbe::Tls)
    }

    /// Spawn a server whose listener rejects normal probes (e.g. mTLS `require`);
    /// readiness is a TCP accept only.
    fn spawn_tcp(config_for: impl Fn(u16) -> String) -> RadRaddy {
        Self::spawn_with_probe(config_for, &[], ReadyProbe::Tcp)
    }

    /// Spawn a server for a `udp` listener (L4 P2); readiness is a UDP send
    /// succeeding while the child stays alive.
    fn spawn_udp(config_for: impl Fn(u16) -> String) -> RadRaddy {
        Self::spawn_with_probe(config_for, &[], ReadyProbe::Udp)
    }

    /// Like [`spawn_udp`], with extra CLI arguments.
    fn spawn_udp_with_args(config_for: impl Fn(u16) -> String, extra: &[String]) -> RadRaddy {
        Self::spawn_with_probe(config_for, extra, ReadyProbe::Udp)
    }

    /// Spawn a server for a UDP listener bound to the IPv6 loopback address.
    fn spawn_udp_ipv6(config_for: impl Fn(u16) -> String) -> RadRaddy {
        Self::spawn_with_probe(config_for, &[], ReadyProbe::UdpV6)
    }

    fn spawn_with_env_probe(
        config_for: impl Fn(u16) -> String,
        env: &[(&str, &str)],
        probe: ReadyProbe,
    ) -> RadRaddy {
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
            cmd.env("RUST_LOG", "error").stderr(Stdio::inherit());
            for (name, value) in env {
                cmd.env(name, value);
            }
            let child = cmd.spawn().expect("failed to spawn the raddy binary");
            let mut raddy = RadRaddy {
                child,
                port,
                config_path,
            };
            let ready = match probe {
                ReadyProbe::Plain => raddy.wait_for_ready(),
                ReadyProbe::Tls => raddy.wait_for_ready_tls(),
                ReadyProbe::Tcp => raddy.wait_for_ready_tcp(),
                ReadyProbe::Udp => raddy.wait_for_ready_udp(),
                ReadyProbe::UdpV6 => raddy.wait_for_ready_udp_v6(),
            };
            if ready {
                return raddy;
            }
            // The child exited (bind failure) or never listened; retry.
            let _ = raddy.child.kill();
            let _ = raddy.child.wait();
        }
    }

    fn spawn_with_probe(
        config_for: impl Fn(u16) -> String,
        extra: &[String],
        probe: ReadyProbe,
    ) -> RadRaddy {
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
            let ready = match probe {
                ReadyProbe::Plain => raddy.wait_for_ready(),
                ReadyProbe::Tls => raddy.wait_for_ready_tls(),
                ReadyProbe::Tcp => raddy.wait_for_ready_tcp(),
                ReadyProbe::Udp => raddy.wait_for_ready_udp(),
                ReadyProbe::UdpV6 => raddy.wait_for_ready_udp_v6(),
            };
            if ready {
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

    /// Poll until the TLS listener serves a handshake (spec §5.7); the plain
    /// `wait_for_ready` probe cannot reach a TLS listener.
    fn wait_for_ready_tls(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.child.try_wait().expect("try_wait failed").is_some() {
                return false;
            }
            if tls_request(self.port, "localhost", "/", &[]).is_ok() {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Poll until the child is alive and its listener accepts TCP. For sites
    /// that reject every probe request (e.g. mTLS `require`, spec §5.7), a
    /// successful TCP accept is the strongest readiness signal available.
    fn wait_for_ready_tcp(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.child.try_wait().expect("try_wait failed").is_some() {
                return false;
            }
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Poll until the child is alive and a datagram send to the UDP listener
    /// succeeds. UDP is connectionless (a send succeeds regardless), so this
    /// mainly catches a child that failed to bind (e.g. a port conflict) and
    /// exits; the test polls the actual relay to confirm readiness.
    fn wait_for_ready_udp(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.child.try_wait().expect("try_wait failed").is_some() {
                return false;
            }
            if let Ok(sock) = UdpSocket::bind("127.0.0.1:0") {
                if sock.send_to(b"ready", ("127.0.0.1", self.port)).is_ok() {
                    return true;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Poll until an IPv6 UDP listener is alive and accepts a datagram send.
    fn wait_for_ready_udp_v6(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        let target = SocketAddr::new("::1".parse().expect("IPv6 loopback"), self.port);
        while Instant::now() < deadline {
            if self.child.try_wait().expect("try_wait failed").is_some() {
                return false;
            }
            if let Ok(sock) = UdpSocket::bind("[::1]:0") {
                if sock.send_to(b"ready", target).is_ok() {
                    return true;
                }
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

/// Send a GET over the IPv6 loopback address and parse its response.
fn send_request_ipv6(port: u16, host: Option<&str>, path: &str) -> Response {
    let mut stream = TcpStream::connect(("::1", port)).expect("IPv6 request connect failed");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set IPv6 request timeout");
    let host_line = host.map(|h| format!("Host: {h}\r\n")).unwrap_or_default();
    let request = format!("GET {path} HTTP/1.1\r\n{host_line}Connection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write IPv6 request");
    read_one_response(&mut stream)
}

/// Send a GET over TLS (spec §5.7) and return the parsed response. The server
/// certificate is not verified (test sites use self-signed certs). `Err` on
/// connect/handshake failure — callers use this to assert mTLS rejections.
fn tls_request(port: u16, host: &str, path: &str, headers: &[&str]) -> Result<Response, String> {
    let connector = tls_connector(None);
    let stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut tls = connector
        .connect(host, stream)
        .map_err(|e| format!("TLS handshake failed: {e}"))?;
    let extra = headers
        .iter()
        .map(|h| format!("{h}\r\n"))
        .collect::<String>();
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n");
    tls.write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    tls.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf);
    let head_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
    let head = &text[..head_end];
    Ok(Response {
        status: parse_status(head),
        headers: head.to_lowercase(),
        body: text[head_end..].to_string(),
    })
}

/// A TLS client connector with verification off and optional ALPN protocols
/// (each `&[u8]` is the wire-format ALPN list, e.g. `b"\x02h2\x08http/1.1"`).
fn tls_connector(alpn: Option<&[u8]>) -> SslConnector {
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    if let Some(alpn) = alpn {
        builder.set_alpn_protos(alpn).unwrap();
    }
    builder.build()
}

/// The ALPN protocol a TLS client negotiates (spec §5.6), if any.
fn tls_negotiated_alpn(port: u16, host: &str) -> Option<String> {
    let connector = tls_connector(Some(b"\x02h2\x08http/1.1"));
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let tls = connector.connect(host, stream).unwrap();
    tls.ssl()
        .selected_alpn_protocol()
        .map(|proto| String::from_utf8_lossy(proto).into_owned())
}

/// The PEM of the leaf certificate a TLS server presents (spec §5.7).
fn tls_peer_cert_pem(port: u16, host: &str) -> Option<String> {
    let connector = tls_connector(None);
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let tls = connector.connect(host, stream).ok()?;
    let cert = tls.ssl().peer_certificate()?;
    String::from_utf8(cert.to_pem().ok()?).ok()
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
fn https_upstream_is_proxied_with_skip_verify() {
    // An `https://` upstream (spec §5.4) with `tls_skip_verify`: raddy talks TLS
    // to the self-signed test upstream and returns its body unchanged.
    let (up_port, up) = TlsEchoUpstream::spawn();
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    reverse_proxy {{\n        to https://127.0.0.1:{up_port}\n        tls_skip_verify\n    }}\n}}\n"
        )
    });
    wait_until(
        || try_request(raddy.port(), Some("localhost"), "/").is_some(),
        "server to accept requests",
    );
    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "label=secure");
    assert!(
        up.hit_count() >= 1,
        "the TLS upstream must have served the request"
    );
}

#[test]
fn https_upstream_untrusted_cert_yields_502() {
    // The same self-signed upstream without `tls_skip_verify`: certificate
    // verification fails and raddy answers 502 rather than forwarding.
    let (up_port, _up) = TlsEchoUpstream::spawn();
    let raddy = RadRaddy::spawn(|port| {
        format!(":{port} {{\n    reverse_proxy https://127.0.0.1:{up_port}\n}}\n")
    });
    wait_until(
        || try_request(raddy.port(), Some("localhost"), "/").is_some(),
        "server to accept requests",
    );
    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 502);
    // The upstream is reached only to the TLS handshake, never to its HTTP
    // response — its body must not leak through a failed verification.
    assert_ne!(resp.body, "label=secure");
}

#[test]
fn websocket_upgrade_is_proxied() {
    // A WebSocket-style upgrade (spec §5.5): raddy forwards the `Connection:
    // upgrade` request, the upstream answers 101, and raddy tunnels bytes
    // bidirectionally.
    let (up_port, _up) = UpgradeEchoUpstream::spawn();
    let raddy =
        RadRaddy::spawn(|port| format!(":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"));
    let mut stream = TcpStream::connect(("127.0.0.1", raddy.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = "GET /chat HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    stream.write_all(req.as_bytes()).unwrap();
    // Read the 101 response head.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match stream.read(&mut byte) {
            Ok(0) => panic!("upstream closed before the 101 response"),
            Ok(_) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => panic!("read error waiting for the 101 response"),
        }
    }
    let head_str = String::from_utf8_lossy(&head).to_lowercase();
    assert!(
        head_str.starts_with("http/1.1 101"),
        "expected 101 Switching Protocols, got: {head_str}"
    );
    // The tunnel is live: bytes written by the client are echoed by the upstream.
    stream.write_all(b"ping").unwrap();
    let mut echo = [0u8; 4];
    stream.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"ping");
}

/// Write a cert + key PEM pair to temp files (spec §5.7 test helper).
fn write_pem_pair(cert_pem: &str, key_pem: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir();
    let stem = format!("raddy-site-cert-{}", std::process::id());
    let cert_path = dir.join(format!("{stem}.pem"));
    let key_path = dir.join(format!("{stem}.key"));
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();
    (cert_path, key_path)
}

#[test]
fn static_tls_site_serves_configured_certificate_and_proxies() {
    // A named site with a `tls <cert> <key>` source (spec §5.7) is served over
    // TLS with exactly that certificate, and requests proxy to the upstream.
    let (up_port, _up) = EchoUpstream::spawn("secure");
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_pem = cert.cert.pem();
    let key_pem = cert.signing_key.serialize_pem();
    let (cert_path, key_path) = write_pem_pair(&cert_pem, &key_pem);
    let raddy = RadRaddy::spawn_tls(|port| {
        format!(
            "localhost:{port} {{\n    tls {} {}\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n",
            cert_path.display(),
            key_path.display()
        )
    });
    let resp = tls_request(raddy.port(), "localhost", "/", &[]).expect("tls request");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "label=secure");
    // The served leaf is the certificate we configured.
    let served = tls_peer_cert_pem(raddy.port(), "localhost").expect("peer cert");
    assert_eq!(served.trim(), cert_pem.trim());
}

#[test]
fn tls_internal_serves_a_self_signed_certificate() {
    let (up_port, _up) = EchoUpstream::spawn("internal");
    let raddy = RadRaddy::spawn_tls(|port| {
        format!(
            "localhost:{port} {{\n    tls internal\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"
        )
    });
    let resp = tls_request(raddy.port(), "localhost", "/", &[]).expect("tls request");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "label=internal");
    assert!(
        tls_peer_cert_pem(raddy.port(), "localhost").is_some(),
        "an internal site must serve a certificate"
    );
}

#[test]
fn mtls_require_rejects_clients_without_certificate() {
    // `tls client_auth require` (spec §5.7): a client presenting no certificate
    // cannot complete the handshake.
    let (up_port, _up) = EchoUpstream::spawn("mtls");
    let ca = rcgen::generate_simple_self_signed(vec!["Test CA".to_string()]).unwrap();
    let ca_path = std::env::temp_dir().join(format!("raddy-ca-{}.pem", std::process::id()));
    std::fs::write(&ca_path, ca.cert.pem()).unwrap();
    // The listener rejects every TLS client without a certificate, so a TCP
    // accept is the only usable readiness probe.
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!(
            "localhost:{port} {{\n    tls internal\n    tls client_auth require {}\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n",
            ca_path.display()
        )
    });
    assert!(
        tls_request(raddy.port(), "localhost", "/", &[]).is_err(),
        "a client without a certificate must be rejected"
    );
}

#[test]
fn mtls_require_fails_closed_on_unreadable_ca() {
    // Deleting the CA file after startup must NOT open the mTLS gate: an
    // unreadable CA falls back to an empty trust store, so `require` still
    // rejects every client (P1).
    let (up_port, _up) = EchoUpstream::spawn("mtls");
    let ca = rcgen::generate_simple_self_signed(vec!["Test CA".to_string()]).unwrap();
    let ca_path = std::env::temp_dir().join(format!("raddy-ca-del-{}.pem", std::process::id()));
    std::fs::write(&ca_path, ca.cert.pem()).unwrap();
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!(
            "localhost:{port} {{\n    tls internal\n    tls client_auth require {}\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n",
            ca_path.display()
        )
    });
    // Remove the CA before the first handshake (the TCP readiness probe never
    // performs one), then verify the gate stays closed.
    std::fs::remove_file(&ca_path).unwrap();
    assert!(
        tls_request(raddy.port(), "localhost", "/", &[]).is_err(),
        "an unreadable CA must fail closed (reject), not open the gate"
    );
}

#[test]
fn mtls_optional_accepts_clients_without_certificate() {
    // `tls client_auth optional` requests but does not require a certificate.
    let (up_port, _up) = EchoUpstream::spawn("mtls-opt");
    let ca = rcgen::generate_simple_self_signed(vec!["Test CA".to_string()]).unwrap();
    let ca_path = std::env::temp_dir().join(format!("raddy-ca-opt-{}.pem", std::process::id()));
    std::fs::write(&ca_path, ca.cert.pem()).unwrap();
    let raddy = RadRaddy::spawn_tls(|port| {
        format!(
            "localhost:{port} {{\n    tls internal\n    tls client_auth optional {}\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n",
            ca_path.display()
        )
    });
    let resp = tls_request(raddy.port(), "localhost", "/", &[]).expect("tls request");
    assert_eq!(resp.status, 200);
}

#[test]
fn h2_negotiated_on_tls_listener() {
    // TLS listeners advertise h2 over ALPN (spec §5.6); a client offering h2
    // and http/1.1 negotiates h2.
    let (up_port, _up) = EchoUpstream::spawn("h2");
    let raddy = RadRaddy::spawn_tls(|port| {
        format!(
            "localhost:{port} {{\n    tls internal\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"
        )
    });
    assert_eq!(
        tls_negotiated_alpn(raddy.port(), "localhost").as_deref(),
        Some("h2")
    );
}

#[test]
fn method_matcher_routes_requests() {
    // A `method` matcher (spec §5.9) routes by request method within one site.
    let (a_port, _a) = EchoUpstream::spawn("A");
    let (b_port, _b) = EchoUpstream::spawn("B");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    handle method GET {{\n        reverse_proxy 127.0.0.1:{a_port}\n    }}\n    handle method POST {{\n        reverse_proxy 127.0.0.1:{b_port}\n    }}\n}}\n"
        )
    });
    let get = send_request_method("GET", raddy.port(), Some("localhost"), "/", &[]);
    assert_eq!(get.status, 200);
    assert_eq!(get.body, "label=A");
    let post = send_request_method("POST", raddy.port(), Some("localhost"), "/", &[]);
    assert_eq!(post.status, 200);
    assert_eq!(post.body, "label=B");
}

#[test]
fn host_matcher_selects_within_catch_all() {
    // A `host` matcher routes by the normalized Host header inside a catch-all.
    let (a_port, _a) = EchoUpstream::spawn("A");
    let (b_port, _b) = EchoUpstream::spawn("B");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    handle host api.example.com {{\n        reverse_proxy 127.0.0.1:{a_port}\n    }}\n    handle host www.example.com {{\n        reverse_proxy 127.0.0.1:{b_port}\n    }}\n}}\n"
        )
    });
    let api = send_request(raddy.port(), Some("api.example.com"), "/");
    assert_eq!(api.body, "label=A");
    let www = send_request(raddy.port(), Some("www.example.com"), "/");
    assert_eq!(www.body, "label=B");
}

#[test]
fn respond_terminal_answers_directly() {
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    handle path /health {{\n        respond 200 ok\n    }}\n    reverse_proxy 127.0.0.1:1\n}}\n"
        )
    });
    let resp = send_request(raddy.port(), Some("localhost"), "/health");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "ok");
    // A path outside the handle falls through to the reverse proxy (dead
    // upstream → 502), proving the handle is mutually exclusive.
    let resp = send_request(raddy.port(), Some("localhost"), "/other");
    assert_eq!(resp.status, 502);
}

#[test]
fn error_terminal_returns_status() {
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    handle path /boom {{\n        error 503\n    }}\n    reverse_proxy 127.0.0.1:1\n}}\n"
        )
    });
    let resp = send_request(raddy.port(), Some("localhost"), "/boom");
    assert_eq!(resp.status, 503);
}

#[test]
fn handle_path_strips_prefix() {
    // `handle_path /api/*` strips the prefix before the upstream sees it.
    let (up_port, _up) = PathEchoUpstream::spawn();
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    handle_path /api/* {{\n        reverse_proxy 127.0.0.1:{up_port}\n    }}\n    reverse_proxy 127.0.0.1:1\n}}\n"
        )
    });
    let resp = send_request(raddy.port(), Some("localhost"), "/api/users/1");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "path=/users/1");
}

#[test]
fn rewrite_modifier_changes_path() {
    // A `rewrite` modifier (spec §5.9) transforms the path the upstream sees.
    let (up_port, _up) = PathEchoUpstream::spawn();
    let raddy = RadRaddy::spawn(|port| {
        format!(":{port} {{\n    rewrite /v1{{uri}}\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n")
    });
    let resp = send_request(raddy.port(), Some("localhost"), "/x");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "path=/v1/x");
}

#[test]
fn basic_auth_requires_credentials() {
    // `basic_auth` (spec §5.10): no/wrong credentials → 401 with
    // WWW-Authenticate; correct credentials → proxied.
    use base64::Engine;
    let hash = bcrypt::hash("secret", 4).unwrap();
    let (up_port, _up) = EchoUpstream::spawn("private");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    basic_auth admin {hash}\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"
        )
    });
    let no_auth = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(no_auth.status, 401);
    assert!(
        no_auth.headers.contains("www-authenticate"),
        "a basic_auth 401 must carry WWW-Authenticate"
    );
    let bad = format!(
        "Authorization: Basic {}",
        base64::engine::general_purpose::STANDARD.encode("admin:wrong")
    );
    let resp = send_request_hdr(raddy.port(), Some("localhost"), "/", &[bad.as_str()]);
    assert_eq!(resp.status, 401);
    let good = format!(
        "Authorization: Basic {}",
        base64::engine::general_purpose::STANDARD.encode("admin:secret")
    );
    let resp = send_request_hdr(raddy.port(), Some("localhost"), "/", &[good.as_str()]);
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "label=private");
}

#[test]
fn forward_auth_delegates_and_rejects() {
    // `forward_auth` (spec §5.10): the auth upstream's 2xx lets the request
    // through to the real upstream; its 401 rejects it.
    let (auth_port, _auth) = AuthUpstream::spawn();
    let (real_port, _real) = EchoUpstream::spawn("real");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    forward_auth 127.0.0.1:{auth_port}\n    reverse_proxy 127.0.0.1:{real_port}\n}}\n"
        )
    });
    let ok = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(ok.status, 200);
    assert_eq!(ok.body, "label=real");
    let denied = send_request(raddy.port(), Some("localhost"), "/deny");
    assert_eq!(denied.status, 401);
}

#[test]
fn rate_limit_keys_on_header_value() {
    // `rate_limit header <name>` (spec §5.2) buckets per header value.
    let (up_port, _up) = EchoUpstream::spawn("limited");
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    rate_limit header X-API-Key 2r/s\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n"
        )
    });
    let key1 = "X-API-Key: key1";
    assert_eq!(
        send_request_hdr(raddy.port(), Some("localhost"), "/", &[key1]).status,
        200
    );
    assert_eq!(
        send_request_hdr(raddy.port(), Some("localhost"), "/", &[key1]).status,
        200
    );
    // The third request with the same key is rate limited.
    assert_eq!(
        send_request_hdr(raddy.port(), Some("localhost"), "/", &[key1]).status,
        429
    );
    // A different key has its own bucket.
    assert_eq!(
        send_request_hdr(raddy.port(), Some("localhost"), "/", &["X-API-Key: key2"]).status,
        200
    );
}

#[test]
fn env_vars_expand_in_config() {
    // `{$ENV}` (spec §5.12) is expanded from the process environment at parse
    // time, so an upstream target can come from an env var.
    let (up_port, _up) = EchoUpstream::spawn("env");
    let upstream = format!("127.0.0.1:{up_port}");
    let raddy = RadRaddy::spawn_with_env(
        |port| format!(":{port} {{\n    reverse_proxy {{$UPSTREAM}}\n}}\n"),
        &[("UPSTREAM", upstream.as_str())],
    );
    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "label=env");
}

#[test]
fn env_var_with_special_chars_is_a_single_argument() {
    // Token-level {$ENV} expansion (spec §5.12, P2): a value containing spaces
    // and `#` is one directive argument — it cannot turn into a comment or
    // split into multiple arguments.
    let raddy = RadRaddy::spawn_with_env(
        |port| format!(":{port} {{\n    respond 200 {{$BODY}}\n}}\n"),
        &[("BODY", "hello # world")],
    );
    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "hello # world");
}

#[test]
fn file_import_splices_site_directives() {
    let (up_port, _up) = EchoUpstream::spawn("imported");
    let imported = std::env::temp_dir().join(format!(
        "raddy_site_import_{}.Raddyfile",
        std::process::id()
    ));
    std::fs::write(&imported, format!("reverse_proxy 127.0.0.1:{up_port}\n")).unwrap();
    let imported_c = imported.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(":{port} {{\n    import {}\n}}\n", imported_c.display())
    });
    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "label=imported");
    let _ = std::fs::remove_file(&imported);
}

#[test]
fn access_log_common_format_is_written() {
    // The global `access_log <path> format=common` directive (spec §5.13)
    // writes classic combined log lines.
    let log_path =
        std::env::temp_dir().join(format!("raddy_access_common_{}.log", std::process::id()));
    let (up_port, _up) = EchoUpstream::spawn("logged");
    let log_c = log_path.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            "{{\n    access_log {} format=common\n}}\n:{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n",
            log_c.display()
        )
    });
    let resp = send_request(raddy.port(), Some("localhost"), "/x");
    assert_eq!(resp.status, 200);
    wait_until(
        || {
            std::fs::read_to_string(&log_path)
                .map(|s| s.contains("GET /x HTTP/1.1"))
                .unwrap_or(false)
        },
        "the common access-log line to appear",
    );
    let content = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        content.contains("GET /x HTTP/1.1"),
        "common log line missing: {content}"
    );
    assert!(content.contains(" 200 "), "status missing: {content}");
    let _ = std::fs::remove_file(&log_path);
}

#[test]
fn access_log_off_skips_a_site() {
    // A site with `access_log off` (spec §5.13) produces no access-log line even
    // when logging is enabled by the `--access-log` flag.
    let log_path =
        std::env::temp_dir().join(format!("raddy_access_off_{}.log", std::process::id()));
    let (up_port, _up) = EchoUpstream::spawn("off");
    let args = vec![String::from("--access-log"), log_path.display().to_string()];
    let raddy = RadRaddy::spawn_with_args(
        |port| {
            format!(":{port} {{\n    access_log off\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n")
        },
        &args,
    );
    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 200);
    // The logging hook runs after the response is written; give it a moment,
    // then assert no 200 line (the probe's 400s may still appear).
    thread::sleep(Duration::from_millis(300));
    let content = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !content.contains("\"status\":200"),
        "access_log off must skip the site: {content}"
    );
    let _ = std::fs::remove_file(&log_path);
}

#[test]
fn access_log_off_excludes_respond_terminal() {
    // `access_log off` also excludes non-proxy terminals (respond/error/redir/
    // file_server) from the log (P2), and their bodies are counted.
    let log_path = std::env::temp_dir().join(format!(
        "raddy_access_off_respond_{}.log",
        std::process::id()
    ));
    let args = vec![String::from("--access-log"), log_path.display().to_string()];
    let raddy = RadRaddy::spawn_with_args(
        |port| format!(":{port} {{\n    access_log off\n    respond 200 hello-world\n}}\n"),
        &args,
    );
    let resp = send_request(raddy.port(), Some("localhost"), "/");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, "hello-world");
    thread::sleep(Duration::from_millis(300));
    let content = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !content.contains("\"status\":200"),
        "an off respond site must not be logged: {content}"
    );
    let _ = std::fs::remove_file(&log_path);
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
fn multi_domain_site_block_routes_all_domains() {
    let raddy = RadRaddy::spawn(|port| {
        format!(
            "a.example.com:{port}, b.example.com:{port} {{\n    respond 200 shared-domain\n}}\n"
        )
    });
    for host in ["a.example.com", "b.example.com"] {
        let response = send_request(raddy.port(), Some(host), "/");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "shared-domain");
    }
}

#[test]
fn h2c_upstream_is_proxied() {
    let (upstream_port, _upstream) = H2EchoUpstream::spawn();
    let raddy = RadRaddy::spawn(|port| {
        format!(":{port} {{\n    reverse_proxy h2c://127.0.0.1:{upstream_port}\n}}\n")
    });
    let response = send_request(raddy.port(), Some("localhost"), "/h2c");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "h2-ok");
}

#[test]
fn h2_tls_upstream_is_proxied() {
    let (upstream_port, _upstream) = H2TlsEchoUpstream::spawn();
    let raddy = RadRaddy::spawn(|port| {
        format!(
            ":{port} {{\n    reverse_proxy {{\n        to h2://127.0.0.1:{upstream_port}\n        tls_skip_verify\n    }}\n}}\n"
        )
    });
    let response = send_request(raddy.port(), Some("localhost"), "/h2");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "h2-tls");
}

#[test]
fn http_listener_accepts_ipv6_clients() {
    let raddy = RadRaddy::spawn(|port| format!(":{port} {{\n    respond 200 ipv6-ok\n}}\n"));
    let response = send_request_ipv6(raddy.port(), Some("localhost"), "/v6");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "ipv6-ok");
}

#[test]
fn http_proxy_connects_to_an_ipv6_upstream() {
    let (upstream_port, _upstream) = Ipv6EchoUpstream::spawn("ipv6-upstream");
    let raddy = RadRaddy::spawn(|port| {
        format!(":{port} {{\n    reverse_proxy [::1]:{upstream_port}\n}}\n")
    });
    let response = send_request_ipv6(raddy.port(), Some("localhost"), "/upstream-v6");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "label=ipv6-upstream");
}

#[test]
fn wildcard_site_routes_http_and_tls_sni() {
    let raddy = RadRaddy::spawn_tls(|port| {
        format!(
            "*.example.com:{port}, localhost:{port} {{\n    tls internal\n    respond 200 wildcard-ok\n}}\n"
        )
    });
    let response = tls_request(raddy.port(), "api.example.com", "/wildcard", &[]).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "wildcard-ok");
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
    // A body comfortably above the compress-minimum (64 B): gzip must apply.
    // (A tiny body is skipped — see the tiny-body test below.)
    let content = "hello compressible ".repeat(8);
    std::fs::write(dir.join("hello.txt"), &content).unwrap();
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
    assert_eq!(plain.body, content);

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

#[test]
fn file_server_skips_compressing_tiny_bodies() {
    // A body under the 64 B compress minimum must be served uncompressed: the
    // codec framing would make it larger than the payload.
    let dir = std::env::temp_dir().join(format!("raddy_fs_tiny_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tiny.txt"), "tiny").unwrap();
    let dir_cfg = dir.clone();
    let raddy = RadRaddy::spawn(move |port| {
        format!(
            ":{port} {{\n    root {}\n    encode gzip\n    file_server\n}}\n",
            dir_cfg.display()
        )
    });
    let port = raddy.port();
    wait_until(
        || try_request(port, Some("localhost"), "/tiny.txt").is_some_and(|r| r.status == 200),
        "tiny file response",
    );
    let resp = send_request_hdr(
        port,
        Some("localhost"),
        "/tiny.txt",
        &["Accept-Encoding: gzip"],
    );
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("content-encoding"), None);
    assert_eq!(resp.body, "tiny");

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
fn reverse_proxy_streams_brotli_before_eos() {
    streaming_compression_scenario("br", "br", |body| {
        let mut decoder = brotli::Decompressor::new(body, 4096);
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
fn zero_downtime_upgrade_hands_off_udp_listener_and_flow() {
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
    let (up_port, _upstream) = UdpEchoUpstream::spawn("upgrade");
    let raddy = RadRaddy::spawn_udp_with_args(
        |port| format!("udp 127.0.0.1:{port} {{\n    to 127.0.0.1:{up_port}\n}}\n"),
        &extra,
    );
    let client = UdpSocket::bind("127.0.0.1:0").expect("bind persistent UDP client");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set persistent UDP timeout");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut before = None;
    while Instant::now() < deadline {
        before = udp_roundtrip_on(&client, raddy.port(), "before");
        if before.as_deref() == Some("upgrade:before") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(before.as_deref(), Some("upgrade:before"));
    let old_pid = read_pid_file(&pidfile);

    let mut cmd = Command::new(BIN);
    cmd.args(["upgrade", "-c"]).arg(&raddy.config_path);
    cmd.args(&extra);
    cmd.env("RUST_LOG", "error");
    let status = cmd.status().expect("failed to spawn UDP upgrade");
    assert!(status.success(), "UDP upgrade should succeed: {status:?}");

    assert_eq!(
        udp_roundtrip_on(&client, raddy.port(), "after").as_deref(),
        Some("upgrade:after"),
        "the persistent client flow must continue after upgrade"
    );
    assert!(
        !process_alive(old_pid),
        "old UDP process should have exited"
    );
    let new_pid = read_pid_file(&pidfile);
    assert_ne!(new_pid, old_pid, "UDP upgrade should replace the process");
    assert!(process_alive(new_pid), "replacement UDP process should run");
    // SAFETY: new_pid is the replacement process written by the test instance.
    unsafe {
        libc::kill(new_pid, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && process_alive(new_pid) {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_alive(new_pid),
        "replacement UDP process should stop"
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

// ---------------------------------------------------------------------------
// Layer-4 raw TCP proxy (L4_PROXY_PLAN P0)
// ---------------------------------------------------------------------------

#[test]
fn raw_tcp_proxy_relays_bidirectionally() {
    // A `tcp` listener proxies a raw byte stream to the upstream and back.
    let (echo_port, _echo) = TcpEchoUpstream::spawn();
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!("tcp 127.0.0.1:{port} {{\n    to 127.0.0.1:{echo_port}\n}}\n")
    });
    assert_eq!(tcp_echo(raddy.port(), "ping-l4"), "echo:ping-l4");
}

#[test]
fn raw_tcp_proxy_round_robins_across_upstreams() {
    let (a_port, a) = TcpEchoUpstream::spawn();
    let (b_port, b) = TcpEchoUpstream::spawn();
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!("tcp 127.0.0.1:{port} {{\n    to 127.0.0.1:{a_port} 127.0.0.1:{b_port}\n}}\n")
    });
    // Alternate connections must reach both upstreams (round-robin). The
    // readiness probe already made one connection, so a few more cover both.
    for _ in 0..4 {
        assert!(
            tcp_echo(raddy.port(), "rr") == "echo:rr",
            "unexpected round-robin echo"
        );
    }
    assert!(
        a.hit_count() >= 1 && b.hit_count() >= 1,
        "round-robin must reach both upstreams: a={}, b={}",
        a.hit_count(),
        b.hit_count()
    );
}

#[test]
fn raw_tcp_proxy_idle_timeout_closes_connection() {
    // A 1s idle timeout must close a connection with no traffic.
    let (echo_port, _echo) = TcpEchoUpstream::spawn();
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!("tcp 127.0.0.1:{port} {{\n    to 127.0.0.1:{echo_port}\n    idle_timeout 1s\n}}\n")
    });
    let mut stream = TcpStream::connect(("127.0.0.1", raddy.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // No traffic: the idle watchdog should close the connection within ~1s.
    let mut buf = [0u8; 8];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(
        n, 0,
        "an idle connection must be closed (EOF), got {n} bytes"
    );
}

#[test]
fn raw_tcp_proxy_enforces_max_connections() {
    // The second connection must be rejected while the first connection keeps
    // its admission permit by remaining open and idle.
    let (echo_port, echo) = TcpEchoUpstream::spawn();
    let l4_port = free_port();
    let metrics_port = free_port();
    let extra = [format!("--metrics-addr=127.0.0.1:{metrics_port}")];
    let _raddy = RadRaddy::spawn_with_probe(
        |http_port| {
            format!(
                ":{http_port} {{\n    respond 200 ready\n}}\n\
                 tcp 127.0.0.1:{l4_port} {{\n    to 127.0.0.1:{echo_port}\n    max_connections 1\n}}\n"
            )
        },
        &extra,
        ReadyProbe::Plain,
    );
    let first = TcpStream::connect(("127.0.0.1", l4_port)).unwrap();
    wait_until(
        || echo.hit_count() >= 1,
        "the first admitted connection to reach the upstream",
    );

    let mut second = TcpStream::connect(("127.0.0.1", l4_port)).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut buf = [0u8; 16];
    let _ = second.read(&mut buf);
    wait_until(
        || metric_value(metrics_port, "raddy_l4_tcp_rejected_total").unwrap_or(0) >= 1,
        "the TCP admission rejection metric",
    );
    drop(first);
}

#[test]
fn raw_tcp_proxy_propagates_half_close_and_drains_response() {
    // The upstream replies only after seeing EOF from the client. The proxy
    // must forward that half-close and still relay the upstream response.
    let (upstream_port, _upstream) = TcpHalfCloseUpstream::spawn();
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!("tcp 127.0.0.1:{port} {{\n    to 127.0.0.1:{upstream_port}\n}}\n")
    });
    let mut stream = TcpStream::connect(("127.0.0.1", raddy.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"before-half-close").unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"after-half-close");
}

#[test]
fn raw_tcp_proxy_closes_on_failed_upstream_connect() {
    // A reserved local port has no listener. The client connection must be
    // closed promptly, and the failed connect must be observable in metrics.
    let l4_port = free_port();
    let dead_port = free_port();
    let metrics_port = free_port();
    let extra = [format!("--metrics-addr=127.0.0.1:{metrics_port}")];
    let _raddy = RadRaddy::spawn_with_probe(
        |http_port| {
            format!(
                ":{http_port} {{\n    respond 200 ready\n}}\n\
                 tcp 127.0.0.1:{l4_port} {{\n    to 127.0.0.1:{dead_port}\n    connect_timeout 200ms\n}}\n"
            )
        },
        &extra,
        ReadyProbe::Plain,
    );
    let started = Instant::now();
    let mut stream = TcpStream::connect(("127.0.0.1", l4_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    wait_until(
        || metric_value(metrics_port, "raddy_l4_tcp_connect_failures_total").unwrap_or(0) >= 1,
        "the failed upstream-connect metric",
    );
    let mut buf = [0u8; 16];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(
        n, 0,
        "a failed upstream connect must close the client stream"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "failed upstream connect exceeded its bound"
    );
}

#[test]
fn raw_tcp_proxy_health_check_routes_around_dead_upstream() {
    // A `tcp` listener with an active health check must route new connections
    // only to healthy upstreams once the dead one is marked unhealthy.
    let (a_port, a) = TcpEchoUpstream::spawn();
    let dead_port = free_port(); // nothing listening
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!(
            "tcp 127.0.0.1:{port} {{\n    to 127.0.0.1:{a_port} 127.0.0.1:{dead_port}\n    health_check {{\n        interval 200ms\n        timeout 200ms\n        consecutive_failures 2\n        consecutive_successes 1\n    }}\n}}\n"
        )
    });
    // Give the health check time to mark the dead upstream unhealthy.
    thread::sleep(Duration::from_secs(2));
    for _ in 0..5 {
        assert_eq!(
            tcp_echo(raddy.port(), "hc"),
            "echo:hc",
            "the dead upstream must not receive traffic"
        );
    }
    assert!(
        a.hit_count() >= 1,
        "the healthy upstream must have served traffic"
    );
}

/// Send `msg` over a fresh connection to a `tcp` listener and return the echo.
fn tcp_echo(port: u16, msg: &str) -> String {
    tcp_echo_opt(port, msg).expect("tcp echo request failed")
}

/// Like [`tcp_echo`], but `None` on any failure (used for polling during a
/// reload).
fn tcp_echo_opt(port: u16, msg: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.write_all(msg.as_bytes()).ok()?;
    let mut buf = [0u8; 128];
    let expected = 5 + msg.len();
    let mut total = 0;
    while total < expected {
        let n = stream.read(&mut buf[total..]).ok()?;
        if n == 0 {
            return None;
        }
        total += n;
    }
    Some(String::from_utf8_lossy(&buf[..total]).into_owned())
}

#[test]
fn raw_tcp_proxy_reload_updates_upstream_set_for_new_connections() {
    // A SIGHUP reload that changes the upstream set must apply to *new*
    // connections (L4 plan §reload semantics): traffic flows to the new
    // upstream, while the listener itself is untouched.
    let (a_port, a) = TcpEchoUpstream::spawn();
    let (b_port, b) = TcpEchoUpstream::spawn();
    let mut raddy = RadRaddy::spawn_tcp(|port| {
        format!("tcp 127.0.0.1:{port} {{\n    to 127.0.0.1:{a_port}\n}}\n")
    });
    assert_eq!(tcp_echo(raddy.port(), "pre"), "echo:pre");

    raddy.reload(&format!(
        "tcp 127.0.0.1:{} {{\n    to 127.0.0.1:{b_port}\n}}\n",
        raddy.port()
    ));
    // Poll with a connection each round (NOT short-circuited by b's hit count,
    // which only rises once the reload applies): keep connecting until the
    // reloaded upstream serves one.
    wait_until(
        || tcp_echo_opt(raddy.port(), "post").as_deref() == Some("echo:post") && b.hit_count() >= 1,
        "the reloaded upstream to serve new connections",
    );
    assert!(
        a.hit_count() >= 1,
        "the original upstream served the pre-reload connection"
    );
}

#[test]
fn raw_tcp_proxy_reload_rejects_listener_topology_change() {
    // A reload that changes the layer-4 listener *topology* (the bound
    // address) is rejected: listeners are fixed at startup (ADR-010), so the
    // original listener keeps serving.
    let (a_port, _a) = TcpEchoUpstream::spawn();
    let mut raddy = RadRaddy::spawn_tcp(|port| {
        format!("tcp 127.0.0.1:{port} {{\n    to 127.0.0.1:{a_port}\n}}\n")
    });
    let other = free_port();
    raddy.reload(&format!(
        "tcp 127.0.0.1:{other} {{\n    to 127.0.0.1:{a_port}\n}}\n"
    ));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        tcp_echo(raddy.port(), "still"),
        "echo:still",
        "the original listener must keep serving after a rejected reload"
    );
}

/// Build a minimal TLS ClientHello carrying the given SNI (the L4 P1
/// inspector must route on it without terminating TLS).
fn build_test_client_hello(name: &str) -> Vec<u8> {
    let mut hello = Vec::new();
    hello.extend_from_slice(&[0x03, 0x03]); // client_version TLS 1.2
    hello.extend_from_slice(&[0u8; 32]); // random
    hello.push(0); // empty session_id
    hello.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // one cipher suite
    hello.push(1);
    hello.push(0); // null compression
    let name_bytes = name.as_bytes();
    let mut list = vec![0u8]; // host_name type
    list.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
    list.extend_from_slice(name_bytes);
    let mut payload = Vec::new();
    payload.extend_from_slice(&(list.len() as u16).to_be_bytes());
    payload.extend_from_slice(&list);
    let mut exts = Vec::new();
    exts.extend_from_slice(&[0x00, 0x00]); // server_name
    exts.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    exts.extend_from_slice(&payload);
    hello.extend_from_slice(&(exts.len() as u16).to_be_bytes());
    hello.extend_from_slice(&exts);
    let mut msg = vec![0x01]; // ClientHello
    let len = hello.len();
    msg.extend_from_slice(&[
        ((len >> 16) & 0xff) as u8,
        ((len >> 8) & 0xff) as u8,
        (len & 0xff) as u8,
    ]);
    msg.extend_from_slice(&hello);
    let mut rec = vec![0x16, 0x03, 0x01]; // handshake record
    rec.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    rec.extend_from_slice(&msg);
    rec
}

/// Send a ClientHello with `name` through the listener and require the relay
/// to forward it (the raw upstream echoes it back).
fn send_sni(port: u16, name: &str) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(&build_test_client_hello(name)).unwrap();
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert!(
        n > 0,
        "the relay must forward the ClientHello to the upstream for {name}"
    );
}

/// Send raw bytes through a TLS-terminated TCP listener and read the relayed
/// response. Verification is disabled because the internal test certificate is
/// intentionally self-signed.
fn tls_raw_roundtrip(port: u16, payload: &[u8]) -> Vec<u8> {
    let connector = tls_connector(None);
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect TLS TCP listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set TLS TCP read timeout");
    let mut tls = connector
        .connect("localhost", stream)
        .expect("TLS TCP handshake");
    tls.write_all(payload).expect("write TLS TCP payload");
    let mut response = vec![0u8; payload.len() + 5];
    let mut read = 0;
    while read < response.len() {
        match tls.read(&mut response[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => break,
        }
    }
    response.truncate(read);
    response
}

#[test]
fn raw_tcp_proxy_sni_routes_by_hostname() {
    // A `sni`-routing listener forwards a ClientHello to the upstream matching
    // its exact SNI (L4 P1), without terminating TLS.
    let (a_port, a) = TcpEchoUpstream::spawn();
    let (b_port, b) = TcpEchoUpstream::spawn();
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!(
            "tcp 127.0.0.1:{port} {{\n    sni a.test 127.0.0.1:{a_port}\n    sni b.test 127.0.0.1:{b_port}\n}}\n"
        )
    });
    send_sni(raddy.port(), "a.test");
    send_sni(raddy.port(), "b.test");
    assert!(a.hit_count() >= 1, "SNI a.test must route to A");
    assert!(b.hit_count() >= 1, "SNI b.test must route to B");
}

#[test]
fn raw_tcp_proxy_can_terminate_tls_before_relaying() {
    let (upstream_port, _upstream) = TcpEchoUpstream::spawn();
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!("tcp 127.0.0.1:{port} {{\n    tls internal\n    to 127.0.0.1:{upstream_port}\n}}\n")
    });
    assert_eq!(tls_raw_roundtrip(raddy.port(), b"hello"), b"echo:hello");
}

#[test]
fn raw_tcp_proxy_sni_routes_one_label_wildcards() {
    let (wildcard_port, wildcard) = TcpEchoUpstream::spawn();
    let (specific_port, specific) = TcpEchoUpstream::spawn();
    let (fallback_port, fallback) = TcpEchoUpstream::spawn();
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!(
            "tcp 127.0.0.1:{port} {{\n    sni *.example.com 127.0.0.1:{wildcard_port}\n    sni *.sub.example.com 127.0.0.1:{specific_port}\n    fallback 127.0.0.1:{fallback_port}\n}}\n"
        )
    });
    send_sni(raddy.port(), "api.example.com");
    send_sni(raddy.port(), "api.sub.example.com");
    send_sni(raddy.port(), "deep.api.example.com");
    assert!(wildcard.hit_count() >= 1, "one-label wildcard must route");
    assert!(
        specific.hit_count() >= 1,
        "more-specific wildcard must route"
    );
    assert!(
        fallback.hit_count() >= 1,
        "multi-label names must use fallback"
    );
    assert_eq!(
        wildcard.hit_count(),
        1,
        "a multi-label name must not match the less-specific wildcard"
    );
}

#[test]
fn raw_tcp_proxy_sni_fallback_serves_unmatched() {
    let (a_port, a) = TcpEchoUpstream::spawn();
    let (fb_port, fb) = TcpEchoUpstream::spawn();
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!(
            "tcp 127.0.0.1:{port} {{\n    sni a.test 127.0.0.1:{a_port}\n    fallback 127.0.0.1:{fb_port}\n}}\n"
        )
    });
    send_sni(raddy.port(), "a.test");
    send_sni(raddy.port(), "unknown.test");
    assert!(a.hit_count() >= 1, "SNI a.test must route to A");
    assert!(
        fb.hit_count() >= 1,
        "an unmatched SNI must route to the fallback"
    );
}

#[test]
fn raw_tcp_proxy_sni_without_route_or_fallback_closes() {
    let (a_port, _a) = TcpEchoUpstream::spawn();
    let raddy = RadRaddy::spawn_tcp(|port| {
        format!("tcp 127.0.0.1:{port} {{\n    sni a.test 127.0.0.1:{a_port}\n}}\n")
    });
    // An unknown SNI with no fallback is closed (EOF, no echo).
    let mut stream = TcpStream::connect(("127.0.0.1", raddy.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(&build_test_client_hello("unknown.test"))
        .unwrap();
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "an unmatched SNI without a fallback must be closed");
}

#[test]
fn raw_udp_proxy_relays_datagrams() {
    // A `udp` listener relays datagrams to the upstream and replies back.
    let (echo_port, _e) = UdpEchoUpstream::spawn("echo");
    let raddy = RadRaddy::spawn_udp(|port| {
        format!("udp 127.0.0.1:{port} {{\n    to 127.0.0.1:{echo_port}\n}}\n")
    });
    wait_until(
        || udp_roundtrip(raddy.port(), "ping").as_deref() == Some("echo:ping"),
        "the udp relay to echo a datagram",
    );
}

#[test]
fn raw_udp_proxy_supports_ipv6_listeners() {
    // IPv6 loopback is expected on the supported Linux environments; keep the
    // test portable for hosts where IPv6 is explicitly disabled.
    if UdpSocket::bind("[::1]:0").is_err() {
        return;
    }
    let (echo_port, _echo) = UdpEchoUpstream::spawn("v6");
    let raddy = RadRaddy::spawn_udp_ipv6(|port| {
        format!("udp [::1]:{port} {{\n    to 127.0.0.1:{echo_port}\n}}\n")
    });
    wait_until(
        || udp_roundtrip_ipv6(raddy.port(), "v6").as_deref() == Some("v6:v6"),
        "the IPv6 UDP listener to relay a datagram",
    );
}

#[test]
fn raw_udp_proxy_supports_ipv6_upstreams() {
    let (echo_port, _echo) = UdpEchoUpstream::spawn_ipv6("v6-upstream");
    let raddy = RadRaddy::spawn_udp(|port| {
        format!("udp 127.0.0.1:{port} {{\n    to [::1]:{echo_port}\n}}\n")
    });
    wait_until(
        || udp_roundtrip(raddy.port(), "v6-upstream").as_deref() == Some("v6-upstream:v6-upstream"),
        "UDP IPv6 upstream to reply",
    );
}

#[test]
fn raw_udp_proxy_round_robins_and_pins_by_ip_hash() {
    let (a_port, _a) = UdpEchoUpstream::spawn("A");
    let (b_port, _b) = UdpEchoUpstream::spawn("B");

    // round_robin: each new flow (fresh client socket) alternates upstreams.
    let rr = RadRaddy::spawn_udp(|port| {
        format!(
            "udp 127.0.0.1:{port} {{\n    to 127.0.0.1:{a_port} 127.0.0.1:{b_port}\n    lb_policy round_robin\n}}\n"
        )
    });
    wait_until(
        || udp_roundtrip(rr.port(), "x").is_some(),
        "round-robin udp relay",
    );
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..6 {
        let r = udp_roundtrip(rr.port(), "x").expect("rr reply");
        seen.insert(r.split(':').next().unwrap().to_string());
    }
    assert_eq!(
        seen.len(),
        2,
        "round-robin must reach both upstreams: {seen:?}"
    );

    // ip_hash: source-IP stickiness pins every 127.0.0.1 client (any source
    // port) to the same upstream.
    let ih = RadRaddy::spawn_udp(|port| {
        format!(
            "udp 127.0.0.1:{port} {{\n    to 127.0.0.1:{a_port} 127.0.0.1:{b_port}\n    lb_policy ip_hash\n}}\n"
        )
    });
    wait_until(
        || udp_roundtrip(ih.port(), "x").is_some(),
        "ip_hash udp relay",
    );
    let mut labels = std::collections::BTreeSet::new();
    for _ in 0..6 {
        let r = udp_roundtrip(ih.port(), "x").expect("ih reply");
        labels.insert(r.split(':').next().unwrap().to_string());
    }
    assert_eq!(
        labels.len(),
        1,
        "ip_hash must pin 127.0.0.1 to one upstream: {labels:?}"
    );
}

#[test]
fn raw_udp_proxy_idle_timeout_evicts_flows() {
    // A flow idle for `idle_timeout` is evicted (counted in metrics).
    let (echo_port, _e) = UdpEchoUpstream::spawn("echo");
    let metrics_port = free_port();
    let raddy = RadRaddy::spawn_udp_with_args(
        |port| {
            format!(
                "udp 127.0.0.1:{port} {{\n    to 127.0.0.1:{echo_port}\n    idle_timeout 1s\n}}\n"
            )
        },
        &[format!("--metrics-addr=127.0.0.1:{metrics_port}")],
    );
    wait_until(
        || udp_roundtrip(raddy.port(), "x").is_some(),
        "udp relay to create a flow",
    );
    // Wait for the idle eviction (1s) to be counted by the metrics endpoint.
    wait_until(
        || {
            try_request(metrics_port, None, "/metrics").is_some_and(|r| {
                r.body.lines().any(|l| {
                    l.starts_with("raddy_l4_udp_idle_evictions_total{")
                        && l.split_whitespace()
                            .last()
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(0)
                            >= 1
                })
            })
        },
        "the idle-eviction metric to count a flow",
    );
}

#[test]
fn raw_udp_proxy_drops_oversized_datagrams() {
    let (echo_port, _echo) = UdpEchoUpstream::spawn("echo");
    let metrics_port = free_port();
    let raddy = RadRaddy::spawn_udp_with_args(
        |port| {
            format!(
                "udp 127.0.0.1:{port} {{\n    to 127.0.0.1:{echo_port}\n    max_datagram_size 8\n}}\n"
            )
        },
        &[format!("--metrics-addr=127.0.0.1:{metrics_port}")],
    );
    wait_until(
        || udp_roundtrip(raddy.port(), "small").as_deref() == Some("echo:small"),
        "the UDP listener to become ready before the oversized packet",
    );
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    client
        .send_to(b"123456789", ("127.0.0.1", raddy.port()))
        .unwrap();
    let mut buf = [0u8; 64];
    assert!(
        client.recv_from(&mut buf).is_err(),
        "an oversized datagram must not reach the upstream"
    );
    wait_until(
        || metric_value(metrics_port, "raddy_l4_udp_oversized_drops_total").unwrap_or(0) >= 1,
        "the UDP oversized-datagram metric",
    );
}

#[test]
fn raw_udp_proxy_reload_updates_datagram_limit() {
    let (echo_port, _echo) = UdpEchoUpstream::spawn("echo");
    let metrics_port = free_port();
    let mut raddy = RadRaddy::spawn_udp_with_args(
        |port| {
            format!(
                "udp 127.0.0.1:{port} {{\n    to 127.0.0.1:{echo_port}\n    max_datagram_size 8\n}}\n"
            )
        },
        &[format!("--metrics-addr=127.0.0.1:{metrics_port}")],
    );
    wait_until(
        || udp_roundtrip(raddy.port(), "small").as_deref() == Some("echo:small"),
        "the initial UDP listener to become ready",
    );

    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    client
        .send_to(b"123456789", ("127.0.0.1", raddy.port()))
        .unwrap();
    let mut buf = [0u8; 64];
    assert!(client.recv_from(&mut buf).is_err());
    wait_until(
        || metric_value(metrics_port, "raddy_l4_udp_oversized_drops_total").unwrap_or(0) >= 1,
        "the initial UDP datagram limit to reject the oversized packet",
    );

    raddy.reload(&format!(
        "udp 127.0.0.1:{} {{\n    to 127.0.0.1:{echo_port}\n    max_datagram_size 16\n}}\n",
        raddy.port()
    ));
    wait_until(
        || udp_roundtrip(raddy.port(), "123456789").as_deref() == Some("echo:123456789"),
        "the reloaded UDP datagram limit to accept the packet",
    );
}

#[test]
fn raw_udp_proxy_evicts_flows_at_capacity() {
    let (echo_port, _echo) = UdpEchoUpstream::spawn("echo");
    let metrics_port = free_port();
    let raddy = RadRaddy::spawn_udp_with_args(
        |port| {
            format!("udp 127.0.0.1:{port} {{\n    to 127.0.0.1:{echo_port}\n    max_flows 1\n}}\n")
        },
        &[format!("--metrics-addr=127.0.0.1:{metrics_port}")],
    );
    wait_until(
        || udp_roundtrip(raddy.port(), "first").as_deref() == Some("echo:first"),
        "the first bounded UDP flow to receive a reply",
    );
    wait_until(
        || udp_roundtrip(raddy.port(), "second").as_deref() == Some("echo:second"),
        "the second bounded UDP flow to receive a reply",
    );
    wait_until(
        || metric_value(metrics_port, "raddy_l4_udp_capacity_evictions_total").unwrap_or(0) >= 1,
        "the UDP capacity-eviction metric",
    );
}

#[test]
fn raw_udp_proxy_reload_updates_upstream_for_new_flows() {
    let (a_port, _a) = UdpEchoUpstream::spawn("A");
    let (b_port, _b) = UdpEchoUpstream::spawn("B");
    let mut raddy = RadRaddy::spawn_udp(|port| {
        format!("udp 127.0.0.1:{port} {{\n    to 127.0.0.1:{a_port}\n}}\n")
    });
    wait_until(
        || udp_roundtrip(raddy.port(), "before").as_deref() == Some("A:before"),
        "the initial UDP upstream",
    );
    raddy.reload(&format!(
        "udp 127.0.0.1:{} {{\n    to 127.0.0.1:{b_port}\n}}\n",
        raddy.port()
    ));
    wait_until(
        || udp_roundtrip(raddy.port(), "after").as_deref() == Some("B:after"),
        "the reloaded UDP upstream to serve new flows",
    );
}

#[test]
fn raw_udp_proxy_writes_typed_flow_access_records() {
    let (echo_port, _echo) = UdpEchoUpstream::spawn("echo");
    let log_path = std::env::temp_dir().join(format!(
        "raddy_l4_udp_access_{}_{}.log",
        std::process::id(),
        free_port()
    ));
    let raddy = RadRaddy::spawn_udp_with_args(
        |port| {
            format!(
                "udp 127.0.0.1:{port} {{\n    to 127.0.0.1:{echo_port}\n    idle_timeout 1s\n}}\n"
            )
        },
        &[String::from("--access-log"), log_path.display().to_string()],
    );
    wait_until(
        || udp_roundtrip(raddy.port(), "logged").as_deref() == Some("echo:logged"),
        "the UDP flow to receive a reply before logging",
    );
    wait_until(
        || {
            std::fs::read_to_string(&log_path)
                .map(|content| {
                    content.contains("\"listener\":\"udp/")
                        && content.contains("\"outcome\":\"evicted\"")
                })
                .unwrap_or(false)
        },
        "the typed UDP flow access record",
    );
    let _ = std::fs::remove_file(&log_path);
}

#[test]
fn udp_listener_conflicts_with_overlapping_udp_bind() {
    // Two UDP listeners on the same address:port are rejected.
    let config = std::env::temp_dir().join(format!(
        "raddy_l4_udp_conflict_{}.Raddyfile",
        std::process::id()
    ));
    std::fs::write(
        &config,
        "udp :53 {\n    to 1.1.1.1:53\n}\nudp 0.0.0.0:53 {\n    to 8.8.8.8:53\n}\n",
    )
    .unwrap();
    let out = Command::new(BIN)
        .args(["check", "-c"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "check must reject overlapping UDP listeners"
    );
    let _ = std::fs::remove_file(&config);
}

#[test]
fn zero_downtime_upgrade_hands_off_raw_tcp_listeners() {
    // The zero-downtime upgrade must hand the raw-TCP listener's fd to the new
    // process: after `raddy upgrade`, new TCP connections are served by the
    // replacement (the plan's P0 acceptance criterion).
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
    let (tcp_echo_port, _echo) = TcpEchoUpstream::spawn();
    let l4_port = free_port();
    let raddy = RadRaddy::spawn_with_args(
        move |port| {
            format!(
                ":{port} {{\n    reverse_proxy 127.0.0.1:{up_port}\n}}\n\
                 tcp 127.0.0.1:{l4_port} {{\n    to 127.0.0.1:{tcp_echo_port}\n}}\n"
            )
        },
        &extra,
    );
    let http_port = raddy.port();
    let old_pid = read_pid_file(&pidfile);
    assert!(process_alive(old_pid), "initial instance should be running");

    // The old process serves both the HTTP site and the raw-TCP listener.
    let mut old_conn = TcpStream::connect(("127.0.0.1", l4_port)).unwrap();
    old_conn
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    old_conn.write_all(b"pre").unwrap();
    let mut buf = [0u8; 16];
    let n = old_conn.read(&mut buf).unwrap();
    assert_eq!(
        &buf[..n],
        b"echo:pre",
        "old process must serve the tcp listener"
    );
    assert_eq!(
        send_request(http_port, Some("localhost"), "/").status,
        200,
        "old process must serve HTTP"
    );

    // Run the upgrade: the replacement must claim the raw-TCP listener's fd.
    let status = Command::new(BIN)
        .args(["upgrade", "-c"])
        .arg(&raddy.config_path)
        .args(&extra)
        .status()
        .expect("failed to spawn raddy upgrade");
    assert!(status.success(), "raddy upgrade should succeed: {status:?}");

    // The replacement serves new raw-TCP connections on the same listener.
    let mut new_conn = TcpStream::connect(("127.0.0.1", l4_port)).unwrap();
    new_conn
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    new_conn.write_all(b"post").unwrap();
    let n = new_conn.read(&mut buf).unwrap();
    assert_eq!(
        &buf[..n],
        b"echo:post",
        "the new process must serve the handed-off raw-TCP listener"
    );
    // HTTP on the replacement works too.
    wait_until(
        || try_request(http_port, Some("localhost"), "/").is_some_and(|r| r.status == 200),
        "HTTP after the upgrade",
    );

    drop(old_conn);
    let new_pid = read_pid_file(&pidfile);
    assert_ne!(new_pid, old_pid, "upgrade should replace the process");
    assert!(process_alive(new_pid), "replacement should be running");

    // Stop the replacement (it is detached from `RadRaddy`'s Drop).
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
fn tcp_listener_conflicts_with_http_site_at_check() {
    // `raddy check` must reject a raw-TCP listener on an HTTP site's port.
    let config = std::env::temp_dir().join(format!(
        "raddy_l4_conflict_{}.Raddyfile",
        std::process::id()
    ));
    std::fs::write(
        &config,
        "tcp :8080 {\n    to 127.0.0.1:1\n}\n:8080 {\n    reverse_proxy 127.0.0.1:1\n}\n",
    )
    .unwrap();
    let out = Command::new(BIN)
        .args(["check", "-c"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "check must reject an L4/HTTP port collision"
    );
    let _ = std::fs::remove_file(&config);
}
