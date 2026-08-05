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

//! nginx.conf → Raddyfile converter (common subset, ARCHITECTURE §7).
//!
//! Supported subset: `server` blocks with `listen` / `server_name`, `root`,
//! `location` blocks containing `proxy_pass` (plain HTTP) or `root`, server
//! `proxy_pass`, `add_header` → `header_down`, and `return <3xx> <url>` →
//! `redir`. nginx's static-serving mechanics (`try_files`, `index`) are
//! approximated by Raddy's `file_server`; anything else is skipped with a
//! warning. nginx variables (`$host`, `$request_uri`, …) are mapped to the
//! Raddy placeholders.

use super::{parse_stmts, Converted, Stmt};

/// Convert an nginx.conf to a Raddyfile.
pub fn convert(input: &str) -> Result<Converted, String> {
    let lines: Vec<&str> = input.lines().collect();
    let stmts = parse_stmts(&lines)?;
    let mut conv = NginxConv::default();
    for stmt in &stmts {
        match stmt.words.first().map(String::as_str) {
            Some("server") => conv.server(stmt),
            Some(other) => conv.warn(
                stmt.line,
                format!("top-level '{other}' is not a server block; skipped"),
            ),
            None => conv.warn(stmt.line, "empty block outside a server; skipped"),
        }
    }
    Ok(Converted {
        raddyfile: conv.out,
        warnings: conv.warnings,
    })
}

#[derive(Default)]
struct NginxConv {
    warnings: Vec<String>,
    out: String,
}

impl NginxConv {
    fn warn(&mut self, line: usize, message: impl Into<String>) {
        let message = message.into();
        self.warnings.push(format!("nginx:{line}: {message}"));
    }

