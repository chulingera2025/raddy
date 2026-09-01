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

//! Prometheus metrics (M5): QPS counter and request-duration histogram.
//!
//! Metrics are registered on the default Prometheus registry, so the metrics
//! listener ([`crate::server::startup`]'s `--metrics-addr`) reports them via
//! pingora's `PrometheusHttpApp`.

use prometheus::{Histogram, IntCounter};
use std::sync::LazyLock;

/// Total HTTP requests served by the proxy (QPS source).
pub static REQUESTS: LazyLock<IntCounter> = LazyLock::new(|| {
    prometheus::register_int_counter!(
        "raddex_requests_total",
        "Total HTTP requests served by raddex"
    )
    .expect("register raddex_requests_total")
});

/// Request duration in seconds.
pub static DURATION: LazyLock<Histogram> = LazyLock::new(|| {
    prometheus::register_histogram!(
        "raddex_request_duration_seconds",
        "HTTP request duration in seconds"
    )
    .expect("register raddex_request_duration_seconds")
});

/// Record one completed request: bump the counter and observe the duration.
pub fn record_request(duration_secs: f64) {
    REQUESTS.inc();
    DURATION.observe(duration_secs);
}
