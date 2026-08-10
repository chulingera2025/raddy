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

//! Static file serving for the `file_server` directive (M5).
//!
//! B3b2: files are streamed from disk in bounded chunks instead of being read
//! fully into memory, and the terminal serves single HTTP byte ranges
//! (RFC 9110 §14.2). Full (200) responses are compressed incrementally through
//! [`crate::proxy::compress::Encoder`] when the client accepts an algorithm;
//! partial (206) and 416 responses are never compressed.

use crate::config::ast::{Encoding, Modifier};
use crate::proxy::compress;
use bytes::Bytes;
use pingora::prelude::*;
use pingora::proxy::Session;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// How many bytes are read from disk per chunk while streaming a file. The
/// response body is built from one chunk plus codec state, so memory stays
/// bounded by this buffer regardless of file size.
const CHUNK_SIZE: usize = 64 * 1024;

/// Serve the request `path` from `root`, optionally compressing full responses
/// with the site's `encode` algorithms (M5, B3b2).
///
/// GET streams the file — or the requested single byte range — from disk in
/// [`CHUNK_SIZE`] chunks; HEAD returns the headers a GET would produce without
/// opening or reading the file body. `modifiers` are the effective
/// block/terminal modifiers: the site's `header_down` rewrites are applied to
/// the final response header exactly like the reverse-proxy path.
pub async fn serve(
    session: &mut Session,
    root: &str,
    path: &str,
    encode: &[Encoding],
    modifiers: &[Modifier],
    bytes_written: &mut usize,
) -> Result<()> {
    // Only GET and HEAD are served.
    let method = session.req_header().method.clone();
    if method != http::Method::GET && method != http::Method::HEAD {
        session.respond_error(405).await?;
        return Ok(());
    }
    let is_head = method == http::Method::HEAD;

    let Some(file_path) = resolve(root, path) else {
        session.respond_error(404).await?;
        return Ok(());
    };

    // Determine the file size without reading the body. HEAD never opens the
    // file; GET opens before any headers are committed, so an open/stat failure
    // is still served as a plain 404 rather than a broken 200.
    let (mut file, size) = if is_head {
        let meta = match tokio::fs::metadata(&file_path).await {
            Ok(meta) => meta,
            Err(_) => {
                session.respond_error(404).await?;
                return Ok(());
            }
        };
        (None, meta.len())
    } else {
        let file = match tokio::fs::File::open(&file_path).await {
            Ok(file) => file,
            Err(_) => {
                session.respond_error(404).await?;
                return Ok(());
            }
        };
        let size = match file.metadata().await {
            Ok(meta) => meta.len(),
            Err(_) => {
                session.respond_error(404).await?;
                return Ok(());
            }
        };
        (Some(file), size)
    };

    // Resolve the requested range (RFC 9110 §14.2) against the file size.
    let range = parse_range(session.req_header().headers.get(http::header::RANGE), size);

    // The status, the exact body byte count to send, and (for a partial
    // response) the half-open byte interval to read from the file.
    let (status, body_len, stream_range) = match range {
        RangeSpec::None => (200u16, size, None),
        RangeSpec::Satisfiable { start, end } => (206, end - start, Some((start, end))),
        RangeSpec::Unsatisfiable => {
            // 416: the request named a byte range that cannot be served as a
            // single range. The response carries the file size and no body.
            let mut resp = ResponseHeader::build(416, None)?;
            resp.insert_header(http::header::CONTENT_RANGE, format!("bytes */{size}"))?;
            resp.insert_header(http::header::ACCEPT_RANGES, "bytes")?;
            crate::proxy::handler::apply_header_down(modifiers, session, &mut resp);
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(());
        }
    };

    let is_partial = status == 206;
    // Compression is negotiated only for full GET responses: partial (206)
    // ranges must be byte-exact and are never compressed, and HEAD is served
    // un-compressed with the full Content-Length so it never reads the body.
    let algo = if is_partial || is_head {
        None
    } else {
        compress::choose(
            encode,
            session
                .req_header()
                .headers
                .get(http::header::ACCEPT_ENCODING),
        )
    };

    let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
    let mut resp = ResponseHeader::build(status, None)?;
    resp.insert_header(http::header::CONTENT_TYPE, mime.as_ref())?;
    resp.insert_header(http::header::ACCEPT_RANGES, "bytes")?;
    if let Some(algo) = algo {
        resp.insert_header(http::header::CONTENT_ENCODING, algo.token())?;
        // The compressed length is unknown until the stream finishes, so no
        // Content-Length is set; pingora frames the response close-delimited.
    } else {
        resp.insert_header(http::header::CONTENT_LENGTH, body_len.to_string())?;
    }
    if let Some((start, end)) = stream_range {
        // Content-Range uses an inclusive end (RFC 9110 §14.4).
        resp.insert_header(
            http::header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", end - 1, size),
        )?;
    }
    if status == 200 && !is_head && !encode.is_empty() {
        // The representation depends on the request's Accept-Encoding, so a
        // shared cache must vary on it — even when this particular client did
        // not ask for compression (RFC 9110 §12.5.3).
        crate::proxy::handler::merge_vary_accept_encoding(&mut resp);
    }
    // `header_down` applies to the final response header; a rewrite overrides
    // the terminal's own Content-Type/Content-Length/Content-Encoding, matching
    // the reverse-proxy path's overwrite behavior.
    crate::proxy::handler::apply_header_down(modifiers, session, &mut resp);

    // A HEAD request (or a bodyless uncompressed response) has no body: the
    // header is written with end-of-stream and the file body is never read.
    let no_body = is_head || (body_len == 0 && algo.is_none());
    if no_body {
        session.write_response_header(Box::new(resp), true).await?;
        return Ok(());
    }

    session.write_response_header(Box::new(resp), false).await?;

    // Position the file at the range start (if any).
    if let Some((start, _)) = stream_range {
        if start > 0 {
            file.as_mut()
                .expect("a GET response always has an open file")
                .seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| Error::because(InternalError, "failed to seek static file", e))?;
        }
    }

    let mut encoder = algo.map(compress::Encoder::new).transpose().map_err(|e| {
        Error::because(InternalError, "failed to initialize compression encoder", e)
    })?;

    // Stream the body in bounded chunks. Once Content-Encoding is committed,
    // every chunk goes through the incremental encoder; a read or codec failure
    // aborts the response rather than emitting raw bytes under the encoding
    // header (the Content-Length was removed, so no framing is broken).
    let mut remaining = body_len;
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = if want == 0 {
            0
        } else {
            let file = file
                .as_mut()
                .expect("a GET response always has an open file");
            match file.read(&mut buf[..want]).await {
                Ok(n) => n,
                Err(e) => {
                    return Err(Error::because(
                        InternalError,
                        "failed to read static file",
                        e,
                    ));
                }
            }
        };
        if n == 0 && remaining > 0 {
            // The file shrank between stat and read, so the committed
            // Content-Length / Content-Encoding can no longer be honored.
            return Err(Error::explain(
                InternalError,
                "static file truncated while streaming",
            ));
        }
        remaining -= n as u64;
        let last = remaining == 0;
        let raw = &buf[..n];
        if let Some(encoder) = encoder.as_mut() {
            let mut out = encoder
                .write(raw)
                .map_err(|e| Error::because(InternalError, "response compression failed", e))?;
            if last {
                out.extend(encoder.finish().map_err(|e| {
                    Error::because(InternalError, "response compression failed", e)
                })?);
            }
            // Count the compressed bytes actually sent (the access-log `%b`,
            // spec §5.13).
            *bytes_written += out.len();
            session
                .write_response_body(Some(Bytes::from(out)), last)
                .await?;
        } else {
            *bytes_written += n;
            session
                .write_response_body(Some(Bytes::copy_from_slice(raw)), last)
                .await?;
        }
        if last {
            break;
        }
    }
    Ok(())
}