    fn server(&mut self, stmt: &Stmt) {
        let mut port = 80u16;
        let mut ssl = false;
        let mut server_name: Option<String> = None;
        let mut root: Option<String> = None;
        let mut locations: Vec<&Stmt> = Vec::new();
        let mut server_proxy: Option<String> = None;
        // Site-level transforms collected while scanning, emitted up front.
        let mut out_headers: Vec<(String, String)> = Vec::new();
        let mut out_redirs: Vec<String> = Vec::new();

        for directive in &stmt.block {
            let name = directive.words.first().map(String::as_str).unwrap_or("");
            match name {
                "listen" => match parse_listen(&directive.words) {
                    Ok((listen_port, is_ssl)) => {
                        port = listen_port;
                        ssl = is_ssl;
                    }
                    Err(e) => self.warn(directive.line, e),
                },
                "server_name" => {
                    let names = &directive.words[1..];
                    if names.len() > 1 {
                        self.warn(directive.line, "multiple server_name; using the first");
                    }
                    match names.first() {
                        Some(name) => {
                            server_name = self.normalize_server_name(name, directive.line)
                        }
                        None => self.warn(directive.line, "server_name with no value; skipped"),
                    }
                }
                "root" => {
                    if directive.words.len() == 2 {
                        root = Some(directive.words[1].clone());
                    } else {
                        self.warn(directive.line, "root expects exactly one path; skipped");
                    }
                }
                "location" => locations.push(directive),
                "proxy_pass" => {
                    if let Some(target) = self.map_proxy_pass(&directive.words, directive.line) {
                        server_proxy = Some(target);
                    }
                }
                "add_header" => {
                    if directive.words.len() >= 3 {
                        match map_nginx_value(&directive.words[2..].join(" ")) {
                            Ok(value) => out_headers.push((directive.words[1].clone(), value)),
                            Err(e) => {
                                self.warn(directive.line, format!("add_header: {e}; skipped"))
                            }
                        }
                    } else {
                        self.warn(
                            directive.line,
                            "add_header requires a name and value; skipped",
                        );
                    }
                }
                "return" => {
                    let words = &directive.words;
                    if words.len() == 3 && is_3xx(words[1].as_str()) {
                        match map_nginx_value(&words[2]) {
                            Ok(url) => out_redirs.push(format!("redir {url} {}", words[1])),
                            Err(e) => self.warn(directive.line, format!("return: {e}; skipped")),
                        }
                    } else {
                        self.warn(
                            directive.line,
                            "only 'return <3xx> <url>' is supported; skipped",
                        );
                    }
                }
                other => self.warn(
                    directive.line,
                    format!("unsupported directive '{other}'; skipped"),
                ),
            }
        }

        if ssl && port != 443 {
            self.warn(
                stmt.line,
                format!("ssl on non-443 port {port} is not supported; serving plain HTTP"),
            );
        }

        let addr = match server_name {
            Some(name) => format!("{name}:{port}"),
            None => format!(":{port}"),
        };
        let mut body = String::new();
        // Site-level transforms (always emitted before the terminals).
        for (name, value) in &out_headers {
            body.push_str(&format!("    header_down {name} {value}\n"));
        }
        for redir in &out_redirs {
            body.push_str(&format!("    {redir}\n"));
        }

        // Static default: a server-level root serves paths no location claims.
        // A server-level proxy_pass (invalid nginx, handled defensively) also
        // claims the default path.
        let mut default_handled = server_proxy.is_some();

        if let Some(root) = &root {
            body.push_str(&format!("    root {root}\n"));
        }
        for location in &locations {
            let Some(path) = self.location_path(location) else {
                continue;
            };
            // Per-location: at most one terminal intent (proxy, redirect, or
            // static), plus a location-scoped root.
            let mut proxy: Option<String> = None;
            let mut redir: Option<(String, String)> = None;
            let mut loc_root: Option<String> = None;
            for directive in &location.block {
                match directive.words.first().map(String::as_str).unwrap_or("") {
                    "proxy_pass" => proxy = self.map_proxy_pass(&directive.words, directive.line),
                    "root" => {
                        if directive.words.len() == 2 {
                            loc_root = Some(directive.words[1].clone());
                        } else {
                            self.warn(directive.line, "root expects exactly one path; skipped");
                        }
                    }
                    "return" => {
                        let words = &directive.words;
                        if words.len() == 3 && is_3xx(words[1].as_str()) {
                            match map_nginx_value(&words[2]) {
                                Ok(url) => redir = Some((url, words[1].clone())),
                                Err(e) => {
                                    self.warn(directive.line, format!("return: {e}; skipped"))
                                }
                            }
                        } else {
                            self.warn(
                                directive.line,
                                "only 'return <3xx> <url>' is supported; skipped",
                            );
                        }
                    }
                    other => self.warn(
                        directive.line,
                        format!("unsupported location directive '{other}'; skipped"),
                    ),
                }
            }
            if path == "/" {
                if let Some(target) = proxy {
                    body.push_str(&format!("    reverse_proxy {target}\n"));
                    default_handled = true;
                } else if let Some((url, code)) = redir {
                    body.push_str(&format!("    redir {url} {code}\n"));
                    default_handled = true;
                } else if loc_root.is_some() || root.is_some() {
                    // A location-scoped root must be emitted for the file_server
                    // to have one (a duplicate of the server root is harmless —
                    // the last `root` line wins).
                    if let Some(loc_root) = &loc_root {
                        body.push_str(&format!("    root {loc_root}\n"));
                    }
                    body.push_str("    file_server\n");
                    default_handled = true;
                } else {
                    self.warn(
                        location.line,
                        "location '/' has nothing convertible; skipped",
                    );
                }
            } else if let Some(target) = proxy {
                body.push_str(&format!(
                    "    handle {path} {{\n        reverse_proxy {target}\n    }}\n"
                ));
            } else if let Some((url, code)) = redir {
                body.push_str(&format!(
                    "    handle {path} {{\n        redir {url} {code}\n    }}\n"
                ));
            } else {
                match loc_root.clone().or_else(|| root.clone()) {
                    Some(dir) => body.push_str(&format!(
                        "    handle {path} {{\n        root {dir}\n        file_server\n    }}\n"
                    )),
                    None => self.warn(
                        location.line,
                        format!("location '{path}' has neither proxy_pass nor root; skipped"),
                    ),
                }
            }
        }
        if root.is_some() && !default_handled {
            body.push_str("    file_server\n");
        }
        // A server-level `proxy_pass` (unusual in nginx) is the catch-all
        // fallback, so it is emitted last — after the location handles.
        if let Some(target) = &server_proxy {
            body.push_str(&format!("    reverse_proxy {target}\n"));
        }

        if body.trim().is_empty() {
            self.warn(
                stmt.line,
                format!("server '{addr}' had nothing convertible; skipped"),
            );
            return;
        }
        self.out.push_str(&format!("{addr} {{\n{body}}}\n"));
    }

