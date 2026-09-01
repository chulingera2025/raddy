# Performance comparison

The cross-proxy benchmark lives in [`bench/`](../bench/). It runs Nginx, Caddy,
and Raddex in isolated Docker target runs against the same origin, scenario
matrix, TLS certificate, and resource limits, then normalizes each scenario
against Nginx.

## Summary

**Raddex sits between Caddy and Nginx.** It is substantially leaner than Caddy
on every axis measured, and it does not match Nginx on raw throughput or
per-request CPU.

| Metric (Nginx = 100%) | Caddy | Raddex | Better is |
| --- | ---: | ---: | --- |
| Max stable throughput (concurrency scan) | 39.7% | **59.6%** | higher |
| p99 latency (median of 9 scenarios) | 154.5% | **116.6%** | lower |
| CPU per request (median of 9 scenarios) | 274.7% | **146.5%** | lower |
| Peak memory (median of 9 scenarios) | 306.2% | **174.0%** | lower |

Absolute peak on the test machine: 15 603 QPS for Nginx, 9 297 for Raddex,
6 201 for Caddy. No target recorded a non-zero error rate in any scenario.

Individual results worth noting:

- **HTTPS with HTTP/2 multiplexing.** At a 4 000 QPS target, Nginx and Raddex
  both sustained it (4 008 QPS); Caddy reached 1 008 QPS (25.1%) at 6.5×
  Nginx's CPU per request.
- **Connection churn is Raddex's weakest scenario.** p99 is 361% of Nginx
  (5.08 ms vs 1.41 ms), worse than Caddy's 116%. Keep-alive workloads are fine;
  a fresh connection per request is not where Raddex is strong today.
- **Large responses are Raddex's best.** At 1 MiB it beats Nginx on both p99
  (45.8%) and CPU per request (70.8%) — the only scenario where it does.

## Run provenance

| | |
| --- | --- |
| Profile | `full` |
| Run ID | `20260901T101125Z-669719` |
| Raddex commit | `376526c916e12a00409e367cfeb2dc4757b3f070` |
| Host | 10-core / 7 GiB Debian 12, Docker 29.7 |
| Per-target limit | 2 CPUs, 1 GiB |
| Workers | Nginx 2 workers, Raddex 2 Pingora threads |
| Load generator | `oha` 1.16.0 |
| Timing | 10 s warm-up, 30 s measurement, 3 repetitions, median |
| Images | `nginx:1.30.4-alpine`, `caddy:2.11.4-alpine`, Rust 1.97.1 |

## How to read the tables

Every scenario uses Nginx as its own baseline:

```text
Nginx = 1.00x = 100%
```

Seven of the nine scenarios drive a **fixed** request rate, so all three
targets report `100.0%` throughput there — that is every target hitting the
target rate, not a tie on capacity. Only the concurrency scan searches for
maximum stable throughput, so it is the only meaningful throughput figure. A
point counts as stable only at an error rate at or below 0.1%.

The report keeps the dimensions separate instead of inventing a combined score:
throughput (higher is better); p99 latency, CPU milliseconds per request, and
peak memory (lower is better); error rate as an absolute percentage.

Ratios are only valid between targets in the same scenario and the same run.
Do not compare absolute QPS from two different machines.

## Normalized results

