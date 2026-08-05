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

use std::collections::BTreeSet;
use std::net::SocketAddr;

use thiserror::Error;

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
        matcher: Option<PathMatcher>,
        to: Vec<Upstream>,
        /// `lb_policy` inside the block (defaults to round-robin).
        lb_policy: LbPolicy,
        /// `health_check` inside the block (none = no active health check).
        health_check: Option<HealthCheckSpec>,
    },
    Handle {
        path: String,
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
}

/// An upstream target. The host is resolved to an address during validation;
/// `resolved` is `None` until then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub host: String,
    pub port: u16,
    pub resolved: Option<SocketAddr>,
}

/// A path matcher: a request path matches if it falls under the prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMatcher {
    /// The prefix path (with or without a trailing `*`), e.g. `/api/*`.
    pub prefix: String,
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
}

/// A compiled terminal directive.
#[derive(Debug, Clone)]
pub struct Terminal {
    /// Matchers (e.g. a `handle` path and/or an inline matcher); empty means
    /// always matches. All matchers must match for the terminal to serve.
    pub matchers: Vec<PathMatcher>,
    pub kind: TerminalKind,
    /// Modifiers scoped to this terminal (from an enclosing `handle` block).
    pub modifiers: Vec<Modifier>,
}

/// The runtime behavior of a terminal directive.
#[derive(Debug, Clone)]
pub enum TerminalKind {
    ReverseProxy {
        upstreams: Vec<SocketAddr>,
        /// The selection policy (round-robin unless `lb_policy` is set).
        lb_policy: LbPolicy,
        /// The active health-check spec, if configured.
        health_check: Option<HealthCheckSpec>,
    },
    /// Static file serving; runtime lands in M5.
    FileServer { root: String },
    /// A redirect; the target is a template expanded at request time.
    Redir { to: ValueTemplate, code: u16 },
}

/// A declarative transform, applied regardless of position (ADR-012).
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
