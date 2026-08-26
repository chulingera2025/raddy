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

//! Per-listener site selection (Q3) and the 400/404 fallbacks (Q4).
//!
//! Selection is scoped to one listener: the candidate set is the sites whose
//! port matches the request's local listener port. Named sites are matched by
//! the normalized Host header; the `:port` catch-all serves when no named site
//! matches. A missing or malformed Host is a 400; a valid but unmatched Host
//! (with no catch-all) is a 404.

use crate::config::ast::{wildcard_match_specificity, CompiledConfig, CompiledSite, SiteKey};

/// The outcome of selecting a site for a request.
#[derive(Debug)]
pub enum Selection<'a> {
    /// A site will serve the request.
    Site(&'a CompiledSite),
    /// Host header missing or malformed → 400 (Q4).
    BadRequest,
    /// No site on this listener matches → 404 (Q4).
    NotFound,
}

/// The result of normalizing a Host header value.
enum Normalized {
    /// A matchable hostname.
    Matchable(String),
    /// The host was empty after stripping the port and trailing dot.
    Empty,
    /// Non-ASCII, so it can never match a named site in v0.1.
    NonAscii,
    /// The `:port` part was not a valid port number → 400 (RFC 9112 §3.2).
    InvalidPort,
    /// The host used an invalid or unbracketed IPv6 form → 400.
    InvalidHost,
}

/// Normalize a Host header value for site matching: strip `:port` (validating
/// it — `example.com:notaport` must not silently match `example.com`), strip a
/// trailing dot, ASCII-lowercase.
fn normalize_host(raw: &[u8]) -> Normalized {
    let (host, port) = if raw.first() == Some(&b'[') {
        let Some(close) = raw.iter().position(|&b| b == b']') else {
            return Normalized::InvalidHost;
        };
        let inner = &raw[1..close];
        if inner.is_empty()
            || std::str::from_utf8(inner)
                .ok()
                .and_then(|s| s.parse::<std::net::Ipv6Addr>().ok())
                .is_none()
        {
            return Normalized::InvalidHost;
        }
        let suffix = &raw[close + 1..];
        if suffix.is_empty() {
            (&raw[..=close], None)
        } else if let Some(port) = suffix.strip_prefix(b":") {
            (&raw[..=close], Some(port))
        } else {
            return Normalized::InvalidHost;
        }
    } else {
        match raw.iter().rposition(|&b| b == b':') {
            Some(idx) => (&raw[..idx], Some(&raw[idx + 1..])),
            None => (raw, None),
        }
    };
    if let Some(port) = port {
        if !is_valid_port(port) {
            return Normalized::InvalidPort;
        }
    }
    let host = match host.strip_suffix(b".") {
        Some(host) => host,
        None => host,
    };
    if host.is_empty() {
        return Normalized::Empty;
    }
    if host.contains(&b':') && !(host.first() == Some(&b'[') && host.last() == Some(&b']')) {
        return Normalized::InvalidHost;
    }
    let mut out = String::new();
    for &b in host {
        if !b.is_ascii() {
            return Normalized::NonAscii;
        }
        out.push((b as char).to_ascii_lowercase());
    }
    Normalized::Matchable(out)
}

/// Whether `bytes` is a valid TCP port number: 1–5 ASCII digits in 1..=65535
/// (RFC 9112 §3.2).
fn is_valid_port(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > 5 || bytes.iter().any(|b| !b.is_ascii_digit()) {
        return false;
    }
    matches!(
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| s.parse::<u32>().ok()),
        Some(1..=65535)
    )
}

