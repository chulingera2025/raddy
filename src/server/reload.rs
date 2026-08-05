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

//! SIGHUP-triggered config hot reload (ADR-010).
//!
//! Pingora's Server handles only SIGINT/SIGTERM/SIGQUIT, so SIGHUP is free for
//! Raddy. A dedicated thread re-reads and re-validates the Raddyfile on every
//! SIGHUP, atomically swapping the snapshot on success and keeping the old one
//! on failure. Listeners are never touched (ADR-010).

use crate::config::snapshot::{self, ConfigStore};
use crate::proxy::lb::LoadBalancerPool;
use signal_hook::consts::SIGHUP;
use std::path::PathBuf;
use std::sync::Arc;

/// Spawn a background thread that performs the hot reload.
///
/// The thread owns a handle to the [`ConfigStore`]; on success it stores the
/// new snapshot, on failure it logs the error and the previous snapshot stays
/// in service (Q6). The `PathBuf` is owned so the thread outlives the caller.
/// The load-balancing pool is reconciled against the new snapshot so removed
/// sites stop their health probes.
pub fn spawn(config_path: PathBuf, store: Arc<ConfigStore>, lb_pool: Arc<LoadBalancerPool>) {
    std::thread::spawn(move || {
        let mut signals = match signal_hook::iterator::Signals::new([SIGHUP]) {
            Ok(signals) => signals,
            Err(e) => {
                tracing::error!("failed to install SIGHUP handler: {e}");
                return;
            }
        };
        for _ in signals.forever() {
            match snapshot::build(&config_path) {
                Ok(new_snapshot) => {
                    lb_pool.reconcile(&new_snapshot);
                    store.store(new_snapshot);
                    tracing::info!("config reloaded from {}", config_path.display());
                }
                Err(e) => {
                    tracing::error!("config reload failed, keeping previous config: {e}");
                }
            }
        }
    });
}
