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

//! DNS-01 challenge providers (spec §5.3).
//!
//! # Adding a provider
//!
//! Providers are data, not code paths: the parser, the validator, and the ACME
//! worker all read [`REGISTRY`] and never name a specific provider. Adding one
//! touches exactly two places, both in this directory:
//!
//! 1. a new `src/server/dns/<provider>.rs` implementing [`DnsProvider`] and
//!    [`DnsRecord`];
//! 2. one `mod` line plus one [`DnsProviderSpec`] entry in [`REGISTRY`] below.
//!
//! Nothing else changes — the `dns_challenge` grammar, the "unknown provider"
//! error, the required/unknown-credential checks, and `raddex check` all derive
//! from the spec's [`DnsProviderSpec::fields`]. See `CONTRIBUTING.md`.
//!
//! # Credentials
//!
//! A provider declares the credential fields it needs; the parser collects them
//! from either the one-line shorthand or the block form and hands back a
//! [`DnsCredentials`] map. Every credential *value* is treated as a secret and
//! is redacted from `Debug` output — field names are kept so a misparse is still
//! diagnosable.

pub mod cloudflare;

use cloudflare::Cloudflare;
use std::fmt;

/// A provider-specific DNS-01 record handle: removes the TXT record published
/// by [`DnsProvider::present`] once the ACME order has been validated.
pub trait DnsRecord: Send + Sync {
    /// Remove the challenge TXT record.
    fn cleanup(self: Box<Self>) -> Result<(), String>;
}

/// A DNS-01 challenge provider: publishes and removes the
/// `_acme-challenge.<host>` TXT record proving domain control (spec §5.3).
pub trait DnsProvider: Send + Sync {
    /// Publish the challenge TXT record for `host` carrying `key_authorization`,
    /// returning a handle that removes it via [`DnsRecord::cleanup`].
    fn present(&self, host: &str, dns_value: &str) -> Result<Box<dyn DnsRecord>, String>;
}

/// One credential a provider accepts inside a `dns_challenge` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DnsCredentialField {
    /// The directive keyword inside the block, e.g. `api_token`.
    pub name: &'static str,
    /// Whether issuance cannot proceed without it. Optional fields are for
    /// values with a working default (a region, an API base URL).
    pub required: bool,
    /// One line describing the value, shown in the configuration reference.
    pub description: &'static str,
}

/// The credential values parsed from one `dns_challenge` block.
///
/// Ordered and tiny (a handful of entries), so lookups are a linear scan rather
/// than a map. Values are secrets: [`Debug`] prints field names but redacts
/// every value.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DnsCredentials {
    entries: Vec<(String, String)>,
}

impl DnsCredentials {
    /// Build a credential set from `(name, value)` pairs.
    pub fn from_pairs(entries: Vec<(String, String)>) -> Self {
        Self { entries }
    }

    /// The value configured for `name`, if any.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    /// The value configured for `name`, or an error naming the missing field.
    ///
    /// A provider's `build` uses this for its required fields; the parser has
    /// already rejected a config that omits one, so an error here means the
    /// registry entry and the provider disagree.
    pub fn require(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("missing required credential '{name}'"))
    }

    /// The configured field names, in configuration order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(name, _)| name.as_str())
    }
}

impl fmt::Debug for DnsCredentials {
    /// Redacts every credential value; a leaked token in a log or a panic
    /// message is worse than an unreadable `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = f.debug_struct("DnsCredentials");
        for (name, _) in &self.entries {
            out.field(name, &"<redacted>");
        }
        out.finish()
    }
}

/// The static description of one DNS-01 provider: its keyword, the credentials
/// it accepts, and how to construct its client.
pub struct DnsProviderSpec {
    /// The `dns_challenge` keyword selecting this provider, e.g. `cloudflare`.
    pub keyword: &'static str,
    /// The credentials the provider accepts, in reference-documentation order.
    pub fields: &'static [DnsCredentialField],
    /// The field the one-line form `dns_challenge <keyword> <value>` fills.
    /// `None` for providers that need more than one credential and therefore
    /// require the block form.
    pub shorthand_field: Option<&'static str>,
    /// Construct the runtime client from validated credentials.
    pub build: fn(&DnsCredentials) -> Result<Box<dyn DnsProvider>, String>,
}

/// Compared by keyword: the registry holds one entry per keyword, so that is
/// the identity. A derived comparison would compare function pointers, which is
/// both meaningless and not guaranteed stable.
impl PartialEq for DnsProviderSpec {
    fn eq(&self, other: &Self) -> bool {
        self.keyword == other.keyword
    }
}

impl Eq for DnsProviderSpec {}

impl fmt::Debug for DnsProviderSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DnsProviderSpec")
            .field("keyword", &self.keyword)
            .finish()
    }
}

