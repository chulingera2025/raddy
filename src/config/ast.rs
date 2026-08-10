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

//! Pure-data AST and compiled types for the config plane.
//!
//! A [`Raddyfile`] is parsed from source into the AST below, then validated,
//! resolved, and compiled into [`CompiledConfig`] — the pure data that becomes
//! the runtime [`crate::config::snapshot::ConfigSnapshot`]. Per ADR-012 the
//! compiled form separates *terminal* directives (which directive serves a
//! request, in write order) from *modifier* directives (declarative transforms
//! applied to whichever terminal serves), so the request plane never interprets
//! the file line by line.

use pingora::tls::x509::X509;
use pingora::utils::tls::CertKey;
use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use thiserror::Error;

// ---------------------------------------------------------------------------
// CIDR networks and rate limiting (spec §4 / §5.2)
// ---------------------------------------------------------------------------

/// An IP network used by `trusted_proxies`: an address plus a prefix length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cidr {
    /// The network base address (only the masked bits are meaningful).
    network: IpAddr,
    /// The prefix length: `0..=32` for IPv4, `0..=128` for IPv6.
    prefix: u8,
}

impl Cidr {
    /// Parse a CIDR: `<address>/<prefix>`, or a bare address (a single host).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (addr_str, prefix) = match raw.split_once('/') {
            Some((addr, p)) => (addr, Some(p)),
            None => (raw, None),
        };
        let address: IpAddr = addr_str
            .parse()
            .map_err(|_| format!("invalid IP address '{addr_str}'"))?;
        let max_prefix = match address {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        let prefix = match prefix {
            Some(p) => {
                let n: u8 = p
                    .parse()
                    .map_err(|_| format!("invalid CIDR prefix '{p}'"))?;
                if n > max_prefix {
                    return Err(format!("CIDR prefix {n} is too large for this address"));
                }
                n
            }
            None => max_prefix,
        };
        Ok(Self {
            network: address,
            prefix,
        })
    }

    /// Whether an IP address falls inside this network.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix as u32)
                };
                (u32::from(net) & mask) == (u32::from(ip) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = if self.prefix == 0 {
                    0u128
                } else {
                    u128::MAX << (128 - self.prefix as u32)
                };
                (u128::from(net) & mask) == (u128::from(ip) & mask)
            }
            _ => false,
        }
    }
}

/// The key a `rate_limit` directive counts on. `remote_ip` is the only key in
/// v0.1.2 (spec §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateLimitKey {
    RemoteIp,
}

/// The time unit of a rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateUnit {
    Second,
    Minute,
    Hour,
    Day,
}

impl RateUnit {
    /// The number of seconds in this unit.
    fn secs(self) -> u64 {
        match self {
            RateUnit::Second => 1,
            RateUnit::Minute => 60,
            RateUnit::Hour => 3600,
            RateUnit::Day => 86400,
        }
    }
}

/// A compiled `rate_limit` spec: what to count on, plus a token bucket with a
/// refill rate and capacity (spec §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateSpec {
    /// What the limit counts (`remote_ip` in v0.1.2).
    pub key: RateLimitKey,
    /// Tokens refilled per `unit`.
    pub count: u64,
    pub unit: RateUnit,
    /// Token-bucket capacity (≥ 1); defaults to `count` in the parser.
    pub burst: u64,
}

impl RateSpec {
    /// The continuous refill rate in tokens per second.
    pub fn tokens_per_second(&self) -> f64 {
        self.count as f64 / self.unit.secs() as f64
    }
}

// ---------------------------------------------------------------------------
// Value templates (`{host}`, `{uri}`, `{remote_host}`)
// ---------------------------------------------------------------------------

/// A value with `{placeholder}` variables, expanded per request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueTemplate {
    parts: Vec<TemplatePart>,
}

/// A literal or variable segment of a [`ValueTemplate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplatePart {
    Literal(String),
    Var(Variable),
}

/// Supported request-time variables. This is the Q6 whitelist: any other
/// placeholder is a validation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variable {
    /// Original Host header value (`{host}`).
    Host,
    /// Request URI including query (`{uri}`).
    Uri,
    /// Client socket address (`{remote_host}`).
    RemoteHost,
}