/// Select the site that serves a request on the given listener port.
///
/// `host` is `None` when the request carried no Host header.
pub fn select<'a>(config: &'a CompiledConfig, port: u16, host: Option<&[u8]>) -> Selection<'a> {
    let normalized = match host {
        None => return Selection::BadRequest,
        Some(raw) => match normalize_host(raw) {
            Normalized::Empty | Normalized::InvalidPort | Normalized::InvalidHost => {
                return Selection::BadRequest;
            }
            Normalized::NonAscii => None,
            Normalized::Matchable(h) => Some(h),
        },
    };

    let mut catch_all = None;
    let mut wildcard = None;
    let mut wildcard_suffix_len = 0;
    for site in &config.sites {
        if site.key.port() != port {
            continue;
        }
        match &site.key {
            SiteKey::Named { host: named, .. } => {
                if normalized.as_deref() == Some(named.as_str()) {
                    return Selection::Site(site);
                }
                if let Some(host) = normalized.as_deref() {
                    if let Some(suffix_len) = wildcard_match_specificity(named, host) {
                        if suffix_len > wildcard_suffix_len {
                            wildcard = Some(site);
                            wildcard_suffix_len = suffix_len;
                        }
                    }
                }
            }
            SiteKey::CatchAll { .. } => catch_all = Some(site),
        }
    }
    match wildcard.or(catch_all) {
        Some(site) => Selection::Site(site),
        None => Selection::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ast::{CompiledSite, Terminal};

    fn catch_all(port: u16) -> CompiledSite {
        CompiledSite {
            key: SiteKey::CatchAll { port },
            terminals: Vec::<Terminal>::new(),
            modifiers: Vec::new(),
            trusted_proxies: None,
            tls: None,
            access_log_off: false,
        }
    }

    fn named(host: &str, port: u16) -> CompiledSite {
        CompiledSite {
            key: SiteKey::Named {
                host: host.to_string(),
                port,
            },
            terminals: Vec::new(),
            modifiers: Vec::new(),
            trusted_proxies: None,
            tls: None,
            access_log_off: false,
        }
    }

    #[test]
    fn named_site_matches_normalized_host() {
        // Hosts are normalized (lowercased) by the parser; the request side
        // applies the same normalization, so case/port/dot differences match.
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![named("api.example.com", 8080), catch_all(80)],
            layer4: vec![],
        };
        assert!(matches!(
            select(&config, 8080, Some(b"api.example.com")),
            Selection::Site(_)
        ));
        // Port and trailing dot are stripped; case is folded.
        assert!(matches!(
            select(&config, 8080, Some(b"API.EXAMPLE.COM.:8080")),
            Selection::Site(_)
        ));
    }

    #[test]
    fn catch_all_serves_when_no_named_match() {
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![named("api.example.com", 8080), catch_all(8080)],
            layer4: vec![],
        };
        assert!(matches!(
            select(&config, 8080, Some(b"other.example.com")),
            Selection::Site(site) if matches!(site.key, SiteKey::CatchAll { .. })
        ));
    }

    #[test]
    fn wildcard_matches_one_label_and_prefers_specific_suffix() {
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![
                named("*.example.com", 8080),
                named("*.sub.example.com", 8080),
                catch_all(8080),
            ],
            layer4: vec![],
        };
        assert!(matches!(
            select(&config, 8080, Some(b"api.example.com")),
            Selection::Site(site)
                if matches!(&site.key, SiteKey::Named { host, .. } if host == "*.example.com")
        ));
        assert!(matches!(
            select(&config, 8080, Some(b"api.sub.example.com")),
            Selection::Site(site)
                if matches!(&site.key, SiteKey::Named { host, .. } if host == "*.sub.example.com")
        ));
        assert!(matches!(
            select(&config, 8080, Some(b"deep.api.example.com")),
            Selection::Site(site) if matches!(site.key, SiteKey::CatchAll { .. })
        ));
        assert!(matches!(
            select(&config, 8080, Some(b"example.com")),
            Selection::Site(site) if matches!(site.key, SiteKey::CatchAll { .. })
        ));
    }

    #[test]
    fn bracketed_ipv6_host_matches_with_or_without_port() {
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![named("[::1]", 8080)],
            layer4: vec![],
        };
        assert!(matches!(
            select(&config, 8080, Some(b"[::1]:8080")),
            Selection::Site(_)
        ));
        assert!(matches!(
            select(&config, 8080, Some(b"[::1]")),
            Selection::Site(_)
        ));
    }

    #[test]
    fn missing_host_is_bad_request() {
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![catch_all(80)],
            layer4: vec![],
        };
        assert!(matches!(select(&config, 80, None), Selection::BadRequest));
    }

    #[test]
    fn malformed_host_is_bad_request() {
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![catch_all(80)],
            layer4: vec![],
        };
        assert!(matches!(
            select(&config, 80, Some(b":8080")),
            Selection::BadRequest
        ));
    }

    #[test]
    fn invalid_host_port_is_bad_request_not_a_match() {
        // `Host: api.example.com:notaport` must not silently strip the port and
        // match the named site (RFC 9112 §3.2 requires 400).
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![named("api.example.com", 8080)],
            layer4: vec![],
        };
        for bad in [
            b"api.example.com:notaport".as_slice(),
            b"api.example.com:99999".as_slice(),
            b"api.example.com:0".as_slice(),
            b"api.example.com:".as_slice(),
        ] {
            assert!(
                matches!(select(&config, 8080, Some(bad)), Selection::BadRequest),
                "{bad:?} must be a 400, not a site match"
            );
        }
        // A valid port still matches.
        assert!(matches!(
            select(&config, 8080, Some(b"api.example.com:8080")),
            Selection::Site(_)
        ));
    }

    #[test]
    fn unmatched_host_is_not_found() {
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![named("api.example.com", 8080)],
            layer4: vec![],
        };
        assert!(matches!(
            select(&config, 8080, Some(b"other.example.com")),
            Selection::NotFound
        ));
    }

    #[test]
    fn non_ascii_host_falls_through_to_catch_all() {
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![catch_all(80)],
            layer4: vec![],
        };
        // 例え.jp is non-ASCII → cannot match → catch-all serves.
        assert!(matches!(
            select(&config, 80, Some("例え.jp".as_bytes())),
            Selection::Site(_)
        ));
    }
}
