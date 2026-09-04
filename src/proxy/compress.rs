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

//! Response compression for the `encode` directive (M5).
//!
//! Implements gzip, zstd, and brotli compression honoring the `encode` parameter
//! order (priority) and the client's `Accept-Encoding` (RADDEXFILE_SPEC §5): the
//! first configured algorithm the client accepts is used.
//!
//! B3b1: the reverse-proxy path compresses incrementally through [`Encoder`] —
//! one continuous gzip member / zstd frame / brotli stream per response — instead
//! of buffering the whole body. B3b2: the `file_server` terminal streams through
//! the same [`Encoder`] for full (200) responses; the whole-buffer [`compress`]
//! helper is retained only as a one-shot convenience for tests.

use crate::config::ast::Encoding;
use http::HeaderValue;
use std::io::{self, Write};

/// A concrete compression algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algo {
    Gzip,
    Zstd,
    Brotli,
}

/// Responses smaller than this many bytes are never compressed: the codec
/// framing (gzip header/footer, zstd frame, brotli stream) makes a tiny body
/// *larger* after encoding, so the encoding is pure overhead (spec §5.11).
pub const MIN_COMPRESS_BYTES: usize = 64;

impl Algo {
    /// The `Content-Encoding` token for this algorithm.
    pub fn token(self) -> &'static str {
        match self {
            Algo::Gzip => "gzip",
            Algo::Zstd => "zstd",
            Algo::Brotli => "br",
        }
    }
}

/// Choose the encoding for a request given the site's `encode` priorities and
/// the client's `Accept-Encoding` header. `None` means no compression.
///
/// Matching is case-insensitive (RFC 9110 §12.5.3). A `q` weight of `0` marks an
/// algorithm as not acceptable; an explicit token match always overrides a `*`
/// wildcard, so `gzip;q=0, *;q=1` excludes gzip but admits zstd. Among the
/// configured algorithms the first one the client accepts wins, which makes the
/// config order the preference on equal supported quality.
pub fn choose(encode: &[Encoding], accept_encoding: Option<&HeaderValue>) -> Option<Algo> {
    if encode.is_empty() {
        return None;
    }
    let header = accept_encoding?.to_str().ok()?;
    let entries = parse_accept_encoding(header);
    for enc in encode {
        let algo = match enc {
            Encoding::Gzip => Algo::Gzip,
            Encoding::Zstd => Algo::Zstd,
            Encoding::Brotli => Algo::Brotli,
        };
        if accepted(&entries, algo.token()) {
            return Some(algo);
        }
    }
    None
}

/// A parsed `Accept-Encoding` entry: a lowercased token and its q weight.
struct Entry {
    token: String,
    q: f32,
}

/// Parse an `Accept-Encoding` header into (token, q) entries, lowercasing the
/// tokens so matching is case-insensitive. Entries without a `;q=` default to
/// weight 1.0; an unparsable weight is also treated as 1.0 so that a malformed
/// header does not silently disable compression.
fn parse_accept_encoding(header: &str) -> Vec<Entry> {
    header
        .split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (name, rest) = match entry.split_once(';') {
                Some((name, rest)) => (name, Some(rest)),
                None => (entry, None),
            };
            let token = name.trim().to_ascii_lowercase();
            if token.is_empty() {
                return None;
            }
            let q = rest.map(parse_q).unwrap_or(1.0);
            Some(Entry { token, q })
        })
        .collect()
}

/// Whether the client accepts `token` given the parsed entries.
///
/// The most specific reference wins: an explicit `token` entry (any q) beats a
/// `*` wildcard, so `gzip;q=0, *;q=1` excludes gzip but admits other
/// algorithms. For duplicated tokens the first occurrence wins.
fn accepted(entries: &[Entry], token: &str) -> bool {
    if let Some(entry) = entries.iter().find(|e| e.token == token) {
        return entry.q > 0.0;
    }
    match entries.iter().find(|e| e.token == "*") {
        Some(entry) => entry.q > 0.0,
        None => false,
    }
}

/// Parse a `;q=...` weight (defaults to 1.0). The parameter name is matched
/// case-insensitively (`Q=` is accepted).
fn parse_q(rest: &str) -> f32 {
    let rest = rest.trim();
    let value = rest
        .strip_prefix("q=")
        .or_else(|| rest.strip_prefix("Q="))
        .and_then(|v| v.trim().parse().ok());
    value.unwrap_or(1.0)
}

/// A per-request incremental compression encoder (B3b1).
///
/// Writes compressed bytes into an internal `Vec` that the caller drains after
/// each [`write`](Self::write), so memory stays bounded by the codec state plus
/// the current chunk — the whole response is never accumulated. One encoder is
/// created per compressed response and emits exactly one gzip member or zstd
/// frame.
pub enum Encoder {
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
    // Boxed: the brotli state is far larger than the gzip/zstd codecs.
    Brotli(Box<brotli::CompressorWriter<Vec<u8>>>),
}

