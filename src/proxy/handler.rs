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

//! The Pingora request plane: site selection, terminal dispatch, header
//! rewrites, and the plain-HTTP forwarding path (Q3/Q4/Q5/Q8).
//!
//! [`ProxyHandler`] is process-lifetime: it holds the [`ConfigStore`] (the
//! atomic snapshot) and the round-robin counters, and it is created once at
//! startup. Reloads swap only the snapshot inside the store, never this object,
//! so the upstream Connector's connection pools survive (ADR-011).

use crate::config::ast::{
    AccessLogFormat, Cidr, Encoding, LbPolicy, Modifier, RateLimitKey, RateSpec, SiteKey,
    TemplatePart, TerminalKind, UpstreamTls, ValueTemplate, Variable,
};
use crate::config::snapshot::ConfigStore;
use crate::proxy::compress;
use crate::proxy::lb::{LbSpec, LoadBalancerPool};
use crate::proxy::ratelimit::RateLimiter;
use crate::proxy::site;
use crate::server::acme::ChallengeStore;
use async_trait::async_trait;
use bytes::Bytes;
use pingora::prelude::*;
use pingora::proxy::Session;
use serde::Serialize;
use std::fs::File;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The access-log destination and format (spec §5.13).
pub struct AccessLog {
    pub file: Mutex<File>,
    pub format: AccessLogFormat,
}

/// The process-lifetime proxy handler.
pub struct ProxyHandler {
    store: Arc<ConfigStore>,
    /// Load-balancing pool (per-site/terminal balancers, ADR-011).
    pool: Arc<LoadBalancerPool>,
    /// HTTP-01 challenge registry, consulted before site selection so the
    /// ACME challenge is served regardless of the site routing.
    challenges: Arc<ChallengeStore>,
    /// Single-node rate limiter (M10, ADR-011: state survives reloads).
    rate_limiter: Arc<RateLimiter>,
    /// Optional access-log destination and format (spec §5.13).
    access_log: Option<AccessLog>,
}

impl ProxyHandler {
    /// Create a handler served by one [`ConfigStore`].
    pub fn new(
        store: Arc<ConfigStore>,
        challenges: Arc<ChallengeStore>,
        access_log: Option<AccessLog>,
        pool: Arc<LoadBalancerPool>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            store,
            pool,
            challenges,
            access_log,
            rate_limiter,
        }
    }

    /// Evaluate the auth guards (spec §5.10) for the effective modifier
    /// directives. Returns the rejection to answer with, or `None` when the
    /// request passes. A passing `forward_auth` stores the auth upstream's
    /// response headers in `ctx.forward_auth_headers`.
    async fn check_auth_guards(
        &self,
        session: &mut Session,
        modifiers: &[Modifier],
        ctx: &mut ProxyCtx,
    ) -> Result<Option<AuthReject>, Box<Error>> {
        // `basic_auth`: the request must match one of the configured users.
        let users: Vec<&(String, String)> = modifiers
            .iter()
            .filter_map(|m| match m {
                Modifier::BasicAuth { users } => Some(users.iter()),
                _ => None,
            })
            .flatten()
            .collect();
        if !users.is_empty() && !basic_auth_matches(session, &users) {
            return Ok(Some(AuthReject::BasicUnauthorized));
        }
        // `forward_auth`: last occurrence wins.
        if let Some(target) = modifiers.iter().rev().find_map(|m| match m {
            Modifier::ForwardAuth { target } => Some(target.as_str()),
            _ => None,
        }) {
            match forward_auth_check(session, target).await? {
                ForwardAuthOutcome::Pass(headers) => ctx.forward_auth_headers = headers,
                ForwardAuthOutcome::Reject(status) => return Ok(Some(AuthReject::Status(status))),
            }
        }
        Ok(None)
    }

    /// Whether the selected site disabled access logging with `access_log off`
    /// (spec §5.13).
    fn site_access_log_off(&self, ctx: &ProxyCtx) -> bool {
        let Some(key) = ctx.site_key.as_ref() else {
            return false;
        };
        self.store
            .load()
            .sites
            .iter()
            .any(|site| &site.key == key && site.access_log_off)
    }
}

/// One structured access-log line (M5).
#[derive(Serialize)]
struct AccessLogEntry {
    /// Epoch milliseconds of the request start.
    ts: u64,
    client: String,
    method: String,
    path: String,
    status: u16,
    duration_ms: u128,
    /// Response body bytes written to the client.
    bytes: usize,
}

/// Per-request state carried across the `ProxyHttp` hook chain.
#[derive(Default)]
pub struct ProxyCtx {
    /// The selected site key (for load-balancer state).
    site_key: Option<SiteKey>,
    /// The index of the selected terminal within its site.
    terminal_index: usize,
    /// The load-balancing spec of the selected terminal.
    lb_spec: Option<LbSpec>,
    /// Compiled TLS options for a `https://` reverse-proxy block (spec §5.4);
    /// `None` for plain-HTTP terminals.
    tls: Option<UpstreamTls>,
    /// The effective modifier directives (block-level + terminal-scoped).
    modifiers: Vec<Modifier>,
    /// The site's `encode` priorities (empty = no compression).
    encode_algos: Vec<Encoding>,
    /// Per-request streaming compression encoder, created in `response_filter`
    /// when a response is compressed (B3b1). Bounded by the codec state plus the
    /// current chunk; `None` when the response is not compressed.
    encoder: Option<compress::Encoder>,
    /// When the request started, as a monotonic clock (for access-log duration).
    start: Option<Instant>,
    /// The wall-clock request start in epoch milliseconds (the access-log `ts`;
    /// captured in `request_filter` so it is the request start, not the logging
    /// time). `None` only if the clock read failed.
    start_wall: Option<u64>,
    /// The effective client IP per the §4 trust model. Resolved once per
    /// selected site from its `trusted_proxies` (or the global list) and shared
    /// by rate limiting, `ip_hash`, and the access log. `None` for requests that
    /// fail before a site is selected (ACME/400/404) or when the peer address is
    /// unavailable — callers then fall back to the TCP peer.
    effective_client_ip: Option<IpAddr>,
    /// The request path the selected terminal serves, after any `handle_path`
    /// prefix strip and `rewrite` modifiers (spec §5.9). `None` until a
    /// reverse-proxy terminal is selected (only forwarding needs it).
    serve_path: Option<String>,
    /// Response headers from a passing `forward_auth` upstream (spec §5.10),
    /// copied onto the request before the real upstream sees it.
    forward_auth_headers: Vec<(String, String)>,
    /// Response body bytes written to the client (spec §5.13), counted in
    /// `response_body_filter` for the proxied path and by the direct-respond
    /// terminals.
    response_bytes: usize,
}