| Scenario | Target | Throughput | p99 latency | CPU/request | Memory | Error rate |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| HTTP/1.1 concurrency scan / small response | Nginx | 100.0% | 100.0% | 100.0% | 100.0% | 0.000% |
| HTTP/1.1 concurrency scan / small response | Caddy | 39.7% | 316.1% | 225.9% | 270.0% | 0.000% |
| HTTP/1.1 concurrency scan / small response | Raddex | 59.6% | 126.5% | 155.8% | 121.4% | 0.000% |
| HTTP/1.1 connection churn / small response | Nginx | 100.0% | 100.0% | 100.0% | 100.0% | 0.000% |
| HTTP/1.1 connection churn / small response | Caddy | 100.0% | 116.5% | 274.7% | 244.6% | 0.000% |
| HTTP/1.1 connection churn / small response | Raddex | 100.0% | 361.4% | 192.9% | 124.5% | 0.000% |
| HTTP/1.1 keep-alive / 1 MiB response | Nginx | 100.0% | 100.0% | 100.0% | 100.0% | 0.000% |
| HTTP/1.1 keep-alive / 1 MiB response | Caddy | 100.6% | 49.7% | 71.9% | 258.4% | 0.000% |
| HTTP/1.1 keep-alive / 1 MiB response | Raddex | 100.2% | 45.8% | 70.8% | 174.0% | 0.000% |
| HTTP/1.1 keep-alive / 4 KiB response | Nginx | 100.0% | 100.0% | 100.0% | 100.0% | 0.000% |
| HTTP/1.1 keep-alive / 4 KiB response | Caddy | 100.0% | 162.9% | 279.3% | 333.2% | 0.000% |
| HTTP/1.1 keep-alive / 4 KiB response | Raddex | 100.0% | 95.2% | 146.5% | 214.8% | 0.000% |
| HTTP/1.1 keep-alive / small response | Nginx | 100.0% | 100.0% | 100.0% | 100.0% | 0.000% |
| HTTP/1.1 keep-alive / small response | Caddy | 100.0% | 153.9% | 265.1% | 306.2% | 0.000% |
| HTTP/1.1 keep-alive / small response | Raddex | 100.0% | 126.3% | 143.6% | 186.6% | 0.000% |
| HTTPS / HTTP/1.1 | Nginx | 100.0% | 100.0% | 100.0% | 100.0% | 0.000% |
| HTTPS / HTTP/1.1 | Caddy | 100.0% | 151.6% | 277.8% | 230.2% | 0.000% |
| HTTPS / HTTP/1.1 | Raddex | 100.0% | 113.9% | 163.5% | 164.9% | 0.000% |
| HTTPS / HTTP/2 multiplexing | Nginx | 100.0% | 100.0% | 100.0% | 100.0% | 0.000% |
| HTTPS / HTTP/2 multiplexing | Caddy | 25.1% | 316.7% | 645.4% | 347.3% | 0.000% |
| HTTPS / HTTP/2 multiplexing | Raddex | 100.0% | 215.4% | 330.7% | 164.3% | 0.000% |
| Multi-condition route / small response | Nginx | 100.0% | 100.0% | 100.0% | 100.0% | 0.000% |
| Multi-condition route / small response | Caddy | 100.0% | 168.0% | 276.2% | 310.5% | 0.000% |
| Multi-condition route / small response | Raddex | 100.0% | 109.2% | 138.7% | 215.0% | 0.000% |
| Simple route / small response | Nginx | 100.0% | 100.0% | 100.0% | 100.0% | 0.000% |
| Simple route / small response | Caddy | 100.0% | 154.5% | 259.9% | 321.1% | 0.000% |
| Simple route / small response | Raddex | 100.0% | 116.6% | 137.0% | 216.2% | 0.000% |

## Raw reference metrics

| Scenario | Target | Reference load | Max stable QPS | Reference p99 | Error rate | CPU ms/request | Peak memory |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| HTTP/1.1 concurrency scan / small response | Nginx | 16 | 15603.36 | 2.401 ms | 0.000% | 0.134 | 7.30 MiB |
| HTTP/1.1 concurrency scan / small response | Caddy | 16 | 6200.82 | 7.589 ms | 0.000% | 0.302 | 19.71 MiB |
| HTTP/1.1 concurrency scan / small response | Raddex | 16 | 9296.52 | 3.037 ms | 0.000% | 0.208 | 8.86 MiB |
| HTTP/1.1 connection churn / small response | Nginx | 500 | 999.89 | 1.406 ms | 0.000% | 0.334 | 7.46 MiB |
| HTTP/1.1 connection churn / small response | Caddy | 500 | 1000.01 | 1.638 ms | 0.000% | 0.917 | 18.26 MiB |
| HTTP/1.1 connection churn / small response | Raddex | 500 | 999.88 | 5.081 ms | 0.000% | 0.644 | 9.29 MiB |
| HTTP/1.1 keep-alive / 1 MiB response | Nginx | 4 | 16.03 | 10.304 ms | 0.000% | 5.594 | 6.61 MiB |
| HTTP/1.1 keep-alive / 1 MiB response | Caddy | 4 | 16.13 | 5.122 ms | 0.000% | 4.020 | 17.07 MiB |
| HTTP/1.1 keep-alive / 1 MiB response | Raddex | 4 | 16.07 | 4.715 ms | 0.000% | 3.963 | 11.49 MiB |
| HTTP/1.1 keep-alive / 4 KiB response | Nginx | 500 | 1999.94 | 0.857 ms | 0.000% | 0.260 | 7.16 MiB |
| HTTP/1.1 keep-alive / 4 KiB response | Caddy | 500 | 1999.84 | 1.396 ms | 0.000% | 0.726 | 23.86 MiB |
| HTTP/1.1 keep-alive / 4 KiB response | Raddex | 500 | 1999.97 | 0.816 ms | 0.000% | 0.381 | 15.38 MiB |
| HTTP/1.1 keep-alive / small response | Nginx | 1000 | 3999.69 | 0.805 ms | 0.000% | 0.235 | 7.17 MiB |
| HTTP/1.1 keep-alive / small response | Caddy | 1000 | 3999.79 | 1.239 ms | 0.000% | 0.622 | 21.96 MiB |
| HTTP/1.1 keep-alive / small response | Raddex | 1000 | 3999.68 | 1.017 ms | 0.000% | 0.337 | 13.38 MiB |
| HTTPS / HTTP/1.1 | Nginx | 500 | 999.96 | 0.936 ms | 0.000% | 0.262 | 10.06 MiB |
| HTTPS / HTTP/1.1 | Caddy | 500 | 999.98 | 1.419 ms | 0.000% | 0.729 | 23.16 MiB |
| HTTPS / HTTP/1.1 | Raddex | 500 | 999.92 | 1.066 ms | 0.000% | 0.429 | 16.59 MiB |
| HTTPS / HTTP/2 multiplexing | Nginx | 1000 | 4008.07 | 0.622 ms | 0.000% | 0.120 | 8.04 MiB |
| HTTPS / HTTP/2 multiplexing | Caddy | 1000 | 1007.82 | 1.970 ms | 0.000% | 0.777 | 27.92 MiB |
| HTTPS / HTTP/2 multiplexing | Raddex | 1000 | 4008.18 | 1.340 ms | 0.000% | 0.398 | 13.21 MiB |
| Multi-condition route / small response | Nginx | 1000 | 3999.71 | 0.850 ms | 0.000% | 0.236 | 7.20 MiB |
| Multi-condition route / small response | Caddy | 1000 | 3999.85 | 1.428 ms | 0.000% | 0.652 | 22.35 MiB |
| Multi-condition route / small response | Raddex | 1000 | 3999.64 | 0.928 ms | 0.000% | 0.328 | 15.48 MiB |
| Simple route / small response | Nginx | 1000 | 3999.74 | 0.809 ms | 0.000% | 0.240 | 7.16 MiB |
| Simple route / small response | Caddy | 1000 | 3999.78 | 1.250 ms | 0.000% | 0.624 | 22.98 MiB |
| Simple route / small response | Raddex | 1000 | 3999.74 | 0.943 ms | 0.000% | 0.329 | 15.47 MiB |