    /// Map a `location` header line to a Raddy path (or warn and skip it).
    fn location_path(&mut self, location: &Stmt) -> Option<String> {
        match location.words.get(1).map(String::as_str) {
            Some(path) if path.starts_with('/') => Some(path.to_string()),
            Some("=") => {
                self.warn(
                    location.line,
                    "exact-match location '=' is not supported; skipped",
                );
                None
            }
            Some("~") | Some("~*") | Some("^~") => {
                self.warn(
                    location.line,
                    "regex/preferential location is not supported; skipped",
                );
                None
            }
            _ => {
                self.warn(location.line, "location requires a path; skipped");
                None
            }
        }
    }

    /// Normalize a `server_name`: strip a wildcard, and treat `_` (nginx's
    /// catch-all placeholder) as "no name" → a `:port` catch-all site.
    fn normalize_server_name(&mut self, name: &str, line: usize) -> Option<String> {
        if name == "_" {
            self.warn(
                line,
                "server_name '_' is a catch-all; using a :port catch-all site",
            );
            return None;
        }
        if let Some(rest) = name.strip_prefix("*.") {
            self.warn(
                line,
                format!("wildcard server_name '{name}' approximated to '{rest}'"),
            );
            Some(rest.to_string())
        } else {
            Some(name.to_string())
        }
    }

    /// Map a `proxy_pass` value to a Raddy upstream (plain HTTP host:port).
    fn map_proxy_pass(&mut self, words: &[String], line: usize) -> Option<String> {
        let url = words.get(1)?;
        let rest = match url.split_once("://") {
            Some(("http", rest)) => rest,
            Some(("https", _)) => {
                self.warn(
                    line,
                    format!("https proxy_pass '{url}' skipped (raddy supports plain-HTTP upstreams only)"),
                );
                return None;
            }
            Some((other, _)) => {
                self.warn(line, format!("unsupported proxy_pass scheme '{other}'"));
                return None;
            }
            None => url.as_str(),
        };
        let host_port = match rest.split_once('/') {
            Some((host_port, path)) if !path.is_empty() => {
                self.warn(line, format!("proxy_pass URI path '/{path}' dropped"));
                host_port
            }
            Some((host_port, _)) => host_port, // trailing slash only
            None => rest,
        };
        if host_port.is_empty() {
            self.warn(line, "empty proxy_pass target");
            return None;
        }
        if host_port.contains(':') {
            Some(host_port.to_string())
        } else {
            Some(format!("{host_port}:80"))
        }
    }
}

/// Parse a `listen` line: `<port>[ ssl][ …]`.
fn parse_listen(words: &[String]) -> Result<(u16, bool), String> {
    let spec = words
        .get(1)
        .ok_or_else(|| "listen requires a port".to_string())?;
    let port = spec
        .rsplit(':')
        .next()
        .unwrap_or("")
        .parse::<u16>()
        .map_err(|_| format!("invalid listen address '{spec}'"))?;
    let ssl = words[1..].iter().any(|w| w == "ssl");
    Ok((port, ssl))
}

/// Whether a token is a 3xx status code.
fn is_3xx(token: &str) -> bool {
    token
        .parse::<u16>()
        .map(|code| (300..=399).contains(&code))
        .unwrap_or(false)
}