#[async_trait]
impl ProxyHttp for ProxyHandler {
    type CTX = ProxyCtx;

    fn new_ctx(&self) -> Self::CTX {
        Self::CTX::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        ctx.start = Some(Instant::now());
        // The access log's `ts` documents the request start, so capture the wall
        // clock now rather than at logging time (latency still uses the Instant).
        ctx.start_wall = Some(epoch_now_ms());
        // ACME HTTP-01 challenge: serve `/.well-known/acme-challenge/<token>`
        // from the challenge store before any site selection, so the response
        // is served regardless of the site routing.
        if let Some(token) = challenge_token(request_path(session)) {
            if let Some(key_authorization) = self.challenges.lookup(token) {
                session
                    .respond_error_with_body(200, bytes::Bytes::from(key_authorization))
                    .await?;
                return Ok(true);
            }
        }

        // The snapshot is loaded once per request, so an in-flight request keeps
        // the configuration it started with across a concurrent reload (ADR-011).
        let config = self.store.load();
        let port = listener_port(session);
        let host = host_header(session);

        match site::select(&config, port, host) {
            site::Selection::BadRequest => {
                session.respond_error(400).await?;
                Ok(true)
            }
            site::Selection::NotFound => {
                session.respond_error(404).await?;
                Ok(true)
            }
            site::Selection::Site(site) => {
                // Owned so a mutable session borrow can coexist while matching.
                let path = request_path(session).to_string();
                let is_tls = session_is_tls(session);
                // Site-scoped `trusted_proxies` override the global list (§4).
                let trusted: Vec<Cidr> = match &site.trusted_proxies {
                    Some(networks) => networks.clone(),
                    None => config.global.trusted_proxies.clone(),
                };
                // Resolve the effective client IP once per site (§4). rate_limit,
                // `ip_hash`, matchers, and the access log all consume this same
                // value; requests that fail before a site is selected
                // (ACME/400/404) fall back to the TCP peer.
                ctx.effective_client_ip = client_ip(session, &trusted);
                for (index, terminal) in site.terminals.iter().enumerate() {
                    if !matchers_match(&terminal.matchers, session, ctx.effective_client_ip, is_tls)
                    {
                        continue;
                    }
                    // Effective modifiers: block-level then terminal-scoped
                    // (ADR-012), shared by the rate-limit guard and the dispatch.
                    let mut modifiers = site.modifiers.clone();
                    modifiers.extend(terminal.modifiers.iter().cloned());
                    // Rate limiting (spec §5.2): each `rate_limit` directive has
                    // its own token bucket, keyed by `remote_ip` or a request
                    // header value; an empty bucket rejects the request with 429.
                    for (offset, modifier) in modifiers.iter().enumerate() {
                        if let Modifier::RateLimit { spec } = modifier {
                            if let Some(key) = rate_limit_key(session, ctx, spec) {
                                if !self
                                    .rate_limiter
                                    .allow(&site.key, index, offset, &key, spec)
                                {
                                    session.respond_error(429).await?;
                                    return Ok(true);
                                }
                            }
                        }
                    }
                    // Auth guards (spec §5.10): `basic_auth` and `forward_auth`
                    // gate whichever terminal serves the block.
                    if let Some(reject) = self.check_auth_guards(session, &modifiers, ctx).await? {
                        match reject {
                            AuthReject::BasicUnauthorized => {
                                let mut resp = ResponseHeader::build(401, None)?;
                                resp.insert_header(
                                    http::header::WWW_AUTHENTICATE,
                                    "Basic realm=\"restricted\"",
                                )?;
                                resp.insert_header(http::header::CONTENT_LENGTH, "0")?;
                                session.write_response_header(Box::new(resp), true).await?;
                            }
                            AuthReject::Status(status) => {
                                session.respond_error(status).await?;
                            }
                        }
                        return Ok(true);
                    }
                    // The effective request path the terminal serves: `handle_path`
                    // strips its matched prefix, then `rewrite` modifiers
                    // transform the path (spec §5.9).
                    let serve_path =
                        serving_path(terminal.strip_prefix.as_deref(), &modifiers, &path, session);
                    match &terminal.kind {
                        TerminalKind::Redir { to, code } => {
                            let location = expand_template(to, session);
                            let mut resp = ResponseHeader::build(*code, None)?;
                            resp.insert_header(http::header::LOCATION, location)?;
                            resp.insert_header(http::header::CONTENT_LENGTH, "0")?;
                            // `header_down` applies to the final response header,
                            // overriding the redirect's own Location/Content-Length
                            // exactly like the reverse-proxy path.
                            apply_header_down(&modifiers, session, &mut resp);
                            session.write_response_header(Box::new(resp), true).await?;
                            return Ok(true);
                        }
                        TerminalKind::ReverseProxy {
                            upstreams,
                            lb_policy,
                            health_check,
                            tls,
                        } => {
                            ctx.site_key = Some(site.key.clone());
                            ctx.terminal_index = index;
                            ctx.lb_spec = Some(LbSpec {
                                upstreams: upstreams.clone(),
                                policy: *lb_policy,
                                health_check: *health_check,
                            });
                            ctx.tls = tls.clone();
                            ctx.modifiers = modifiers;
                            ctx.encode_algos = encode_algos(&ctx.modifiers);
                            ctx.serve_path = Some(serve_path);
                            // Continue to upstream_peer for forwarding.
                            return Ok(false);
                        }
                        TerminalKind::FileServer { root } => {
                            let encode = encode_algos(&modifiers);
                            crate::proxy::fs::serve(
                                session,
                                root,
                                &serve_path,
                                &encode,
                                &modifiers,
                            )
                            .await?;
                            return Ok(true);
                        }
                        TerminalKind::Respond { status, body } => {
                            let mut resp = ResponseHeader::build(*status, None)?;
                            match body {
                                Some(body) => {
                                    resp.insert_header(
                                        http::header::CONTENT_LENGTH,
                                        body.len().to_string(),
                                    )?;
                                    apply_header_down(&modifiers, session, &mut resp);
                                    session.write_response_header(Box::new(resp), false).await?;
                                    session
                                        .write_response_body(Some(Bytes::from(body.clone())), true)
                                        .await?;
                                }
                                None => {
                                    resp.insert_header(http::header::CONTENT_LENGTH, "0")?;
                                    apply_header_down(&modifiers, session, &mut resp);
                                    session.write_response_header(Box::new(resp), true).await?;
                                }
                            }
                            return Ok(true);
                        }
                        TerminalKind::Error { status, message } => {
                            match message {
                                Some(message) => {
                                    let body = Bytes::from(message.clone());
                                    let mut resp = ResponseHeader::build(*status, None)?;
                                    resp.insert_header(
                                        http::header::CONTENT_LENGTH,
                                        body.len().to_string(),
                                    )?;
                                    apply_header_down(&modifiers, session, &mut resp);
                                    session.write_response_header(Box::new(resp), false).await?;
                                    session.write_response_body(Some(body), true).await?;
                                }
                                None => {
                                    session.respond_error(*status).await?;
                                }
                            }
                            return Ok(true);
                        }
                    }
                }
                // A site matched, but no terminal matched the request path → 404.
                session.respond_error(404).await?;
                Ok(true)
            }
        }
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let site_key = ctx
            .site_key
            .as_ref()
            .expect("upstream_peer requires a selected reverse-proxy terminal");
        let lb_spec = ctx
            .lb_spec
            .as_ref()
            .expect("upstream_peer requires a selected reverse-proxy terminal");
        let balancer = self
            .pool
            .balancer_for(site_key, ctx.terminal_index, lb_spec.clone());
        // `ip_hash` keys on the effective client IP for per-IP session
        // stickiness; the other policies ignore the key. The effective IP is
        // resolved once per site (spec §4) so a trusted `X-Forwarded-For` client
        // is honored here exactly as in rate limiting and the access log.
        let key = match lb_spec.policy {
            LbPolicy::IpHash => ctx
                .effective_client_ip
                .or_else(|| {
                    session
                        .client_addr()
                        .and_then(|addr| addr.as_inet())
                        .map(|addr| addr.ip())
                })
                .map(|ip| ip.to_string().into_bytes())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let addr = balancer
            .select(&key)
            .ok_or_else(|| Error::explain(HTTPStatus(502), "no healthy upstreams available"))?;
        // Find the selected upstream to learn its scheme and original host (the
        // default SNI); the balancer only ever selects configured upstreams.
        let upstream = lb_spec.upstreams.iter().find(|p| p.addr == addr);
        let is_tls = upstream.is_some_and(|p| p.tls);
        let default_sni = upstream.map_or("", |p| p.host.as_str());
        let peer = if is_tls {
            // A `https://` upstream (spec §5.4): TLS with the compiled options.
            // `UpstreamTls` is always present when any upstream is TLS (the
            // validator guarantees it), so fall back to plain HTTP defensively.
            match ctx.tls.as_ref() {
                Some(tls) => build_tls_peer(addr, default_sni, tls),
                None => HttpPeer::new(addr, false, String::new()),
            }
        } else {
            HttpPeer::new(addr, false, String::new())
        };
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // Path rewrite (spec §5.9): the request may have been transformed by a
        // `handle_path` prefix strip or a `rewrite` modifier; rebuild the
        // upstream URI so the backend sees the effective path.
        if let Some(serve_path) = &ctx.serve_path {
            if serve_path != session.req_header().uri.path() {
                let path_and_query = match session.req_header().uri.query() {
                    Some(query) if !query.is_empty() && !serve_path.contains('?') => {
                        format!("{serve_path}?{query}")
                    }
                    _ => serve_path.clone(),
                };
                if let Ok(uri) = http::Uri::from_maybe_shared(path_and_query) {
                    upstream_request.uri = uri;
                }
            }
        }
        // Headers from a passing `forward_auth` upstream (spec §5.10), copied
        // onto the request so the real upstream sees the identity headers.
        for (name, value) in &ctx.forward_auth_headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(name.as_bytes()),
                http::HeaderValue::from_str(value),
            ) {
                let _ = upstream_request.insert_header(name, value);
            }
        }
        let ops = header_ops(&ctx.modifiers, true, session);
        apply_header_ops(&ops, |name, value| {
            let _ = upstream_request.insert_header(name, value);
        });
        Ok(())
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // `header_down` is a declarative transform on the final response header,
        // shared with the `redir` and `file_server` terminals.
        apply_header_down(&ctx.modifiers, session, upstream_response);

