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

//! Config snapshot and atomic store.
//!
//! [`ConfigSnapshot`] is the pure-data compiled config that the request plane
//! reads. [`ConfigStore`] holds it behind an `arc-swap` pointer so a reload
//! swaps the whole snapshot atomically (ADR-011). [`build`] is the single
//! read → parse → validate → compile pipeline shared by startup, SIGHUP
//! reload, and `raddex check` (Q7).

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::ast::{CompiledConfig, ConfigError};
use crate::config::parser;
use crate::config::validate;

/// The immutable, fully-validated configuration served by the request plane.
pub type ConfigSnapshot = CompiledConfig;

/// Atomic holder for the current snapshot.
///
/// Requests load the current snapshot at the start of processing; a reload
/// stores a new one. Old snapshots stay alive for in-flight requests. Note:
/// `ArcSwap<T>` is itself `ArcSwapAny<Arc<T>>`, so the stored type is the
/// snapshot directly (not `Arc<ConfigSnapshot>`).
#[derive(Debug)]
pub struct ConfigStore {
    current: ArcSwap<ConfigSnapshot>,
}

impl ConfigStore {
    /// Create a store holding the given initial snapshot.
    pub fn new(initial: ConfigSnapshot) -> Self {
        Self {
            current: ArcSwap::from(Arc::new(initial)),
        }
    }

    /// Load the current snapshot.
    pub fn load(&self) -> Arc<ConfigSnapshot> {
        self.current.load_full()
    }

    /// Atomically replace the current snapshot.
    pub fn store(&self, snapshot: ConfigSnapshot) {
        self.current.store(Arc::new(snapshot));
    }
}

/// Build a config snapshot from a Raddexfile: read, parse, validate, compile —
/// one atomic step (Q6). On any error nothing is produced and nothing changes.
pub fn build(path: &Path) -> Result<ConfigSnapshot, ConfigError> {
    let file = path.display().to_string();
    let input = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        file: file.clone(),
        source,
    })?;
    let raddexfile = parser::parse(&file, &input)?;
    validate::validate_and_compile(&file, &raddexfile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_swap_is_visible() {
        let snapshot = build(Path::new("examples/Raddexfile")).unwrap();
        let store = ConfigStore::new(snapshot);
        let a = store.load();
        assert_eq!(a.sites.len(), 2);

        let b = build(Path::new("examples/Raddexfile")).unwrap();
        store.store(b);
        assert_eq!(store.load().sites.len(), 2);
    }
}