/// Map nginx `$variables` and `{placeholders}` to Raddy value placeholders.
fn map_nginx_value(raw: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let name_end = rest[start + 1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map(|e| start + 1 + e)
            .unwrap_or(rest.len());
        let variable = &rest[start + 1..name_end];
        let mapped = match variable {
            "host" => "{host}",
            "uri" | "request_uri" => "{uri}",
            "remote_addr" => "{remote_host}",
            other => return Err(format!("unsupported nginx variable '${other}'")),
        };
        out.push_str(mapped);
        rest = &rest[name_end..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_slash_proxy_pass() {
        let input = "server {\n    listen 80;\n    server_name example.com;\n    location / {\n        proxy_pass http://127.0.0.1:8080;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com:80 {\n    reverse_proxy 127.0.0.1:8080\n}\n"
        );
        assert!(
            c.warnings.is_empty(),
            "unexpected warnings: {:?}",
            c.warnings
        );
    }

    #[test]
    fn static_server_serves_file_server() {
        let input =
            "server {\n    listen 80;\n    server_name example.com;\n    root /var/www/html;\n}\n";
        let c = convert(input).unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com:80 {\n    root /var/www/html\n    file_server\n}\n"
        );
    }

    #[test]
    fn location_slash_with_only_location_root_emits_root() {
        // A `location /` root with no server-level root must still emit the
        // `root` line the `file_server` requires.
        let input = "server {\n    listen 80;\n    server_name example.com;\n    location / {\n        root /var/www/html;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com:80 {\n    root /var/www/html\n    file_server\n}\n"
        );
    }

    #[test]
    fn location_prefix_proxy_plus_static_default() {
        let input = "server {\n    listen 443 ssl;\n    server_name example.com;\n    root /var/www/html;\n    location /api/ {\n        proxy_pass http://127.0.0.1:8080;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com:443 {\n    root /var/www/html\n    handle /api/ {\n        reverse_proxy 127.0.0.1:8080\n    }\n    file_server\n}\n"
        );
    }

    #[test]
    fn return_redirect_maps_variables() {
        let input = "server {\n    listen 80;\n    server_name example.com;\n    location / {\n        return 301 https://$host$request_uri;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com:80 {\n    redir https://{host}{uri} 301\n}\n"
        );
    }

    #[test]
    fn add_header_maps_to_header_down() {
        let input = "server {\n    listen 80;\n    server_name example.com;\n    add_header X-A raddy;\n    location / {\n        proxy_pass http://127.0.0.1:8080;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert_eq!(
            c.raddyfile,
            "example.com:80 {\n    header_down X-A raddy\n    reverse_proxy 127.0.0.1:8080\n}\n"
        );
    }

    #[test]
    fn catch_all_without_server_name() {
        let input = "server {\n    listen 8080;\n    location / {\n        proxy_pass http://127.0.0.1:9000;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert_eq!(
            c.raddyfile,
            ":8080 {\n    reverse_proxy 127.0.0.1:9000\n}\n"
        );
    }

    #[test]
    fn multiple_server_blocks_become_multiple_sites() {
        let input = "server {\n    listen 80;\n    server_name a.test;\n    location / {\n        proxy_pass http://127.0.0.1:9001;\n    }\n}\nserver {\n    listen 80;\n    server_name b.test;\n    location / {\n        proxy_pass http://127.0.0.1:9002;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert_eq!(
            c.raddyfile,
            "a.test:80 {\n    reverse_proxy 127.0.0.1:9001\n}\nb.test:80 {\n    reverse_proxy 127.0.0.1:9002\n}\n"
        );
    }

    #[test]
    fn wildcard_server_name_is_approximated() {
        let input = "server {\n    listen 80;\n    server_name *.example.com;\n    location / {\n        proxy_pass http://127.0.0.1:8080;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert!(c.raddyfile.starts_with("example.com:80 {"));
        assert!(c
            .warnings
            .iter()
            .any(|w| w.contains("wildcard server_name")));
    }

    #[test]
    fn https_upstream_is_skipped_with_warning() {
        let input = "server {\n    listen 80;\n    server_name example.com;\n    location / {\n        proxy_pass https://127.0.0.1:8443;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert!(c.raddyfile.trim().is_empty());
        assert!(c.warnings.iter().any(|w| w.contains("https proxy_pass")));
    }

    #[test]
    fn unsupported_directives_warn() {
        let input = "server {\n    listen 80;\n    server_name example.com;\n    client_max_body_size 10m;\n    location / {\n        proxy_pass http://127.0.0.1:8080;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert!(c.raddyfile.contains("reverse_proxy"));
        assert!(c
            .warnings
            .iter()
            .any(|w| w.contains("unsupported directive 'client_max_body_size'")));
    }

    #[test]
    fn exact_match_location_is_skipped() {
        let input = "server {\n    listen 80;\n    server_name example.com;\n    location = /health {\n        proxy_pass http://127.0.0.1:8080;\n    }\n}\n";
        let c = convert(input).unwrap();
        assert!(c.raddyfile.trim().is_empty());
        assert!(c
            .warnings
            .iter()
            .any(|w| w.contains("exact-match location")));
    }
}
