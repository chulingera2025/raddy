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

//! Bounded TLS ClientHello SNI inspection (L4 P1).
//!
//! Reads a bounded prefix of a client's TLS stream until a complete
//! ClientHello is available, extracts the SNI from the `server_name`
//! extension, and forwards the *exact* buffered bytes upstream unchanged — it
//! never reconstructs or synthesizes a ClientHello, and never depends on
//! OS-level `peek`. Fragmented records are handled by accumulating bytes until
//! the ClientHello message is complete; the handshake header is assumed at the
//! start of the first record (the standard ClientHello layout).
//!
//! The parser is a pure function over a byte buffer, so every framing boundary
//! is unit-tested and covered by the dedicated `parse_client_hello` fuzz target
//! (the [`crate::config::parser`] no-panic discipline applies here too).

use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Upper bound on the inspected prefix. A ClientHello is normally a few hundred
/// bytes and well under this; an input that needs more is oversized/malformed.
pub const MAX_CLIENT_HELLO_BYTES: usize = 64 * 1024;

/// Per-read bound while assembling the ClientHello (an inspection timeout): a
/// client that trickles bytes forever must not pin a worker.
pub const CLIENT_HELLO_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The result of parsing a byte buffer as a ClientHello prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseOutcome {
    /// The buffer holds a complete ClientHello; this is its SNI (lowercased).
    Sni(String),
    /// The buffer holds a complete ClientHello with no `server_name` extension.
    NoSni,
    /// The buffer does not yet hold a complete ClientHello (need more bytes).
    NeedMore,
    /// The buffer is not a well-formed ClientHello, or exceeds the bound.
    Malformed,
}

/// The outcome of reading and inspecting a client's TLS prefix.
#[derive(Debug)]
pub enum InspectOutcome {
    /// SNI extracted; `prefix` is the exact bytes to forward upstream.
    Sni { name: String, prefix: Vec<u8> },
    /// A complete ClientHello with no SNI.
    NoSni { prefix: Vec<u8> },
    /// Malformed, oversized, EOF-before-ClientHello, or read timeout. `prefix`
    /// is whatever was read (possibly empty), so a fallback upstream still
    /// sees the exact bytes the client sent.
    Malformed { prefix: Vec<u8> },
}

/// Parse the SNI from a ClientHello prefix. Pure and total (no panics on any
/// input): every length is bounds-checked against the buffer.
pub fn parse_client_hello_sni(buf: &[u8]) -> ParseOutcome {
    // TLS record: content type (1) + version (2) + length (2).
    if buf.len() < 5 {
        return ParseOutcome::NeedMore;
    }
    if buf[0] != 0x16 {
        // Not a TLS handshake record.
        return ParseOutcome::Malformed;
    }
    let record_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    if record_len > MAX_CLIENT_HELLO_BYTES {
        return ParseOutcome::Malformed;
    }
    // The handshake message header sits at the start of the record payload.
    if buf.len() < 5 + 4 {
        return ParseOutcome::NeedMore;
    }
    if buf[5] != 0x01 {
        // Not a ClientHello handshake message.
        return ParseOutcome::Malformed;
    }
    let hello_len = ((buf[6] as usize) << 16) | ((buf[7] as usize) << 8) | buf[8] as usize;
    // The full ClientHello is the 4-byte handshake header plus `hello_len`
    // bytes of body, which may continue into the next record(s).
    let total = 5 + 4 + hello_len;
    if buf.len() < total {
        if total > MAX_CLIENT_HELLO_BYTES {
            return ParseOutcome::Malformed;
        }
        return ParseOutcome::NeedMore;
    }
    parse_hello(&buf[9..total])
}

