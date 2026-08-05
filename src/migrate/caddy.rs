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

//! Caddyfile → Raddyfile converter (common subset, ARCHITECTURE §7).
//!
//! The supported subset is intentionally small: single-domain sites with
//! `reverse_proxy` (inline or `to` block form), `root` + `file_server`,
//! `handle` blocks, `header_up`/`header_down`, `encode`, and `redir`. Anything
//! else is skipped and reported as a warning so no config is silently dropped.
//! Raddy's site address and placeholder semantics differ slightly from Caddy's,
//! so the converter approximates: `http://` becomes an explicit `:80` port,
//! Caddy placeholders are mapped to the Raddy equivalents, and a `redir`
//! without a status code gets Caddy's default `temporary` (302).

use super::{parse_stmts, Converted, Stmt};

/// Convert a Caddyfile to a Raddyfile.
pub fn convert(input: &str) -> Result<Converted, String> {
    let lines: Vec<&str> = input.lines().collect();
    let stmts = parse_stmts(&lines)?;
    let mut conv = CaddyConv::default();
    for stmt in &stmts {
        if stmt.words.is_empty() {
            // A top-level `{ ... }` with no directive is Caddy's global
            // options block.
            conv.warn(stmt.line, "global options block is not supported; skipped");
            continue;
        }
        if stmt.block.is_empty() {
            conv.warn(
                stmt.line,
                format!(
                    "top-level directive '{}' has no block; skipped",
                    stmt.words[0]
                ),
            );
            continue;
        }
        conv.site(stmt);
    }
    Ok(Converted {
        raddyfile: conv.out,
        warnings: conv.warnings,
    })
}

#[derive(Default)]
struct CaddyConv {
    warnings: Vec<String>,
    out: String,
}

impl CaddyConv {
    fn warn(&mut self, line: usize, message: impl Into<String>) {
        let message = message.into();
        self.warnings.push(format!("caddyfile:{line}: {message}"));
    }

    fn site(&mut self, stmt: &Stmt) {
        let addr = match map_site_addr(&stmt.words) {
            Ok(addr) => addr,
            Err(e) => {
                self.warn(stmt.line, e);
                return;
            }
        };
        let mut body = String::new();
        for directive in &stmt.block {
            self.directive(directive, &mut body, 1);
        }
        if body.trim().is_empty() {
            self.warn(
                stmt.line,
                format!("site '{addr}' had nothing convertible; skipped"),
            );
            return;
        }
        self.out.push_str(&format!("{addr} {{\n{body}}}\n"));
    }

    fn directive(&mut self, stmt: &Stmt, out: &mut String, depth: usize) {
        let pad = "    ".repeat(depth);
        let name = stmt.words.first().map(String::as_str).unwrap_or("");
        match name {
            "reverse_proxy" => self.reverse_proxy(stmt, out, depth),
            "root" => {
                let mut args = stmt.words[1..].to_vec();
                // Caddy `root * /path` carries a leading matcher; drop it.
                if args.first().map(String::as_str) == Some("*") {
                    args.remove(0);
                }
                if args.len() != 1 {
                    self.warn(stmt.line, "root expects exactly one path; skipped");
                    return;
                }
                out.push_str(&format!("{pad}root {}\n", args[0]));
            }
            "file_server" => out.push_str(&format!("{pad}file_server\n")),
            "handle" => {
                if stmt.words.len() != 2 {
                    self.warn(stmt.line, "handle expects exactly one path; skipped");
                    return;
                }
                out.push_str(&format!("{pad}handle {} {{\n", stmt.words[1]));
                for inner in &stmt.block {
                    self.directive(inner, out, depth + 1);
                }
                out.push_str(&format!("{pad}}}\n"));
            }
            "header_up" | "header_down" => self.header(stmt, out, &pad, name),
            "encode" => {
                if stmt.words.len() < 2 {
                    self.warn(stmt.line, "encode requires at least one algorithm; skipped");
                    return;
                }
                out.push_str(&format!("{pad}encode {}\n", stmt.words[1..].join(" ")));
            }
            "redir" => self.redir(stmt, out, &pad),
            "tls" => self.warn(
                stmt.line,
                "tls directive skipped; raddy handles certificates via ACME automatically",
            ),
            other => self.warn(
                stmt.line,
                format!("unsupported directive '{other}'; skipped"),
            ),
        }
    }

