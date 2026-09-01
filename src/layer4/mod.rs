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

//! Layer 4 proxy subsystem (L4_PROXY_PLAN).
//!
//! The subsystem owns transport concepts only — listener identity, upstream
//! resolution/selection, connection limits, timeouts, and relay — and never
//! represents transport choices as HTTP [`crate::config::ast::TerminalKind`]
//! variants.
//!
//! **The L4 data path is native Tokio.** Each listener binds its own socket,
//! runs its own accept loop, terminates its own TLS, and relays with
//! `tokio::io`; nothing it forwards passes through Pingora. Pingora remains the
//! process host — listeners register as `BackgroundService`s, observe
//! `ShutdownWatch`, and use `Fds` for the zero-downtime-upgrade descriptor
//! handoff — but that is lifecycle, not data. HTTP keeps using the Pingora
//! proxy engine (`crate::proxy`).

pub mod tcp;
pub mod tls;
pub mod tls_accept;
pub mod udp;

use crate::config::ast::Layer4Listener;

/// A short, filesystem-safe key for one listener's upgrade-handoff artifacts.
///
/// The listener address appears in paths beside the upgrade socket, and an
/// address contains `:` and `/` — so it is hashed (FNV-1a) rather than escaped.
/// Shared by the TCP and UDP handoffs so both derive the same key shape.
#[cfg(unix)]
pub(crate) fn handoff_key(listener: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in listener.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Publish a handoff artifact atomically: write a private temporary file, sync
/// it, then rename into place, so a reader never observes a partial file.
///
/// `what` names the artifact in error messages (e.g. `"TCP handoff status"`).
#[cfg(unix)]
pub(crate) fn write_handoff_file(path: &str, payload: &[u8], what: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temp = format!("{path}.tmp.{}", std::process::id());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| format!("create {what}: {e}"))?;
    file.write_all(payload)
        .map_err(|e| format!("write {what}: {e}"))?;
    file.sync_data().map_err(|e| format!("sync {what}: {e}"))?;
    std::fs::rename(&temp, path).map_err(|e| format!("publish {what}: {e}"))?;
    Ok(())
}

/// The set of layer-4 listeners in a compiled snapshot, in config order.
pub fn listeners(compiled: &crate::config::ast::CompiledConfig) -> &[Layer4Listener] {
    &compiled.layer4
}