        // Compression (M5, B3b1): if the site enables `encode` and the client
        // accepts an algorithm, mark the response and stream it through a
        // per-request encoder in `response_body_filter`. Never double-compress
        // an already-encoded response, and skip requests/responses that carry no
        // body (HEAD, 1xx, 204, 304, and partial 206 responses).
        let compressible = session.req_header().method != http::Method::HEAD
            && !upstream_response
                .headers
                .contains_key(http::header::CONTENT_ENCODING)
            && !matches!(upstream_response.status.as_u16(), 204 | 304 | 206 | 1..=199);
        if compressible && !ctx.encode_algos.is_empty() {
            // The representation depends on the request's Accept-Encoding, so a
            // shared cache must vary on it (RFC 9110 §12.5.3) — even when this
            // particular client did not ask for compression.
            merge_vary_accept_encoding(upstream_response);
            let accept = session
                .req_header()
                .headers
                .get(http::header::ACCEPT_ENCODING);
            if let Some(algo) = compress::choose(&ctx.encode_algos, accept) {
                match compress::Encoder::new(algo) {
                    Ok(encoder) => {
                        ctx.encoder = Some(encoder);
                        upstream_response
                            .insert_header(http::header::CONTENT_ENCODING, algo.token())?;
                        upstream_response.remove_header(&http::header::CONTENT_LENGTH);
                    }
                    Err(e) => {
                        // The header is not yet committed, so skipping
                        // compression is safe — an encoder init failure is
                        // essentially only a zstd context allocation failure.
                        tracing::warn!("failed to initialize {algo:?} encoder: {e}");
                    }
                }
            }
        }
        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        let Some(encoder) = ctx.encoder.as_mut() else {
            // Uncompressed chunk: count the bytes written downstream.
            if let Some(chunk) = body {
                ctx.response_bytes += chunk.len();
            }
            return Ok(None);
        };
        let chunk = body.take().unwrap_or_default();
        // Feed the chunk and flush so the compressed bytes reach the client
        // before the response ends; at EOS, finalize the single member/frame.
        // A codec error here must propagate — never fall back to raw bytes —
        // because `Content-Encoding` was already committed to the client.
        let out = if chunk.is_empty() && !end_of_stream {
            Vec::new()
        } else {
            let mut out = encoder.write(&chunk).map_err(compression_error)?;
            if end_of_stream {
                out.extend(encoder.finish().map_err(compression_error)?);
            }
            out
        };
        if !out.is_empty() {
            *body = Some(Bytes::from(out));
        }
        if let Some(chunk) = body {
            ctx.response_bytes += chunk.len();
        }
        Ok(None)
    }

    async fn logging(&self, session: &mut Session, _e: Option<&Error>, ctx: &mut Self::CTX) {
        // Prometheus metrics (QPS + latency) for every completed request.
        if let Some(start) = ctx.start {
            crate::observ::metrics::record_request(start.elapsed().as_secs_f64());
        }
        // Access log (spec §5.13), if configured.
        let Some(log) = &self.access_log else {
            return;
        };
        // A site that set `access_log off` is excluded.
        if self.site_access_log_off(ctx) {
            return;
        }
        if let Some(entry) = access_log_entry(session, ctx) {
            if let Ok(mut guard) = log.file.lock() {
                match log.format {
                    AccessLogFormat::Json => {
                        let _ = serde_json::to_writer(&mut *guard, &entry);
                        let _ = writeln!(guard);
                    }
                    AccessLogFormat::Common => {
                        let _ = writeln!(guard, "{}", common_log_line(&entry, session));
                    }
                }
                let _ = guard.flush();
            }
        }
    }
}

