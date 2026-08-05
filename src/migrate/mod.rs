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

//! Migration: convert a common Caddyfile / nginx.conf subset into a Raddyfile.
//!
//! This is an **independent external converter** (ARCHITECTURE §7): it emits
//! Raddyfile text using only existing directives and never back-influences the
//! Raddyfile grammar or parser. The CLI validates the emitted Raddyfile before
//! printing, so a converter bug surfaces as an error rather than an unparseable
//! config. Unsupported source syntax is skipped and reported as a warning —
//! nothing is silently dropped.

use std::fmt;
use std::path::Path;

pub mod caddy;
pub mod nginx;

/// The source configuration format for `raddy import`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ImportFormat {
    Caddyfile,
    Nginx,
}

impl fmt::Display for ImportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportFormat::Caddyfile => write!(f, "caddyfile"),
            ImportFormat::Nginx => write!(f, "nginx"),
        }
    }
}

/// A converted Raddyfile plus warnings about skipped or approximated syntax.
#[derive(Debug, Default)]
pub struct Converted {
    /// The emitted Raddyfile (clean; a valid config when non-empty).
    pub raddyfile: String,
    /// Human-readable warnings, one per skipped/approximated source item.
    pub warnings: Vec<String>,
}

/// A migration failure.
#[derive(Debug)]
pub enum MigrateError {
    /// The source file could not be read.
    Io {
        file: String,
        source: std::io::Error,
    },
    /// The source could not be converted (structural problem, not a warning).
    Invalid { message: String },
}

impl fmt::Display for MigrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrateError::Io { file, source } => {
                write!(f, "failed to read {file}: {source}")
            }
            MigrateError::Invalid { message } => write!(f, "invalid source: {message}"),
        }
    }
}

impl std::error::Error for MigrateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MigrateError::Io { source, .. } => Some(source),
            MigrateError::Invalid { .. } => None,
        }
    }
}

/// Convert a source config file into a Raddyfile.
pub fn import(format: ImportFormat, path: &Path) -> Result<Converted, MigrateError> {
    let input = std::fs::read_to_string(path).map_err(|source| MigrateError::Io {
        file: path.display().to_string(),
        source,
    })?;
    match format {
        ImportFormat::Caddyfile => {
            caddy::convert(&input).map_err(|message| MigrateError::Invalid { message })
        }
        ImportFormat::Nginx => {
            nginx::convert(&input).map_err(|message| MigrateError::Invalid { message })
        }
    }
}

// ---------------------------------------------------------------------------
// Shared minimal statement parser (line-oriented, brace-nested)
// ---------------------------------------------------------------------------

/// One parsed statement: a directive name + arguments, optionally a nested
/// block of statements.
#[derive(Debug)]
pub(super) struct Stmt {
    /// 1-based source line (for warnings).
    pub line: usize,
    /// The directive name and its arguments (no `{`, `}`, or `;`).
    pub words: Vec<String>,
    /// Nested statements when the line opened a block; empty otherwise.
    pub block: Vec<Stmt>,
}

/// Parse a line-oriented config into a statement tree.
///
/// Handles both the Caddyfile style (`directive arg {`, `}` on its own line)
/// and the nginx style (`directive arg;`). Blank lines and `#` comments are
/// skipped; a trailing `;` is stripped from the last word.
pub(super) fn parse_stmts(lines: &[&str]) -> Result<Vec<Stmt>, String> {
    let mut i = 0;
    let stmts = parse_into(lines, &mut i)?;
    if i < lines.len() && lines[i].trim() == "}" {
        return Err("unexpected '}'".to_string());
    }
    Ok(stmts)
}

fn parse_into(lines: &[&str], i: &mut usize) -> Result<Vec<Stmt>, String> {
    let mut out = Vec::new();
    while *i < lines.len() {
        let t = lines[*i].trim();
        if t.is_empty() || t.starts_with('#') {
            *i += 1;
            continue;
        }
        if t == "}" {
            break; // the caller consumes the closing brace
        }
        let line = *i + 1;
        let mut words: Vec<String> = t
            .split_whitespace()
            .map(|w| w.trim_end_matches(';').to_string())
            .collect();
        if words.is_empty() {
            *i += 1;
            continue;
        }
        if words.last().map(String::as_str) == Some("{") {
            words.pop();
            *i += 1;
            let block = parse_into(lines, i)?;
            if *i >= lines.len() || lines[*i].trim() != "}" {
                return Err(format!("unclosed '{{' opened on line {line}"));
            }
            *i += 1;
            out.push(Stmt { line, words, block });
            continue;
        }
        *i += 1;
        out.push(Stmt {
            line,
            words,
            block: Vec::new(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::parse_stmts;

    #[test]
    fn parses_nested_statements_and_semicolons() {
        let input = "\
server {
    listen 80;
    server_name example.com;
    location / {
        proxy_pass http://127.0.0.1:8080;
    }
}
example.com {
    reverse_proxy 127.0.0.1:8080
}
";
        let lines: Vec<&str> = input.lines().collect();
        let stmts = parse_stmts(&lines).unwrap();
        assert_eq!(stmts.len(), 2);
        let server = &stmts[0];
        assert_eq!(server.words[0], "server");
        assert_eq!(server.block.len(), 3);
        let location = &server.block[2];
        assert_eq!(location.words, ["location", "/"]);
        assert_eq!(
            location.block[0].words,
            ["proxy_pass", "http://127.0.0.1:8080"]
        );
        // The Caddy-style site is also a top-level block.
        let site = &stmts[1];
        assert_eq!(site.words, ["example.com"]);
        assert_eq!(site.block[0].words, ["reverse_proxy", "127.0.0.1:8080"]);
    }

    #[test]
    fn rejects_unclosed_block() {
        let lines: Vec<&str> = "server {\n    listen 80;\n".lines().collect();
        let err = parse_stmts(&lines).unwrap_err();
        assert!(err.contains("unclosed"));
    }

    #[test]
    fn strips_comments_and_blank_lines() {
        let input = "# comment\n\nserver {\n    # inner\n    listen 80;\n}\n";
        let lines: Vec<&str> = input.lines().collect();
        let stmts = parse_stmts(&lines).unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].block.len(), 1);
    }
}
