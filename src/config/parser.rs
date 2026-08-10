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
use std::collections::HashMap;

/// The maximum size of a file pulled in by `import` (spec §5.12), so a config
/// can never read unbounded input (e.g. `/dev/zero`).
const MAX_IMPORT_BYTES: u64 = 1 << 20;

/// Parse a Raddyfile into its source AST.
///
/// The input is preprocessed before the grammar parse (spec §5.12): `{$ENV}`
/// placeholders are expanded from the environment, `(name)` snippet definitions
/// are captured, and `import` statements are spliced in (recursively).
pub fn parse(file: &str, input: &str) -> Result<Raddyfile, ConfigError> {
    let expanded = expand_env(input).map_err(|message| ConfigError::Parse {
        file: file.to_string(),
        line: 1,
        col: 1,
        message,
    })?;
    let tokens = lex(&expanded).map_err(|message| ConfigError::Parse {
        file: file.to_string(),
        line: 1,
        col: 1,
        message,
    })?;
    let mut snippets = HashMap::new();
    let mut stack = Vec::new();
    let tokens =
        expand_imports(file, tokens, &mut snippets, &mut stack, true).map_err(|message| {
            ConfigError::Parse {
                file: file.to_string(),
                line: 1,
                col: 1,
                message,
            }
        })?;
    Parser {
        file,
        tokens,
        pos: 0,
        stmt_pos: (1, 1),
    }
    .parse_raddyfile()
}

/// Expand environment variables in a config's raw text (spec §5.12): every
/// `{$NAME}` is replaced by the value of `NAME`. A missing variable or an
/// unterminated placeholder is a parse error.
fn expand_env(input: &str) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("{$") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err("unterminated '{$...}' environment placeholder".to_string());
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err("empty environment variable name in '{$}'".to_string());
        }
        let value =
            std::env::var(name).map_err(|_| format!("environment variable '{name}' is not set"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Expand `import` statements and collect `(name)` snippets in a token stream
/// (spec §5.12). Snippet definitions at the top level are captured and removed;
/// an `import <target>` at any nesting level is replaced by the snippet's tokens
/// (when `<target>` names a snippet) or by the recursively-expanded tokens of
/// the named file (resolved relative to `file`).
fn expand_imports(
    file: &str,
    tokens: Vec<Token>,
    snippets: &mut HashMap<String, Vec<Token>>,
    stack: &mut Vec<String>,
    top_level: bool,
) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        match &token.kind {
            TokenKind::Newline | TokenKind::RBrace => {
                out.push(token.clone());
                i += 1;
            }
            TokenKind::LBrace => {
                let (content, end) = capture_block(&tokens, i);
                if end == i {
                    // Unclosed block; leave it for the parser to reject.
                    out.push(token.clone());
                    i += 1;
                    continue;
                }
                out.push(token.clone());
                out.extend(expand_imports(file, content, snippets, stack, false)?);
                out.push(tokens[end].clone());
                i = end + 1;
            }
            TokenKind::Word(_) => {
                // Read the statement's words (up to a newline, `{`, or `}`).
                let mut words: Vec<Token> = Vec::new();
                let mut j = i;
                let mut opens_block = false;
                while j < tokens.len() {
                    match &tokens[j].kind {
                        TokenKind::Word(_) => {
                            words.push(tokens[j].clone());
                            j += 1;
                        }
                        TokenKind::LBrace => {
                            opens_block = true;
                            break;
                        }
                        _ => break,
                    }
                }
                // A top-level `(name) { ... }` is a snippet definition.
                if top_level && opens_block && words.len() == 1 {
                    if let Some(snippet_name) = strip_snippet_name(word_str(&words[0])) {
                        let (content, end) = capture_block(&tokens, j);
                        snippets.insert(snippet_name.to_string(), content);
                        i = end + 1;
                        continue;
                    }
                }
                // `import <target>` splices a snippet or an imported file.
                if words.len() == 2 && word_str(&words[0]) == "import" {
                    let target = word_str(&words[1]);
                    if let Some(snippet) = snippets.get(target).cloned() {
                        out.extend(snippet);
                    } else {
                        let (imported_path, imported_text) = read_imported(file, target)?;
                        if stack.contains(&imported_path) {
                            return Err(format!("import cycle involving {imported_path}"));
                        }
                        stack.push(imported_path.clone());
                        let expanded = expand_env(&imported_text)?;
                        let imported_tokens = lex(&expanded)
                            .map_err(|message| format!("{imported_path}: {message}"))?;
                        out.extend(expand_imports(
                            &imported_path,
                            imported_tokens,
                            snippets,
                            stack,
                            true,
                        )?);
                        stack.pop();
                    }
                    i = j;
                    continue;
                }
                out.extend(words);
                i = j;
            }
        }
    }
    Ok(out)
}

/// Read an imported file (spec §5.12), resolving `target` relative to `file`'s
/// directory, bounded to [`MAX_IMPORT_BYTES`].
fn read_imported(file: &str, target: &str) -> Result<(String, String), String> {
    use std::io::Read;
    let path = std::path::Path::new(file)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(target);
    let mut content = String::new();
    let f = std::fs::File::open(&path)
        .map_err(|e| format!("failed to read imported file {}: {e}", path.display()))?;
    f.take(MAX_IMPORT_BYTES)
        .read_to_string(&mut content)
        .map_err(|e| format!("failed to read imported file {}: {e}", path.display()))?;
    Ok((path.display().to_string(), content))
}