impl ValueTemplate {
    /// Create a template from its parts.
    pub fn new(parts: Vec<TemplatePart>) -> Self {
        Self { parts }
    }

    /// The template's parts, in order.
    pub fn parts(&self) -> &[TemplatePart] {
        &self.parts
    }

    /// Parse a raw value into a template, expanding `{name}` placeholders.
    ///
    /// Only the Q6 whitelist (`host`, `uri`, `remote_host`) is accepted; any
    /// other placeholder is an error.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '{' {
                let mut name = String::new();
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c2);
                }
                if !closed {
                    return Err("unclosed '{' in value".to_string());
                }
                let var = match name.as_str() {
                    "host" => Variable::Host,
                    "uri" => Variable::Uri,
                    "remote_host" => Variable::RemoteHost,
                    other => return Err(format!("unknown placeholder '{{{other}}}'")),
                };
                if !literal.is_empty() {
                    parts.push(TemplatePart::Literal(std::mem::take(&mut literal)));
                }
                parts.push(TemplatePart::Var(var));
            } else {
                literal.push(c);
            }
        }
        if !literal.is_empty() {
            parts.push(TemplatePart::Literal(literal));
        }
        Ok(Self { parts })
    }
}

// ---------------------------------------------------------------------------
// Source AST
// ---------------------------------------------------------------------------

/// The parsed Raddyfile: a global block plus site blocks.
#[derive(Debug, Clone)]
pub struct Raddyfile {
    pub global: GlobalConfig,
    pub sites: Vec<Site>,
}

/// Global configuration from the leading bare `{ ... }` block.
#[derive(Debug, Clone, Default)]
pub struct GlobalConfig {
    pub acme_email: Option<String>,
    pub log_level: Option<LogLevel>,
    /// Networks whose `X-Forwarded-For` is trusted for the real client IP
    /// (spec §4); empty = trust nobody, use the TCP peer.
    pub trusted_proxies: Vec<Cidr>,
    /// DNS-01 challenge credentials (spec §5.3); when set, certificate
    /// issuance uses DNS-01 instead of HTTP-01.
    pub dns_challenge: Option<DnsChallenge>,
}

/// DNS-01 challenge configuration (spec §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsChallenge {
    /// The DNS provider used to publish the challenge TXT record.
    pub provider: DnsProvider,
    /// The provider's API token (a secret; e.g. a Cloudflare token with
    /// `Zone: DNS: Edit` permission).
    pub api_token: String,
}

/// Supported DNS-01 providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsProvider {
    Cloudflare,
}

impl DnsProvider {
    /// The accepted keyword spellings, for error messages.
    pub const ALL: [&'static str; 1] = ["cloudflare"];
}

/// Global log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// The set of accepted keyword spellings, for error messages.
    pub const ALL: [&'static str; 4] = ["debug", "info", "warn", "error"];
}

/// A site block.
#[derive(Debug, Clone)]
pub struct Site {
    pub key: SiteKey,
    pub directives: Vec<Directive>,
}

/// Identifies a site: a named host or a `:port` catch-all.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SiteKey {
    /// A named host, optionally with an explicit port (default 443, M4).
    Named { host: String, port: u16 },
    /// Serves every request on the port that matches no named site.
    CatchAll { port: u16 },
}

impl SiteKey {
    /// The listener port this site is served on.
    pub fn port(&self) -> u16 {
        match self {
            SiteKey::Named { port, .. } => *port,
            SiteKey::CatchAll { port } => *port,
        }
    }

    /// A human-readable form of the key, used in error messages.
    pub fn describe(&self) -> String {
        match self {
            SiteKey::Named { host, port } => format!("{host}:{port}"),
            SiteKey::CatchAll { port } => format!(":{port}"),
        }
    }
}

