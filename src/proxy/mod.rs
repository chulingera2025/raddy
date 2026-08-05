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

//! The request plane: the Pingora `ProxyHttp` implementation.
//!
//! Site selection is per listener (ADR/CONTEXT: "site selection"). M2 serves
//! plain-HTTP listeners only; SNI matching on TLS listeners lands in M4.

pub mod compress;
pub mod fs;
pub mod handler;
pub mod lb;
pub mod site;
