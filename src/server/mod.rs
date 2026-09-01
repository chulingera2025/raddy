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

//! The service layer: CLI, startup sequence, SIGHUP reload, and the
//! zero-downtime binary upgrade.
//!
//! Reload never changes listener topology (ADR-010); topology changes go through
//! the zero-downtime binary upgrade (`raddex upgrade`, ADR-008).

pub mod acme;
pub mod cli;
pub mod dns;
pub mod issuance_queue;
pub mod reload;
pub mod startup;
pub mod upgrade;