/// One directive in a site or block, in source form.
#[derive(Debug, Clone)]
pub enum Directive {
    ReverseProxy {
        /// Inline matcher (spec §5.9); empty = always matches.
        matcher: Vec<Matcher>,
        to: Vec<Upstream>,
        /// `lb_policy` inside the block (defaults to round-robin).
        lb_policy: LbPolicy,
        /// `health_check` inside the block (none = no active health check).
        health_check: Option<HealthCheckSpec>,
        /// TLS sub-directives for `https://` upstreams (spec §5.4).
        tls: ProxyTlsConfig,
    },
    Handle {
        matcher: Vec<Matcher>,
        directives: Vec<Directive>,
    },
    /// Like `handle`, but the matched path prefix is stripped from the URI
    /// before the block's terminal runs (spec §5.9).
    HandlePath {
        matcher: Vec<Matcher>,
        directives: Vec<Directive>,
    },
    HeaderUp {
        name: String,
        value: ValueTemplate,
    },
    HeaderDown {
        name: String,
        value: ValueTemplate,
    },
    FileServer,
    Root {
        path: String,
    },
    Encode {
        algorithms: Vec<Encoding>,
    },
    Redir {
        to: String,
        code: u16,
    },
    /// `rewrite <to>` — rewrite the request URI before forwarding (modifier,
    /// spec §5.9). Conditional rewrites belong inside a `handle` block.
    Rewrite {
        to: ValueTemplate,
    },
    /// `respond <status> [<body>]` — answer directly (terminal, spec §5.9).
    Respond {
        status: u16,
        body: Option<String>,
    },
    /// `error [<status>] [<message>]` — trigger the internal error response
    /// (terminal, spec §5.9).
    Error {
        status: Option<u16>,
        message: Option<String>,
    },
    /// `basic_auth <user> <bcrypt-hash>` — HTTP Basic auth guard (spec §5.10).
    BasicAuth {
        user: String,
        hash: String,
    },
    /// `forward_auth <target>` — delegate auth to an upstream (spec §5.10).
    ForwardAuth {
        target: String,
    },
    /// Site-scoped `trusted_proxies`, overriding the global list for this site
    /// (spec §4). Compiled into [`CompiledSite::trusted_proxies`].
    TrustedProxies {
        networks: Vec<Cidr>,
    },
    /// A `rate_limit` guard (spec §5.2). Compiled into [`Modifier::RateLimit`].
    RateLimit {
        spec: RateSpec,
    },
    /// Per-site TLS configuration (spec §5.7): certificate source and options.
    Tls {
        config: TlsConfig,
    },
}

/// An upstream target. The host is resolved to an address during validation;
/// `resolved` is `None` until then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub host: String,
    pub port: u16,
    /// `https://` scheme prefix → the upstream connection is TLS (spec §5.4).
    pub tls: bool,
    pub resolved: Option<SocketAddr>,
}

/// TLS sub-directives of a `reverse_proxy` block (spec §5.4), in parse form.
/// They apply to every `https://` upstream in the block.
#[derive(Debug, Clone, Default)]
pub struct ProxyTlsConfig {
    /// `tls_servername <host>`: SNI/hostname sent to the upstream (default: the
    /// upstream host). Required when the address is an IP but the cert is for a
    /// name.
    pub servername: Option<String>,
    /// `tls_skip_verify`: disable upstream certificate and hostname verification.
    pub skip_verify: bool,
    /// `tls_ca <pem-file>`: extra root CA(s), repeatable.
    pub ca_files: Vec<String>,
    /// `tls_cert <cert-file> <key-file>`: a client certificate for upstream mTLS.
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
}

/// The certificate source of a site's `tls` directive (spec §5.7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TlsSource {
    /// Automatic ACME issuance (the default).
    #[default]
    Acme,
    /// `tls internal` — a self-signed certificate generated at startup.
    Internal,
    /// `tls <cert-file> <key-file>` — a static PEM certificate chain + key.
    Static { cert_file: String, key_file: String },
}

/// A TLS protocol version for `tls min_version` / `tls max_version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TlsVersion {
    Tls12,
    Tls13,
}

/// The mTLS client-certificate verification mode (`tls client_auth`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthMode {
    /// Ask for a client certificate but accept clients without one.
    Optional,
    /// Reject clients without a valid client certificate.
    Require,
}

/// Client-certificate authentication (spec §5.7): verify against `ca_file`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientAuth {
    pub mode: ClientAuthMode,
    pub ca_file: String,
}

/// Per-site TLS configuration from the `tls` directive (spec §5.7).
///
/// Options are independent and each may appear on its own `tls` line; the
/// compiler merges them. `source` defaults to ACME when no source is given.
#[derive(Debug, Clone, Default)]
pub struct TlsConfig {
    /// Certificate source (ACME by default).
    pub source: TlsSource,
    /// `tls min_version` (None = no floor).
    pub min_version: Option<TlsVersion>,
    /// `tls max_version` (None = no ceiling).
    pub max_version: Option<TlsVersion>,
    /// `tls ciphers <list>` (OpenSSL cipher list).
    pub ciphers: Option<String>,
    /// `tls client_auth` — mutual TLS.
    pub client_auth: Option<ClientAuth>,
}