    fn header(&mut self, stmt: &Stmt, out: &mut String, pad: &str, name: &str) {
        let mut args = stmt.words[1..].to_vec();
        // Caddy allows an inline matcher (`header_up /api/* X-Foo bar`); Raddy
        // header directives have none, so a leading `/`-path is dropped.
        if args.first().map(|w| w.starts_with('/')).unwrap_or(false) {
            self.warn(stmt.line, "inline matcher on header directive dropped");
            args.remove(0);
        }
        if args.len() < 2 {
            self.warn(
                stmt.line,
                format!("{name} requires a name and value; skipped"),
            );
            return;
        }
        let header_name = args[0].clone();
        let value = match map_value(&args[1..].join(" ")) {
            Ok(value) => value,
            Err(e) => {
                self.warn(stmt.line, format!("{name}: {e}; skipped"));
                return;
            }
        };
        out.push_str(&format!("{pad}{name} {header_name} {value}\n"));
    }

    fn redir(&mut self, stmt: &Stmt, out: &mut String, pad: &str) {
        let mut args = stmt.words[1..].to_vec();
        // Drop an inline path matcher (`redir /old /new`).
        if args.len() >= 2
            && args[0].starts_with('/')
            && args[1].starts_with('/')
            && !args[0].contains("://")
        {
            self.warn(stmt.line, "inline matcher on redir dropped");
            args.remove(0);
        }
        if args.is_empty() {
            self.warn(stmt.line, "redir requires a target; skipped");
            return;
        }
        // A trailing status code (keyword or 3xx number) is split off.
        let (target, code) = match args.len() {
            1 => (args[0].clone(), None),
            _ => {
                let last = args.last().cloned().unwrap_or_default();
                if is_redirect_code(&last) {
                    args.pop();
                    (args.join(" "), Some(last))
                } else {
                    (args.join(" "), None)
                }
            }
        };
        if target.is_empty() {
            self.warn(stmt.line, "redir target must not be empty; skipped");
            return;
        }
        let target = match map_value(&target) {
            Ok(target) => target,
            Err(e) => {
                self.warn(stmt.line, format!("redir: {e}; skipped"));
                return;
            }
        };
        // Caddy's default is 302 (temporary); Raddy's is 308, so be explicit.
        out.push_str(&format!(
            "{pad}redir {target} {}\n",
            code.unwrap_or_else(|| "temporary".into())
        ));
    }

    fn reverse_proxy(&mut self, stmt: &Stmt, out: &mut String, depth: usize) {
        let pad = "    ".repeat(depth);
        let mut args = stmt.words[1..].to_vec();
        let matcher = if args.first().map(|w| w.starts_with('/')).unwrap_or(false) {
            Some(args.remove(0))
        } else {
            None
        };
        let matcher_prefix = matcher.map(|m| format!("{m} ")).unwrap_or_default();

        if stmt.block.is_empty() {
            if args.is_empty() {
                self.warn(
                    stmt.line,
                    "reverse_proxy requires an upstream target; skipped",
                );
                return;
            }
            let targets = self.map_upstreams(&args, stmt.line);
            if targets.is_empty() {
                return;
            }
            if targets.len() == 1 {
                out.push_str(&format!(
                    "{pad}reverse_proxy {matcher_prefix}{}\n",
                    targets[0]
                ));
            } else {
                // Raddy's inline form takes one target; several need `to`.
                out.push_str(&format!("{pad}reverse_proxy {matcher_prefix}{{\n"));
                out.push_str(&format!("{pad}    to {}\n", targets.join(" ")));
                out.push_str(&format!("{pad}}}\n"));
            }
            return;
        }

        // Block form: translate `to` and `load_balancing`, warn the rest.
        out.push_str(&format!("{pad}reverse_proxy {matcher_prefix}{{\n"));
        for inner in &stmt.block {
            match inner.words.first().map(String::as_str).unwrap_or("") {
                "to" => {
                    let targets = self.map_upstreams(&inner.words[1..], inner.line);
                    if !targets.is_empty() {
                        out.push_str(&format!("{pad}    to {}\n", targets.join(" ")));
                    }
                }
                "load_balancing" => {
                    if inner.words.get(1).map(String::as_str) == Some("policy") {
                        let policy = inner.words.get(2).map(String::as_str).unwrap_or("");
                        let mapped = match policy {
                            "round_robin" => "round_robin",
                            "random" => "random",
                            "ip_hash" => "ip_hash",
                            _ => {
                                self.warn(
                                    inner.line,
                                    format!("unsupported load_balancing policy '{policy}'"),
                                );
                                "round_robin"
                            }
                        };
                        out.push_str(&format!("{pad}    lb_policy {mapped}\n"));
                    } else {
                        self.warn(
                            inner.line,
                            "only 'load_balancing policy' is supported; skipped",
                        );
                    }
                }
                other => self.warn(
                    inner.line,
                    format!("unsupported reverse_proxy option '{other}'; skipped"),
                ),
            }
        }
        out.push_str(&format!("{pad}}}\n"));
    }

