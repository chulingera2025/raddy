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
//! [`DnsProvider`] is the runtime trait a provider implements; the config only
//! carries a [`DnsProviderKind`] plus the API token, and [`build`] constructs
//! the client. The whole surface for adding a provider is: implement the trait
//! for the new client, add a [`DnsProviderKind`] variant, and add a `build`
//! arm — a small, self-contained contribution.

use crate::config::ast::DnsProviderKind;
use crate::server::cloudflare::Cloudflare;

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

/// Build the runtime provider client for a configured provider kind (spec §5.3).
pub fn build(provider: DnsProviderKind, api_token: &str) -> Result<Box<dyn DnsProvider>, String> {
    match provider {
        DnsProviderKind::Cloudflare => Ok(Box::new(Cloudflare::new(api_token))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_constructs_the_configured_provider() {
        // The factory wires a provider kind to its concrete client; exercising
        // the client itself is the cloudflare module's test concern.
        let provider = build(DnsProviderKind::Cloudflare, "test-token").expect("cloudflare");
        let _ = provider;
    }
}
