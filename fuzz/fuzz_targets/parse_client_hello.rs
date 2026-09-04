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

//! Fuzz target for the bounded TLS ClientHello inspector.
//!
//! The parser is a pure function over arbitrary bytes. It must classify every
//! input without panicking, while enforcing all record and handshake bounds.
//!
//! Run with (requires the nightly toolchain + `cargo-fuzz`):
//!
//! ```text
//! cargo +nightly fuzz run parse_client_hello -- -max_total_time=60
//! ```

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = raddex::layer4::tls::parse_client_hello_sni(data);
});