/// Build a TLS `HttpPeer` to a `https://` upstream (spec §5.4): SNI from the
/// compiled options (falling back to the upstream host), certificate
/// verification unless `tls_skip_verify`, plus any custom CA and client cert.
fn build_tls_peer(addr: SocketAddr, default_sni: &str, tls: &UpstreamTls) -> HttpPeer {
    let sni = if tls.servername.is_empty() {
        default_sni.to_string()
    } else {
        tls.servername.clone()
    };
    let mut peer = HttpPeer::new(addr, true, sni);
    peer.options.verify_cert = tls.verify_cert;
    peer.options.verify_hostname = tls.verify_cert;
    peer.options.ca = tls.ca.clone();
    peer.client_cert_key = tls.client_cert.clone();
    peer
}

/// The rejection an auth guard produces (spec §5.10).
enum AuthReject {
    /// 401 with `WWW-Authenticate: Basic` (a failed `basic_auth`).
    BasicUnauthorized,
    /// A status to answer with (a failed `forward_auth`).
    Status(u16),
}

/// The outcome of a `forward_auth` delegation (spec §5.10).
enum ForwardAuthOutcome {
    /// The auth upstream granted access; its response headers are copied back.
    Pass(Vec<(String, String)>),
    /// The auth upstream refused with this status.
    Reject(u16),
}

/// Whether the request's Basic credentials match any configured user.
fn basic_auth_matches(session: &Session, users: &[&(String, String)]) -> bool {
    use base64::Engine;
    let Some(auth) = session
        .req_header()
        .headers
        .get(http::header::AUTHORIZATION)
    else {
        return false;
    };
    let Ok(auth) = auth.to_str() else {
        return false;
    };
    let Some(encoded) = auth.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((user, password)) = decoded.split_once(':') else {
        return false;
    };
    users.iter().any(|(configured, hash)| {
        configured == user && bcrypt::verify(password, hash).unwrap_or(false)
    })
}

/// Delegate authentication to the `host:port` target (spec §5.10): forward a
/// GET carrying the original Authorization and X-Forwarded-For headers; a 2xx
/// passes (copying response headers), a 403 is passed through, anything else
/// rejects with 401.
async fn forward_auth_check(
    session: &Session,
    target: &str,
) -> Result<ForwardAuthOutcome, Box<Error>> {
    let (host, port) = target
        .rsplit_once(':')
        .ok_or_else(|| Error::explain(InternalError, "invalid forward_auth target"))?;
    let port: u16 = port
        .parse()
        .map_err(|_| Error::explain(InternalError, "invalid forward_auth port"))?;
    let mut stream = TcpStream::connect((host, port))
        .await
        .map_err(|e| Error::because(InternalError, "forward_auth connect failed", e))?;
    let auth = session
        .req_header()
        .headers
        .get(http::header::AUTHORIZATION)
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    let xff = session
        .req_header()
        .headers
        .get("x-forwarded-for")
        .map(|v| v.to_str().unwrap_or("").to_string())
        .unwrap_or_default();
    let path = session
        .req_header()
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: {auth}\r\nX-Forwarded-For: {xff}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| Error::because(InternalError, "forward_auth write failed", e))?;
    // Read the response head through \r\n\r\n (bounded).
    let mut head = Vec::new();
    let mut buf = [0u8; 512];
    while head.len() < 16 * 1024 {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| Error::because(InternalError, "forward_auth read failed", e))?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head_str = String::from_utf8_lossy(&head);
    let status = head_str
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    if !(200..300).contains(&status) {
        let reject = if status == 403 { 403 } else { 401 };
        return Ok(ForwardAuthOutcome::Reject(reject));
    }
    // Parse response headers (skipping the status line) for copying back.
    let mut headers = Vec::new();
    for line in head_str.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            if !name.is_empty() {
                headers.push((name, value));
            }
        }
    }
    Ok(ForwardAuthOutcome::Pass(headers))
}

