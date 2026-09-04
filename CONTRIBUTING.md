# Contributing to Raddex

Thanks for considering a contribution. This document covers the checks every
change must pass, the one rule that is easy to trip over, and a step-by-step
walkthrough for the contribution the project most needs right now: **a new
DNS-01 provider**.

## Before you start

```bash
cargo build --locked
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI runs exactly these four and treats any clippy warning as an error. A source
build needs stable Rust, OpenSSL development headers, and CMake.

### The one rule that is easy to trip over

The Raddexfile is a **public interface**. `docs/RADDEXFILE_SPEC.md` is its
source of truth, and the red line at the top of that file is real: *any syntax
not specified there must be added there before it is implemented.* A PR that
introduces new configuration syntax without updating the spec (and its Chinese
translation, `docs/RADDEXFILE_SPEC.zh_CN.md`) will be asked to do so first.

Adding a DNS-01 provider does **not** introduce new syntax — the grammar is
already general — so it only needs a row in the provider table.

### Comments and documentation

Every public item carries a doc comment stating purpose, parameters, return
value, and errors. Inline comments explain *why*, not what. This is a house
style the whole tree follows; please match the surrounding code.

## Adding a DNS-01 provider

DNS-01 providers are **data, not code paths**. The parser, the validator, the
error messages, and `raddex check` all read the registry in
`src/server/dns/mod.rs` and never name a specific provider. That means adding
one touches exactly two files, plus docs and tests.

### 1. Write the client

Create `src/server/dns/<provider>.rs` implementing two traits:

```rust
use super::{DnsProvider, DnsRecord};

/// A <Provider> DNS-01 client.
pub struct MyProvider {
    // credentials + a bounded HTTP agent
}

/// Handle returned by `present`, used to delete the record afterwards.
struct RecordHandle {
    // whatever identifies the record for deletion
}

impl DnsRecord for RecordHandle {
    fn cleanup(self: Box<Self>) -> Result<(), String> {
        // delete the TXT record
    }
}

impl DnsProvider for MyProvider {
    fn present(&self, host: &str, dns_value: &str) -> Result<Box<dyn DnsRecord>, String> {
        // publish `_acme-challenge.<host>` TXT = dns_value
    }
}
```

Two constraints on the client, both load-bearing:

- **Every network call must be bounded.** The ACME issuance worker runs on its
  own single-threaded runtime and serializes orders, so blocking I/O here is
  fine — but a call with no timeout hangs issuance for every host. Use a `ureq`
  agent with finite connect/read/write timeouts, as `cloudflare.rs` does.
- **`cleanup` must be safe to call after a failed order.** It runs from a
  `Drop` guard, including when the attempt timed out.

Take a private constructor that accepts an overridable API base URL (see
`Cloudflare::with_base`) — that is what makes the provider testable without
real credentials.

### 2. Register it

In `src/server/dns/mod.rs`, add the module and one `DnsProviderSpec`:

```rust
pub mod myprovider;
use myprovider::MyProvider;

static MYPROVIDER: DnsProviderSpec = DnsProviderSpec {
    keyword: "myprovider",
    fields: &[
        DnsCredentialField {
            name: "access_key_id",
            required: true,
            description: "the API access key id",
        },
        DnsCredentialField {
            name: "secret_access_key",
            required: true,
            description: "the API secret",
        },
        DnsCredentialField {
            name: "region",
            required: false,
            description: "API region, defaults to the provider's global endpoint",
        },
    ],
    // Only for a provider with exactly one required credential; otherwise None,
    // which makes the block form mandatory.
    shorthand_field: None,
    build: |credentials| {
        Ok(Box::new(MyProvider::new(
            credentials.require("access_key_id")?,
            credentials.require("secret_access_key")?,
            credentials.get("region"),
        )))
    },
};

pub static REGISTRY: &[&DnsProviderSpec] = &[&CLOUDFLARE, &MYPROVIDER];
```

That is the whole integration. The config below now parses, validates, and
issues certificates with no other change:

```caddyfile
{
    acme_email ops@example.com
    dns_challenge myprovider {
        access_key_id     {$MYPROVIDER_KEY_ID}
        secret_access_key {$MYPROVIDER_SECRET}
        region            eu-west-1
    }
}
```

You get these for free, derived from `fields`:

- `invalid dns_challenge provider 'x' (expected cloudflare, myprovider)`
- `dns_challenge myprovider requires 'access_key_id' (the API access key id)`
- `unknown myprovider credential 'regoin' (expected access_key_id, ...)`
- `duplicate dns_challenge myprovider credential 'region'`
- rejection of an empty required value, and `raddex check` coverage

**Credentials are secrets.** Do not add a `Debug` impl that prints them;
`DnsCredentials` already redacts every value, and there is a test asserting it.

### 3. Test it

Providers are tested against an in-test HTTP server, not the real API — see the
`MockApi` in `src/server/dns/cloudflare.rs`, which binds `127.0.0.1:0`, answers
the provider's API calls, and records what was created so the test can assert
the TXT record name and content. Copy that shape and cover at least:

- `present` publishes `_acme-challenge.<host>` with the right value;
- the returned handle's `cleanup` deletes that record;
- an API error becomes an `Err` rather than a panic or a hang.

The registry itself is already covered: `registry_entries_are_self_consistent`
catches a duplicate keyword, an empty field list, and a `shorthand_field` that
names a field you did not declare.

### 4. Document it

- Add a row to the provider table in `docs/RADDEXFILE_SPEC.md` §5.3 **and**
  `docs/RADDEXFILE_SPEC.zh_CN.md` §5.3.
- Note the required API permission (the Cloudflare row names
  `Zone: DNS: Edit`); this is the single most common setup mistake.
- Add a `CHANGELOG.md` entry under `Unreleased` → `Added`.

If you cannot test against the real provider, say so in the PR. A provider with
a mocked test and an untested live path is still useful — it just needs a
maintainer or a user with an account to confirm before release.

## Pull requests

- One logical change per PR.
- Explain the *why* in the description; the diff already shows the what.
- Note anything you could not verify, and how you tested what you could.