/// Parse a ClientHello *body* (after the handshake header) for the SNI.
fn parse_hello(hello: &[u8]) -> ParseOutcome {
    // client_version (2) + random (32).
    if hello.len() < 34 {
        return ParseOutcome::Malformed;
    }
    let mut i = 34;
    // session_id: 1-byte length + data.
    if hello.len() < i + 1 {
        return ParseOutcome::Malformed;
    }
    let sid_len = hello[i] as usize;
    i += 1;
    if hello.len() < i + sid_len {
        return ParseOutcome::Malformed;
    }
    i += sid_len;
    // cipher_suites: 2-byte length + data.
    if hello.len() < i + 2 {
        return ParseOutcome::Malformed;
    }
    let cs_len = u16::from_be_bytes([hello[i], hello[i + 1]]) as usize;
    i += 2;
    if hello.len() < i + cs_len {
        return ParseOutcome::Malformed;
    }
    i += cs_len;
    // compression methods: 1-byte length + data.
    if hello.len() < i + 1 {
        return ParseOutcome::Malformed;
    }
    let cm_len = hello[i] as usize;
    i += 1;
    if hello.len() < i + cm_len {
        return ParseOutcome::Malformed;
    }
    i += cm_len;
    // extensions: 2-byte total length, then (type, length, data) triples.
    if hello.len() < i + 2 {
        // A ClientHello with no extensions has no SNI.
        return ParseOutcome::NoSni;
    }
    let ext_len = u16::from_be_bytes([hello[i], hello[i + 1]]) as usize;
    i += 2;
    if hello.len() < i + ext_len {
        return ParseOutcome::Malformed;
    }
    let ext_end = i + ext_len;
    while i + 4 <= ext_end {
        let ext_type = u16::from_be_bytes([hello[i], hello[i + 1]]);
        let len = u16::from_be_bytes([hello[i + 2], hello[i + 3]]) as usize;
        i += 4;
        if i + len > ext_end {
            return ParseOutcome::Malformed;
        }
        if ext_type == 0x0000 {
            // server_name extension.
            return parse_server_name(&hello[i..i + len]);
        }
        i += len;
    }
    ParseOutcome::NoSni
}

/// Parse a `server_name` extension payload: a 2-byte name-list length, then
/// name entries (type + 2-byte length + bytes). Only `host_name` (type 0) is
/// meaningful; the first one is the SNI (RFC 6066).
fn parse_server_name(data: &[u8]) -> ParseOutcome {
    if data.len() < 2 {
        return ParseOutcome::Malformed;
    }
    let list_len = u16::from_be_bytes([data[0], data[1]]) as usize;
    let Some(list) = data.get(2..2 + list_len) else {
        return ParseOutcome::Malformed;
    };
    let mut i = 0;
    while i + 3 <= list.len() {
        let name_type = list[i];
        let name_len = u16::from_be_bytes([list[i + 1], list[i + 2]]) as usize;
        i += 3;
        if i + name_len > list.len() {
            return ParseOutcome::Malformed;
        }
        if name_type == 0 {
            let name = &list[i..i + name_len];
            return match std::str::from_utf8(name) {
                Ok(s) => ParseOutcome::Sni(s.to_ascii_lowercase()),
                Err(_) => ParseOutcome::Malformed,
            };
        }
        i += name_len;
    }
    ParseOutcome::NoSni
}