    /// Normalize upstream targets: drop the scheme, default a missing port to
    /// 80, and reject unsupported forms (IPv6). Returns the valid ones.
    fn map_upstreams(&mut self, args: &[String], line: usize) -> Vec<String> {
        let mut targets = Vec::new();
        for arg in args {
            match normalize_upstream(arg) {
                Ok(target) => targets.push(target),
                Err(e) => self.warn(line, e),
            }
        }
        targets
    }
}

/// Map a Caddy site address to a Raddy site key.
fn map_site_addr(words: &[String]) -> Result<String, String> {
    let joined = words.join("");
    let first = joined.split(',').next().unwrap_or("").trim();
    if first.is_empty() {
        return Err("site block has no address".to_string());
    }
    let (scheme, rest) = match first.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, first),
    };
    match scheme {
        // `http://` implies plain HTTP on 80; Raddy named sites default to 443.
        Some("http") if !rest.contains(':') => Ok(format!("{rest}:80")),
        Some("https") if !rest.contains(':') => Ok(rest.to_string()),
        Some(other) => Err(format!("unsupported site scheme '{other}'")),
        None => Ok(rest.to_string()),
    }
}

/// Normalize a reverse-proxy upstream: strip the scheme and any URI path,
/// default the port. A trailing path (unusual for Caddy) is dropped like nginx.
fn normalize_upstream(target: &str) -> Result<String, String> {
    if target.starts_with('[') {
        return Err(format!("IPv6 upstream '{target}' is not supported"));
    }
    let rest = match target.split_once("://") {
        Some(("http", rest)) => rest,
        // Raddy v0.1.2 has plain-HTTP upstreams only; skipping https upstreams
        // rather than silently downgrading them.
        Some(("https", _)) => {
            return Err(format!(
                "https upstream '{target}' skipped (raddy supports plain-HTTP upstreams only)"
            ))
        }
        Some((other, _)) => return Err(format!("unsupported upstream scheme '{other}'")),
        None => target,
    };
    let host_port = rest.split('/').next().unwrap_or("").to_string();
    if host_port.is_empty() {
        return Err("empty upstream target".to_string());
    }
    if host_port.contains(':') {
        Ok(host_port)
    } else {
        Ok(format!("{host_port}:80"))
    }
}