## Contribute a run

Every published number comes from one host, and seven of the nine scenarios
drive a **fixed** request rate — so they measure cost at that rate, not capacity.
Both limits shrink as more people run the suite on hardware that is not this one.

If you run it, a result is worth contributing. Open an issue titled
`benchmark: <cpu> / <distro>` and attach:

- `bench/results/<run-id>/summary.json` — the aggregated data, which carries the
  normalized values, the profile, and the Raddex commit;
- the host: CPU model and core count, total RAM, kernel version, Docker version,
  and whether it is bare metal, a VM, or a container host;
- any non-default `BENCH_CPUS` / `BENCH_MEMORY_LIMIT` / `RADDEX_THREADS`.

A submission is comparable when it used `run.sh full` on an otherwise idle host,
with the pinned images from `bench/versions.env` unchanged. `quick` runs and
runs on a busy machine are still interesting for spotting a large discrepancy,
but say so — they are not directly comparable with the numbers above.

Results that contradict these are the most useful ones to receive.

## Scope and limits

The suite covers HTTP/1.1, HTTPS/HTTP/1.1, HTTPS/HTTP/2, response sizes,
connection churn, routing, and a concurrency scan. It deliberately excludes
ACME, caching, TCP/UDP, QUIC/HTTP3, and proxy-specific features; those need
separate benchmarks with their own fairness definitions.

**Access logging is off for all three targets.** Nginx sets `access_log off`,
the Caddy config defines no logger, and the Raddex config sets no `access_log`
directive — so the comparison is fair, but Raddex's access-log path is not
exercised here. That path is not free: it serializes on a single mutex and
flushes per request, so enabling it changes the profile these numbers describe.

## Run it

```bash
./bench/scripts/run.sh full     # the comparable run these numbers come from
./bench/scripts/run.sh quick    # 3-second smoke test, NOT comparable
./bench/scripts/run.sh test     # report-script unit tests, no proxies started
```

The host needs only Docker Engine and Docker Compose v2; the load generator and
the Matplotlib report generator are built inside pinned containers. Resource
limits can be overridden for a controlled experiment:

```bash
BENCH_CPUS=4 BENCH_MEMORY_LIMIT=2g ./bench/scripts/run.sh full
```

A run writes raw data under `bench/results/<run-id>/` and refreshes the SVG
assets under `page/public/benchmarks/`. See [`bench/README.md`](../bench/README.md)
for the topology and scenario matrix.

`examples/loadtest.rs` remains a small developer smoke test; it opens a fresh
connection per request and is not the cross-proxy comparison.
