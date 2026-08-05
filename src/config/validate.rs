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
use std::collections::HashSet;
use std::net::{SocketAddr, ToSocketAddrs};

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

    for directive in &site.directives {
        match directive {
            Directive::ReverseProxy {
                matcher,
                to,
                lb_policy,
                health_check,
            } => {
                let upstreams = resolve_upstreams(file, to)?;
                let matchers = matcher.iter().cloned().collect();
                terminals.push(Terminal {
                    matchers,
                    kind: TerminalKind::ReverseProxy {
                        upstreams,
                        lb_policy: *lb_policy,
                        health_check: *health_check,
                    },
                    modifiers: Vec::new(),
                });
            }
            Directive::Handle { path, directives } => {
                compile_handle_block(file, path, directives, &mut terminals)?
            }
            Directive::FileServer => terminals.push(Terminal {
                matchers: Vec::new(),
                kind: TerminalKind::FileServer {
                    root: String::new(),
                },
                modifiers: Vec::new(),
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
                });
            }
        }
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
    })
}

/// Compile the directives inside a `handle` block.
///
/// Every terminal inherits the handle's path matcher and the block's own
/// modifiers. The returned terminals already carry their handle-scoped
/// modifiers; block-level modifiers are appended later in [`compile_site`].
fn compile_handle_block(
    file: &str,
    path: &str,
    directives: &[Directive],
    out: &mut Vec<Terminal>,
) -> Result<(), ConfigError> {
    let path_matcher = PathMatcher {
        prefix: crate::config::parser::strip_matcher_wildcard(path).to_string(),
    };
    let mut scoped_modifiers: Vec<Modifier> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    let mut block_terminals: Vec<Terminal> = Vec::new();

    for directive in directives {
        match directive {
            Directive::ReverseProxy {
                matcher,
                to,
                lb_policy,
                health_check,
            } => {
                let upstreams = resolve_upstreams(file, to)?;
                let mut matchers = vec![path_matcher.clone()];
                if let Some(m) = matcher {
                    matchers.push(m.clone());
                }
                block_terminals.push(Terminal {
                    matchers,
                    kind: TerminalKind::ReverseProxy {
                        upstreams,
                        lb_policy: *lb_policy,
                        health_check: *health_check,
                    },
                    modifiers: Vec::new(),
                });
            }
            Directive::FileServer => block_terminals.push(Terminal {
                matchers: vec![path_matcher.clone()],
                kind: TerminalKind::FileServer {
                    root: String::new(),
                },
                modifiers: Vec::new(),
            }),
            Directive::Redir { to, code } => {
                let to =
                    ValueTemplate::parse(to).map_err(|message| validate_error(file, message))?;
                block_terminals.push(Terminal {
                    matchers: vec![path_matcher.clone()],
                    kind: TerminalKind::Redir { to, code: *code },
                    modifiers: Vec::new(),
                });
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
            Directive::Handle { .. } => {
                return Err(validate_error(
                    file,
                    "nested handle blocks are not supported in v0.1",
                ));
            }
        }
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

/// Resolve every upstream's host to a concrete address at build time, so the
/// snapshot stays pure data and the request plane performs no DNS (ADR-011).
fn resolve_upstreams(file: &str, to: &[Upstream]) -> Result<Vec<SocketAddr>, ConfigError> {
    let mut resolved = Vec::with_capacity(to.len());
    for upstream in to {
        let addr = (upstream.host.as_str(), upstream.port)
            .to_socket_addrs()
            .map_err(|e| {
                validate_error(
                    file,
                    format!(
                        "failed to resolve upstream {}:{}: {e}",
                        upstream.host, upstream.port
                    ),
                )
            })?
            .next()
            .ok_or_else(|| {
                validate_error(
                    file,
                    format!(
                        "no address resolved for upstream {}:{}",
                        upstream.host, upstream.port
                    ),
                )
            })?;
        resolved.push(addr);
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
                assert_eq!(upstreams[0].port(), 8080);
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
            TerminalKind::ReverseProxy { upstreams, .. } => assert_eq!(upstreams.len(), 1),
            other => panic!("expected reverse proxy, got {other:?}"),
        }
    }
}
