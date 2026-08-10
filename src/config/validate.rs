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

//! Semantic validation, upstream resolution, and compilation.
//!
//! Implements the Q6 validation checklist (site conflicts, address/port range,
//! upstream `host:port`, directive arity, global items, placeholder whitelist)
//! and compiles the source AST into the ADR-012 terminal/modifier form. This is
//! the single code path shared by startup, SIGHUP reload, and `raddy check`
//! (Q7).

use crate::config::ast::*;
use crate::config::resolver::resolve_host;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

/// Validate a parsed Raddyfile, resolve upstreams, and compile it.
///
/// Any semantic problem is reported as a [`ConfigError::Validate`]; nothing is
/// produced on error (Q6: no partial config).
pub fn validate_and_compile(
    file: &str,
    raddyfile: &Raddyfile,
) -> Result<CompiledConfig, ConfigError> {
    validate_global(file, &raddyfile.global)?;
    if raddyfile.sites.is_empty() {
        return Err(validate_error(file, "no sites defined"));
    }

    let mut seen = HashSet::new();
    let mut sites = Vec::with_capacity(raddyfile.sites.len());
    for site in &raddyfile.sites {
        if !seen.insert(site.key.clone()) {
            return Err(validate_error(
                file,
                format!("duplicate site '{}'", site.key.describe()),
            ));
        }
        sites.push(compile_site(file, site)?);
    }

    Ok(CompiledConfig {
        global: raddyfile.global.clone(),
        sites,
    })
}

fn validate_global(file: &str, global: &GlobalConfig) -> Result<(), ConfigError> {
    if let Some(email) = &global.acme_email {
        // Minimal shape check; full syntax validation belongs to the ACME
        // milestone (M4).
        if !email.contains('@') {
            return Err(validate_error(
                file,
                format!("invalid acme_email '{email}'"),
            ));
        }
    }
    Ok(())
}

/// Compile a single site into terminal/modifier form.
///
/// Two passes, so the compiled form is position-free as required by ADR-012:
/// (1) collect terminals (in write order), scope modifiers, and the effective
/// `root`; (2) attach the block-level modifiers to every terminal and resolve
/// `file_server` roots. Handle-scoped modifiers are appended after block-level
/// ones, so a deeper scope overrides for the same header.
fn compile_site(file: &str, site: &Site) -> Result<CompiledSite, ConfigError> {
    let mut terminals = Vec::new();
    let mut modifiers: Vec<Modifier> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    // Site-scoped `trusted_proxies` (None = inherit the global list, §4).
    let mut site_trusted: Option<Vec<Cidr>> = None;
    // Site-scoped `tls` configuration, merged across `tls` lines (spec §5.7).
    let mut site_tls: Option<TlsConfig> = None;
    // Auth guard accumulation (spec §5.10): all `basic_auth` users form one
    // table; `forward_auth` is a single delegation target (last wins).
    let mut basic_users: Vec<(String, String)> = Vec::new();
    let mut forward_auth: Option<String> = None;

    for directive in &site.directives {
        match directive {
            Directive::ReverseProxy {
                matcher,
                to,
                lb_policy,
                health_check,
                tls,
            } => terminals.push(compile_reverse_proxy_terminal(
                file,
                matcher.clone(),
                None,
                to,
                *lb_policy,
                *health_check,
                tls,
            )?),
            Directive::Handle {
                matcher,
                directives,
            } => compile_handle_block(file, matcher, false, directives, &mut terminals)?,
            Directive::HandlePath {
                matcher,
                directives,
            } => compile_handle_block(file, matcher, true, directives, &mut terminals)?,
            Directive::FileServer => terminals.push(Terminal {
                matchers: Vec::new(),
                kind: TerminalKind::FileServer {
                    root: String::new(),
                },
                modifiers: Vec::new(),
                strip_prefix: None,
            }),
            Directive::Root { path } => roots.push(path.clone()),
            Directive::HeaderUp { name, value } => {
                validate_header_name(file, name)?;
                modifiers.push(Modifier::HeaderUp {
                    name: name.clone(),
                    value: value.clone(),
                });
            }
            Directive::HeaderDown { name, value } => {
                validate_header_name(file, name)?;
                modifiers.push(Modifier::HeaderDown {
                    name: name.clone(),
                    value: value.clone(),
                });
            }
            Directive::Encode { algorithms } => {
                validate_encodings(file, algorithms)?;
                modifiers.push(Modifier::Encode {
                    algorithms: algorithms.clone(),
                });
            }
            Directive::Redir { to, code } => {
                let to =
                    ValueTemplate::parse(to).map_err(|message| validate_error(file, message))?;
                terminals.push(Terminal {
                    matchers: Vec::new(),
                    kind: TerminalKind::Redir { to, code: *code },
                    modifiers: Vec::new(),
                    strip_prefix: None,
                });
            }
            Directive::Rewrite { to } => modifiers.push(Modifier::Rewrite { to: to.clone() }),
            Directive::Respond { status, body } => terminals.push(Terminal {
                matchers: Vec::new(),
                kind: TerminalKind::Respond {
                    status: *status,
                    body: body.clone(),
                },
                modifiers: Vec::new(),
                strip_prefix: None,
            }),
            Directive::Error { status, message } => terminals.push(Terminal {
                matchers: Vec::new(),
                kind: TerminalKind::Error {
                    status: status.unwrap_or(500),
                    message: message.clone(),
                },
                modifiers: Vec::new(),
                strip_prefix: None,
            }),
            Directive::TrustedProxies { networks } => {
                // Last occurrence wins (same as the global block).
                site_trusted = Some(networks.clone());
            }
            Directive::RateLimit { spec } => {
                modifiers.push(Modifier::RateLimit { spec: spec.clone() })
            }
            Directive::Tls { config } => merge_tls(&mut site_tls, config),
            Directive::BasicAuth { user, hash } => basic_users.push((user.clone(), hash.clone())),
            Directive::ForwardAuth { target } => forward_auth = Some(target.clone()),
        }
    }

    // Auth guards (spec §5.10) are emitted as single modifiers: all `basic_auth`
    // lines form one user table, and `forward_auth` (last wins) delegates.
    if !basic_users.is_empty() {
        modifiers.push(Modifier::BasicAuth { users: basic_users });
    }
    if let Some(target) = forward_auth {
        modifiers.push(Modifier::ForwardAuth { target });
    }

    // Validate the merged per-site TLS config (file existence, version range).
    if let Some(tls) = &site_tls {
        validate_tls_config(file, tls)?;
    }

    // Pass 2: block-level modifiers apply to every terminal (ADR-012); a
    // block-level `file_server` (root still empty) takes the last `root` in the
    // site scope. Handle-scoped roots were already resolved in
    // [`compile_handle_block`].
    let root = roots.last().cloned();
    for terminal in &mut terminals {
        terminal.modifiers.extend(modifiers.clone());
        if let TerminalKind::FileServer { root: file_root } = &mut terminal.kind {
            if file_root.is_empty() {
                *file_root = root.clone().ok_or_else(|| {
                    validate_error(file, "file_server requires a 'root' directive in its scope")
                })?;
            }
        }
    }

    Ok(CompiledSite {
        key: site.key.clone(),
        terminals,
        modifiers,
        trusted_proxies: site_trusted,
        tls: site_tls,
    })
}

