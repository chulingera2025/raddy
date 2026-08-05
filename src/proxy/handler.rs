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
    Encoding, LbPolicy, Modifier, SiteKey, TemplatePart, TerminalKind, ValueTemplate, Variable,
};
use crate::config::snapshot::ConfigStore;
use crate::proxy::compress::{self, Algo};
use crate::proxy::lb::{LbSpec, LoadBalancerPool};
use crate::proxy::site;
use crate::server::acme::ChallengeStore;
use async_trait::async_trait;
use bytes::Bytes;
use pingora::prelude::*;
use pingora::proxy::Session;
use serde::Serialize;
use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The process-lifetime proxy handler.
pub struct ProxyHandler {
    store: Arc<ConfigStore>,
    /// Load-balancing pool (per-site/terminal balancers, ADR-011).
    pool: Arc<LoadBalancerPool>,
    /// HTTP-01 challenge registry, consulted before site selection so the
    /// ACME challenge is served regardless of the site routing.
    challenges: Arc<ChallengeStore>,
    /// Optional structured access-log destination (M5).
    access_log: Option<Mutex<File>>,
}

impl ProxyHandler {
    /// Create a handler served by one [`ConfigStore`].
    pub fn new(
        store: Arc<ConfigStore>,
        challenges: Arc<ChallengeStore>,
        access_log: Option<Mutex<File>>,
        pool: Arc<LoadBalancerPool>,
    ) -> Self {
        Self {
            store,
            pool,
            challenges,
            access_log,
        }
    }
}

/// One structured access-log line (M5).
#[derive(Serialize)]
struct AccessLogEntry {
    /// Epoch milliseconds of the request.
    ts: u64,
    client: String,
    method: String,
    path: String,
    status: u16,
    duration_ms: u128,
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
    /// The effective modifier directives (block-level + terminal-scoped).
    modifiers: Vec<Modifier>,
    /// The site's `encode` priorities (empty = no compression).
    encode_algos: Vec<Encoding>,
    /// The encoding chosen for this response, if any.
    response_encoding: Option<Algo>,
    /// Accumulated response body for compression (buffered until end of stream).
    body_buffer: Vec<u8>,
    /// When the request started (for access-log duration).
    start: Option<Instant>,
}

#[async_trait]
impl ProxyHttp for ProxyHandler {
    type CTX = ProxyCtx;