/// The word content of a token (assumed to be a `Word`).
fn word_str(token: &Token) -> &str {
    match &token.kind {
        TokenKind::Word(w) => w.as_str(),
        _ => unreachable!("word_str called on a non-word token"),
    }
}

/// The snippet name of a `(name)` token, or `None`.
fn strip_snippet_name(token: &str) -> Option<&str> {
    token
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .filter(|name| !name.is_empty())
}

/// Given tokens starting at an `{`, return the tokens between the braces and
/// the index of the matching `}` (or the opening index when unclosed).
fn capture_block(tokens: &[Token], lb: usize) -> (Vec<Token>, usize) {
    let mut depth = 0usize;
    let mut end = lb;
    for (k, token) in tokens.iter().enumerate().skip(lb) {
        match token.kind {
            TokenKind::LBrace => depth += 1,
            TokenKind::RBrace => {
                depth -= 1;
                if depth == 0 {
                    end = k;
                    break;
                }
            }
            _ => {}
        }
    }
    if end == lb {
        // Unclosed block (no matching `}`); leave it for the parser to reject.
        return (Vec::new(), lb);
    }
    (tokens[lb + 1..end].to_vec(), end)
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

/// Parse a positive integer health-check counter (the second token). A zero
/// counter would disable the threshold it controls, so it is rejected.
fn parse_count(line: &[String], name: &str) -> Result<usize, String> {
    if line.len() != 2 {
        return Err(format!("'{name}' requires exactly one integer value"));
    }
    let v = &line[1];
    let n: usize = v
        .parse()
        .map_err(|_| format!("invalid {name} value '{v}'"))?;
    if n == 0 {
        return Err(format!("{name} must be >= 1"));
    }
    Ok(n)
}

/// Parse a rate token like `50r/s`, `1200r/m`, `3r/h`, `1r/d` (spec §5.2) into
/// a count and a time unit.
fn parse_rate(token: &str) -> Result<(u64, RateUnit), String> {
    let (count_str, unit) = if let Some(v) = token.strip_suffix("r/s") {
        (v, RateUnit::Second)
    } else if let Some(v) = token.strip_suffix("r/m") {
        (v, RateUnit::Minute)
    } else if let Some(v) = token.strip_suffix("r/h") {
        (v, RateUnit::Hour)
    } else if let Some(v) = token.strip_suffix("r/d") {
        (v, RateUnit::Day)
    } else {
        return Err(format!(
            "invalid rate '{token}' (expected <count>r/<s|m|h|d>)"
        ));
    };
    let count: u64 = count_str
        .parse()
        .map_err(|_| format!("invalid rate '{token}'"))?;
    if count < 1 {
        return Err(format!("rate count must be >= 1: '{token}'"));
    }
    Ok((count, unit))
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
            "handle" => self.parse_handle(words, block_open, false),
            "handle_path" => self.parse_handle(words, block_open, true),
            "header_up" => self.parse_header(words, block_open, true),
            "header_down" => self.parse_header(words, block_open, false),
            "file_server" => self.parse_file_server(words, block_open),
            "root" => self.parse_root(words, block_open),
            "encode" => self.parse_encode(words, block_open),
            "redir" => self.parse_redir(words, block_open),
            "rewrite" => self.parse_rewrite(words, block_open),
            "respond" => self.parse_respond(words, block_open),
            "error" => self.parse_error(words, block_open),
            "basic_auth" => self.parse_basic_auth(words, block_open),
            "forward_auth" => self.parse_forward_auth(words, block_open),
            "rate_limit" => self.parse_rate_limit(words, block_open),
            "trusted_proxies" => self.parse_trusted_proxies(words, block_open),
            "tls" => self.parse_tls(words, block_open),
            other => Err(self.err(format!("unknown directive '{other}'"))),
        }
    }

    fn parse_reverse_proxy(
        &mut self,
        words: &[String],
        block_open: bool,
    ) -> Result<Directive, ConfigError> {
        // Optional inline matcher (spec §5.9): a bare `/path`, or matcher terms
        // (`method GET`, `host api.example.com`, …). The remaining tokens are
        // the upstream targets (non-block form).
        let (matcher, rest) = self.parse_matchers(&words[1..])?;

        let mut targets: Vec<String> = Vec::new();
        let mut lb_policy = None;
        let mut health_check = None;
        let mut tls = ProxyTlsConfig::default();
        if block_open {
            // Block form: `{ to <upstream>... [lb_policy <p>] [health_check { ... }] [tls_* ...] }`.
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
                    "tls_servername" => {
                        if nested {
                            return Err(self.err("unexpected '{' after tls_servername"));
                        }
                        if tls.servername.is_some() {
                            return Err(self.err("duplicate tls_servername"));
                        }
                        if line.len() != 2 {
                            return Err(self.err("tls_servername requires exactly one argument"));
                        }
                        tls.servername = Some(line[1].clone());
                    }
                    "tls_skip_verify" => {
                        if nested {
                            return Err(self.err("unexpected '{' after tls_skip_verify"));
                        }
                        if line.len() != 1 {
                            return Err(self.err("tls_skip_verify takes no arguments"));
                        }
                        tls.skip_verify = true;
                    }
                    "tls_ca" => {
                        if nested {
                            return Err(self.err("unexpected '{' after tls_ca"));
                        }
                        if line.len() != 2 {
                            return Err(self.err("tls_ca requires exactly one argument"));
                        }
                        tls.ca_files.push(line[1].clone());
                    }
                    "tls_cert" => {
                        if nested {
                            return Err(self.err("unexpected '{' after tls_cert"));
                        }
                        if line.len() != 3 {
                            return Err(self.err("tls_cert requires a certificate and a key file"));
                        }
                        tls.cert_file = Some(line[1].clone());
                        tls.key_file = Some(line[2].clone());
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
            tls,
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
        // Optional scheme prefix decides whether the upstream connection is TLS
        // (spec §5.4); a bare `host:port` stays plain HTTP.
        let (tls, rest) = if let Some(r) = s.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = s.strip_prefix("http://") {
            (false, r)
        } else {
            (false, s)
        };
        if rest.starts_with('[') {
            return Err(self.err("IPv6 upstream addresses are not supported in v0.1"));
        }
        let (host, port_str) = rest
            .rsplit_once(':')
            .ok_or_else(|| self.err(format!("upstream '{s}' must be host:port")))?;
        if host.is_empty() {
            return Err(self.err("empty host in upstream address"));
        }
        let port = self.parse_port(port_str)?;
        Ok(Upstream {
            host: host.to_string(),
            port,
            tls,
            resolved: None,
        })
    }

    /// Parse a `handle` (or, with `strip`, `handle_path`) block: a matcher plus
    /// a directive block (spec §5.9).
    fn parse_handle(
        &mut self,
        words: &[String],
        block_open: bool,
        strip: bool,
    ) -> Result<Directive, ConfigError> {
        let (matcher, rest) = self.parse_matchers(&words[1..])?;
        if matcher.is_empty() {
            return Err(self.err("handle requires a matcher"));
        }
        if let Some(extra) = rest.first() {
            return Err(self.err(format!("unexpected '{extra}' after the handle matcher")));
        }
        if !block_open {
            return Err(self.err("handle requires a block"));
        }
        let directives = self.parse_directive_block()?;
        if strip {
            Ok(Directive::HandlePath {
                matcher,
                directives,
            })
        } else {
            Ok(Directive::Handle {
                matcher,
                directives,
            })
        }
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
            return Err(self.err("encode requires at least one algorithm (gzip, zstd, br)"));
        }
        let mut algorithms = Vec::new();
        for alg in &words[1..] {
            match alg.as_str() {
                "gzip" => algorithms.push(Encoding::Gzip),
                "zstd" => algorithms.push(Encoding::Zstd),
                "br" => algorithms.push(Encoding::Brotli),
                other => {
                    return Err(self.err(format!(
                        "unknown encode algorithm '{other}' (expected gzip, zstd, or br)"
                    )))
                }
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

    /// Parse matcher terms (spec §5.9) from the head of `tokens`, stopping at
    /// the first token that is not a matcher keyword / bare `/path` / `!`.
    /// Returns the matchers and the unconsumed remainder (upstream targets, a
    /// rewrite target, …).
    fn parse_matchers<'t>(
        &self,
        tokens: &'t [String],
    ) -> Result<(Vec<Matcher>, &'t [String]), ConfigError> {
        let mut matchers = Vec::new();
        let mut rest = tokens;
        while let Some((matcher, consumed)) = self.parse_one_matcher(rest)? {
            matchers.push(matcher);
            rest = &rest[consumed..];
        }
        Ok((matchers, rest))
    }

    /// Parse a single matcher term if `tokens` starts with one. Returns the
    /// matcher and the number of tokens it consumed.
    fn parse_one_matcher(
        &self,
        tokens: &[String],
    ) -> Result<Option<(Matcher, usize)>, ConfigError> {
        let Some(first) = tokens.first() else {
            return Ok(None);
        };
        if first.starts_with('/') {
            // Bare `/path` shorthand for `path /path`.
            if first.len() < 2 {
                return Err(self.err("path matcher must not be empty"));
            }
            return Ok(Some((
                Matcher::Path(strip_matcher_wildcard(first).to_string()),
                1,
            )));
        }
        // Negation: `!kind ...`.
        let (negated, kind) = match first.strip_prefix('!') {
            Some(kind) => (true, kind),
            None => (false, first.as_str()),
        };
        if !matches!(
            kind,
            "path" | "host" | "method" | "header" | "query" | "remote_ip" | "protocol"
        ) {
            return Ok(None);
        }
        // Validate arity before indexing `tokens` below.
        let arity = match kind {
            "path" | "host" | "method" | "remote_ip" | "protocol" => 1,
            "header" | "query" => 2,
            _ => unreachable!("matcher keyword checked above"),
        };
        if tokens.len() <= arity {
            return Err(self.err(format!("matcher '{kind}' is missing arguments")));
        }
        let matcher = match kind {
            "path" => Matcher::Path(strip_matcher_wildcard(&tokens[1]).to_string()),
            "host" => Matcher::Host(tokens[1].clone()),
            "method" => Matcher::Method(tokens[1].clone()),
            "header" => Matcher::Header {
                name: tokens[1].clone(),
                value: tokens[2].clone(),
            },
            "query" => Matcher::Query {
                key: tokens[1].clone(),
                value: tokens[2].clone(),
            },
            "remote_ip" => {
                Matcher::RemoteIp(Cidr::parse(&tokens[1]).map_err(|message| self.err(message))?)
            }
            "protocol" => Matcher::Protocol(match tokens[1].as_str() {
                "http" => Protocol::Http,
                "https" => Protocol::Https,
                other => {
                    return Err(self.err(format!(
                        "invalid protocol '{other}' (expected http or https)"
                    )))
                }
            }),
            _ => unreachable!("matcher keyword checked above"),
        };
        let matcher = if negated {
            Matcher::Not(Box::new(matcher))
        } else {
            matcher
        };
        Ok(Some((matcher, arity + 1)))
    }

    /// Parse `rewrite <to>` — rewrite the request URI before the terminal
    /// serves (modifier, spec §5.9). Conditional rewrites belong in a `handle`
    /// block. Placeholder fragments (`{uri}`) are separate tokens, so they are
    /// concatenated like `redir`'s target.
    fn parse_rewrite(&self, words: &[String], block_open: bool) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after rewrite"));
        }
        if words.len() < 2 {
            return Err(self.err("rewrite requires a target"));
        }
        let target = concat_tokens(&words[1..]);
        if target.is_empty() {
            return Err(self.err("rewrite target must not be empty"));
        }
        let to = ValueTemplate::parse(&target).map_err(|message| self.err(message))?;
        Ok(Directive::Rewrite { to })
    }

    /// Parse `respond <status> [<body>]` (terminal, spec §5.9).
    fn parse_respond(&self, words: &[String], block_open: bool) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after respond"));
        }
        if !(2..=3).contains(&words.len()) {
            return Err(self.err("respond expects: respond <status> [<body>]"));
        }
        let status = words[1]
            .parse::<u16>()
            .map_err(|_| self.err(format!("invalid respond status '{}'", words[1])))?;
        if !(100..=599).contains(&status) {
            return Err(self.err("respond status must be between 100 and 599"));
        }
        Ok(Directive::Respond {
            status,
            body: words.get(2).cloned(),
        })
    }

    /// Parse `error [<status>] [<message>]` (terminal, spec §5.9).
    fn parse_error(&self, words: &[String], block_open: bool) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after error"));
        }
        let mut status = None;
        let mut message = None;
        for arg in &words[1..] {
            if let Ok(code) = arg.parse::<u16>() {
                if status.is_some() {
                    return Err(self.err("duplicate error status"));
                }
                if !(100..=599).contains(&code) {
                    return Err(self.err("error status must be between 100 and 599"));
                }
                status = Some(code);
            } else {
                if message.is_some() {
                    return Err(self.err("duplicate error message"));
                }
                message = Some(arg.clone());
            }
        }
        Ok(Directive::Error { status, message })
    }

    /// Parse `basic_auth <user> <bcrypt-hash>` (guard, spec §5.10).
    fn parse_basic_auth(
        &self,
        words: &[String],
        block_open: bool,
    ) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after basic_auth"));
        }
        if words.len() != 3 {
            return Err(self.err("basic_auth expects: basic_auth <user> <bcrypt-hash>"));
        }
        Ok(Directive::BasicAuth {
            user: words[1].clone(),
            hash: words[2].clone(),
        })
    }

    /// Parse `forward_auth <target>` (guard, spec §5.10). The target is an
    /// upstream `host:port`.
    fn parse_forward_auth(
        &self,
        words: &[String],
        block_open: bool,
    ) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after forward_auth"));
        }
        if words.len() != 2 {
            return Err(self.err("forward_auth expects: forward_auth <host:port>"));
        }
        let target = &words[1];
        if !target.contains(':') {
            return Err(self.err("forward_auth target must be <host:port>"));
        }
        Ok(Directive::ForwardAuth {
            target: target.clone(),
        })
    }

    /// Parse `trusted_proxies <cidr>...` (spec §4).
    fn parse_trusted_proxies(
        &self,
        words: &[String],
        block_open: bool,
    ) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after trusted_proxies"));
        }
        if words.len() < 2 {
            return Err(self.err("trusted_proxies requires at least one CIDR"));
        }
        let networks = words[1..]
            .iter()
            .map(|w| Cidr::parse(w).map_err(|message| self.err(message)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Directive::TrustedProxies { networks })
    }

    /// Parse `tls` — the per-site TLS directive (spec §5.7). Forms:
    /// `tls` (ACME default), `tls internal`, `tls <cert> <key>`,
    /// `tls min_version|max_version <1.2|1.3>`, `tls ciphers <list>`,
    /// `tls client_auth <optional|require> <ca-file>`. Options are merged
    /// across separate `tls` lines by the compiler.
    fn parse_tls(&self, words: &[String], block_open: bool) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after tls"));
        }
        let mut config = TlsConfig::default();
        let args = &words[1..];
        match args.first() {
            None => {}
            Some(first) => match first.as_str() {
                "internal" => {
                    if args.len() != 1 {
                        return Err(self.err("tls internal takes no arguments"));
                    }
                    config.source = TlsSource::Internal;
                }
                "min_version" | "max_version" => {
                    if args.len() != 2 {
                        return Err(self.err(format!("tls {first} requires exactly one version")));
                    }
                    let version = match args[1].as_str() {
                        "1.2" => TlsVersion::Tls12,
                        "1.3" => TlsVersion::Tls13,
                        other => {
                            return Err(self.err(format!(
                                "invalid TLS version '{other}' (expected 1.2 or 1.3)"
                            )))
                        }
                    };
                    if first == "min_version" {
                        config.min_version = Some(version);
                    } else {
                        config.max_version = Some(version);
                    }
                }
                "ciphers" => {
                    if args.len() < 2 {
                        return Err(self.err("tls ciphers requires a cipher list"));
                    }
                    // Space-separated cipher names are joined with `:` (OpenSSL's
                    // cipher-list separator).
                    config.ciphers = Some(args[1..].join(":"));
                }
                "client_auth" => {
                    if args.len() != 3 {
                        return Err(self.err(
                            "tls client_auth expects: client_auth <optional|require> <ca-file>",
                        ));
                    }
                    let mode = match args[1].as_str() {
                        "optional" => ClientAuthMode::Optional,
                        "require" => ClientAuthMode::Require,
                        other => {
                            return Err(self.err(format!(
                                "invalid client_auth mode '{other}' (expected optional or require)"
                            )))
                        }
                    };
                    config.client_auth = Some(ClientAuth {
                        mode,
                        ca_file: args[2].clone(),
                    });
                }
                _ => {
                    // Static certificate pair: `tls <cert-file> <key-file>`.
                    if args.len() != 2 {
                        return Err(self.err(
                            "tls expects: internal | <cert-file> <key-file> | min_version/max_version/ciphers/client_auth",
                        ));
                    }
                    config.source = TlsSource::Static {
                        cert_file: args[0].clone(),
                        key_file: args[1].clone(),
                    };
                }
            },
        }
        Ok(Directive::Tls { config })
    }

    /// Parse `rate_limit <key> <rate> [burst=<n>]` (spec §5.2).
    fn parse_rate_limit(
        &self,
        words: &[String],
        block_open: bool,
    ) -> Result<Directive, ConfigError> {
        if block_open {
            return Err(self.err("unexpected '{' after rate_limit"));
        }
        if words.len() < 3 {
            return Err(self.err("rate_limit expects: rate_limit <key> <rate> [burst=<n>]"));
        }
        // The key selects what is counted (spec §5.2): `remote_ip` or
        // `header <name>`.
        let (key, rate_idx) = match words[1].as_str() {
            "remote_ip" => (RateLimitKey::RemoteIp, 2),
            "header" => {
                if words.len() < 4 {
                    return Err(self.err("rate_limit header expects: header <name> <rate>"));
                }
                (RateLimitKey::Header(words[2].clone()), 3)
            }
            other => {
                return Err(self.err(format!(
                    "unknown rate_limit key '{other}' (expected 'remote_ip' or 'header <name>')"
                )))
            }
        };
        let rate = words
            .get(rate_idx)
            .ok_or_else(|| self.err("rate_limit requires a rate after the key"))?;
        let (count, unit) = parse_rate(rate).map_err(|message| self.err(message))?;
        let mut burst = count;
        if let Some(arg) = words.get(rate_idx + 1) {
            match arg.strip_prefix("burst=") {
                Some(value) => {
                    burst = value
                        .parse::<u64>()
                        .map_err(|_| self.err(format!("invalid burst '{value}'")))?;
                    if burst < 1 {
                        return Err(self.err("burst must be >= 1"));
                    }
                }
                None => {
                    return Err(self.err(format!(
                        "unexpected argument '{arg}' (expected 'burst=<n>')"
                    )))
                }
            }
        }
        Ok(Directive::RateLimit {
            spec: RateSpec {
                key,
                count,
                unit,
                burst,
            },
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
            "trusted_proxies" => {
                if words.len() < 2 {
                    return Err(self.err("trusted_proxies requires at least one CIDR"));
                }
                let networks = words[1..]
                    .iter()
                    .map(|w| Cidr::parse(w).map_err(|message| self.err(message)))
                    .collect::<Result<Vec<_>, _>>()?;
                global.trusted_proxies = networks;
            }
            "dns_challenge" => {
                if words.len() != 3 {
                    return Err(self.err(
                        "dns_challenge requires a provider and an API token: dns_challenge cloudflare <api_token>",
                    ));
                }
                let provider = match words[1].as_str() {
                    "cloudflare" => DnsProvider::Cloudflare,
                    other => {
                        return Err(self.err(format!(
                            "invalid dns_challenge provider '{other}' (expected {})",
                            DnsProvider::ALL.join(", ")
                        )));
                    }
                };
                if words[2].is_empty() {
                    return Err(self.err("dns_challenge requires a non-empty API token"));
                }
                global.dns_challenge = Some(DnsChallenge {
                    provider,
                    api_token: words[2].clone(),
                });
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

/// Normalize and validate a site hostname: an ASCII DNS hostname with non-empty
/// labels of `[A-Za-z0-9-]`, no leading/trailing `-`, at most 253 chars total
/// and 63 per label. One trailing dot (FQDN form) is stripped; single-label
/// hosts such as `localhost` are allowed. Whitespace, slashes, backslashes,
/// colons, empty labels, and non-ASCII characters are rejected — a host that
/// cannot be a real DNS name could never match a request (and would make the
/// config a footgun for SNI/ACME).
fn normalize_host_name(host: &str) -> Result<String, String> {
    if host.chars().any(char::is_whitespace) {
        return Err("site hostname must not contain whitespace".to_string());
    }
    if !host.is_ascii() {
        return Err("site hostname must be ASCII in v0.1".to_string());
    }
    // One trailing dot marks an FQDN; a second would leave an empty final
    // label, rejected below.
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return Err("site hostname must not be empty".to_string());
    }
    if host.len() > 253 {
        return Err("site hostname is too long (max 253 chars)".to_string());
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err("site hostname contains an empty label".to_string());
        }
        if label.len() > 63 {
            return Err("site hostname label is too long (max 63 chars)".to_string());
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("site hostname contains an invalid character".to_string());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("site hostname label must not start or end with '-'".to_string());
        }
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
    fn parses_dns_challenge_cloudflare() {
        let input = "{ dns_challenge cloudflare abc123 }\napi.example.com {\n    reverse_proxy 127.0.0.1:8080\n}\n";
        let rf = parse("test", input).unwrap();
        let challenge = rf.global.dns_challenge.expect("dns_challenge parsed");
        assert_eq!(challenge.provider, DnsProvider::Cloudflare);
        assert_eq!(challenge.api_token, "abc123");
    }

    #[test]
    fn rejects_invalid_dns_challenge() {
        // Unknown provider.
        let err = parse("test", "{ dns_challenge route53 tok }\n").unwrap_err();
        assert!(err.to_string().contains("invalid dns_challenge provider"));
        // Missing token.
        let err = parse("test", "{ dns_challenge cloudflare }\n").unwrap_err();
        assert!(err
            .to_string()
            .contains("requires a provider and an API token"));
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
            Directive::Handle {
                matcher,
                directives,
            } => {
                assert!(matches!(
                    matcher.as_slice(),
                    [Matcher::Path(prefix)] if prefix == "/static"
                ));
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
                assert!(!matcher.is_empty());
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
                ..
            } => {
                assert!(matcher.is_empty());
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
    fn rejects_zero_consecutive_failures() {
        let input = ":8080 {\n    reverse_proxy {\n        to 127.0.0.1:8081\n        health_check {\n            consecutive_failures 0\n        }\n    }\n}\n";
        let err = parse("test", input).unwrap_err();
        assert!(err
            .to_string()
            .contains("consecutive_failures must be >= 1"));
    }

    #[test]
    fn rejects_zero_consecutive_successes() {
        let input = ":8080 {\n    reverse_proxy {\n        to 127.0.0.1:8081\n        health_check {\n            consecutive_successes 0\n        }\n    }\n}\n";
        let err = parse("test", input).unwrap_err();
        assert!(err
            .to_string()
            .contains("consecutive_successes must be >= 1"));
    }

    #[test]
    fn normalizes_valid_host_names() {
        // (input, expected normalized output).
        let cases: &[(&str, &str)] = &[
            ("example.com", "example.com"),
            ("EXAMPLE.COM", "example.com"),
            ("example.com.", "example.com"),
            ("localhost", "localhost"),
            ("LocalHost", "localhost"),
            ("sub-domain.example.com", "sub-domain.example.com"),
            ("xn--bcher-kva.example", "xn--bcher-kva.example"),
            ("a", "a"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_host_name(input).as_deref(),
                Ok(*expected),
                "host '{input}'"
            );
        }
    }

    #[test]
    fn rejects_invalid_host_names() {
        let mut cases: Vec<String> = vec![
            "".into(),                 // empty
            ".".into(),                // only a trailing dot
            "..".into(),               // double dot
            "foo bar".into(),          // whitespace
            "foo\tbar".into(),         // tab
            "example.com:8080".into(), // colon (must be split as a port first)
            "foo/bar.com".into(),      // slash
            "foo\\bar.com".into(),     // backslash
            "../../tmp/x".into(),      // path traversal
            "foo..com".into(),         // empty label
            ".com".into(),             // leading empty label
            "example.com..".into(),    // trailing double dot
            "-foo.com".into(),         // label leading hyphen
            "foo-.com".into(),         // label trailing hyphen
            "-foo-.com".into(),        // both
            "例え.jp".into(),          // non-ASCII
        ];
        // A label longer than 63 chars and a host longer than 253 chars.
        cases.push(format!("{}.com", "a".repeat(64)));
        cases.push(format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(63)
        ));
        for input in &cases {
            assert!(
                normalize_host_name(input).is_err(),
                "host '{input}' must be rejected"
            );
        }
    }

    #[test]
    fn validates_site_host_addresses() {
        // (site address line, expected valid). The three audit cases — extra
        // port, whitespace, path traversal — must be rejected.
        let cases: &[(&str, bool)] = &[
            // Valid: single-label, multi-label, explicit port, trailing dot,
            // catch-all, uppercase (normalized by the parser).
            ("localhost", true),
            ("localhost:8080", true),
            ("example.com", true),
            ("example.com:443", true),
            ("api.example.com.", true),
            ("sub-domain.example.com", true),
            ("EXAMPLE.COM", true),
            ("a", true),
            (":8080", true),
            // Invalid.
            ("foo bar:8080", false),        // whitespace
            ("foo bar", false),             // whitespace
            ("../../tmp/x:443", false),     // path traversal
            ("foo/bar.com", false),         // slash
            ("foo\\bar.com", false),        // backslash
            ("example.com:8080:90", false), // extra port
            ("example.com:", false),        // empty port
            ("-foo.com", false),            // leading hyphen
            ("foo-.com", false),            // trailing hyphen
            ("foo..com", false),            // empty label
            ("example.com..", false),       // trailing double dot
            (".com", false),                // leading empty label
            ("例え.jp", false),             // non-ASCII
            (":0", false),                  // zero port
            ("foo.com:0", false),           // zero port
            ("foo:bar", false),             // non-numeric port
        ];

        for (addr, valid) in cases {
            let input = format!("{addr} {{\n    reverse_proxy 127.0.0.1:9000\n}}\n");
            assert_eq!(
                parse("test", &input).is_ok(),
                *valid,
                "site address '{addr}' expected {}",
                if *valid { "valid" } else { "invalid" }
            );
        }
    }

    #[test]
    fn parser_normalizes_site_host_case_and_dot() {
        // Uppercase + trailing dot collapse through the full parse path.
        let input = "API.Example.COM. {\n    reverse_proxy 127.0.0.1:9000\n}\n";
        let rf = parse("test", input).unwrap();
        assert!(matches!(
            &rf.sites[0].key,
            SiteKey::Named { host, port: 443 } if host == "api.example.com"
        ));
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

    #[test]
    fn parses_trusted_proxies_in_global_block() {
        let input = "{ trusted_proxies 10.0.0.0/8 172.16.0.0/12 127.0.0.1 }\n:80 { redir https://{host}{uri} permanent }\n";
        let rf = parse("test", input).unwrap();
        assert_eq!(rf.global.trusted_proxies.len(), 3);
        assert!(rf.global.trusted_proxies[0].contains("10.1.2.3".parse().unwrap()));
        assert!(!rf.global.trusted_proxies[0].contains("11.0.0.1".parse().unwrap()));
        // A bare address is a single host.
        assert!(rf.global.trusted_proxies[2].contains("127.0.0.1".parse().unwrap()));
        assert!(!rf.global.trusted_proxies[2].contains("127.0.0.2".parse().unwrap()));
    }

    #[test]
    fn parses_trusted_proxies_in_site_block() {
        let input =
            ":8080 {\n    trusted_proxies 10.0.0.0/8\n    reverse_proxy 127.0.0.1:9000\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::TrustedProxies { networks } => {
                assert_eq!(networks.len(), 1);
                assert!(networks[0].contains("10.9.8.7".parse().unwrap()));
            }
            other => panic!("expected trusted_proxies, got {other:?}"),
        }
    }

    #[test]
    fn parses_rate_limit_with_burst() {
        let input = ":8080 {\n    rate_limit remote_ip 1200r/m burst=2000\n    reverse_proxy 127.0.0.1:9000\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::RateLimit { spec } => {
                assert_eq!(spec.key, RateLimitKey::RemoteIp);
                assert_eq!(spec.count, 1200);
                assert_eq!(spec.unit, RateUnit::Minute);
                assert_eq!(spec.burst, 2000);
            }
            other => panic!("expected rate_limit, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_burst_defaults_to_rate() {
        let input = ":8080 {\n    rate_limit remote_ip 3r/s\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::RateLimit { spec } => assert_eq!(spec.burst, spec.count),
            other => panic!("expected rate_limit, got {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_rate() {
        let err = parse("test", ":8080 {\n    rate_limit remote_ip 50/s\n}\n").unwrap_err();
        assert!(err.to_string().contains("invalid rate"));
    }

    #[test]
    fn rejects_zero_burst() {
        let err = parse(
            "test",
            ":8080 {\n    rate_limit remote_ip 50r/s burst=0\n}\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("burst must be >= 1"));
    }

    #[test]
    fn rejects_unknown_rate_limit_key() {
        let err = parse("test", ":8080 {\n    rate_limit path /api 50r/s\n}\n").unwrap_err();
        assert!(err.to_string().contains("unknown rate_limit key"));
    }

    #[test]
    fn rejects_rate_limit_in_global_block() {
        let err = parse(
            "test",
            "{ rate_limit remote_ip 1r/s }\n:80 { redir / permanent }\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown global directive"));
    }

    #[test]
    fn rejects_bare_trusted_proxies() {
        let err = parse("test", "{ trusted_proxies }\n:80 { redir / permanent }\n").unwrap_err();
        assert!(err
            .to_string()
            .contains("trusted_proxies requires at least one CIDR"));
    }

    #[test]
    fn rejects_invalid_cidr() {
        let err = parse("test", ":8080 {\n    trusted_proxies 10.0.0.0/33\n}\n").unwrap_err();
        assert!(err.to_string().contains("CIDR prefix"));
    }

    #[test]
    fn rate_limit_errors_report_line_and_column() {
        let err = parse("test", ":8080 {\n    rate_limit remote_ip bad\n}\n").unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("test:2:5"),
            "expected file:line:col, got: {text}"
        );
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
            // v0.1.2: trusted_proxies + rate_limit grammar.
            "{ trusted_proxies 10.0.0.0/8 172.16.0.0/12 127.0.0.1 }\n:8080 {\n    rate_limit remote_ip 50r/s burst=100\n    reverse_proxy 127.0.0.1:9000\n}\napi.test:8081 {\n    trusted_proxies 192.168.0.0/16\n    rate_limit remote_ip 3r/d\n    redir /old permanent\n}\n",
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

    #[test]
    fn parses_general_matchers() {
        // `handle <matcher>` accepts matcher terms beyond the bare path
        // (spec §5.9), ANDed together.
        let input = "example.com {\n    handle method GET host api.example.com {\n        reverse_proxy 127.0.0.1:9000\n    }\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::Handle {
                matcher,
                directives,
            } => {
                assert_eq!(matcher.len(), 2);
                assert!(matches!(matcher[0], Matcher::Method(ref m) if m == "GET"));
                assert!(matches!(matcher[1], Matcher::Host(ref h) if h == "api.example.com"));
                assert_eq!(directives.len(), 1);
            }
            other => panic!("expected handle, got {other:?}"),
        }
    }

    #[test]
    fn parses_header_query_remote_ip_and_protocol_matchers() {
        let input = "example.com {\n    handle header Content-Type application/json query page 1 remote_ip 10.0.0.0/8 protocol https {\n        reverse_proxy 127.0.0.1:9000\n    }\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::Handle { matcher, .. } => {
                assert!(matches!(
                    matcher[0],
                    Matcher::Header { ref name, ref value }
                        if name == "Content-Type" && value == "application/json"
                ));
                assert!(matches!(
                    matcher[1],
                    Matcher::Query { ref key, ref value } if key == "page" && value == "1"
                ));
                assert!(matches!(matcher[2], Matcher::RemoteIp(_)));
                assert!(matches!(matcher[3], Matcher::Protocol(Protocol::Https)));
            }
            other => panic!("expected handle, got {other:?}"),
        }
    }

    #[test]
    fn parses_negated_matcher() {
        let input = "example.com {\n    handle !path /admin/* {\n        reverse_proxy 127.0.0.1:9000\n    }\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::Handle { matcher, .. } => {
                assert!(matches!(
                    matcher[0],
                    Matcher::Not(ref inner) if matches!(**inner, Matcher::Path(ref p) if p == "/admin")
                ));
            }
            other => panic!("expected handle, got {other:?}"),
        }
    }

    #[test]
    fn reverse_proxy_inline_matcher_is_general() {
        let input =
            ":8080 {\n    reverse_proxy method POST {\n        to 127.0.0.1:9000\n    }\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::ReverseProxy { matcher, to, .. } => {
                assert!(matches!(matcher.as_slice(), [Matcher::Method(ref m)] if m == "POST"));
                assert_eq!(to.len(), 1);
            }
            other => panic!("expected reverse_proxy, got {other:?}"),
        }
    }

    #[test]
    fn parses_handle_path_and_rewrite_and_respond_and_error() {
        let input = "example.com {\n    handle_path /api/* {\n        reverse_proxy 127.0.0.1:9000\n    }\n    rewrite /v1\n    respond 200 ok\n    error 503\n}\n";
        let rf = parse("test", input).unwrap();
        let directives = &rf.sites[0].directives;
        assert!(matches!(directives[0], Directive::HandlePath { .. }));
        assert!(matches!(directives[1], Directive::Rewrite { .. }));
        match &directives[2] {
            Directive::Respond { status, body } => {
                assert_eq!(*status, 200);
                assert_eq!(body.as_deref(), Some("ok"));
            }
            other => panic!("expected respond, got {other:?}"),
        }
        match &directives[3] {
            Directive::Error { status, .. } => assert_eq!(*status, Some(503)),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_matcher_with_missing_arguments() {
        let err = parse(
            "test",
            "example.com {\n    handle method {\n        reverse_proxy 127.0.0.1:9000\n    }\n}\n",
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("matcher 'method' is missing arguments"));
    }

    #[test]
    fn parses_basic_auth_and_forward_auth() {
        let input = "example.com {\n    basic_auth admin $2b$12$abcdef\n    forward_auth 127.0.0.1:9999\n    reverse_proxy 127.0.0.1:9000\n}\n";
        let rf = parse("test", input).unwrap();
        let directives = &rf.sites[0].directives;
        match &directives[0] {
            Directive::BasicAuth { user, hash } => {
                assert_eq!(user, "admin");
                assert_eq!(hash, "$2b$12$abcdef");
            }
            other => panic!("expected basic_auth, got {other:?}"),
        }
        assert!(matches!(directives[1], Directive::ForwardAuth { .. }));
        assert!(matches!(directives[2], Directive::ReverseProxy { .. }));
    }

    #[test]
    fn parses_rate_limit_header_key() {
        let input =
            ":8080 {\n    rate_limit header X-API-Key 10r/s\n    reverse_proxy 127.0.0.1:9000\n}\n";
        let rf = parse("test", input).unwrap();
        match &rf.sites[0].directives[0] {
            Directive::RateLimit { spec } => {
                assert_eq!(spec.key, RateLimitKey::Header("X-API-Key".to_string()));
                assert_eq!(spec.count, 10);
            }
            other => panic!("expected rate_limit, got {other:?}"),
        }
    }

    #[test]
    fn snippet_import_is_spliced() {
        // A `(name)` definition is captured and `import name` splices it
        // (spec §5.12), including inside a site block.
        let input = "(base) {\n    header_up X-Raddy yes\n}\n:8080 {\n    import base\n    reverse_proxy 127.0.0.1:9000\n}\n";
        let rf = parse("test", input).unwrap();
        assert_eq!(rf.sites.len(), 1);
        let directives = &rf.sites[0].directives;
        assert_eq!(directives.len(), 2);
        assert!(matches!(directives[0], Directive::HeaderUp { .. }));
        assert!(matches!(directives[1], Directive::ReverseProxy { .. }));
    }

    #[test]
    fn file_import_is_spliced() {
        let dir = std::env::temp_dir();
        let imported = dir.join(format!("raddy_import_{}.Raddyfile", std::process::id()));
        std::fs::write(&imported, "reverse_proxy 127.0.0.1:9000\n").unwrap();
        let input = format!(":8080 {{\n    import {}\n}}\n", imported.display());
        let rf = parse("test", &input).unwrap();
        assert_eq!(rf.sites[0].directives.len(), 1);
        assert!(matches!(
            rf.sites[0].directives[0],
            Directive::ReverseProxy { .. }
        ));
        let _ = std::fs::remove_file(&imported);
    }

    #[test]
    fn missing_import_file_is_an_error() {
        let err = parse(
            "test",
            ":8080 {\n    import /nonexistent/raddy_import_file\n}\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to read imported file"));
    }
}