/// The transport of the listener that received a request (`protocol` matcher).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Https,
}

/// One matcher term (spec §5.9). A directive or `handle` block may carry
/// several terms; all must match (AND). A term prefixed with `!` negates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// `path <prefix>` — the request path equals the prefix or falls under it
    /// (`/api` matches `/api` and `/api/...`, not `/apix`). A bare `/path`
    /// value is shorthand for this. `*` at the end is stripped (`/api/*`).
    Path(String),
    /// `host <host>` — the normalized Host header (port stripped,
    /// ASCII-lowercased) equals the value.
    Host(String),
    /// `method <method>` — the request method equals the value (case-sensitive;
    /// write `GET`, `POST`, …).
    Method(String),
    /// `header <name> <value>` — the request header `name` equals `value`
    /// (header name case-insensitive; value exact).
    Header { name: String, value: String },
    /// `query <key> <value>` — a query parameter `key` equals `value`.
    Query { key: String, value: String },
    /// `remote_ip <cidr>` — the real client IP (spec §4) is within the network.
    RemoteIp(Cidr),
    /// `protocol <http|https>` — the transport of the receiving listener.
    Protocol(Protocol),
    /// `!<term>` — negates a matcher term.
    Not(Box<Matcher>),
}

/// A compression algorithm for the `encode` directive (runtime in M5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    Gzip,
    Zstd,
}

/// The load-balancing policy of a `reverse_proxy` block (M9, §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LbPolicy {
    RoundRobin,
    Random,
    /// Consistent hash on the client IP (per-IP session stickiness).
    IpHash,
}

/// The active health-check configuration of a `reverse_proxy` block (M9, §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthCheckSpec {
    /// How often to probe each upstream.
    pub interval: std::time::Duration,
    /// Per-probe connection timeout.
    pub timeout: std::time::Duration,
    /// Consecutive failures before an upstream is removed.
    pub consecutive_failures: usize,
    /// Consecutive successes before an upstream is restored.
    pub consecutive_successes: usize,
}