/// Compile the directives inside a `handle` (or `handle_path`) block.
///
/// Every terminal inherits the block's matchers (spec §5.9); a `handle_path`
/// block additionally strips the matched path prefix before its terminal
/// serves. The returned terminals already carry their handle-scoped modifiers;
/// block-level modifiers are appended later in [`compile_site`].
fn compile_handle_block(
    file: &str,
    matcher: &[Matcher],
    strip: bool,
    directives: &[Directive],
    out: &mut Vec<Terminal>,
) -> Result<(), ConfigError> {
    // The path prefix a `handle_path` block strips (the first `path` matcher;
    // a handle_path with no path matcher strips nothing).
    let strip_prefix = if strip {
        matcher.iter().find_map(|m| match m {
            Matcher::Path(prefix) => Some(prefix.clone()),
            _ => None,
        })
    } else {
        None
    };
    let mut scoped_modifiers: Vec<Modifier> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    let mut block_terminals: Vec<Terminal> = Vec::new();
    // Auth guard accumulation within the handle scope (spec §5.10).
    let mut basic_users: Vec<(String, String)> = Vec::new();
    let mut forward_auth: Option<String> = None;

    for directive in directives {
        match directive {
            Directive::ReverseProxy {
                matcher: inline,
                to,
                lb_policy,
                health_check,
                tls,
            } => {
                let mut matchers = matcher.to_vec();
                matchers.extend(inline.iter().cloned());
                block_terminals.push(compile_reverse_proxy_terminal(
                    file,
                    matchers,
                    strip_prefix.clone(),
                    to,
                    *lb_policy,
                    *health_check,
                    tls,
                )?);
            }
            Directive::FileServer => block_terminals.push(Terminal {
                matchers: matcher.to_vec(),
                kind: TerminalKind::FileServer {
                    root: String::new(),
                },
                modifiers: Vec::new(),
                strip_prefix: strip_prefix.clone(),
            }),
            Directive::Redir { to, code } => {
                let to =
                    ValueTemplate::parse(to).map_err(|message| validate_error(file, message))?;
                block_terminals.push(Terminal {
                    matchers: matcher.to_vec(),
                    kind: TerminalKind::Redir { to, code: *code },
                    modifiers: Vec::new(),
                    strip_prefix: strip_prefix.clone(),
                });
            }
            Directive::Respond { status, body } => block_terminals.push(Terminal {
                matchers: matcher.to_vec(),
                kind: TerminalKind::Respond {
                    status: *status,
                    body: body.clone(),
                },
                modifiers: Vec::new(),
                strip_prefix: strip_prefix.clone(),
            }),
            Directive::Error { status, message } => block_terminals.push(Terminal {
                matchers: matcher.to_vec(),
                kind: TerminalKind::Error {
                    status: status.unwrap_or(500),
                    message: message.clone(),
                },
                modifiers: Vec::new(),
                strip_prefix: strip_prefix.clone(),
            }),
            Directive::Rewrite { to } => {
                scoped_modifiers.push(Modifier::Rewrite { to: to.clone() })
            }
            Directive::Root { path } => roots.push(path.clone()),
            Directive::HeaderUp { name, value } => {
                validate_header_name(file, name)?;
                scoped_modifiers.push(Modifier::HeaderUp {
                    name: name.clone(),
                    value: value.clone(),
                });
            }
            Directive::HeaderDown { name, value } => {
                validate_header_name(file, name)?;
                scoped_modifiers.push(Modifier::HeaderDown {
                    name: name.clone(),
                    value: value.clone(),
                });
            }
            Directive::Encode { algorithms } => {
                validate_encodings(file, algorithms)?;
                scoped_modifiers.push(Modifier::Encode {
                    algorithms: algorithms.clone(),
                });
            }
            Directive::RateLimit { spec } => {
                scoped_modifiers.push(Modifier::RateLimit { spec: spec.clone() });
            }
            Directive::TrustedProxies { .. } => {
                return Err(validate_error(
                    file,
                    "trusted_proxies is only allowed at the global or site-block level",
                ));
            }
            Directive::Tls { .. } => {
                return Err(validate_error(
                    file,
                    "tls is only allowed at the site-block level",
                ));
            }
            Directive::Handle { .. } | Directive::HandlePath { .. } => {
                return Err(validate_error(
                    file,
                    "nested handle blocks are not supported in v0.1",
                ));
            }
            Directive::BasicAuth { user, hash } => basic_users.push((user.clone(), hash.clone())),
            Directive::ForwardAuth { target } => forward_auth = Some(target.clone()),
        }
    }

    // Handle-scoped auth guards (spec §5.10) apply only to this block's terminal.
    if !basic_users.is_empty() {
        scoped_modifiers.push(Modifier::BasicAuth { users: basic_users });
    }
    if let Some(target) = forward_auth {
        scoped_modifiers.push(Modifier::ForwardAuth { target });
    }

    let root = roots.last().cloned();
    for mut terminal in block_terminals {
        terminal.modifiers.extend(scoped_modifiers.clone());
        if let TerminalKind::FileServer { root: file_root } = &mut terminal.kind {
            *file_root = root.clone().ok_or_else(|| {
                validate_error(file, "file_server requires a 'root' directive in its scope")
            })?;
        }
        out.push(terminal);
    }

    Ok(())
}