/// Build a structured access-log entry from the session and context.
fn access_log_entry(session: &Session, ctx: &ProxyCtx) -> Option<AccessLogEntry> {
    let status = session
        .response_written()
        .map(|resp| resp.status.as_u16())
        .unwrap_or(0);
    let duration_ms = ctx
        .start
        .map(|start| start.elapsed().as_millis())
        .unwrap_or(0);
    // The effective client IP when a site resolved one (spec §4); requests that
    // failed before site selection (ACME/400/404) fall back to the TCP peer.
    let client = ctx
        .effective_client_ip
        .map(|ip| ip.to_string())
        .or_else(|| {
            session
                .client_addr()
                .and_then(|addr| addr.as_inet())
                .map(|addr| addr.ip().to_string())
        })
        .unwrap_or_default();
    let method = session.req_header().method.to_string();
    let uri = &session.req_header().uri;
    let path = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());
    let ts = ctx.start_wall.unwrap_or_else(epoch_now_ms);
    Some(AccessLogEntry {
        ts,
        client,
        method,
        path,
        status,
        duration_ms,
        bytes: ctx.response_bytes,
    })
}

/// The classic combined log line for the `common` format (spec §5.13):
/// `%h %l %u %t "%r" %>s %b "%{Referer}i" "%{User-Agent}i"`.
fn common_log_line(entry: &AccessLogEntry, session: &Session) -> String {
    let referer = header_or_dash(session, http::header::REFERER);
    let user_agent = header_or_dash(session, http::header::USER_AGENT);
    let ts = common_log_time(entry.ts);
    format!(
        "{} - - [{}] \"{} {} HTTP/1.1\" {} {} \"{}\" \"{}\"",
        entry.client, ts, entry.method, entry.path, entry.status, entry.bytes, referer, user_agent
    )
}

/// A request header value or `-` for the common log format.
fn header_or_dash(session: &Session, name: http::header::HeaderName) -> String {
    session
        .req_header()
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string()
}

/// Format an epoch-millis timestamp as `[10/Aug/2026:06:00:00 +0000]`.
fn common_log_time(epoch_ms: u64) -> String {
    // A tiny hand-rolled formatter keeps the combined log line dependency-free.
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = epoch_ms / 1000;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Days since epoch → (year, month, day) via the civil-from-days algorithm.
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{:02}/{}/{:04}:{:02}:{:02}:{:02} +0000",
        day,
        MONTHS[month as usize - 1],
        year,
        hour,
        min,
        sec
    )
}

/// Convert days since the Unix epoch to a (year, month, day) date
/// (Howard Hinnant's civil-from-days algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// The current wall clock in epoch milliseconds (the access-log `ts`).
fn epoch_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The port of the local listener a request arrived on (per-listener selection).
fn listener_port(session: &Session) -> u16 {
    session
        .digest()
        .and_then(|digest| digest.socket_digest.as_ref())
        .and_then(|socket| socket.local_addr())
        .and_then(|addr| addr.as_inet())
        .map(|addr| addr.port())
        .unwrap_or(0)
}

/// The raw Host header value, if present.
fn host_header(session: &Session) -> Option<&[u8]> {
    session
        .req_header()
        .headers
        .get(http::header::HOST)
        .map(|value| value.as_bytes())
}

/// The request path (matcher input).
fn request_path(session: &Session) -> &str {
    session.req_header().uri.path()
}

/// The ACME HTTP-01 token if the path is `/.well-known/acme-challenge/<token>`.
fn challenge_token(path: &str) -> Option<&str> {
    const PREFIX: &str = "/.well-known/acme-challenge/";
    path.strip_prefix(PREFIX).filter(|token| !token.is_empty())
}

/// The real client IP per the §4 trust model, or `None` when the peer address
/// is unavailable (callers then fall back to the TCP peer).
fn client_ip(session: &Session, trusted: &[Cidr]) -> Option<IpAddr> {
    let peer = session.client_addr()?.as_inet()?.ip();
    let xff = session
        .req_header()
        .headers
        .get("x-forwarded-for")
        .map(|value| value.to_str().unwrap_or_default());
    Some(resolve_client_ip(peer, xff, trusted))
}

/// Resolve the real client IP: the TCP peer, unless the peer is a trusted
/// proxy — then the rightmost `X-Forwarded-For` entry that is not a trusted
/// proxy (malformed entries are skipped). When the whole chain is trusted or
/// absent, the trusted peer itself is the client (spec §4).
fn resolve_client_ip(peer: IpAddr, xff: Option<&str>, trusted: &[Cidr]) -> IpAddr {
    if trusted.iter().all(|cidr| !cidr.contains(peer)) {
        return peer;
    }
    let xff = xff.unwrap_or_default();
    for entry in xff.split(',').rev() {
        let Ok(addr) = entry.trim().parse::<IpAddr>() else {
            continue;
        };
        if trusted.iter().all(|cidr| !cidr.contains(addr)) {
            return addr;
        }
    }
    peer
}

/// All matchers must match for a terminal to serve (spec §5.9, ADR-012).
fn matchers_match(
    matchers: &[crate::config::ast::Matcher],
    session: &Session,
    ip: Option<IpAddr>,
    is_tls: bool,
) -> bool {
    matchers
        .iter()
        .all(|matcher| matcher_matches(matcher, session, ip, is_tls))
}

