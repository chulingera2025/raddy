---
title: Performance comparison
description: Where Raddex actually lands against Nginx and Caddy, measured with a reproducible Docker benchmark.
---

The repository includes a Docker benchmark under
[`bench/`](https://github.com/chulingera2025/raddex/tree/main/bench). It starts
one proxy at a time against the same origin, uses the same scenario matrix, TLS
certificate, and resource limits, and normalizes each scenario against Nginx.

## Summary

**Raddex sits between Caddy and Nginx.** It is substantially leaner than Caddy
on every axis measured, and it does not match Nginx on raw throughput or
per-request CPU. If you are replacing Nginx purely for speed, this data does
not justify the switch; if you are weighing Caddy-style configuration
ergonomics against Nginx-style efficiency, that is the gap Raddex targets.

| Metric (Nginx = 100%) | Caddy | Raddex | Better is |
| --- | ---: | ---: | --- |
| Max stable throughput (concurrency scan) | 39.7% | **59.6%** | higher |
| p99 latency (median of 9 scenarios) | 154.5% | **116.6%** | lower |
| CPU per request (median of 9 scenarios) | 274.7% | **146.5%** | lower |
| Peak memory (median of 9 scenarios) | 306.2% | **174.0%** | lower |

Absolute peak on the test machine: 15 603 QPS for Nginx, 9 297 for Raddex,
6 201 for Caddy. No target recorded a non-zero error rate in any scenario.

- **HTTPS with HTTP/2 multiplexing.** At a 4 000 QPS target, Nginx and Raddex
  both sustained it (4 008 QPS); Caddy reached 1 008 QPS (25.1%) at 6.5×
  Nginx's CPU per request.
- **Connection churn is Raddex's weakest scenario.** p99 is 361% of Nginx
  (5.08 ms vs 1.41 ms), worse than Caddy's 116%. Keep-alive workloads are fine;
  a fresh connection per request is not where Raddex is strong today.
- **Large responses are Raddex's best.** At 1 MiB it beats Nginx on both p99
  (45.8%) and CPU per request (70.8%) — the only scenario where it does.

## How to read this

Seven of the nine scenarios drive a **fixed** request rate, so all three
targets report `100.0%` throughput there — that is every target hitting the
target rate, not a tie on capacity. Only the concurrency scan searches for
maximum stable throughput, so it is the only meaningful throughput figure. A
point counts as stable only at an error rate at or below 0.1%.

Ratios are valid only between targets in the same scenario and the same run.
Do not compare absolute QPS across machines.

## Test environment

The published numbers come from a 10-core / 7 GiB Debian 12 host running Docker
29.7, with each proxy limited to 2 CPUs and 1 GiB, Nginx at 2 workers and
Raddex at 2 Pingora threads, `oha` 1.16.0 as the load generator, and 10 s
warm-up + 30 s measurement + 3 repetitions aggregated by median.

**Access logging is off for all three targets.** The comparison is fair, but
Raddex's access-log path is not exercised here — it serializes on a single
mutex and flushes per request, so enabling it changes this profile.

The suite does not compare ACME, caching, TCP/UDP, QUIC/HTTP3, or
implementation-specific features.

## Charts

Each scenario is normalized independently against Nginx (`1.00x = 100%`).

![Relative maximum stable throughput](/benchmarks/throughput.svg)

![Relative p99 latency](/benchmarks/latency-p99.svg)

![Relative CPU cost per request](/benchmarks/cpu-per-request.svg)

![Relative peak memory](/benchmarks/memory.svg)

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

## Run the benchmark

```bash
./bench/scripts/run.sh full     # the comparable run these numbers come from
./bench/scripts/run.sh quick    # 3-second smoke test, NOT comparable
./bench/scripts/run.sh test     # report-script unit tests only
```

The host needs only Docker Engine and Docker Compose v2. See the
[benchmark README](https://github.com/chulingera2025/raddex/tree/main/bench)
for the scenario matrix and tuning controls, and
[docs/PERFORMANCE.md](https://github.com/chulingera2025/raddex/blob/main/docs/PERFORMANCE.md)
for the full per-scenario tables.