/// Resolve every upstream's host to all of its concrete addresses at build
/// time, so the snapshot stays pure data and the request plane performs no DNS
/// (ADR-011).
///
/// Each hostname contributes every unique `SocketAddr` it resolves to —
/// flattened into the terminal's backend list with first-seen order preserved
/// and duplicates dropped — so a hostname with several A/AAAA records becomes
/// several backends instead of the v0.1 "first address only". An explicit IP
/// literal contributes exactly itself (preserved unchanged). `resolver` is
/// injectable so tests never touch the network; the production resolver
/// (`crate::config::resolver::resolve_host`) applies a timeout and a bounded
/// thread pool, so a hung DNS server yields a diagnosable error, never a hang.
/// Compile a `reverse_proxy` directive into a terminal: resolve the upstream
/// targets (keeping each one's TLS scheme and original host), and compile the
/// block's TLS sub-directives.
fn compile_reverse_proxy_terminal(
    file: &str,
    matchers: Vec<Matcher>,
    strip_prefix: Option<String>,
    to: &[Upstream],
    lb_policy: LbPolicy,
    health_check: Option<HealthCheckSpec>,
    tls: &ProxyTlsConfig,
) -> Result<Terminal, ConfigError> {
    let upstreams = resolve_upstreams(file, to, resolve_host)?;
    let tls = build_upstream_tls(file, tls, to.iter().any(|u| u.tls))?;
    Ok(Terminal {
        matchers,
        kind: TerminalKind::ReverseProxy {
            upstreams,
            lb_policy,
            health_check,
            tls,
        },
        modifiers: Vec::new(),
        strip_prefix,
    })
}