    fn new_ctx(&self) -> Self::CTX {
        Self::CTX::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        ctx.start = Some(Instant::now());
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
                for (index, terminal) in site.terminals.iter().enumerate() {
                    if !matchers_match(&terminal.matchers, &path) {
                        continue;
                    }
                    match &terminal.kind {
                        TerminalKind::Redir { to, code } => {
                            let location = expand_template(to, session);
                            let mut resp = ResponseHeader::build(*code, None)?;
                            resp.insert_header(http::header::LOCATION, location)?;
                            resp.insert_header(http::header::CONTENT_LENGTH, "0")?;
                            session.write_response_header(Box::new(resp), true).await?;
                            return Ok(true);
                        }
                        TerminalKind::ReverseProxy {
                            upstreams,
                            lb_policy,
                            health_check,
                        } => {
                            ctx.site_key = Some(site.key.clone());
                            ctx.terminal_index = index;
                            ctx.lb_spec = Some(LbSpec {
                                upstreams: upstreams.clone(),
                                policy: *lb_policy,
                                health_check: *health_check,
                            });
                            ctx.modifiers = site.modifiers.clone();
                            ctx.modifiers.extend(terminal.modifiers.iter().cloned());
                            ctx.encode_algos = encode_algos(&ctx.modifiers);
                            // Continue to upstream_peer for forwarding.
                            return Ok(false);
                        }
                        TerminalKind::FileServer { root } => {
                            let mut modifiers = site.modifiers.clone();
                            modifiers.extend(terminal.modifiers.iter().cloned());
                            let encode = encode_algos(&modifiers);
                            crate::proxy::fs::serve(session, root, &path, &encode).await?;
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
        // `ip_hash` keys on the client IP for per-IP session stickiness; the
        // other policies ignore the key.
        let key = match lb_spec.policy {
            LbPolicy::IpHash => session
                .client_addr()
                .and_then(|addr| addr.as_inet())
                .map(|addr| addr.ip().to_string().into_bytes())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let addr = balancer
            .select(&key)
            .ok_or_else(|| Error::explain(HTTPStatus(502), "no healthy upstreams available"))?;
        // v0.1: plain-HTTP upstreams only (Q8) — no TLS, empty SNI.
        Ok(Box::new(HttpPeer::new(addr, false, String::new())))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        for (name, value) in header_ops(&ctx.modifiers, true, session) {
            match http::HeaderName::from_bytes(name.as_bytes()) {
                Ok(header_name) => {
                    if let Err(e) = upstream_request.insert_header(header_name, value) {
                        tracing::warn!("failed to set request header '{name}': {e}");
                    }
                }
                Err(_) => tracing::warn!("skipping invalid header name '{name}'"),
            }
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        for (name, value) in header_ops(&ctx.modifiers, false, session) {
            match http::HeaderName::from_bytes(name.as_bytes()) {
                Ok(header_name) => {
                    if let Err(e) = upstream_response.insert_header(header_name, value) {
                        tracing::warn!("failed to set response header '{name}': {e}");
                    }
                }
                Err(_) => tracing::warn!("skipping invalid header name '{name}'"),
            }
        }

        // Compression (M5): if the site enables `encode` and the client accepts
        // an algorithm, mark the response and let response_body_filter buffer +
        // compress it. Never double-compress an already-encoded response, and
        // skip statuses that carry no body.
        if !upstream_response
            .headers
            .contains_key(http::header::CONTENT_ENCODING)
            && !matches!(upstream_response.status.as_u16(), 204 | 304 | 206 | 1..=199)
        {
            let accept = session
                .req_header()
                .headers
                .get(http::header::ACCEPT_ENCODING);
            if let Some(algo) = compress::choose(&ctx.encode_algos, accept) {
                ctx.response_encoding = Some(algo);
                upstream_response.insert_header(http::header::CONTENT_ENCODING, algo.token())?;
                upstream_response.remove_header(&http::header::CONTENT_LENGTH);
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
        if let Some(algo) = ctx.response_encoding {
            if let Some(chunk) = body.take() {
                ctx.body_buffer.extend_from_slice(&chunk);
            }
            if end_of_stream {
                // Flush the fully-buffered compressed body now.
                let compressed = compress::compress(algo, &ctx.body_buffer);
                ctx.body_buffer.clear();
                *body = Some(Bytes::from(compressed));
            }
        }
        Ok(None)
    }

    async fn logging(&self, session: &mut Session, _e: Option<&Error>, ctx: &mut Self::CTX) {
        // Prometheus metrics (QPS + latency) for every completed request.
        if let Some(start) = ctx.start {
            crate::observ::metrics::record_request(start.elapsed().as_secs_f64());
        }
        // Structured JSON access log (if configured).
        let Some(log) = &self.access_log else {
            return;
        };
        if let Some(entry) = access_log_entry(session, ctx) {
            if let Ok(mut guard) = log.lock() {
                let _ = serde_json::to_writer(&mut *guard, &entry);
                let _ = writeln!(guard);
                let _ = guard.flush();
            }
        }
    }
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
    let client = session
        .client_addr()
        .and_then(|addr| addr.as_inet())
        .map(|addr| addr.ip().to_string())
        .unwrap_or_default();
    let method = session.req_header().method.to_string();
    let uri = &session.req_header().uri;
    let path = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(AccessLogEntry {
        ts,
        client,
        method,
        path,
        status,
        duration_ms,
    })
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

/// All matchers must match for a terminal to serve (ADR-012).
fn matchers_match(matchers: &[crate::config::ast::PathMatcher], path: &str) -> bool {
    matchers
        .iter()
        .all(|matcher| path_matches(&matcher.prefix, path))
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
}