/// The outcome of parsing a request `Range` header against a file of `size`.
enum RangeSpec {
    /// No range, or a non-`bytes` range unit (ignored per RFC 9110 §14.2).
    None,
    /// A satisfiable single byte range; `end` is exclusive, `start` inclusive.
    Satisfiable { start: u64, end: u64 },
    /// A `bytes` range that cannot be satisfied: start past EOF, an empty
    /// suffix, malformed syntax, or multiple ranges (multipart is out of scope).
    Unsatisfiable,
}

/// Parse a single `bytes=` range per RFC 9110 §14.2.
///
/// `start` is inclusive and the returned `end` is exclusive (the RFC's end is
/// inclusive, so it is bumped by one and capped at `size`). Returns
/// [`RangeSpec::None`] when the header is absent or uses another unit, and
/// [`RangeSpec::Unsatisfiable`] for any `bytes` range that cannot be served as
/// one continuous slice.
fn parse_range(header: Option<&http::HeaderValue>, size: u64) -> RangeSpec {
    let Some(header) = header else {
        return RangeSpec::None;
    };
    // Decode the header as UTF-8 text, not just visible ASCII: `HeaderValue::to_str`
    // rejects obs-text (>= 0x80) bytes, but a valid non-ASCII unit is still a range
    // unit and must be ignored rather than rejected. Genuinely non-UTF-8 bytes are
    // malformed and remain unsatisfiable.
    let Ok(value) = std::str::from_utf8(header.as_bytes()) else {
        return RangeSpec::Unsatisfiable;
    };
    // The range unit is case-insensitive (RFC 9110 §14.2); a non-`bytes` unit
    // is ignored rather than rejected. `get(..6)` (rather than byte indexing)
    // cannot panic when a multibyte character straddles the 6-byte boundary.
    let Some(prefix) = value.get(..6) else {
        return RangeSpec::None;
    };
    if !prefix.eq_ignore_ascii_case("bytes=") {
        return RangeSpec::None;
    }
    // The prefix matched as ASCII "bytes=", so byte 6 is a char boundary and
    // this slice is safe.
    let spec = &value[6..];
    // Multiple ranges require multipart/byteranges, which is out of scope:
    // reject the request as unsatisfiable rather than serving partial data.
    if spec.contains(',') {
        return RangeSpec::Unsatisfiable;
    }
    let spec = spec.trim();
    if spec.is_empty() {
        return RangeSpec::Unsatisfiable;
    }
    // Suffix range: the last N bytes.
    if let Some(suffix) = spec.strip_prefix('-') {
        let Ok(n) = suffix.trim().parse::<u64>() else {
            return RangeSpec::Unsatisfiable;
        };
        if n == 0 || size == 0 {
            return RangeSpec::Unsatisfiable;
        }
        let n = n.min(size);
        return RangeSpec::Satisfiable {
            start: size - n,
            end: size,
        };
    }
    // `start-end` or `start-`.
    let (start_str, end_part) = match spec.split_once('-') {
        Some((start, end)) => (start, end),
        None => return RangeSpec::Unsatisfiable,
    };
    let Ok(start) = start_str.trim().parse::<u64>() else {
        return RangeSpec::Unsatisfiable;
    };
    if start >= size {
        return RangeSpec::Unsatisfiable;
    }
    let end = if end_part.trim().is_empty() {
        size
    } else {
        let Ok(end_inclusive) = end_part.trim().parse::<u64>() else {
            return RangeSpec::Unsatisfiable;
        };
        // An over-large end just serves the rest of the file.
        end_inclusive.saturating_add(1).min(size)
    };
    if end <= start {
        // An empty range (e.g. `bytes=5-4`) is not satisfiable.
        return RangeSpec::Unsatisfiable;
    }
    RangeSpec::Satisfiable { start, end }
}

