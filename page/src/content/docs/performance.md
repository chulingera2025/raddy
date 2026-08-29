---
title: Performance comparison
description: Compare Nginx, Caddy, and Raddy with a reproducible Docker benchmark and Nginx-relative charts.
---

The repository includes an independent Docker benchmark under
[`bench/`](https://github.com/chulingera2025/raddy/tree/main/bench). It starts
one proxy at a time against the same origin, uses the same scenario and
resource limits, and generates raw data plus Markdown, HTML, SVG, and PNG
reports.

## Run the benchmark

From the repository root:

```bash
./bench/scripts/run.sh quick
./bench/scripts/run.sh full
```

Run the report-script unit tests with:

```bash
./bench/scripts/run.sh test
```

The host only needs Docker Engine and Docker Compose v2. The load generator and
Matplotlib run inside the pinned benchmark containers. See the
[benchmark README](https://github.com/chulingera2025/raddy/tree/main/bench)
for the scenario matrix and tuning controls.

## Relative charts

The charts below are generated from the latest committed benchmark snapshot.
Each scenario is normalized independently:

```text
Nginx = 1.00x = 100%
```

![Relative maximum stable throughput](/benchmarks/throughput.svg)

![Relative p99 latency](/benchmarks/latency-p99.svg)

![Relative CPU cost per request](/benchmarks/cpu-per-request.svg)

![Relative peak memory](/benchmarks/memory.svg)

Throughput is higher-is-better. Latency, CPU per request, and memory are
lower-is-better. Error rate is shown as an absolute percentage and is not
normalized.

## Scenarios and limits

The full profile covers HTTP/1.1 keep-alive, response sizes, connection churn,
HTTPS/HTTP/1.1, HTTPS/HTTP/2 multiplexing, routing, and an HTTP/1.1 concurrency
scan. Each target is warmed up, measured, and repeated; repeated values are
aggregated by median. A stable point has an error rate no higher than 0.1%.

The suite does not compare ACME, caching, TCP/UDP, QUIC/HTTP3, or
implementation-specific features. The small `examples/loadtest.rs` program is
still available for a quick developer smoke test, but it is not the formal
cross-proxy benchmark.
