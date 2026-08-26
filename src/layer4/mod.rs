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
//! Raw TCP proxying ships first (P0), UDP follows (P2). The subsystem owns
//! transport concepts only — listener identity, upstream resolution/selection,
//! connection limits, timeouts, and relay — and never represents transport
//! choices as HTTP [`crate::config::ast::TerminalKind`] variants.
//!
//! Each raw-TCP listener runs as its own Pingora `Service<ServerApp>`: Pingora
//! owns accept, socket options, and the inherited-listener (upgrade) handling;
//! the [`tcp::TcpProxyApp`] owns upstream selection, connect, relay, admission,
//! and accounting.

pub mod tcp;
pub mod tls;
pub mod udp;

use crate::config::ast::Layer4Listener;

/// The set of layer-4 listeners in a compiled snapshot, in config order.
pub fn listeners(compiled: &crate::config::ast::CompiledConfig) -> &[Layer4Listener] {
    &compiled.layer4
}