impl DnsProviderSpec {
    /// The field description for `name`, if the provider accepts it.
    fn field(&self, name: &str) -> Option<&'static DnsCredentialField> {
        self.fields.iter().find(|field| field.name == name)
    }

    /// The accepted field names, for error messages.
    fn field_names(&self) -> String {
        self.fields
            .iter()
            .map(|field| field.name)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Check `credentials` against this provider's declared fields: every
    /// required field present and non-empty, and no unknown field.
    ///
    /// This is the whole validation surface for a `dns_challenge` block, so a
    /// new provider gets it without touching the parser or the validator.
    pub fn validate(&self, credentials: &DnsCredentials) -> Result<(), String> {
        for name in credentials.names() {
            if self.field(name).is_none() {
                return Err(format!(
                    "unknown {} credential '{name}' (expected {})",
                    self.keyword,
                    self.field_names()
                ));
            }
        }
        for field in self.fields.iter().filter(|field| field.required) {
            match credentials.get(field.name) {
                None => {
                    return Err(format!(
                        "dns_challenge {} requires '{}' ({})",
                        self.keyword, field.name, field.description
                    ));
                }
                Some("") => {
                    return Err(format!(
                        "dns_challenge {} requires a non-empty '{}'",
                        self.keyword, field.name
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// Construct the runtime client. Callers validate first; a provider that
    /// still reports a missing credential means its registry entry is wrong.
    pub fn client(&self, credentials: &DnsCredentials) -> Result<Box<dyn DnsProvider>, String> {
        (self.build)(credentials)
    }
}

/// The Cloudflare provider: a single scoped API token.
static CLOUDFLARE: DnsProviderSpec = DnsProviderSpec {
    keyword: "cloudflare",
    fields: &[DnsCredentialField {
        name: "api_token",
        required: true,
        description: "a Cloudflare API token with Zone:DNS:Edit on the zone",
    }],
    shorthand_field: Some("api_token"),
    build: |credentials| Ok(Box::new(Cloudflare::new(credentials.require("api_token")?))),
};

/// Every DNS-01 provider Raddex can use. Adding an entry here (plus its module
/// above) is the entire integration surface — see the module documentation.
pub static REGISTRY: &[&DnsProviderSpec] = &[&CLOUDFLARE];

/// The provider registered under `keyword`, if any.
pub fn lookup(keyword: &str) -> Option<&'static DnsProviderSpec> {
    REGISTRY
        .iter()
        .copied()
        .find(|spec| spec.keyword == keyword)
}

/// The registered keywords, for "unknown provider" error messages.
pub fn keywords() -> String {
    REGISTRY
        .iter()
        .map(|spec| spec.keyword)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(pairs: &[(&str, &str)]) -> DnsCredentials {
        DnsCredentials::from_pairs(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn lookup_resolves_registered_providers_only() {
        assert_eq!(
            lookup("cloudflare").map(|spec| spec.keyword),
            Some("cloudflare")
        );
        assert!(lookup("route53").is_none());
    }

    #[test]
    fn registry_entries_are_self_consistent() {
        // A registry entry that names a shorthand field it does not declare
        // would make the one-line form unparseable, and duplicate keywords would
        // make `lookup` order-dependent. Both are contributor mistakes worth
        // catching here rather than in a config error.
        let mut seen = Vec::new();
        for spec in REGISTRY {
            assert!(
                !seen.contains(&spec.keyword),
                "duplicate provider keyword '{}'",
                spec.keyword
            );
            seen.push(spec.keyword);
            assert!(
                !spec.fields.is_empty(),
                "provider '{}' declares no credentials",
                spec.keyword
            );
            if let Some(shorthand) = spec.shorthand_field {
                let field = spec.field(shorthand).unwrap_or_else(|| {
                    panic!("'{}' shorthand names an undeclared field", spec.keyword)
                });
                assert!(
                    field.required,
                    "'{}' shorthand must fill a required field",
                    spec.keyword
                );
            }
        }
    }

    #[test]
    fn validate_requires_declared_required_fields() {
        let spec = lookup("cloudflare").expect("cloudflare");
        assert!(spec.validate(&creds(&[("api_token", "tok")])).is_ok());

        let message = spec.validate(&creds(&[])).unwrap_err();
        assert!(message.contains("requires 'api_token'"), "got: {message}");

        let message = spec.validate(&creds(&[("api_token", "")])).unwrap_err();
        assert!(message.contains("non-empty"), "got: {message}");
    }

    #[test]
    fn validate_rejects_unknown_fields() {
        let spec = lookup("cloudflare").expect("cloudflare");
        let message = spec
            .validate(&creds(&[("api_token", "tok"), ("region", "us-east-1")]))
            .unwrap_err();
        assert!(
            message.contains("unknown cloudflare credential 'region'"),
            "got: {message}"
        );
    }

    #[test]
    fn client_constructs_the_registered_provider() {
        let spec = lookup("cloudflare").expect("cloudflare");
        assert!(spec.client(&creds(&[("api_token", "tok")])).is_ok());
    }

    #[test]
    fn debug_redacts_credential_values_but_keeps_names() {
        // A token must never reach a log line or a panic message.
        let rendered = format!("{:?}", creds(&[("api_token", "super-secret-token")]));
        assert!(!rendered.contains("super-secret-token"), "got: {rendered}");
        assert!(rendered.contains("api_token"), "got: {rendered}");
    }
}