/// Evaluate a single matcher term against the request (spec §5.9).
fn matcher_matches(
    matcher: &crate::config::ast::Matcher,
    session: &Session,
    ip: Option<IpAddr>,
    is_tls: bool,
) -> bool {
    use crate::config::ast::Matcher;
    match matcher {
        Matcher::Path(prefix) => path_matches(prefix, request_path(session)),
        Matcher::Host(host) => normalized_host(session) == host.as_str(),
        Matcher::Method(method) => session.req_header().method.as_str() == method.as_str(),
        Matcher::Header { name, value } => session
            .req_header()
            .headers
            .get(name.as_str())
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == value.as_str()),
        Matcher::Query { key, value } => {
            query_value(session, key).is_some_and(|v| v == value.as_str())
        }
        Matcher::RemoteIp(cidr) => ip.is_some_and(|addr| cidr.contains(addr)),
        Matcher::Protocol(protocol) => match protocol {
            crate::config::ast::Protocol::Http => !is_tls,
            crate::config::ast::Protocol::Https => is_tls,
        },
        Matcher::Not(inner) => !matcher_matches(inner, session, ip, is_tls),
    }
}

/// The normalized Host header (port stripped, trailing dot stripped,
/// ASCII-lowercased) — the same normalization site selection applies.
fn normalized_host(session: &Session) -> String {
    let host = host_header(session).unwrap_or_default();
    let host = match host.iter().position(|&b| b == b':') {
        Some(i) => &host[..i],
        None => host,
    };
    let host = if host.last() == Some(&b'.') {
        &host[..host.len() - 1]
    } else {
        host
    };
    String::from_utf8_lossy(host).to_ascii_lowercase()
}

/// The value of a query parameter (raw, not percent-decoded).
fn query_value(session: &Session, key: &str) -> Option<String> {
    let query = session.req_header().uri.query()?;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(v.to_string());
        }
    }
    None
}

/// The counter identity for a `rate_limit` spec (spec §5.2): the effective
/// client IP for `remote_ip`, or the request header value for `header <name>`
/// (requests without the header share one bucket).
fn rate_limit_key(session: &Session, ctx: &ProxyCtx, spec: &RateSpec) -> Option<String> {
    match &spec.key {
        RateLimitKey::RemoteIp => ctx
            .effective_client_ip
            .map(|ip| ip.to_string())
            .or_else(|| {
                session
                    .client_addr()
                    .and_then(|addr| addr.as_inet())
                    .map(|addr| addr.ip().to_string())
            }),
        RateLimitKey::Header(name) => Some(
            session
                .req_header()
                .headers
                .get(name.as_str())
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string(),
        ),
    }
}

/// Whether the request arrived over TLS (the `protocol` matcher input).
fn session_is_tls(session: &Session) -> bool {
    session
        .digest()
        .is_some_and(|digest| digest.ssl_digest.is_some())
}

/// The effective request path a terminal serves: `handle_path` strips its
/// matched prefix, then `rewrite` modifiers replace the path (spec §5.9).
fn serving_path(
    strip_prefix: Option<&str>,
    modifiers: &[Modifier],
    path: &str,
    session: &Session,
) -> String {
    let mut out = path.to_string();
    if let Some(prefix) = strip_prefix {
        if let Some(stripped) = strip_path_prefix(prefix, &out) {
            out = stripped;
        }
    }
    for modifier in modifiers {
        if let Modifier::Rewrite { to } = modifier {
            out = expand_template(to, session);
        }
    }
    out
}

/// Strip a matched path prefix (`/api` → `/` for `/api`; `/api/users` →
/// `/users`); returns `None` when `path` is not under `prefix`.
fn strip_path_prefix(prefix: &str, path: &str) -> Option<String> {
    if prefix == "/" || path == prefix {
        return Some("/".to_string());
    }
    path.strip_prefix(prefix)
        .filter(|rest| rest.starts_with('/'))
        .map(|rest| rest.to_string())
}

