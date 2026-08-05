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

//! Raddyfile parser (minimal M2 subset), strictly per `RADDYFILE_SPEC`.
//!
//! Line-oriented grammar: one directive per line; a directive that opens a
//! block ends its line with `{`, and the block closes with `}` on its own
//! line. Unsupported syntax must be documented in `RADDYFILE_SPEC.md` before
//! being implemented here (CONTRIBUTING red line).

use crate::config::ast::*;
use crate::config::lexer::{lex, Token, TokenKind};

/// Parse a Raddyfile into its source AST.
pub fn parse(file: &str, input: &str) -> Result<Raddyfile, ConfigError> {
    let tokens = lex(input).map_err(|message| ConfigError::Parse {
        file: file.to_string(),
        line: 1,
        col: 1,
        message,
    })?;
    Parser {
        file,
        tokens,
        pos: 0,
        stmt_pos: (1, 1),
    }
    .parse_raddyfile()
}

/// The default port for a named site without an explicit port (M4 binds TLS).
const NAMED_SITE_DEFAULT_PORT: u16 = 443;
/// The default redirect status code (spec §5).
const REDIR_DEFAULT_CODE: u16 = 308;

/// Parse a duration like `5s`, `2m`, `500ms`, `1h`, or a bare number of
/// seconds (spec §5.1). The value is the second token on the line.
fn parse_duration(line: &[String]) -> Result<std::time::Duration, String> {
    if line.len() != 2 {
        return Err(format!("'{}' requires exactly one duration value", line[0]));
    }
    let s = &line[1];
    let (num, factor_nanos) = if let Some(v) = s.strip_suffix("ms") {
        (v, 1_000_000u64)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1_000_000_000u64)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 60 * 1_000_000_000u64)
    } else if let Some(v) = s.strip_suffix('h') {
        (v, 3600 * 1_000_000_000u64)
    } else {
        (s.as_str(), 1_000_000_000u64)
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid duration '{s}'"))?;
    let nanos = n
        .checked_mul(factor_nanos)
        .ok_or_else(|| format!("duration '{s}' is too large"))?;
    Ok(std::time::Duration::from_nanos(nanos))
}

/// Parse a positive integer health-check counter (the second token).
fn parse_count(line: &[String], name: &str) -> Result<usize, String> {
    if line.len() != 2 {
        return Err(format!("'{name}' requires exactly one integer value"));
    }
    let v = &line[1];
    v.parse::<usize>()
        .map_err(|_| format!("invalid {name} value '{v}'"))
}