/// Map Caddy `{...}` placeholders to their Raddy equivalents.
fn map_value(raw: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find('}')
            .map(|e| start + e + 1)
            .ok_or_else(|| format!("unclosed '{{' in '{raw}'"))?;
        let placeholder = &rest[start + 1..end - 1];
        let mapped = match placeholder {
            "host" => "{host}",
            "uri" => "{uri}",
            "remote_host" => "{remote_host}",
            "http.request.remote" => "{remote_host}",
            "http.request.host" => "{host}",
            "http.request.uri" => "{uri}",
            other => return Err(format!("unsupported placeholder '{{{other}}}'")),
        };
        out.push_str(mapped);
        rest = &rest[end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Whether a token is a `redir` status code (keyword or 3xx number).
fn is_redirect_code(token: &str) -> bool {
    matches!(token, "permanent" | "temporary") || {
        token
            .parse::<u16>()
            .map(|code| (300..=399).contains(&code))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_simple_reverse_proxy() {
        let c = convert("example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n").unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n"
        );
        assert!(
            c.warnings.is_empty(),
            "unexpected warnings: {:?}",
            c.warnings
        );
    }

    #[test]
    fn http_scheme_becomes_explicit_port_80() {
        let c = convert("http://example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n").unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com:80 {\n    reverse_proxy 127.0.0.1:8080\n}\n"
        );
    }

    #[test]
    fn redir_without_code_defaults_to_temporary() {
        let c = convert(":80 {\n    redir https://{host}{uri}\n}\n").unwrap();
        assert_eq!(
            c.raddyfile,
            ":80 {\n    redir https://{host}{uri} temporary\n}\n"
        );
    }

    #[test]
    fn multiple_targets_use_to_block() {
        let c =
            convert("example.com {\n    reverse_proxy 127.0.0.1:8080 127.0.0.1:8081\n}\n").unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com {\n    reverse_proxy {\n        to 127.0.0.1:8080 127.0.0.1:8081\n    }\n}\n"
        );
    }

    #[test]
    fn handle_and_inline_matcher_map_directly() {
        let input = "example.com {\n    handle /static/* {\n        root /var/www/static\n        file_server\n    }\n    reverse_proxy /api/* 127.0.0.1:9000\n}\n";
        let c = convert(input).unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com {\n    handle /static/* {\n        root /var/www/static\n        file_server\n    }\n    reverse_proxy /api/* 127.0.0.1:9000\n}\n"
        );
    }

    #[test]
    fn root_star_matcher_is_dropped() {
        let c = convert("example.com {\n    root * /var/www/html\n    file_server\n}\n").unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com {\n    root /var/www/html\n    file_server\n}\n"
        );
    }

    #[test]
    fn caddy_placeholders_map_to_raddy() {
        let input = "example.com {\n    header_up X-Real-IP {http.request.remote}\n    reverse_proxy 127.0.0.1:8080\n}\n";
        let c = convert(input).unwrap();
        assert!(c.raddyfile.contains("header_up X-Real-IP {remote_host}"));
        assert!(c.warnings.is_empty());
    }

    #[test]
    fn tls_directive_warns_and_is_skipped() {
        let c = convert("example.com {\n    tls internal\n    reverse_proxy 127.0.0.1:8080\n}\n")
            .unwrap();
        assert!(c.raddyfile.contains("reverse_proxy"));
        assert!(!c.raddyfile.contains("tls"));
        assert!(c
            .warnings
            .iter()
            .any(|w| w.contains("tls directive skipped")));
    }

    #[test]
    fn unsupported_directive_warns_and_is_skipped() {
        let c =
            convert("example.com {\n    reverse_proxy 127.0.0.1:8080\n    websocket\n}\n").unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n"
        );
        assert!(c
            .warnings
            .iter()
            .any(|w| w.contains("unsupported directive 'websocket'")));
    }

    #[test]
    fn global_options_block_warns() {
        let input =
            "{\n    email ops@example.com\n}\nexample.com {\n    reverse_proxy 127.0.0.1:8080\n}\n";
        let c = convert(input).unwrap();
        assert!(c.raddyfile.contains("example.com"));
        assert!(c
            .warnings
            .iter()
            .any(|w| w.contains("global options block")));
    }

    #[test]
    fn site_with_nothing_convertible_is_skipped() {
        let c = convert("example.com {\n    websocket /path\n}\n").unwrap();
        assert!(c.raddyfile.trim().is_empty());
        assert!(!c.warnings.is_empty());
    }

    #[test]
    fn https_upstream_is_skipped_with_warning() {
        let c = convert("example.com {\n    reverse_proxy https://127.0.0.1:8443\n}\n").unwrap();
        assert!(c.raddyfile.trim().is_empty());
        assert!(c.warnings.iter().any(|w| w.contains("https upstream")));
    }

    #[test]
    fn unsupported_placeholder_warns() {
        let c = convert("example.com {\n    header_up X {http.request.foo}\n    reverse_proxy 127.0.0.1:8080\n}\n").unwrap();
        assert!(c
            .warnings
            .iter()
            .any(|w| w.contains("unsupported placeholder")));
    }
}
