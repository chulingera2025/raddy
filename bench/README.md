# Raddy proxy comparison benchmark

This directory contains an independent Docker benchmark for Nginx, Caddy, and
Raddy. It measures only capabilities shared by the three HTTP proxies and
normalizes every chart against Nginx.

## Requirements

- Docker Engine with Docker Compose v2;
- enough local CPU, memory, and disk for the pinned build images;
- a Linux host is recommended for comparable container scheduling.

The host does not need Rust, Python packages, Matplotlib, or a load generator.
The origin server, oha, report generator, and proxy builds run in containers.

## Run it

From the repository root:

```bash
./bench/scripts/run.sh quick
```

Use the complete matrix when a longer run is appropriate:

```bash
./bench/scripts/run.sh full
```

Run the report-script unit tests without starting proxy containers:

```bash
./bench/scripts/run.sh test
```

The resource limit can be changed for a controlled local experiment:

```bash
BENCH_CPUS=4 BENCH_MEMORY_LIMIT=2g ./bench/scripts/run.sh quick
```

Each run starts the origin once and starts Nginx, Caddy, and Raddy one at a
time. This prevents the targets from competing with one another. Every target
uses the same response body, paths, TLS certificate, Docker resource limits,
load-generator image, warm-up, duration, and repetition count.
The benchmark defaults Raddy to two Pingora workers to match the Nginx
configuration; override this explicitly with `RADDY_THREADS=1` or another
positive value when investigating worker scaling.

## Scenarios

The matrix is defined in [`scenarios/scenarios.json`](scenarios/scenarios.json),
not in the shell runner. The default full profile covers:

- HTTP/1.1 keep-alive with 128 B, 4 KiB, and 1 MiB responses;
- HTTP/1.1 connection churn;
- HTTPS with HTTP/1.1;
- HTTPS with HTTP/2 multiplexing;
- simple and matched routes;
- an HTTP/1.1 concurrency scan.

Fixed-QPS points are used for latency and error-rate comparisons. The
concurrency scan has no fixed QPS and shows the maximum stable throughput at
each connection count. A stable point has an error rate no higher than 0.1%.
The HTTP/2 scenario sets its `http2_parallel` value explicitly so `oha` opens
multiple concurrent streams per connection instead of measuring only one
stream per connection.

Each target is warmed up, measured, and repeated according to the profile.
Repeated values are aggregated by median.

## Results

Run-specific data is written under:

```text
bench/results/<run-id>/
  raw/             oha JSON, Docker stats, and command metadata
  summary.json     aggregated raw and normalized values
  summary.csv      flattened aggregated values for spreadsheets
  report.md        Markdown report
  report.html      browsable report
  charts/          SVG and PNG charts
```

Stable SVG copies are refreshed under `page/public/benchmarks/` for README and
documentation pages. Charts intentionally do not display host identity or
absolute QPS. Within every scenario:

```text
Nginx = 1.00x = 100%
```

Throughput is normalized as `target / nginx`; latency, CPU per request, and
memory use the same ratio but are interpreted as lower-is-better. Error rate is
shown as an absolute percentage. No combined score is produced.

The raw manifest records the Raddy commit, tool versions, scenario profile, and
configuration hashes. It does not attempt to make absolute results from
different machines directly comparable.

## Tool choices

- `oha` is the primary HTTP load generator. It supplies fixed-QPS and duration
  modes, HTTP/1.1 and HTTP/2, keep-alive control, latency correction, and JSON
  output.
- `h2load` is reserved for a future HTTP/2/h2c protocol-specific suite and is
  not part of this common-proxy ranking.
- `k6` is reserved for future multi-step user-flow tests.

The benchmark does not measure ACME, caching, TCP/UDP, QUIC/HTTP3, or
implementation-specific features that would make a common comparison
misleading.