struct Parser<'a> {
    file: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    /// The start position of the statement being parsed, so errors point at
    /// the offending directive rather than the block terminator.
    stmt_pos: (u32, u32),
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    /// The position of the current token, or of the last token at end of input.
    fn pos(&self) -> (u32, u32) {
        match self.tokens.get(self.pos) {
            Some(token) => (token.line, token.col),
            None => self
                .tokens
                .last()
                .map(|token| (token.line, token.col))
                .unwrap_or((1, 1)),
        }
    }

    fn err(&self, message: impl Into<String>) -> ConfigError {
        let (line, col) = self.stmt_pos;
        ConfigError::Parse {
            file: self.file.to_string(),
            line,
            col,
            message: message.into(),
        }
    }

    /// Read one statement: a line of words, ending at a newline (consumed), an
    /// opening `{` (consumed, sets `block_open`), a closing `}` (not consumed),
    /// or end of input. Leading blank lines are skipped.
    fn parse_statement(&mut self) -> Result<(Vec<String>, bool), ConfigError> {
        while matches!(
            self.peek(),
            Some(Token {
                kind: TokenKind::Newline,
                ..
            })
        ) {
            self.pos += 1;
        }
        self.stmt_pos = self.pos();
        let mut words = Vec::new();
        let mut block_open = false;
        loop {
            match self.peek() {
                Some(Token {
                    kind: TokenKind::Word(w),
                    ..
                }) => {
                    words.push(w.clone());
                    self.pos += 1;
                }
                Some(Token {
                    kind: TokenKind::LBrace,
                    ..
                }) => {
                    block_open = true;
                    self.pos += 1;
                    break;
                }
                Some(Token {
                    kind: TokenKind::Newline,
                    ..
                }) => {
                    self.pos += 1;
                    break;
                }
                Some(Token {
                    kind: TokenKind::RBrace,
                    ..
                }) => break,
                None => break,
            }
        }
        Ok((words, block_open))
    }

    fn parse_raddyfile(&mut self) -> Result<Raddyfile, ConfigError> {
        let mut global = GlobalConfig::default();

        // Skip leading newlines (comment-only lines lex to newlines) before
        // detecting the global block.
        while matches!(
            self.peek(),
            Some(Token {
                kind: TokenKind::Newline,
                ..
            })
        ) {
            self.pos += 1;
        }

        // A leading bare `{` is the global block (spec §3).
        if matches!(
            self.peek(),
            Some(Token {
                kind: TokenKind::LBrace,
                ..
            })
        ) {
            self.pos += 1;
            loop {
                if matches!(
                    self.peek(),
                    Some(Token {
                        kind: TokenKind::RBrace,
                        ..
                    })
                ) {
                    self.pos += 1;
                    break;
                }
                if self.peek().is_none() {
                    return Err(self.err("unexpected end of file in global block"));
                }
                let (words, block_open) = self.parse_statement()?;
                if words.is_empty() {
                    continue;
                }
                if block_open {
                    return Err(self.err("unexpected '{' in global block"));
                }
                self.apply_global(&mut global, &words)?;
            }
        }

        let mut sites = Vec::new();
        loop {
            while matches!(
                self.peek(),
                Some(Token {
                    kind: TokenKind::Newline,
                    ..
                })
            ) {
                self.pos += 1;
            }
            if self.peek().is_none() {
                break;
            }
            if matches!(
                self.peek(),
                Some(Token {
                    kind: TokenKind::RBrace,
                    ..
                })
            ) {
                return Err(self.err("unexpected '}'"));
            }
            sites.push(self.parse_site()?);
        }
        Ok(Raddyfile { global, sites })
    }

    fn parse_site(&mut self) -> Result<Site, ConfigError> {
        let (words, block_open) = self.parse_statement()?;
        if words.len() != 1 {
            return Err(self.err("expected a site address such as ':8080' or 'example.com'"));
        }
        if !block_open {
            return Err(self.err("expected '{' after site address"));
        }
        let key = self.parse_site_key(&words[0])?;
        let directives = self.parse_directive_block()?;
        Ok(Site { key, directives })
    }

    fn parse_site_key(&self, addr: &str) -> Result<SiteKey, ConfigError> {
        if addr.starts_with('[') {
            return Err(self.err("IPv6 listener addresses are not supported in v0.1"));
        }
        if let Some(port_str) = addr.strip_prefix(':') {
            let port = self.parse_port(port_str)?;
            Ok(SiteKey::CatchAll { port })
        } else {
            match addr.rsplit_once(':') {
                Some((host, port_str)) => {
                    if host.is_empty() {
                        return Err(self.err("empty host in site address"));
                    }
                    let port = self.parse_port(port_str)?;
                    Ok(SiteKey::Named {
                        host: normalize_host_name(host).map_err(|m| self.err(m))?,
                        port,
                    })
                }
                None => Ok(SiteKey::Named {
                    host: normalize_host_name(addr).map_err(|m| self.err(m))?,
                    port: NAMED_SITE_DEFAULT_PORT,
                }),
            }
        }
    }

    fn parse_port(&self, s: &str) -> Result<u16, ConfigError> {
        let port: u16 = s
            .parse()
            .map_err(|_| self.err(format!("invalid port '{s}'")))?;
        if port == 0 {
            return Err(self.err("port must be in 1..=65535"));
        }
        Ok(port)
    }

    fn parse_directive_block(&mut self) -> Result<Vec<Directive>, ConfigError> {
        let mut directives = Vec::new();
        loop {
            match self.peek() {
                Some(Token {
                    kind: TokenKind::RBrace,
                    ..
                }) => {
                    self.pos += 1;
                    break;
                }
                Some(Token {
                    kind: TokenKind::LBrace,
                    ..
                }) => {
                    return Err(self.err("unexpected '{'"));
                }
                None => return Err(self.err("unexpected end of file inside block")),
                _ => {}
            }
            let (words, block_open) = self.parse_statement()?;
            if words.is_empty() {
                continue;
            }
            directives.push(self.dispatch(&words, block_open)?);
        }
        Ok(directives)
    }

    fn dispatch(&mut self, words: &[String], block_open: bool) -> Result<Directive, ConfigError> {
        let name = words.first().expect("dispatch called with empty words");
        match name.as_str() {
            "reverse_proxy" => self.parse_reverse_proxy(words, block_open),
            "handle" => self.parse_handle(words, block_open),
            "header_up" => self.parse_header(words, block_open, true),
            "header_down" => self.parse_header(words, block_open, false),
            "file_server" => self.parse_file_server(words, block_open),
            "root" => self.parse_root(words, block_open),
            "encode" => self.parse_encode(words, block_open),
            "redir" => self.parse_redir(words, block_open),
            other => Err(self.err(format!("unknown directive '{other}'"))),
        }
    }

    fn parse_reverse_proxy(
        &mut self,
        words: &[String],
        block_open: bool,
    ) -> Result<Directive, ConfigError> {
        let mut rest = &words[1..];
        let mut matcher = None;
        if let Some(first) = rest.first() {
            if first.starts_with('/') {
                matcher = Some(self.parse_path_matcher(first)?);
                rest = &rest[1..];
            }
        }

        let mut targets: Vec<String> = Vec::new();
        let mut lb_policy = None;
        let mut health_check = None;
        if block_open {
            // Block form: `{ to <upstream>... [lb_policy <p>] [health_check { ... }] }`.
            loop {
                match self.peek() {
                    Some(Token {
                        kind: TokenKind::RBrace,
                        ..
                    }) => {
                        self.pos += 1;
                        break;
                    }
                    None => return Err(self.err("unexpected end of file in reverse_proxy block")),
                    _ => {}
                }
                let (line, nested) = self.parse_statement()?;
                if line.is_empty() {
                    continue;
                }
                let name = line[0].clone();
                match name.as_str() {
                    "to" => {
                        if nested {
                            return Err(self.err("unexpected '{' after 'to'"));
                        }
                        targets.extend(line.into_iter().skip(1));
                    }
                    "lb_policy" => {
                        if nested {
                            return Err(self.err("unexpected '{' after lb_policy"));
                        }
                        if lb_policy.is_some() {
                            return Err(self.err("duplicate lb_policy"));
                        }
                        lb_policy = Some(self.parse_lb_policy(&line)?);
                    }
                    "health_check" => {
                        if health_check.is_some() {
                            return Err(self.err("duplicate health_check"));
                        }
                        health_check = Some(self.parse_health_check(nested)?);
                    }
                    other => {
                        return Err(self.err(format!(
                            "unexpected directive '{other}' in reverse_proxy block"
                        )))
                    }
                }
            }
        } else {
            if rest.len() != 1 {
                return Err(self.err("reverse_proxy requires exactly one target"));
            }
            targets.push(rest[0].clone());
        }

        if targets.is_empty() {
            return Err(self.err("reverse_proxy requires at least one upstream target"));
        }
        let to = targets
            .into_iter()
            .map(|t| self.parse_upstream(&t))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Directive::ReverseProxy {
            matcher,
            to,
            lb_policy: lb_policy.unwrap_or(LbPolicy::RoundRobin),
            health_check,
        })
    }

    /// Parse `lb_policy <round_robin | random | ip_hash>`.
    fn parse_lb_policy(&self, line: &[String]) -> Result<LbPolicy, ConfigError> {
        if line.len() != 2 {
            return Err(self.err("lb_policy requires exactly one argument"));
        }
        match line[1].as_str() {
            "round_robin" => Ok(LbPolicy::RoundRobin),
            "random" => Ok(LbPolicy::Random),
            "ip_hash" => Ok(LbPolicy::IpHash),
            other => Err(self.err(format!(
                "unknown lb_policy '{other}' (expected round_robin, random, or ip_hash)"
            ))),
        }
    }

    /// Parse a `health_check { ... }` block (or bare `health_check` for
    /// defaults). All sub-parameters are optional.
    fn parse_health_check(&mut self, block_open: bool) -> Result<HealthCheckSpec, ConfigError> {
        if !block_open {
            return Ok(HealthCheckSpec::default());
        }
        let mut interval = None;
        let mut timeout = None;
        let mut failures = None;
        let mut successes = None;
        loop {
            match self.peek() {
                Some(Token {
                    kind: TokenKind::RBrace,
                    ..
                }) => {
                    self.pos += 1;
                    break;
                }
                None => return Err(self.err("unexpected end of file in health_check block")),
                _ => {}
            }
            let (line, nested) = self.parse_statement()?;
            if line.is_empty() {
                continue;
            }
            if nested {
                return Err(self.err("unexpected '{' in health_check block"));
            }
            let name = line[0].clone();
            match name.as_str() {
                "interval" => {
                    if interval.is_some() {
                        return Err(self.err("duplicate interval"));
                    }
                    interval = Some(parse_duration(&line).map_err(|m| self.err(m))?);
                }
                "timeout" => {
                    if timeout.is_some() {
                        return Err(self.err("duplicate timeout"));
                    }
                    timeout = Some(parse_duration(&line).map_err(|m| self.err(m))?);
                }
                "consecutive_failures" => {
                    if failures.is_some() {
                        return Err(self.err("duplicate consecutive_failures"));
                    }
                    failures =
                        Some(parse_count(&line, "consecutive_failures").map_err(|m| self.err(m))?);
                }
                "consecutive_successes" => {
                    if successes.is_some() {
                        return Err(self.err("duplicate consecutive_successes"));
                    }
                    successes =
                        Some(parse_count(&line, "consecutive_successes").map_err(|m| self.err(m))?);
                }
                other => return Err(self.err(format!("unknown health_check option '{other}'"))),
            }
        }
        let defaults = HealthCheckSpec::default();
        let spec = HealthCheckSpec {
            interval: interval.unwrap_or(defaults.interval),
            timeout: timeout.unwrap_or(defaults.timeout),
            consecutive_failures: failures.unwrap_or(defaults.consecutive_failures),
            consecutive_successes: successes.unwrap_or(defaults.consecutive_successes),
        };
        // A zero interval would make the health runner probe every tick, and a
        // zero timeout is meaningless; reject both rather than silently hammer
        // upstreams.
        if spec.interval.is_zero() || spec.timeout.is_zero() {
            return Err(self.err("health_check interval and timeout must be greater than zero"));
        }
        Ok(spec)
    }

    fn parse_upstream(&self, s: &str) -> Result<Upstream, ConfigError> {
        if s.starts_with('[') {
            return Err(self.err("IPv6 upstream addresses are not supported in v0.1"));
        }
        let (host, port_str) = s
            .rsplit_once(':')
            .ok_or_else(|| self.err(format!("upstream '{s}' must be host:port")))?;
        if host.is_empty() {
            return Err(self.err("empty host in upstream address"));
        }
        let port = self.parse_port(port_str)?;
        Ok(Upstream {
            host: host.to_string(),
            port,
            resolved: None,
        })
    }

    fn parse_handle(
        &mut self,
        words: &[String],
        block_open: bool,
    ) -> Result<Directive, ConfigError> {
        if words.len() != 2 {
            return Err(self.err("handle requires exactly one path argument"));
        }
        let path = &words[1];
        if !path.starts_with('/') {
            return Err(self.err("handle path must start with '/'"));
        }
        if !block_open {
            return Err(self.err("handle requires a block"));
        }
        let directives = self.parse_directive_block()?;
        Ok(Directive::Handle {
            path: path.clone(),
            directives,
        })
    }

    fn parse_header(
        &self,
        words: &[String],
        block_open: bool,
        is_up: bool,
    ) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after header directive"));
        }
        if words.len() < 3 {
            return Err(self.err(format!(
                "{} requires a header name and value",
                if is_up { "header_up" } else { "header_down" }
            )));
        }
        let name = words[1].clone();
        let value = concat_tokens(&words[2..]);
        let value = ValueTemplate::parse(&value).map_err(|message| self.err(message))?;
        Ok(if is_up {
            Directive::HeaderUp { name, value }
        } else {
            Directive::HeaderDown { name, value }
        })
    }

    fn parse_file_server(
        &self,
        words: &[String],
        block_open: bool,
    ) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after file_server"));
        }
        if words.len() != 1 {
            return Err(self.err("file_server takes no arguments"));
        }
        Ok(Directive::FileServer)
    }

    fn parse_root(&self, words: &[String], block_open: bool) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after root"));
        }
        if words.len() != 2 || words[1].is_empty() {
            return Err(self.err("root requires exactly one non-empty path"));
        }
        Ok(Directive::Root {
            path: words[1].clone(),
        })
    }

    fn parse_encode(&self, words: &[String], block_open: bool) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after encode"));
        }
        if words.len() < 2 {
            return Err(self.err("encode requires at least one algorithm (gzip, zstd)"));
        }
        let mut algorithms = Vec::new();
        for alg in &words[1..] {
            match alg.as_str() {
                "gzip" => algorithms.push(Encoding::Gzip),
                "zstd" => algorithms.push(Encoding::Zstd),
                other => return Err(self.err(format!("unknown encode algorithm '{other}'"))),
            }
        }
        Ok(Directive::Encode { algorithms })
    }

    fn parse_redir(&self, words: &[String], block_open: bool) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after redir"));
        }
        if words.len() < 2 {
            return Err(self.err("redir requires a target"));
        }
        let (target, code) = match words.len() {
            2 => (words[1].clone(), REDIR_DEFAULT_CODE),
            _ => {
                let last = words.last().expect("len >= 3");
                match parse_redirect_code(last) {
                    Some(code) => (concat_tokens(&words[1..words.len() - 1]), code),
                    None => (concat_tokens(&words[1..]), REDIR_DEFAULT_CODE),
                }
            }
        };
        if target.is_empty() {
            return Err(self.err("redir target must not be empty"));
        }
        Ok(Directive::Redir { to: target, code })
    }

    fn parse_path_matcher(&self, s: &str) -> Result<PathMatcher, ConfigError> {
        let prefix = strip_matcher_wildcard(s);
        if prefix.is_empty() {
            return Err(self.err("path matcher must not be empty"));
        }
        if !prefix.starts_with('/') {
            return Err(self.err("path matcher must start with '/'"));
        }
        Ok(PathMatcher {
            prefix: prefix.to_string(),
        })
    }

    fn apply_global(&self, global: &mut GlobalConfig, words: &[String]) -> Result<(), ConfigError> {
        let name = words.first().expect("non-empty");
        match name.as_str() {
            "acme_email" => {
                if words.len() != 2 {
                    return Err(self.err("acme_email requires exactly one argument"));
                }
                global.acme_email = Some(words[1].clone());
            }
            "log_level" => {
                if words.len() != 2 {
                    return Err(self.err("log_level requires exactly one argument"));
                }
                let level = match words[1].as_str() {
                    "debug" => LogLevel::Debug,
                    "info" => LogLevel::Info,
                    "warn" => LogLevel::Warn,
                    "error" => LogLevel::Error,
                    other => {
                        return Err(self.err(format!(
                            "invalid log_level '{other}' (expected {})",
                            LogLevel::ALL.join(", ")
                        )));
                    }
                };
                global.log_level = Some(level);
            }
            other => return Err(self.err(format!("unknown global directive '{other}'"))),
        }
        Ok(())
    }
}

