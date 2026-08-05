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

use crate::config::ast::{CompiledConfig, CompiledSite, SiteKey};

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
}

/// Normalize a Host header value for site matching: strip `:port`, strip a
/// trailing dot, ASCII-lowercase.
fn normalize_host(raw: &[u8]) -> Normalized {
    let host = match raw.split(|&b| b == b':').next() {
        Some(host) => host,
        None => raw,
    };
    let host = match host.strip_suffix(b".") {
        Some(host) => host,
        None => host,
    };
    if host.is_empty() {
        return Normalized::Empty;
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

/// Select the site that serves a request on the given listener port.
///
/// `host` is `None` when the request carried no Host header.
pub fn select<'a>(config: &'a CompiledConfig, port: u16, host: Option<&[u8]>) -> Selection<'a> {
    let normalized = match host {
        None => return Selection::BadRequest,
        Some(raw) => match normalize_host(raw) {
            Normalized::Empty => return Selection::BadRequest,
            Normalized::NonAscii => None,
            Normalized::Matchable(h) => Some(h),
        },
    };

    let mut catch_all = None;
    for site in &config.sites {
        if site.key.port() != port {
            continue;
        }
        match &site.key {
            SiteKey::Named { host: named, .. } => {
                if normalized.as_deref() == Some(named.as_str()) {
                    return Selection::Site(site);
                }
            }
            SiteKey::CatchAll { .. } => catch_all = Some(site),
        }
    }
    match catch_all {
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
        }
    }

    #[test]
    fn named_site_matches_normalized_host() {
        // Hosts are normalized (lowercased) by the parser; the request side
        // applies the same normalization, so case/port/dot differences match.
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![named("api.example.com", 8080), catch_all(80)],
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
        };
        assert!(matches!(
            select(&config, 8080, Some(b"other.example.com")),
            Selection::Site(site) if matches!(site.key, SiteKey::CatchAll { .. })
        ));
    }

    #[test]
    fn missing_host_is_bad_request() {
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![catch_all(80)],
        };
        assert!(matches!(select(&config, 80, None), Selection::BadRequest));
    }

    #[test]
    fn malformed_host_is_bad_request() {
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![catch_all(80)],
        };
        assert!(matches!(
            select(&config, 80, Some(b":8080")),
            Selection::BadRequest
        ));
    }

    #[test]
    fn unmatched_host_is_not_found() {
        let config = CompiledConfig {
            global: Default::default(),
            sites: vec![named("api.example.com", 8080)],
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
        };
        // 例え.jp is non-ASCII → cannot match → catch-all serves.
        assert!(matches!(
            select(&config, 80, Some("例え.jp".as_bytes())),
            Selection::Site(_)
        ));
    }
}