/// Resolve a request path under `root`, guarding against directory traversal.
///
/// Returns `None` for paths that escape the root or do not resolve to a file
/// (a directory resolves to its `index.html`).
fn resolve(root: &str, request_path: &str) -> Option<PathBuf> {
    // Cheap first guard: reject any `..` path segment.
    if request_path.split('/').any(|seg| seg == "..") {
        return None;
    }
    let root_path = Path::new(root);
    let rel = request_path.trim_start_matches('/');
    let candidate = root_path.join(rel);
    let canonical_root = std::fs::canonicalize(root_path).ok()?;
    let canonical = std::fs::canonicalize(&candidate).ok()?;
    if !canonical.starts_with(&canonical_root) {
        return None;
    }
    if canonical.is_dir() {
        let index = canonical.join("index.html");
        let index = std::fs::canonicalize(&index).ok()?;
        return index.starts_with(&canonical_root).then_some(index);
    }
    Some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range_header(s: &str) -> http::HeaderValue {
        http::HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn traversal_is_rejected() {
        assert!(resolve("/tmp", "/../../etc/passwd").is_none());
        assert!(resolve("/tmp", "/a/../b").is_none());
        assert!(resolve("/tmp", "/..").is_none());
    }

    #[test]
    fn missing_and_valid_paths() {
        // A non-existent path under an existing root resolves to None.
        let root = std::env::temp_dir();
        assert!(resolve(root.to_str().unwrap(), "/definitely_missing_raddy_file").is_none());
        // The root itself is a directory with no index.html → None.
        assert!(resolve(root.to_str().unwrap(), "/").is_none());
    }

    #[test]
    fn range_parsing_absent_or_unknown_unit_is_none() {
        assert!(matches!(parse_range(None, 100), RangeSpec::None));
        // A non-bytes unit is ignored per RFC 9110 §14.2.
        assert!(matches!(
            parse_range(Some(&range_header("items=0-4")), 100),
            RangeSpec::None
        ));
        // The unit is case-insensitive.
        assert!(matches!(
            parse_range(Some(&range_header("BYTES=0-4")), 100),
            RangeSpec::Satisfiable { start: 0, end: 5 }
        ));
    }

    #[test]
    fn range_parsing_non_ascii_prefix_never_panics() {
        // Regression: a non-bytes unit whose multibyte character straddles byte
        // 6 used to make the byte-sliced prefix check panic. The unit is
        // unknown, so the range is ignored per RFC 9110 §14.2.
        let value = http::HeaderValue::from_str("abcde\u{e9}").unwrap();
        assert!(matches!(parse_range(Some(&value), 100), RangeSpec::None));
    }

    #[test]
    fn range_parsing_normal() {
        assert!(matches!(
            parse_range(Some(&range_header("bytes=0-4")), 100),
            RangeSpec::Satisfiable { start: 0, end: 5 }
        ));
        assert!(matches!(
            parse_range(Some(&range_header("bytes=95-99")), 100),
            RangeSpec::Satisfiable {
                start: 95,
                end: 100
            }
        ));
        // An end past EOF is capped at the file size.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=95-500")), 100),
            RangeSpec::Satisfiable {
                start: 95,
                end: 100
            }
        ));
        // A single byte.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=5-5")), 100),
            RangeSpec::Satisfiable { start: 5, end: 6 }
        ));
    }

    #[test]
    fn range_parsing_open_ended() {
        assert!(matches!(
            parse_range(Some(&range_header("bytes=95-")), 100),
            RangeSpec::Satisfiable {
                start: 95,
                end: 100
            }
        ));
        // Open-ended from the start is the whole file.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=0-")), 100),
            RangeSpec::Satisfiable { start: 0, end: 100 }
        ));
    }

    #[test]
    fn range_parsing_suffix() {
        assert!(matches!(
            parse_range(Some(&range_header("bytes=-5")), 100),
            RangeSpec::Satisfiable {
                start: 95,
                end: 100
            }
        ));
        // A suffix longer than the file serves the whole file.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=-500")), 100),
            RangeSpec::Satisfiable { start: 0, end: 100 }
        ));
    }

    #[test]
    fn range_parsing_unsatisfiable() {
        // A start at or past EOF.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=100-")), 100),
            RangeSpec::Unsatisfiable
        ));
        assert!(matches!(
            parse_range(Some(&range_header("bytes=500-")), 100),
            RangeSpec::Unsatisfiable
        ));
        // An empty suffix is not a valid range.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=-0")), 100),
            RangeSpec::Unsatisfiable
        ));
        // An empty file cannot satisfy any bytes range.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=0-0")), 0),
            RangeSpec::Unsatisfiable
        ));
        // Reversed bounds.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=5-4")), 100),
            RangeSpec::Unsatisfiable
        ));
        // Malformed numbers.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=abc")), 100),
            RangeSpec::Unsatisfiable
        ));
        assert!(matches!(
            parse_range(Some(&range_header("bytes=0-abc")), 100),
            RangeSpec::Unsatisfiable
        ));
        // A spec with no dash is not a range.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=0")), 100),
            RangeSpec::Unsatisfiable
        ));
    }

    #[test]
    fn range_parsing_multiple_ranges_is_unsatisfiable() {
        // Multipart/byteranges is out of scope: any multi-range request is 416.
        assert!(matches!(
            parse_range(Some(&range_header("bytes=0-1,3-4")), 100),
            RangeSpec::Unsatisfiable
        ));
        assert!(matches!(
            parse_range(Some(&range_header("bytes=0-1,")), 100),
            RangeSpec::Unsatisfiable
        ));
    }
}