/// Concatenate tokens without a separator; a value split by placeholders (e.g.
/// `https://` + `{host}` + `{uri}`) is reassembled into one string.
fn concat_tokens(tokens: &[String]) -> String {
    tokens.concat()
}

/// Normalize a path matcher to its prefix: strip a trailing `*` and any
/// trailing `/` so `/static/*` and `/static/` both match `/static/...`.
pub fn strip_matcher_wildcard(path: &str) -> &str {
    let prefix = path.trim_end_matches('*').trim_end_matches('/');
    if prefix.is_empty() {
        "/"
    } else {
        prefix
    }
}

/// Normalize a site hostname for matching: strip a trailing dot and
/// ASCII-lowercase, so it compares equal to the request-side normalization.
/// Non-ASCII hostnames are rejected in v0.1 (they could never match).
fn normalize_host_name(host: &str) -> Result<String, String> {
    let host = host.trim_end_matches('.');
    if !host.is_ascii() {
        return Err("site hostname must be ASCII in v0.1".to_string());
    }
    Ok(host.to_ascii_lowercase())
}

/// Parse a `redir` status code: a 3xx number or the `permanent`/`temporary`
/// keywords (spec §5).
fn parse_redirect_code(s: &str) -> Option<u16> {
    match s {
        "permanent" => Some(308),
        "temporary" => Some(302),
        _ => {
            let code: u16 = s.parse().ok()?;
            (300..=399).contains(&code).then_some(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_global_and_site() {
        let input = "{ acme_email ops@example.com\nlog_level info }\napi.example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n";
        let rf = parse("test", input).unwrap();
        assert_eq!(rf.global.acme_email.as_deref(), Some("ops@example.com"));
        assert_eq!(rf.global.log_level, Some(LogLevel::Info));
        assert_eq!(rf.sites.len(), 1);
        match &rf.sites[0].key {
            SiteKey::Named { host, port } => {
                assert_eq!(host, "api.example.com");
                assert_eq!(*port, 443);
            }
            _ => panic!("expected named site"),
        }
    }

    #[test]
    fn parses_catch_all_and_placeholder_redir() {
        let input = ":8080 {\n    redir https://{host}{uri} permanent\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].key {
            SiteKey::CatchAll { port } => assert_eq!(*port, 8080),
            _ => panic!("expected catch-all"),
        }
        match &rf.sites[0].directives[0] {
            Directive::Redir { to, code } => {
                assert_eq!(to, "https://{host}{uri}");
                assert_eq!(*code, 308);
            }
            other => panic!("expected redir, got {other:?}"),
        }
    }

    #[test]
    fn parses_handle_with_scope() {
        let input = "example.com {\n    handle /static/* {\n        root /var/www\n        file_server\n        encode zstd gzip\n    }\n    reverse_proxy 127.0.0.1:8080\n    header_up X-Real-IP {remote_host}\n}\n";
        let rf = parse("test", input).unwrap();
        let site = &rf.sites[0];
        assert_eq!(site.directives.len(), 3);
        match &site.directives[0] {
            Directive::Handle { path, directives } => {
                assert_eq!(path, "/static/*");
                assert_eq!(directives.len(), 3);
            }
            other => panic!("expected handle, got {other:?}"),
        }
    }

    #[test]
    fn parses_to_block_form() {
        let input = ":8080 {\n    reverse_proxy /api/* {\n        to 127.0.0.1:8081 127.0.0.1:8082\n    }\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::ReverseProxy { matcher, to, .. } => {
                assert!(matcher.is_some());
                assert_eq!(to.len(), 2);
                assert_eq!(to[0].host, "127.0.0.1");
                assert_eq!(to[0].port, 8081);
                assert_eq!(to[1].port, 8082);
            }
            other => panic!("expected reverse_proxy, got {other:?}"),
        }
    }

    #[test]
    fn parses_lb_policy_and_health_check() {
        let input = ":8080 {\n    reverse_proxy {\n        to 127.0.0.1:8081 127.0.0.1:8082\n        lb_policy random\n        health_check {\n            interval 2s\n            timeout 500ms\n            consecutive_failures 5\n            consecutive_successes 1\n        }\n    }\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::ReverseProxy {
                matcher,
                to,
                lb_policy,
                health_check,
            } => {
                assert!(matcher.is_none());
                assert_eq!(to.len(), 2);
                assert_eq!(*lb_policy, LbPolicy::Random);
                let hc = health_check.as_ref().expect("health check");
                assert_eq!(hc.interval, std::time::Duration::from_secs(2));
                assert_eq!(hc.timeout, std::time::Duration::from_millis(500));
                assert_eq!(hc.consecutive_failures, 5);
                assert_eq!(hc.consecutive_successes, 1);
            }
            other => panic!("expected reverse_proxy, got {other:?}"),
        }
    }

    #[test]
    fn bare_health_check_uses_defaults() {
        let input = ":8080 {\n    reverse_proxy {\n        to 127.0.0.1:8081\n        health_check\n    }\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::ReverseProxy { health_check, .. } => {
                assert_eq!(
                    *health_check.as_ref().expect("health check"),
                    HealthCheckSpec::default()
                );
            }
            other => panic!("expected reverse_proxy, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_lb_policy() {
        let input = ":8080 {\n    reverse_proxy {\n        to 127.0.0.1:8081\n        lb_policy fancy\n    }\n}\n";
        let err = parse("test", input).unwrap_err();
        assert!(err.to_string().contains("unknown lb_policy"));
    }

    #[test]
    fn rejects_unknown_health_check_option() {
        let input = ":8080 {\n    reverse_proxy {\n        to 127.0.0.1:8081\n        health_check {\n            bogus 1s\n        }\n    }\n}\n";
        let err = parse("test", input).unwrap_err();
        assert!(err.to_string().contains("unknown health_check option"));
    }

    #[test]
    fn rejects_zero_health_check_interval() {
        let input = ":8080 {\n    reverse_proxy {\n        to 127.0.0.1:8081\n        health_check {\n            interval 0s\n        }\n    }\n}\n";
        let err = parse("test", input).unwrap_err();
        assert!(err
            .to_string()
            .contains("interval and timeout must be greater than zero"));
    }

    #[test]
    fn errors_on_unknown_directive() {
        let input = ":8080 {\n    bogus_directive x\n}\n";
        let err = parse("test", input).unwrap_err();
        assert!(err.to_string().contains("unknown directive"));
    }

    #[test]
    fn errors_on_unknown_placeholder() {
        let input = ":8080 {\n    header_up X {oops}\n}\n";
        let err = parse("test", input).unwrap_err();
        assert!(err.to_string().contains("unknown placeholder"));
    }

    #[test]
    fn errors_report_line_and_column() {
        let input = ":8080 {\n    bogus_directive x\n}\n";
        let err = parse("test", input).unwrap_err();
        let text = err.to_string();
        // `bogus_directive` sits at line 2, column 5 (after four spaces).
        assert!(
            text.contains("test:2:5"),
            "expected file:line:col, got: {text}"
        );
        assert!(text.contains("unknown directive"));
    }

    #[test]
    fn matcher_wildcard_stripping() {
        assert_eq!(strip_matcher_wildcard("/static/*"), "/static");
        assert_eq!(strip_matcher_wildcard("/static/"), "/static");
        assert_eq!(strip_matcher_wildcard("/api/*"), "/api");
        assert_eq!(strip_matcher_wildcard("/"), "/");
        assert_eq!(strip_matcher_wildcard("/*"), "/");
    }

    // -------------------------------------------------------------------
    // Fuzz-style robustness (M3): the parser must never panic, only return
    // `Ok` or `Err`. A deterministic PRNG keeps failures reproducible; the
    // same inputs run under `cargo fuzz` once that toolchain is available.
    // -------------------------------------------------------------------

    /// A small deterministic PRNG (xorshift64), seeded so failures reproduce.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            debug_assert!(n > 0);
            (self.next() % n as u64) as usize
        }
    }

    /// A charset mixing Raddyfile tokens, whitespace, braces, comments, and
    /// multi-byte UTF-8 (non-ASCII hosts/values must not panic the lexer).
    const FUZZ_CHARS: &[char] = &[
        'a', 'b', 'z', '0', '9', ':', '/', '.', '{', '}', '#', '\n', ' ', '\t', '@', '-', '_', '*',
        '[', ']', '~', '!', '%', '"', '\'', 'é', '例', '中', 'Ａ', '\u{0}', '\x0c', '\x0b',
        '\u{00a0}', '\u{200b}',
    ];

    #[test]
    fn random_inputs_never_panic() {
        let mut rng = XorShift(0xdead_beef_cafe_f00d);
        for _ in 0..20_000 {
            let len = rng.below(300);
            let mut input = String::new();
            for _ in 0..len {
                input.push(FUZZ_CHARS[rng.below(FUZZ_CHARS.len())]);
            }
            let _ = parse("fuzz", &input);
        }
    }

    #[test]
    fn mutated_configs_never_panic() {
        let seeds = [
            // A representative full v0.1 config.
            "{ acme_email ops@example.com\nlog_level info }\n:80 {\n    redir https://{host}{uri} permanent\n}\napi.example.com:8080 {\n    handle /static/* {\n        root /var/www\n        file_server\n        encode zstd gzip\n    }\n    reverse_proxy 127.0.0.1:9000\n    header_up X-Real-IP {remote_host}\n}\n",
            ":8080 {\n    reverse_proxy {\n        to 127.0.0.1:9001 127.0.0.1:9002\n    }\n}\n",
            "# only a comment\n:8080 {\n    header_up X-Test {uri}\n    redir /old permanent\n}\n",
        ];
        let mut rng = XorShift(0x0123_4567_89ab_cdef);
        for seed in seeds {
            // Mutate at char granularity (the charset includes multi-byte
            // UTF-8, so byte-indexed mutations would panic on char boundaries).
            let mut chars: Vec<char> = seed.chars().collect();
            for _ in 0..5_000 {
                // Apply 1..=4 random single-char mutations.
                let count = 1 + rng.below(4);
                for _ in 0..count {
                    if chars.is_empty() {
                        chars.push(FUZZ_CHARS[rng.below(FUZZ_CHARS.len())]);
                        continue;
                    }
                    let pos = rng.below(chars.len());
                    match rng.below(3) {
                        // Insert a character anywhere (including the end).
                        0 => chars.insert(pos, FUZZ_CHARS[rng.below(FUZZ_CHARS.len())]),
                        // Delete a character.
                        1 => {
                            chars.remove(pos);
                        }
                        // Replace a character.
                        _ => chars[pos] = FUZZ_CHARS[rng.below(FUZZ_CHARS.len())],
                    }
                }
                let input: String = chars.iter().collect();
                let _ = parse("fuzz", &input);
            }
        }
    }

    #[test]
    fn valid_prefix_with_garbage_tail_never_panic() {
        let prefix = ":8080 {\n    reverse_proxy 127.0.0.1:9000\n";
        let mut rng = XorShift(0xfeed_face_cafe_beef);
        for _ in 0..5_000 {
            let mut input = prefix.to_string();
            for _ in 0..rng.below(50) {
                input.push(FUZZ_CHARS[rng.below(FUZZ_CHARS.len())]);
            }
            let _ = parse("fuzz", &input);
        }
    }
}