/// The spec §5.1 defaults.
impl Default for HealthCheckSpec {
    fn default() -> Self {
        Self {
            interval: std::time::Duration::from_secs(5),
            timeout: std::time::Duration::from_secs(2),
            consecutive_failures: 3,
            consecutive_successes: 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Compiled form (ADR-012)
// ---------------------------------------------------------------------------

/// Fully validated, resolved, compiled configuration. Pure data, no object
/// state — this is exactly what may live in the swapped snapshot (ADR-011).
#[derive(Debug, Clone)]
pub struct CompiledConfig {
    pub global: GlobalConfig,
    pub sites: Vec<CompiledSite>,
}

impl CompiledConfig {
    /// The listener topology: the set of ports derived from all site keys,
    /// fixed at process start (ADR-010).
    pub fn listeners(&self) -> BTreeSet<u16> {
        self.sites.iter().map(|s| s.key.port()).collect()
    }
}

/// A compiled site: terminal directives in write order plus block-level
/// modifier directives.
#[derive(Debug, Clone)]
pub struct CompiledSite {
    pub key: SiteKey,
    /// Terminal directives in write order; the first whose matchers all match
    /// serves the request and ends site execution.
    pub terminals: Vec<Terminal>,
    /// Block-level modifier directives, applied to whichever terminal serves.
    pub modifiers: Vec<Modifier>,
    /// Site-scoped `trusted_proxies` (None = inherit the global list, §4).
    pub trusted_proxies: Option<Vec<Cidr>>,
    /// Per-site TLS configuration (spec §5.7): certificate source and the TLS
    /// options applied per SNI. `None` = ACME default, no overrides.
    pub tls: Option<TlsConfig>,
}

/// A compiled terminal directive.
#[derive(Debug, Clone)]
pub struct Terminal {
    /// Matchers (spec §5.9); empty means always matches. All matchers must
    /// match for the terminal to serve.
    pub matchers: Vec<Matcher>,
    pub kind: TerminalKind,
    /// Modifiers scoped to this terminal (from an enclosing `handle` block).
    pub modifiers: Vec<Modifier>,
    /// A path prefix stripped from the request before the terminal serves
    /// (the `handle_path` prefix, spec §5.9). `None` = serve the full path.
    pub strip_prefix: Option<String>,
}

/// A compiled upstream: the resolved address plus the peer metadata needed to
/// build the pingora `HttpPeer` (the original host for the default SNI, and the
/// `https://` scheme).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamPeer {
    pub addr: SocketAddr,
    /// `https://` scheme → TLS to this upstream (spec §5.4).
    pub tls: bool,
    /// The original upstream host — the default SNI for TLS (empty for plain
    /// HTTP, or when the operator overrides it with `tls_servername`).
    pub host: String,
}

/// Compiled TLS options for a `reverse_proxy` block that has at least one
/// `https://` upstream (spec §5.4). Immutable config data, carried in the
/// snapshot (ADR-011); the parsed certificates are built once at compile time.
#[derive(Clone)]
pub struct UpstreamTls {
    /// Verify the upstream certificate and hostname (default true;
    /// `tls_skip_verify` clears it).
    pub verify_cert: bool,
    /// SNI/hostname override (`tls_servername`); empty = per-upstream host.
    pub servername: String,
    /// Extra root CAs parsed from `tls_ca` PEM files. System roots are also
    /// trusted.
    pub ca: Option<Arc<Box<[X509]>>>,
    /// Client certificate for upstream mTLS (`tls_cert`).
    pub client_cert: Option<Arc<CertKey>>,
}

impl std::fmt::Debug for UpstreamTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamTls")
            .field("verify_cert", &self.verify_cert)
            .field("servername", &self.servername)
            .field("ca", &self.ca.as_ref().map(|ca| ca.len()))
            .field("client_cert", &self.client_cert.is_some())
            .finish()
    }
}

/// The runtime behavior of a terminal directive.
#[derive(Debug, Clone)]
pub enum TerminalKind {
    ReverseProxy {
        upstreams: Vec<UpstreamPeer>,
        /// The selection policy (round-robin unless `lb_policy` is set).
        lb_policy: LbPolicy,
        /// The active health-check spec, if configured.
        health_check: Option<HealthCheckSpec>,
        /// Block-level TLS options for `https://` upstreams (None when the
        /// block has no TLS upstream).
        tls: Option<UpstreamTls>,
    },
    /// Static file serving; runtime lands in M5.
    FileServer { root: String },
    /// A redirect; the target is a template expanded at request time.
    Redir { to: ValueTemplate, code: u16 },
    /// Answer directly with a status and optional body (spec §5.9).
    Respond { status: u16, body: Option<String> },
    /// Trigger the internal error response with a status/message (spec §5.9).
    Error {
        status: u16,
        message: Option<String>,
    },
}

/// A declarative transform or guard, applied regardless of position (ADR-012).
#[derive(Debug, Clone)]
pub enum Modifier {
    HeaderUp {
        name: String,
        value: ValueTemplate,
    },
    HeaderDown {
        name: String,
        value: ValueTemplate,
    },
    /// Response compression; runtime lands in M5.
    Encode {
        algorithms: Vec<Encoding>,
    },
    /// A rate-limit guard (spec §5.2).
    RateLimit {
        spec: RateSpec,
    },
    /// Rewrite the request URI before forwarding (spec §5.9).
    Rewrite {
        to: ValueTemplate,
    },
    /// HTTP Basic authentication guard (spec §5.10): the request must match one
    /// of the collected `(user, bcrypt-hash)` pairs.
    BasicAuth {
        users: Vec<(String, String)>,
    },
    /// Delegate authentication to an upstream (spec §5.10).
    ForwardAuth {
        target: String,
    },
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A configuration error with the source file name attached.
///
/// Line/column reporting lands in M3 (parser robustness milestone); M2 errors
/// carry the file name and a message.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {file}: {source}")]
    Io {
        file: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse error in {file}:{line}:{col}: {message}")]
    Parse {
        file: String,
        line: u32,
        col: u32,
        message: String,
    },
    #[error("validation error in {file}: {message}")]
    Validate { file: String, message: String },
}