/// Read from `io` until a complete ClientHello is available (bounded by
/// `max_bytes` and a per-read timeout), returning the SNI and the exact prefix
/// bytes to forward upstream.
pub async fn read_client_hello<IO: AsyncRead + Unpin>(
    io: &mut IO,
    max_bytes: usize,
) -> InspectOutcome {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match parse_client_hello_sni(&buf) {
            ParseOutcome::Sni(name) => return InspectOutcome::Sni { name, prefix: buf },
            ParseOutcome::NoSni => return InspectOutcome::NoSni { prefix: buf },
            ParseOutcome::Malformed => return InspectOutcome::Malformed { prefix: buf },
            ParseOutcome::NeedMore => {
                if buf.len() >= max_bytes {
                    return InspectOutcome::Malformed { prefix: buf };
                }
                let n = match tokio::time::timeout(CLIENT_HELLO_READ_TIMEOUT, io.read(&mut chunk))
                    .await
                {
                    Ok(Ok(0)) | Err(_) => {
                        // EOF before a complete ClientHello, or a read timeout.
                        return InspectOutcome::Malformed { prefix: buf };
                    }
                    Ok(Err(_)) => return InspectOutcome::Malformed { prefix: buf },
                    Ok(Ok(n)) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a well-formed ClientHello carrying the given SNI (empty = no
    /// `server_name` extension).
    fn client_hello(sni: Option<&str>, extra_ext: bool) -> Vec<u8> {
        let mut hello = Vec::new();
        // client_version TLS 1.2 (0x0303)
        hello.extend_from_slice(&[0x03, 0x03]);
        // random (32 zero bytes)
        hello.extend_from_slice(&[0u8; 32]);
        // session_id: empty
        hello.push(0);
        // cipher_suites: one suite
        hello.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]);
        // compression: null
        hello.push(1);
        hello.push(0);
        // extensions
        let mut exts = Vec::new();
        if let Some(name) = sni {
            // server_name extension (type 0x0000): payload = 2-byte list
            // length + entries (type, 2-byte length, name).
            let name_bytes = name.as_bytes();
            let mut list = Vec::new();
            list.push(0); // host_name
            list.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
            list.extend_from_slice(name_bytes);
            let mut payload = Vec::new();
            payload.extend_from_slice(&(list.len() as u16).to_be_bytes());
            payload.extend_from_slice(&list);
            exts.extend_from_slice(&[0x00, 0x00]);
            exts.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            exts.extend_from_slice(&payload);
        }
        if extra_ext {
            // a harmless supported_versions extension to skip past
            exts.extend_from_slice(&[0x00, 0x2b, 0x00, 0x02, 0x03, 0x04]);
        }
        hello.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        hello.extend_from_slice(&exts);

        // Handshake header: ClientHello (0x01) + 3-byte length.
        let mut msg = vec![0x01];
        let len = hello.len();
        msg.extend_from_slice(&[
            ((len >> 16) & 0xff) as u8,
            ((len >> 8) & 0xff) as u8,
            (len & 0xff) as u8,
        ]);
        msg.extend_from_slice(&hello);

        // TLS record: handshake (0x16) + version + 2-byte length.
        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend_from_slice(&(msg.len() as u16).to_be_bytes());
        rec.extend_from_slice(&msg);
        rec
    }

    #[test]
    fn extracts_sni() {
        let ch = client_hello(Some("Api.Example.COM"), true);
        assert_eq!(
            parse_client_hello_sni(&ch),
            ParseOutcome::Sni("api.example.com".to_string())
        );
    }

    #[test]
    fn no_sni_extension() {
        let ch = client_hello(None, true);
        assert_eq!(parse_client_hello_sni(&ch), ParseOutcome::NoSni);
    }

    #[test]
    fn fragmented_client_hello_accumulates() {
        // The parser returns NeedMore until enough bytes; a single fragmented
        // input must eventually yield the SNI. Feed it in two halves.
        let ch = client_hello(Some("example.com"), false);
        let half = ch.len() / 2;
        assert_eq!(
            parse_client_hello_sni(&ch[..half]),
            ParseOutcome::NeedMore,
            "a partial ClientHello needs more bytes"
        );
        assert_eq!(
            parse_client_hello_sni(&ch),
            ParseOutcome::Sni("example.com".to_string())
        );
    }

    #[test]
    fn rejects_non_handshake_and_truncated() {
        assert_eq!(
            parse_client_hello_sni(b"GET / HTTP/1.1"),
            ParseOutcome::Malformed
        );
        assert_eq!(
            parse_client_hello_sni(&[0x16, 0x03, 0x01]),
            ParseOutcome::NeedMore
        );
        // Not a ClientHello message type.
        let mut ch = client_hello(Some("example.com"), false);
        ch[5] = 0x02;
        assert_eq!(parse_client_hello_sni(&ch), ParseOutcome::Malformed);
    }

    #[test]
    fn oversize_is_rejected() {
        // A huge (malicious) declared ClientHello length must be Malformed,
        // not silently buffered.
        let mut buf = vec![0x16, 0x03, 0x01, 0xff, 0xff];
        buf.extend_from_slice(&[0x01, 0xff, 0xff, 0xff]);
        assert_eq!(parse_client_hello_sni(&buf), ParseOutcome::Malformed);
    }
}