/// Compile the block's `tls` sub-directives (spec §5.4) into [`UpstreamTls`],
/// reading and parsing the CA and client-certificate files once at build time.
/// Returns `Ok(None)` when the block has no `https://` upstream — TLS options
/// are then rejected as meaningless.
fn build_upstream_tls(
    file: &str,
    config: &ProxyTlsConfig,
    has_tls: bool,
) -> Result<Option<UpstreamTls>, ConfigError> {
    if !has_tls {
        if config.servername.is_some()
            || config.skip_verify
            || !config.ca_files.is_empty()
            || config.cert_file.is_some()
        {
            return Err(validate_error(
                file,
                "tls_servername / tls_skip_verify / tls_ca / tls_cert require an https:// upstream",
            ));
        }
        return Ok(None);
    }
    // Extra root CAs (PEM), appended to the system roots.
    let ca = if config.ca_files.is_empty() {
        None
    } else {
        let mut stack = Vec::new();
        for path in &config.ca_files {
            let pem = std::fs::read_to_string(path)
                .map_err(|e| validate_error(file, format!("failed to read CA file {path}: {e}")))?;
            let certs = pingora::tls::x509::X509::stack_from_pem(pem.as_bytes()).map_err(|e| {
                validate_error(file, format!("invalid CA certificate in {path}: {e}"))
            })?;
            stack.extend(certs);
        }
        Some(Arc::new(stack.into_boxed_slice()))
    };
    // Client certificate for upstream mTLS, if configured.
    let client_cert = match (&config.cert_file, &config.key_file) {
        (Some(cert), Some(key)) => {
            let cert_pem = std::fs::read_to_string(cert).map_err(|e| {
                validate_error(file, format!("failed to read certificate file {cert}: {e}"))
            })?;
            let key_pem = std::fs::read_to_string(key)
                .map_err(|e| validate_error(file, format!("failed to read key file {key}: {e}")))?;
            let cert_key = crate::tls::cert_key_from_pem(&cert_pem, &key_pem)
                .map_err(|e| validate_error(file, format!("invalid client certificate: {e}")))?;
            Some(Arc::new(cert_key))
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(validate_error(
                file,
                "tls_cert requires both a certificate and a key file",
            ))
        }
        (None, None) => None,
    };
    Ok(Some(UpstreamTls {
        verify_cert: !config.skip_verify,
        servername: config.servername.clone().unwrap_or_default(),
        ca,
        client_cert,
    }))
}

fn resolve_upstreams(
    file: &str,
    to: &[Upstream],
    resolver: impl Fn(&str, u16) -> Result<Vec<SocketAddr>, String>,
) -> Result<Vec<UpstreamPeer>, ConfigError> {
    let mut resolved: Vec<UpstreamPeer> = Vec::new();
    for upstream in to {
        let addrs = resolver(&upstream.host, upstream.port)
            .map_err(|message| validate_error(file, message))?;
        if addrs.is_empty() {
            return Err(validate_error(
                file,
                format!(
                    "no address resolved for upstream {}:{}",
                    upstream.host, upstream.port
                ),
            ));
        }
        for addr in addrs {
            if !resolved.iter().any(|peer| peer.addr == addr) {
                resolved.push(UpstreamPeer {
                    addr,
                    tls: upstream.tls,
                    host: upstream.host.clone(),
                });
            }
        }
    }
    Ok(resolved)
}

/// Check a header name is a non-empty HTTP token.
fn validate_header_name(file: &str, name: &str) -> Result<(), ConfigError> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !valid {
        return Err(validate_error(
            file,
            format!("invalid header name '{name}'"),
        ));
    }
    Ok(())
}

/// Check encode algorithms are non-empty and non-duplicated.
fn validate_encodings(file: &str, algorithms: &[Encoding]) -> Result<(), ConfigError> {
    if algorithms.is_empty() {
        return Err(validate_error(
            file,
            "encode requires at least one algorithm",
        ));
    }
    let mut seen = HashSet::new();
    for alg in algorithms {
        if !seen.insert(*alg) {
            return Err(validate_error(file, "duplicate encode algorithm"));
        }
    }
    Ok(())
}

fn validate_error(file: &str, message: impl Into<String>) -> ConfigError {
    ConfigError::Validate {
        file: file.to_string(),
        message: message.into(),
    }
}

/// Merge one `tls` directive into the site's accumulated config (spec §5.7).
/// Each option is independent and the last occurrence wins; `source` is only
/// overridden when the directive actually sets one (bare `tls` = ACME default).
fn merge_tls(site_tls: &mut Option<TlsConfig>, config: &TlsConfig) {
    let target = site_tls.get_or_insert_with(TlsConfig::default);
    if config.source != TlsSource::Acme {
        target.source = config.source.clone();
    }
    if config.min_version.is_some() {
        target.min_version = config.min_version;
    }
    if config.max_version.is_some() {
        target.max_version = config.max_version;
    }
    if config.ciphers.is_some() {
        target.ciphers = config.ciphers.clone();
    }
    if config.client_auth.is_some() {
        target.client_auth = config.client_auth.clone();
    }
}

