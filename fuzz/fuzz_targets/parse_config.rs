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

//! Fuzz target for the Raddexfile parser: the parser must never panic, only
//! return `Ok` or `Err`.
//!
//! Run with (requires the nightly toolchain + `cargo-fuzz`):
//!
//! ```text
//! cargo +nightly fuzz run parse_config -- -max_total_time=60
//! ```
//!
//! The equivalent no-panic coverage runs on stable in the parser's unit tests
//! (`random_inputs_never_panic` / `mutated_configs_never_panic`).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The parser takes `&str`; lossy conversion lets arbitrary bytes (even
    // invalid UTF-8) still exercise every lexer/parser path.
    let input = String::from_utf8_lossy(data);
    let _ = raddex::config::parser::parse("fuzz", &input);
});