/// A prefix match: the request path equals the prefix or falls under it
/// (`/api` matches `/api` and `/api/...`, not `/apix`). The root prefix `/`
/// matches every path.
fn path_matches(prefix: &str, path: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Expand a value template's placeholders from the current request.
fn expand_template(template: &ValueTemplate, session: &Session) -> String {
    let mut out = String::new();
    for part in template.parts() {
        match part {
            TemplatePart::Literal(s) => out.push_str(s),
            TemplatePart::Var(v) => match v {
                Variable::Host => out.push_str(&host_value(session)),
                Variable::Uri => out.push_str(&uri_value(session)),
                Variable::RemoteHost => out.push_str(&remote_host_value(session)),
            },
        }
    }
    out
}

/// `{host}`: the Host header without its port, or empty.
fn host_value(session: &Session) -> String {
    let host = host_header(session).unwrap_or_default();
    let host = match host.split(|&b| b == b':').next() {
        Some(host) => host,
        None => host,
    };
    String::from_utf8_lossy(host).into_owned()
}

/// `{uri}`: the request path including query.
fn uri_value(session: &Session) -> String {
    let uri = &session.req_header().uri;
    match uri.path_and_query() {
        Some(path_and_query) => path_and_query.as_str().to_string(),
        None => uri.path().to_string(),
    }
}

/// `{remote_host}`: the client socket address.
fn remote_host_value(session: &Session) -> String {
    session
        .client_addr()
        .and_then(|addr| addr.as_inet())
        .map(|addr| addr.ip().to_string())
        .unwrap_or_default()
}

/// Compute the header rewrites to apply from `header_up` (request) or
/// `header_down` (response) modifiers, in write order, with placeholders
/// expanded. Callers apply them to the appropriate header map.
fn header_ops(modifiers: &[Modifier], up: bool, session: &Session) -> Vec<(String, String)> {
    let mut ops = Vec::new();
    for modifier in modifiers {
        let (name, value) = match (up, modifier) {
            (true, Modifier::HeaderUp { name, value }) => (name.as_str(), value),
            (false, Modifier::HeaderDown { name, value }) => (name.as_str(), value),
            _ => continue,
        };
        ops.push((name.to_string(), expand_template(value, session)));
    }
    ops
}

/// Apply a set of pre-expanded header rewrites via a caller-supplied inserter.
///
/// Each name is validated as an HTTP token and each value as an HTTP header
/// value; invalid ones are skipped with a warning, never a panic. `insert`
/// semantics replace any existing header of the same name, so a rewrite always
/// wins over the value the terminal or upstream set — this is the single
/// validation/expansion implementation shared by `header_up` (request) and
/// `header_down` (reverse-proxy response, `redir`, and `file_server`). The
/// inserter receives a pre-validated pair for pingora's own `insert_header`,
/// which keeps its header-name case map in sync.
fn apply_header_ops(
    ops: &[(String, String)],
    mut insert: impl FnMut(http::HeaderName, http::HeaderValue),
) {
    for (name, value) in ops {
        let Ok(header_name) = http::HeaderName::from_bytes(name.as_bytes()) else {
            tracing::warn!("skipping invalid header name '{name}'");
            continue;
        };
        match http::HeaderValue::from_str(value) {
            Ok(header_value) => insert(header_name, header_value),
            Err(e) => tracing::warn!("failed to set header '{name}': {e}"),
        }
    }
}

/// Apply the `header_down` modifiers to a response header, with placeholders
/// expanded against `session`. Shared by the reverse-proxy `response_filter`,
/// the `redir` terminal, and the `file_server` terminal.
pub(super) fn apply_header_down(
    modifiers: &[Modifier],
    session: &Session,
    resp: &mut ResponseHeader,
) {
    let ops = header_ops(modifiers, false, session);
    apply_header_ops(&ops, |name, value| {
        // The name and value are pre-validated by apply_header_ops, so pingora's
        // insert_header (which keeps its case map in sync) cannot fail here.
        let _ = resp.insert_header(name, value);
    });
}

/// Collect the `encode` priorities from the effective modifier directives.
fn encode_algos(modifiers: &[Modifier]) -> Vec<Encoding> {
    modifiers
        .iter()
        .filter_map(|m| match m {
            Modifier::Encode { algorithms } => Some(algorithms.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Convert a codec error into a pingora internal error. Only called after
/// `Content-Encoding` was committed to the client, so the returned error aborts
/// the response rather than emitting raw bytes under the encoding header.
fn compression_error(e: io::Error) -> Box<Error> {
    Error::because(InternalError, "response compression failed", e)
}

/// Add `Accept-Encoding` to the response's `Vary` header (RFC 9110 §12.5.3),
/// merging case-insensitively with any existing tokens and avoiding duplicates.
/// An existing `accept-encoding` token leaves the header untouched. Shared by
/// the reverse-proxy `response_filter` and the `file_server` terminal, whose
/// compressed responses also vary on `Accept-Encoding`.
pub(super) fn merge_vary_accept_encoding(resp: &mut ResponseHeader) {
    let mut tokens: Vec<String> = Vec::new();
    let mut has_accept_encoding = false;
    for value in resp.headers.get_all(http::header::VARY) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for token in value.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if token.eq_ignore_ascii_case("accept-encoding") {
                has_accept_encoding = true;
                continue;
            }
            if !tokens.iter().any(|t| t.eq_ignore_ascii_case(token)) {
                tokens.push(token.to_string());
            }
        }
    }
    if has_accept_encoding {
        return;
    }
    tokens.push("Accept-Encoding".into());
    let merged = tokens.join(", ");
    resp.remove_header(&http::header::VARY);
    if let Ok(value) = http::HeaderValue::from_str(&merged) {
        let _ = resp.insert_header(http::header::VARY, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_matcher_boundaries() {
        assert!(path_matches("/api", "/api"));
        assert!(path_matches("/api", "/api/v1/users"));
        assert!(!path_matches("/api", "/apix"));
        assert!(!path_matches("/api", "/"));
        assert!(path_matches("/", "/anything"));
    }

    #[test]
    fn template_parts_are_ordered() {
        let template = ValueTemplate::new(vec![
            TemplatePart::Literal("https://".into()),
            TemplatePart::Var(Variable::Host),
            TemplatePart::Literal("/x".into()),
        ]);
        assert_eq!(template.parts().len(), 3);
        assert!(matches!(template.parts()[0], TemplatePart::Literal(_)));
        assert!(matches!(
            template.parts()[1],
            TemplatePart::Var(Variable::Host)
        ));
    }

    #[test]
    fn challenge_token_extraction() {
        assert_eq!(
            challenge_token("/.well-known/acme-challenge/tok123"),
            Some("tok123")
        );
        assert_eq!(
            challenge_token("/.well-known/acme-challenge/"),
            None,
            "an empty token must not match"
        );
        assert_eq!(challenge_token("/.well-known/acme-challenge"), None);
        assert_eq!(challenge_token("/"), None);
        assert_eq!(challenge_token("/static/x"), None);
    }

    fn ipv4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(a, b, c, d))
    }

    fn trusted(nets: &[&str]) -> Vec<Cidr> {
        nets.iter().map(|n| Cidr::parse(n).unwrap()).collect()
    }

    #[test]
    fn client_ip_untrusted_peer_ignores_xff() {
        // Peer not in trusted_proxies → the X-Forwarded-For header is ignored
        // entirely (the v0.1 default).
        let peer = ipv4(203, 0, 113, 9);
        assert_eq!(
            resolve_client_ip(peer, Some("1.2.3.4"), &trusted(&["10.0.0.0/8"])),
            peer
        );
    }

    #[test]
    fn client_ip_trusted_peer_uses_rightmost_untrusted() {
        let peer = ipv4(10, 0, 0, 1);
        let t = trusted(&["10.0.0.0/8"]);
        // Chain: client -> proxy1(trusted) -> raddy. The rightmost untrusted is
        // the client; proxy1 is trusted and skipped.
        assert_eq!(
            resolve_client_ip(peer, Some("203.0.113.9, 10.0.0.2"), &t),
            ipv4(203, 0, 113, 9)
        );
        // If the last proxy is NOT trusted, the chain cannot be believed.
        assert_eq!(
            resolve_client_ip(peer, Some("203.0.113.9, 198.51.100.7"), &t),
            ipv4(198, 51, 100, 7)
        );
    }

    #[test]
    fn client_ip_all_trusted_or_malformed_chain_falls_back_to_peer() {
        let peer = ipv4(10, 0, 0, 1);
        let t = trusted(&["10.0.0.0/8"]);
        // Every entry is a trusted proxy → the trusted peer is the client.
        assert_eq!(
            resolve_client_ip(peer, Some("10.0.0.2, 10.0.0.3"), &t),
            peer
        );
        // Malformed entries are skipped.
        assert_eq!(
            resolve_client_ip(peer, Some("not-an-ip, 203.0.113.9"), &t),
            ipv4(203, 0, 113, 9)
        );
        // No X-Forwarded-For at all.
        assert_eq!(resolve_client_ip(peer, None, &t), peer);
    }

    #[test]
    fn client_ip_empty_trusted_uses_peer() {
        let peer = ipv4(203, 0, 113, 9);
        assert_eq!(
            resolve_client_ip(peer, Some("1.2.3.4"), &trusted(&[])),
            peer
        );
    }

    #[test]
    fn client_ip_handles_ipv6() {
        use std::net::Ipv6Addr;
        let peer = IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1)); // 2001:db8::1
        let t = trusted(&["2001:db8::/32"]);
        // An untrusted IPv6 (outside the /32) is the real client.
        assert_eq!(
            resolve_client_ip(peer, Some("2001:db9::1"), &t),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db9, 0, 0, 0, 0, 0, 1))
        );
        // An all-trusted chain falls back to the trusted peer.
        assert_eq!(resolve_client_ip(peer, Some("2001:db8::2"), &t), peer);
        assert_eq!(resolve_client_ip(peer, None, &t), peer);
    }

    #[test]
    fn apply_header_ops_skips_invalid_names_and_values() {
        let mut resp = ResponseHeader::build(200, None).unwrap();
        let ops = vec![
            ("X-Good".to_string(), "ok".to_string()),
            // A header name that is not an HTTP token is skipped.
            ("Bad Header".to_string(), "x".to_string()),
            // A header value with a control byte cannot be inserted.
            ("X-Nul".to_string(), "bad\u{0}value".to_string()),
        ];
        apply_header_ops(&ops, |name, value| {
            let _ = resp.insert_header(name, value);
        });
        assert_eq!(
            resp.headers.get("x-good").and_then(|v| v.to_str().ok()),
            Some("ok")
        );
        assert_eq!(resp.headers.get("bad header"), None);
        assert_eq!(
            resp.headers.get("x-nul"),
            None,
            "invalid value must not panic or insert"
        );
    }

    #[test]
    fn apply_header_ops_overwrites_existing_values() {
        let mut resp = ResponseHeader::build(200, None).unwrap();
        resp.insert_header(http::header::LOCATION, "/old").unwrap();
        let ops = vec![("Location".to_string(), "/new".to_string())];
        apply_header_ops(&ops, |name, value| {
            let _ = resp.insert_header(name, value);
        });
        assert_eq!(
            resp.headers.get("location").and_then(|v| v.to_str().ok()),
            Some("/new"),
            "a header_down rewrite must overwrite the existing header"
        );
    }

    #[test]
    fn vary_merge_adds_accept_encoding() {
        let mut resp = ResponseHeader::build(200, None).unwrap();
        merge_vary_accept_encoding(&mut resp);
        assert_eq!(
            resp.headers
                .get(http::header::VARY)
                .and_then(|v| v.to_str().ok()),
            Some("Accept-Encoding")
        );
    }

    #[test]
    fn vary_merge_preserves_tokens_case_insensitively_and_dedups() {
        let mut resp = ResponseHeader::build(200, None).unwrap();
        // Two existing Vary lines with a duplicate token across them (one
        // lowercased) — merging must deduplicate and append a canonical
        // Accept-Encoding.
        resp.insert_header(http::header::VARY, "Origin, Cookie")
            .unwrap();
        resp.append_header(http::header::VARY, "origin").unwrap();
        merge_vary_accept_encoding(&mut resp);
        let values: Vec<String> = resp
            .headers
            .get_all(http::header::VARY)
            .into_iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(values, vec!["Origin, Cookie, Accept-Encoding".to_string()]);
    }

    #[test]
    fn vary_merge_keeps_existing_accept_encoding_untouched() {
        let mut resp = ResponseHeader::build(200, None).unwrap();
        resp.insert_header(http::header::VARY, "accept-encoding, Origin")
            .unwrap();
        merge_vary_accept_encoding(&mut resp);
        let values: Vec<String> = resp
            .headers
            .get_all(http::header::VARY)
            .into_iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(values, vec!["accept-encoding, Origin".to_string()]);
    }

    #[test]
    fn vary_merge_avoids_duplicate_accept_encoding() {
        let mut resp = ResponseHeader::build(200, None).unwrap();
        resp.insert_header(http::header::VARY, "origin").unwrap();
        merge_vary_accept_encoding(&mut resp);
        let values: Vec<String> = resp
            .headers
            .get_all(http::header::VARY)
            .into_iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(values, vec!["origin, Accept-Encoding".to_string()]);
    }

    #[test]
    fn common_log_time_epoch_and_known_date() {
        // Epoch → the classic combined-log timestamp (the caller adds brackets).
        assert_eq!(common_log_time(0), "01/Jan/1970:00:00:00 +0000");
        // 2026-08-10 00:00:00 UTC = 20_675 days since the epoch (counted leap
        // years); the civil-from-days conversion must agree.
        let known = 20_675u64 * 86_400 * 1000;
        assert_eq!(common_log_time(known), "10/Aug/2026:00:00:00 +0000");
    }

    #[test]
    fn common_log_time_handles_time_of_day() {
        // 12:34:56 on the epoch day, in milliseconds.
        assert_eq!(
            common_log_time((12 * 3600 + 34 * 60 + 56) * 1000),
            "01/Jan/1970:12:34:56 +0000"
        );
    }
}
