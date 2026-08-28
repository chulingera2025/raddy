# Performance comparison

The supported cross-proxy benchmark lives in [`bench/`](../bench/). It runs
Nginx, Caddy, and Raddy in isolated Docker target runs and compares them against
the same origin and scenario definition.

## Run the suite

```bash
./bench/scripts/run.sh quick
./bench/scripts/run.sh full
./bench/scripts/run.sh test
```

The host only needs Docker Engine and Docker Compose v2. The load generator and
Matplotlib report generator are built inside pinned containers. See
[`bench/README.md`](../bench/README.md) for the topology, scenario matrix, and
resource controls.

## How to read the result

Every scenario uses Nginx as its local baseline:

```text
Nginx = 1.00x = 100%
```

The report keeps separate dimensions instead of creating a synthetic score:

- throughput: higher is better;
- p99 latency: lower is better;
- CPU milliseconds per request: lower is better;
- peak memory: lower is better;
- error rate: absolute percentage.

The ratio is calculated only between targets in the same scenario and run. Do
not compare absolute QPS from two different machines. A new run refreshes the
stable SVG assets under `page/public/benchmarks/` and leaves the raw run under
`bench/results/<run-id>/`.

## Scope and limits

The common suite covers HTTP/1.1, HTTPS/HTTP/1.1, HTTPS/HTTP/2, response sizes,
connection churn, routing, and concurrency. It deliberately excludes ACME,
caching, TCP/UDP, QUIC/HTTP3, and proxy-specific features. Those require
separate protocol or feature benchmarks with their own fairness definition.

The existing `examples/loadtest.rs` remains useful as a small developer smoke
test, but it uses a fresh connection per request and is not the formal
Nginx/Caddy/Raddy comparison benchmark.