impl Encoder {
    /// Create an encoder for `algo`. Fails only if the underlying codec cannot
    /// be initialized (e.g. a zstd context allocation failure).
    pub fn new(algo: Algo) -> io::Result<Self> {
        match algo {
            Algo::Gzip => Ok(Encoder::Gzip(flate2::write::GzEncoder::new(
                Vec::new(),
                flate2::Compression::default(),
            ))),
            Algo::Zstd => Ok(Encoder::Zstd(zstd::stream::write::Encoder::new(
                Vec::new(),
                3,
            )?)),
            Algo::Brotli => Ok(Encoder::Brotli(Box::new(brotli::CompressorWriter::new(
                Vec::new(),
                4096,
                5,
                22,
            )))),
        }
    }

    /// Compress `chunk`, flush the codec, and return the compressed bytes
    /// produced so far. The flush makes the output decodable incrementally, so
    /// the caller forwards it downstream before the response ends.
    pub fn write(&mut self, chunk: &[u8]) -> io::Result<Vec<u8>> {
        match self {
            Encoder::Gzip(enc) => {
                enc.write_all(chunk)?;
                enc.flush()?;
                Ok(std::mem::take(enc.get_mut()))
            }
            Encoder::Zstd(enc) => {
                enc.write_all(chunk)?;
                enc.flush()?;
                Ok(std::mem::take(enc.get_mut()))
            }
            Encoder::Brotli(enc) => {
                enc.write_all(chunk)?;
                enc.flush()?;
                Ok(std::mem::take(enc.get_mut()))
            }
        }
    }

    /// Finalize the stream, returning the remaining compressed bytes (the gzip
    /// trailer / zstd frame / brotli stream footer). Call once after the last
    /// body chunk; the encoder must not be written again afterwards.
    pub fn finish(&mut self) -> io::Result<Vec<u8>> {
        match self {
            Encoder::Gzip(enc) => {
                enc.try_finish()?;
                Ok(std::mem::take(enc.get_mut()))
            }
            Encoder::Zstd(enc) => {
                enc.do_finish()?;
                Ok(std::mem::take(enc.get_mut()))
            }
            Encoder::Brotli(enc) => {
                // `into_inner` consumes the encoder, so swap in a throwaway
                // placeholder to move the real one out, then finalize it.
                let mut enc = std::mem::replace(
                    enc,
                    Box::new(brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22)),
                );
                enc.flush()?;
                Ok(enc.into_inner())
            }
        }
    }
}

/// Compress `body` with `algo` in one shot.
///
/// The request-plane paths stream through [`Encoder`] instead (B3b1/B3b2); this
/// whole-buffer helper remains for tests and callers that already own the full
/// payload.
pub fn compress(algo: Algo, body: &[u8]) -> Vec<u8> {
    match algo {
        Algo::Gzip => gzip(body),
        Algo::Zstd => zstd(body),
        Algo::Brotli => brotli(body),
    }
}

fn gzip(body: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    if encoder.write_all(body).is_err() {
        return body.to_vec();
    }
    encoder.finish().unwrap_or_else(|_| body.to_vec())
}

fn zstd(body: &[u8]) -> Vec<u8> {
    zstd::encode_all(body, 3).unwrap_or_else(|_| body.to_vec())
}

