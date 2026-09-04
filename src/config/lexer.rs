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

//! Raddexfile lexer (M2/M3 subset).
//!
//! Line-oriented tokenizer that produces positioned [`Token`]s. Braces that
//! open/close blocks (`{` / `}`) are separate tokens, but a `{name}`
//! *placeholder* inside a value (e.g. `header_up X-Real-IP {remote_host}`) is
//! lexed as a single word so it survives to value parsing. `#` starts a comment
//! that runs to end of line. Each token records its 1-based `line`/`col` so the
//! parser can report precise error locations (M3 acceptance).

/// A lexical token and its source position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// 1-based line in the source file.
    pub line: u32,
    /// 1-based column in the source file.
    pub col: u32,
}

/// The kind of a [`Token`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// A bare word (identifier, address, path, value fragment).
    Word(String),
    /// A block-opening brace (`{`).
    LBrace,
    /// A block-closing brace (`}`).
    RBrace,
    /// A statement terminator (newline).
    Newline,
}

/// Tokenize a Raddexfile.
///
/// The lexer is lenient: structural errors (unbalanced braces, unknown
/// directives) are left for the parser to report with a position.
pub fn lex(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    let mut line: u32 = 1;
    let mut col: u32 = 1;

    while let Some(&c) = chars.peek() {
        match c {
            '\n' => {
                chars.next();
                tokens.push(Token {
                    kind: TokenKind::Newline,
                    line,
                    col,
                });
                line += 1;
                col = 1;
            }
            // Skip all other whitespace (spaces, tabs, CR, and exotic spaces
            // such as form feed). The word arm below stops at whitespace
            // without consuming it, so every whitespace char must be consumed
            // here or the lexer loops forever on it.
            c if c.is_whitespace() => {
                chars.next();
                col += 1;
            }
            '{' => {
                let start_line = line;
                let start_col = col;
                chars.next();
                col += 1;
                // A `{name}` placeholder has a `}` before any whitespace; any
                // other `{` is a block brace. Look ahead from just past `{`.
                let mut lookahead = chars.clone();
                let mut is_placeholder = false;
                for c2 in lookahead.by_ref() {
                    if c2 == '}' {
                        is_placeholder = true;
                        break;
                    }
                    if c2.is_whitespace() || c2 == '{' {
                        break;
                    }
                }
                if is_placeholder {
                    let mut word = String::from("{");
                    while let Some(&c2) = chars.peek() {
                        word.push(c2);
                        chars.next();
                        col += 1;
                        if c2 == '}' {
                            break;
                        }
                    }
                    tokens.push(Token {
                        kind: TokenKind::Word(word),
                        line: start_line,
                        col: start_col,
                    });
                } else {
                    tokens.push(Token {
                        kind: TokenKind::LBrace,
                        line: start_line,
                        col: start_col,
                    });
                }
            }
            '}' => {
                chars.next();
                tokens.push(Token {
                    kind: TokenKind::RBrace,
                    line,
                    col,
                });
                col += 1;
            }
            '#' => {
                // Comment to end of line.
                while let Some(&c2) = chars.peek() {
                    if c2 == '\n' {
                        break;
                    }
                    chars.next();
                    col += 1;
                }
            }
            _ => {
                let start_line = line;
                let start_col = col;
                let mut word = String::new();
                while let Some(&c2) = chars.peek() {
                    if c2.is_whitespace() || c2 == '{' || c2 == '}' || c2 == '#' {
                        break;
                    }
                    word.push(c2);
                    chars.next();
                    col += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Word(word),
                    line: start_line,
                    col: start_col,
                });
            }
        }
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_basic_structure() {
        let toks =
            lex("{ log_level info }\n:80 {\n    redir https://{host}{uri} permanent\n}\n").unwrap();
        assert!(toks.iter().any(|t| t.kind == TokenKind::LBrace));
        assert!(toks
            .iter()
            .any(|t| t.kind == TokenKind::Word("log_level".into())));
        assert!(toks
            .iter()
            .any(|t| t.kind == TokenKind::Word("{host}".into())));
        assert!(toks.iter().any(|t| t.kind == TokenKind::RBrace));
    }

    #[test]
    fn placeholder_braces_stay_in_word() {
        let toks = lex("header_up X-Real-IP {remote_host}\n").unwrap();
        assert!(toks
            .iter()
            .any(|t| t.kind == TokenKind::Word("{remote_host}".into())));
        assert!(!toks.iter().any(|t| t.kind == TokenKind::LBrace));
    }

    #[test]
    fn tracks_line_and_column() {
        let toks = lex(":80 {\n    reverse_proxy 127.0.0.1:8080\n}\n").unwrap();
        // `:80` is on line 1 col 1.
        let first = toks
            .iter()
            .find(|t| t.kind == TokenKind::Word(":80".into()))
            .unwrap();
        assert_eq!((first.line, first.col), (1, 1));
        // `reverse_proxy` is on line 2, column 5 (after 4 spaces).
        let rp = toks
            .iter()
            .find(|t| t.kind == TokenKind::Word("reverse_proxy".into()))
            .unwrap();
        assert_eq!((rp.line, rp.col), (2, 5));
    }

    #[test]
    fn strips_comments() {
        let toks = lex("reverse_proxy 127.0.0.1:8080 # upstream\n").unwrap();
        assert_eq!(
            toks,
            vec![
                Token {
                    kind: TokenKind::Word("reverse_proxy".into()),
                    line: 1,
                    col: 1
                },
                Token {
                    kind: TokenKind::Word("127.0.0.1:8080".into()),
                    line: 1,
                    col: 15
                },
                Token {
                    kind: TokenKind::Newline,
                    line: 1,
                    col: 40
                },
            ]
        );
    }

    #[test]
    fn block_braces_are_tokens() {
        let toks = lex("api.example.com {\n").unwrap();
        assert!(toks.iter().any(|t| t.kind == TokenKind::LBrace));
    }

    #[test]
    fn exotic_whitespace_is_consumed_not_word_chars() {
        // Regression: form feed, vertical tab and NBSP used to enter the word
        // arm, which broke without consuming the char and looped forever
        // pushing empty words (OOM). They must be skipped like plain spaces.
        let toks = lex("\x0c\x0b\u{00a0}:80 {\n").unwrap();
        assert!(toks.iter().any(|t| t.kind == TokenKind::Word(":80".into())));
        assert!(
            !toks
                .iter()
                .any(|t| matches!(&t.kind, TokenKind::Word(w) if w.is_empty())),
            "no empty words may be produced: {toks:?}"
        );
    }
}
