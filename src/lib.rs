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

//! Raddy — a minimal high-performance reverse proxy gateway built on Pingora.
//!
//! The crate is organized as a library (modules consumed by the integration
//! tests) plus a thin binary entry point:
//!
//! * [`config`] — Raddyfile parsing (incl. `import`/snippets/`{$ENV}`), validation,
//!   and the atomic config snapshot.
//! * [`proxy`] — the Pingora request plane: site selection, matchers, guards,
//!   load balancing, compression, and forwarding.
//! * [`server`] — startup, SIGHUP reload, the `run`/`check`/`import` CLI, ACME.
//! * [`tls`] — certificate store, the SNI dynamic-certificate callback, and the
//!   per-site `tls` options.
//! * [`observ`] — Prometheus metrics.
//! * [`migrate`] — Caddyfile/nginx.conf → Raddyfile converter (ARCHITECTURE §7).

pub mod config;
pub mod layer4;
pub mod migrate;
pub mod observ;
pub mod proxy;
pub mod server;
pub mod tls;