fn brotli(body: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
    let _ = encoder.write_all(body);
    encoder.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn chooses_first_priority_client_accepts() {
        // encode zstd gzip → zstd wins if the client accepts both.
        let encode = [Encoding::Zstd, Encoding::Gzip];
        assert_eq!(choose(&encode, Some(&hdr("gzip, zstd"))), Some(Algo::Zstd));
        // Client that only accepts gzip → gzip.
        assert_eq!(choose(&encode, Some(&hdr("gzip"))), Some(Algo::Gzip));
        // Client accepting neither → none.
        assert_eq!(choose(&encode, Some(&hdr("br"))), None);
    }

    #[test]
    fn wildcard_and_q0() {
        let encode = [Encoding::Zstd, Encoding::Gzip];
        // `*` accepts anything → first priority (zstd).
        assert_eq!(choose(&encode, Some(&hdr("*"))), Some(Algo::Zstd));
        // q=0 on the top priority excludes it → falls to gzip.
        assert_eq!(
            choose(&encode, Some(&hdr("zstd;q=0, gzip"))),
            Some(Algo::Gzip)
        );
        // No Accept-Encoding header → no compression.
        assert_eq!(choose(&encode, None), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let encode = [Encoding::Zstd, Encoding::Gzip];
        // Uppercase / mixed-case tokens still match the lowercased codec token.
        assert_eq!(choose(&encode, Some(&hdr("GZIP"))), Some(Algo::Gzip));
        assert_eq!(choose(&encode, Some(&hdr("gzip, ZSTD"))), Some(Algo::Zstd));
        // q=0 spelled with an uppercase `Q` is honored.
        assert_eq!(
            choose(&encode, Some(&hdr("ZSTD;Q=0, GZIP"))),
            Some(Algo::Gzip)
        );
    }

    #[test]
    fn explicit_token_q0_overrides_wildcard() {
        let encode = [Encoding::Zstd, Encoding::Gzip];
        // gzip is explicitly refused; the `*` must not resurrect it.
        assert_eq!(choose(&encode, Some(&hdr("gzip;q=0, *"))), Some(Algo::Zstd));
        // A `*;q=0` refuses everything not explicitly listed.
        assert_eq!(choose(&encode, Some(&hdr("*;q=0"))), None);
        // `*;q=0, gzip`: the wildcard refuses zstd, but gzip is explicit.
        assert_eq!(choose(&encode, Some(&hdr("*;q=0, gzip"))), Some(Algo::Gzip));
    }

    #[test]
    fn no_encode_directive_means_no_compression() {
        assert_eq!(choose(&[], Some(&hdr("gzip"))), None);
    }

    #[test]
    fn compress_roundtrips() {
        let body = b"hello world hello world".to_vec();
        for algo in [Algo::Gzip, Algo::Zstd, Algo::Brotli] {
            let compressed = compress(algo, &body);
            assert!(!compressed.is_empty());
            assert_ne!(compressed, body, "{algo:?} output should differ");
        }
    }

    /// Decode a complete compressed stream back to its payload.
    fn decode(algo: Algo, compressed: &[u8]) -> Vec<u8> {
        use std::io::Read;
        match algo {
            Algo::Gzip => {
                let mut decoder = flate2::read::GzDecoder::new(compressed);
                let mut decoded = Vec::new();
                decoder.read_to_end(&mut decoded).unwrap();
                decoded
            }
            Algo::Zstd => {
                let mut decoder = zstd::stream::read::Decoder::new(compressed).unwrap();
                let mut decoded = Vec::new();
                decoder.read_to_end(&mut decoded).unwrap();
                decoded
            }
            Algo::Brotli => {
                let mut decoder = brotli::Decompressor::new(compressed, 4096);
                let mut decoded = Vec::new();
                decoder.read_to_end(&mut decoded).unwrap();
                decoded
            }
        }
    }

    /// Feed `chunks` through one [`Encoder`], concatenating every flushed write
    /// plus the final `finish()` trailer, then decode the single continuous
    /// stream back to the original bytes.
    fn stream_roundtrip(algo: Algo, chunks: &[&[u8]]) -> Vec<u8> {
        let mut encoder = Encoder::new(algo).unwrap();
        let mut compressed = Vec::new();
        let n = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            compressed.extend_from_slice(&encoder.write(chunk).unwrap());
            if i == n - 1 {
                compressed.extend_from_slice(&encoder.finish().unwrap());
            }
        }
        decode(algo, &compressed)
    }

    #[test]
    fn multi_chunk_gzip_is_one_continuous_member() {
        let expected: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
        // Odd-sized chunks force the encoder through many incremental flushes.
        let chunks: Vec<&[u8]> = expected.chunks(8191).collect();
        assert!(chunks.len() > 1, "test needs multiple chunks");
        let decoded = stream_roundtrip(Algo::Gzip, &chunks);
        assert_eq!(decoded, expected);
    }

    #[test]
    fn multi_chunk_zstd_is_one_continuous_frame() {
        let expected: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let chunks: Vec<&[u8]> = expected.chunks(4096).collect();
        assert!(chunks.len() > 1, "test needs multiple chunks");
        let decoded = stream_roundtrip(Algo::Zstd, &chunks);
        assert_eq!(decoded, expected);
    }

    #[test]
    fn multi_chunk_brotli_is_one_continuous_stream() {
        let expected: Vec<u8> = (0..60_000u32).map(|i| (i % 257) as u8).collect();
        let chunks: Vec<&[u8]> = expected.chunks(8191).collect();
        assert!(chunks.len() > 1, "test needs multiple chunks");
        let decoded = stream_roundtrip(Algo::Brotli, &chunks);
        assert_eq!(decoded, expected);
    }

    #[test]
    fn empty_body_is_a_valid_empty_member() {
        for algo in [Algo::Gzip, Algo::Zstd, Algo::Brotli] {
            let mut encoder = Encoder::new(algo).unwrap();
            let mut compressed = encoder.write(&[]).unwrap();
            compressed.extend_from_slice(&encoder.finish().unwrap());
            assert!(!compressed.is_empty(), "{algo:?} must still emit a frame");
            assert!(decode(algo, &compressed).is_empty());
        }
    }
}