/// Validate a merged per-site `tls` config: static cert and mTLS CA files exist
/// and parse, and the version range is consistent (spec §5.7). Runs at compile
/// time so `raddy check` and reload catch it before the files are needed.
fn validate_tls_config(file: &str, config: &TlsConfig) -> Result<(), ConfigError> {
    if let TlsSource::Static {
        cert_file,
        key_file,
    } = &config.source
    {
        let cert_pem = std::fs::read_to_string(cert_file).map_err(|e| {
            validate_error(file, format!("failed to read certificate {cert_file}: {e}"))
        })?;
        let key_pem = std::fs::read_to_string(key_file)
            .map_err(|e| validate_error(file, format!("failed to read key {key_file}: {e}")))?;
        crate::tls::cert_key_from_pem(&cert_pem, &key_pem)
            .map_err(|e| validate_error(file, format!("invalid tls certificate: {e}")))?;
    }
    if let Some(client_auth) = &config.client_auth {
        let ca_pem = std::fs::read_to_string(&client_auth.ca_file).map_err(|e| {
            validate_error(
                file,
                format!("failed to read CA file {}: {e}", client_auth.ca_file),
            )
        })?;
        pingora::tls::x509::X509::stack_from_pem(ca_pem.as_bytes()).map_err(|e| {
            validate_error(
                file,
                format!("invalid CA file {}: {e}", client_auth.ca_file),
            )
        })?;
    }
    if let (Some(min), Some(max)) = (config.min_version, config.max_version) {
        if min > max {
            return Err(validate_error(
                file,
                "tls min_version must not exceed max_version",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parser::parse;

    fn compile(input: &str) -> Result<CompiledConfig, ConfigError> {
        validate_and_compile("test", &parse("test", input).unwrap())
    }

    #[test]
    fn rejects_duplicate_sites() {
        let err =
            compile(":8080 { reverse_proxy 127.0.0.1:1 }\n:8080 { reverse_proxy 127.0.0.1:2 }\n")
                .unwrap_err();
        assert!(err.to_string().contains("duplicate site"));
    }

    #[test]
    fn allows_named_and_catchall_on_same_port() {
        let cfg = compile(
            ":8080 { reverse_proxy 127.0.0.1:1 }\nexample.com:8080 { reverse_proxy 127.0.0.1:2 }\n",
        )
        .unwrap();
        assert_eq!(cfg.sites.len(), 2);
    }

    #[test]
    fn rejects_empty_sites() {
        let err = compile("").unwrap_err();
        assert!(err.to_string().contains("no sites"));
    }

    #[test]
    fn compiles_terminal_and_modifier_split() {
        let cfg = compile(
            "example.com {\n    reverse_proxy 127.0.0.1:8080\n    header_up X-Real-IP {remote_host}\n}\n",
        )
        .unwrap();
        let site = &cfg.sites[0];
        assert_eq!(site.terminals.len(), 1);
        assert_eq!(site.modifiers.len(), 1);
        match &site.terminals[0].kind {
            TerminalKind::ReverseProxy { upstreams, .. } => {
                assert_eq!(upstreams.len(), 1);
                assert_eq!(upstreams[0].addr.port(), 8080);
                assert!(!upstreams[0].tls, "a bare upstream stays plain HTTP");
            }
            other => panic!("expected reverse proxy, got {other:?}"),
        }
    }

    #[test]
    fn header_after_reverse_proxy_still_compiles() {
        // The §7 arrangement must be valid under the declarative modifier model
        // (ADR-012): header_up written after reverse_proxy is still applied.
        let cfg = compile(
            "api.example.com {\n    handle /static/* {\n        root /var/www/html\n        file_server\n        encode zstd gzip\n    }\n    reverse_proxy 127.0.0.1:8080\n    header_up X-Real-IP {remote_host}\n}\n",
        )
        .unwrap();
        let site = &cfg.sites[0];
        assert_eq!(site.terminals.len(), 2);
        assert_eq!(site.modifiers.len(), 1); // block-level header_up
                                             // The handle terminal (file_server) carries the handle-scoped `encode`
                                             // plus the block-level `header_up`.
        assert_eq!(site.terminals[0].matchers.len(), 1);
        assert_eq!(site.terminals[0].modifiers.len(), 2);
        // The block-level reverse_proxy terminal carries the header_up.
        assert_eq!(site.terminals[1].matchers.len(), 0);
        assert_eq!(site.terminals[1].modifiers.len(), 1);
    }

    #[test]
    fn rejects_unknown_placeholder() {
        // The placeholder whitelist (Q6) is enforced during parsing.
        let input = ":8080 {\n    header_up X {oops}\n}\n";
        let err = crate::config::parser::parse("test", input).unwrap_err();
        assert!(err.to_string().contains("unknown placeholder"));
    }

    #[test]
    fn resolves_upstream_hostname() {
        let cfg = compile(":8080 {\n    reverse_proxy localhost:8080\n}\n").unwrap();
        match &cfg.sites[0].terminals[0].kind {
            TerminalKind::ReverseProxy { upstreams, .. } => {
                assert!(
                    !upstreams.is_empty(),
                    "localhost must resolve to at least one address"
                );
                assert!(
                    upstreams.iter().all(|a| a.addr.port() == 8080),
                    "every resolved address must keep the configured port"
                );
            }
            other => panic!("expected reverse proxy, got {other:?}"),
        }
    }

    /// A test upstream entry for `resolve_upstreams`.
    fn upstream(host: &str, port: u16) -> Upstream {
        Upstream {
            host: host.to_string(),
            port,
            tls: false,
            resolved: None,
        }
    }

    /// A `https://` test upstream entry (spec §5.4).
    fn tls_upstream(host: &str, port: u16) -> Upstream {
        Upstream {
            host: host.to_string(),
            port,
            tls: true,
            resolved: None,
        }
    }

    /// Extract the resolved addresses for order/dedup assertions (host and TLS
    /// scheme are asserted separately where relevant).
    fn addrs_of(peers: &[UpstreamPeer]) -> Vec<SocketAddr> {
        peers.iter().map(|p| p.addr).collect()
    }

    fn addr(n: u8) -> SocketAddr {
        format!("10.0.0.{n}:80").parse().unwrap()
    }

    #[test]
    fn flattens_all_unique_addresses_preserving_order() {
        // One hostname resolves to several addresses; a shared address from a
        // second hostname is deduplicated while first-seen order is kept.
        let to = vec![upstream("a.test", 80), upstream("b.test", 80)];
        let got = resolve_upstreams("test", &to, |host, _port| match host {
            "a.test" => Ok(vec![addr(1), addr(2)]),
            "b.test" => Ok(vec![addr(2), addr(3)]),
            _ => unreachable!(),
        })
        .unwrap();
        assert_eq!(addrs_of(&got), vec![addr(1), addr(2), addr(3)]);
    }

    #[test]
    fn explicit_ip_upstreams_preserved_and_deduped() {
        // The production resolver returns an explicit IP literal unchanged; the
        // flatten step must keep it (and drop an exact duplicate upstream).
        let to = vec![upstream("127.0.0.1", 9000), upstream("127.0.0.1", 9000)];
        let got = resolve_upstreams("test", &to, |host, port| {
            Ok(vec![SocketAddr::new(host.parse().unwrap(), port)])
        })
        .unwrap();
        assert_eq!(
            addrs_of(&got),
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), 9000)]
        );
    }

    #[test]
    fn empty_resolution_is_a_config_error() {
        let to = vec![upstream("nope.test", 80)];
        let err = resolve_upstreams("test", &to, |_, _| Ok(vec![])).unwrap_err();
        assert!(err
            .to_string()
            .contains("no address resolved for upstream nope.test:80"));
    }

    #[test]
    fn resolution_timeout_propagates_diagnostic() {
        // The injectable resolver stands in for a timed-out lookup; the
        // config error must carry the diagnostic for `raddy check`/reload.
        let to = vec![upstream("slow.test", 80)];
        let err = resolve_upstreams("test", &to, |_, _| {
            Err("failed to resolve upstream slow.test:80: timed out after 5s".to_string())
        })
        .unwrap_err();
        assert!(err.to_string().contains("timed out after 5s"));
    }

    #[test]
    fn resolver_order_is_stable() {
        // The order the resolver returns is preserved exactly: no re-sorting or
        // reordering is performed by the flatten step.
        let to = vec![upstream("x.test", 80)];
        let got =
            resolve_upstreams("test", &to, |_, _| Ok(vec![addr(3), addr(1), addr(2)])).unwrap();
        assert_eq!(addrs_of(&got), vec![addr(3), addr(1), addr(2)]);
    }

    #[test]
    fn compiles_rate_limit_as_modifier() {
        let cfg = compile(
            ":8080 {\n    rate_limit remote_ip 50r/s burst=100\n    reverse_proxy 127.0.0.1:9000\n}\n",
        )
        .unwrap();
        let site = &cfg.sites[0];
        match &site.modifiers[0] {
            Modifier::RateLimit { spec } => {
                assert_eq!(spec.count, 50);
                assert_eq!(spec.burst, 100);
            }
            other => panic!("expected rate_limit modifier, got {other:?}"),
        }
    }

    #[test]
    fn compiles_handle_scoped_rate_limit() {
        let cfg = compile(
            ":8080 {\n    handle /api/* {\n        rate_limit remote_ip 5r/s\n        reverse_proxy 127.0.0.1:9000\n    }\n    reverse_proxy 127.0.0.1:9001\n}\n",
        )
        .unwrap();
        let site = &cfg.sites[0];
        // The handle terminal carries the handle-scoped rate_limit; the
        // block-level reverse_proxy terminal does not.
        assert_eq!(site.terminals.len(), 2);
        assert!(matches!(
            site.terminals[0].modifiers.first(),
            Some(Modifier::RateLimit { .. })
        ));
        assert!(site.terminals[1].modifiers.is_empty());
    }

    #[test]
    fn site_trusted_proxies_override_global() {
        let cfg = compile(
            "{ trusted_proxies 10.0.0.0/8 }\n:8080 {\n    trusted_proxies 192.168.0.0/16\n    reverse_proxy 127.0.0.1:9000\n}\n",
        )
        .unwrap();
        let site = &cfg.sites[0];
        let networks = site.trusted_proxies.as_ref().expect("site override");
        assert!(networks[0].contains("192.168.5.5".parse().unwrap()));
        assert!(!networks[0].contains("10.1.1.1".parse().unwrap()));
        // Without a site override the compiled site inherits the global list.
        let cfg2 = compile(
            "{ trusted_proxies 10.0.0.0/8 }\n:8080 {\n    reverse_proxy 127.0.0.1:9000\n}\n",
        )
        .unwrap();
        assert!(cfg2.sites[0].trusted_proxies.is_none());
        assert_eq!(cfg2.global.trusted_proxies.len(), 1);
    }

    #[test]
    fn rejects_trusted_proxies_inside_handle() {
        let err = compile(
            ":8080 {\n    handle /x/* {\n        trusted_proxies 10.0.0.0/8\n        file_server\n    }\n}\n",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("trusted_proxies is only allowed at the global or site-block level"));
    }

    #[test]
    fn parses_https_upstream_scheme() {
        // `https://` on a target flips the upstream to TLS (spec §5.4); the
        // bare form stays plain HTTP.
        let cfg = compile(":8080 {\n    reverse_proxy https://127.0.0.1:8443\n}\n").unwrap();
        match &cfg.sites[0].terminals[0].kind {
            TerminalKind::ReverseProxy { upstreams, tls, .. } => {
                assert_eq!(upstreams.len(), 1);
                assert!(upstreams[0].tls);
                assert_eq!(upstreams[0].host, "127.0.0.1");
                let tls = tls
                    .as_ref()
                    .expect("an https upstream compiles TLS options");
                assert!(tls.verify_cert, "verification is on by default");
                assert!(tls.servername.is_empty());
                assert!(tls.ca.is_none());
                assert!(tls.client_cert.is_none());
            }
            other => panic!("expected reverse proxy, got {other:?}"),
        }
    }

    #[test]
    fn compiles_tls_subdirectives() {
        let cfg = compile(
            ":8080 {\n    reverse_proxy {\n        to https://127.0.0.1:8443\n        tls_servername api.internal\n        tls_skip_verify\n    }\n}\n",
        )
        .unwrap();
        match &cfg.sites[0].terminals[0].kind {
            TerminalKind::ReverseProxy { upstreams, tls, .. } => {
                assert!(upstreams[0].tls);
                let tls = tls.as_ref().expect("TLS options compiled");
                assert!(!tls.verify_cert, "tls_skip_verify clears verification");
                assert_eq!(tls.servername, "api.internal");
            }
            other => panic!("expected reverse proxy, got {other:?}"),
        }
    }

    #[test]
    fn rejects_tls_options_without_https_upstream() {
        let err = compile(
            ":8080 {\n    reverse_proxy {\n        to 127.0.0.1:8443\n        tls_servername api.internal\n    }\n}\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("require an https:// upstream"));
    }

    #[test]
    fn tls_ca_requires_valid_pem() {
        let err = compile(
            ":8080 {\n    reverse_proxy {\n        to https://127.0.0.1:8443\n        tls_ca /nonexistent/ca.pem\n    }\n}\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to read CA file"));
    }

    #[test]
    fn tls_cert_reads_pem_files() {
        // Generate a self-signed client cert and write it (plus its key) to
        // temp files; the compiled TLS config must parse them.
        let dir = std::env::temp_dir();
        let stem = format!("raddy-tls-cert-{}", std::process::id());
        let cert_path = dir.join(format!("{stem}.pem"));
        let key_path = dir.join(format!("{stem}.key"));
        let cert = rcgen::generate_simple_self_signed(vec!["client.test".to_string()]).unwrap();
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
        let cfg = compile(&format!(
            ":8080 {{\n    reverse_proxy {{\n        to https://127.0.0.1:8443\n        tls_cert {} {}\n    }}\n}}\n",
            cert_path.display(),
            key_path.display()
        ))
        .unwrap();
        match &cfg.sites[0].terminals[0].kind {
            TerminalKind::ReverseProxy { tls, .. } => {
                let tls = tls.as_ref().expect("TLS options compiled");
                assert!(tls.client_cert.is_some());
            }
            other => panic!("expected reverse proxy, got {other:?}"),
        }
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);
    }

    #[test]
    fn resolve_upstreams_keeps_tls_scheme() {
        let to = vec![tls_upstream("secure.test", 8443)];
        let got = resolve_upstreams("test", &to, |_, port| {
            Ok(vec![SocketAddr::new("10.0.0.9".parse().unwrap(), port)])
        })
        .unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].tls);
        assert_eq!(got[0].host, "secure.test");
        assert_eq!(got[0].addr.port(), 8443);
    }

    /// Write a self-signed cert + key for `host` to temp files, returning the
    /// paths (spec §5.7 test helper). A per-test counter keeps filenames unique
    /// so parallel tests never clobber each other's temp files.
    fn write_test_cert(host: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir();
        let stem = format!(
            "raddy-tls-site-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            host
        );
        let cert_path = dir.join(format!("{stem}.pem"));
        let key_path = dir.join(format!("{stem}.key"));
        let cert = rcgen::generate_simple_self_signed(vec![host.to_string()]).unwrap();
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
        (cert_path, key_path)
    }

    #[test]
    fn compiles_static_tls_source() {
        let (cert, key) = write_test_cert("example.com");
        let cfg = compile(&format!(
            "example.com:8443 {{\n    tls {} {}\n    reverse_proxy 127.0.0.1:9000\n}}\n",
            cert.display(),
            key.display()
        ))
        .unwrap();
        assert!(matches!(
            cfg.sites[0].tls.as_ref().expect("tls config").source,
            TlsSource::Static { .. }
        ));
        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn compiles_internal_tls_without_files() {
        let cfg =
            compile("example.com:8443 {\n    tls internal\n    reverse_proxy 127.0.0.1:9000\n}\n")
                .unwrap();
        assert_eq!(
            cfg.sites[0].tls.as_ref().expect("tls config").source,
            TlsSource::Internal
        );
    }

    #[test]
    fn merges_tls_options_across_lines() {
        let (cert, key) = write_test_cert("example.com");
        let cfg = compile(&format!(
            "example.com:8443 {{\n    tls {} {}\n    tls min_version 1.3\n    reverse_proxy 127.0.0.1:9000\n}}\n",
            cert.display(),
            key.display()
        ))
        .unwrap();
        let tls = cfg.sites[0].tls.as_ref().expect("tls config");
        assert!(matches!(tls.source, TlsSource::Static { .. }));
        assert_eq!(tls.min_version, Some(TlsVersion::Tls13));
        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn compiles_client_auth() {
        let (ca, _) = write_test_cert("ca.example.com");
        let cfg = compile(&format!(
            "example.com:8443 {{\n    tls client_auth require {}\n    reverse_proxy 127.0.0.1:9000\n}}\n",
            ca.display()
        ))
        .unwrap();
        let auth = cfg.sites[0]
            .tls
            .as_ref()
            .expect("tls config")
            .client_auth
            .as_ref()
            .expect("client auth");
        assert_eq!(auth.mode, ClientAuthMode::Require);
        let _ = std::fs::remove_file(&ca);
    }

    #[test]
    fn rejects_missing_static_cert() {
        let err = compile(
            "example.com:8443 {\n    tls /nonexistent/cert.pem /nonexistent/key.pem\n    reverse_proxy 127.0.0.1:9000\n}\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to read certificate"));
    }

    #[test]
    fn rejects_min_above_max_version() {
        let cfg = compile(
            "example.com:8443 {\n    tls min_version 1.3\n    tls max_version 1.2\n    reverse_proxy 127.0.0.1:9000\n}\n",
        )
        .unwrap_err();
        assert!(cfg
            .to_string()
            .contains("min_version must not exceed max_version"));
    }

    #[test]
    fn rejects_tls_inside_handle() {
        let err = compile(
            ":8080 {\n    handle /x/* {\n        tls internal\n        file_server\n    }\n}\n",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("tls is only allowed at the site-block level"));
    }

    #[test]
    fn compiles_basic_auth_into_user_table() {
        // Several `basic_auth` lines compile into a single user-table modifier
        // (spec §5.10).
        let cfg = compile(
            ":8080 {\n    basic_auth admin $2b$12$x\n    basic_auth bob $2b$12$y\n    reverse_proxy 127.0.0.1:9000\n}\n",
        )
        .unwrap();
        let modifiers = &cfg.sites[0].modifiers;
        match modifiers
            .iter()
            .find(|m| matches!(m, Modifier::BasicAuth { .. }))
        {
            Some(Modifier::BasicAuth { users }) => assert_eq!(users.len(), 2),
            _ => panic!("expected a basic_auth modifier"),
        }
    }
}
