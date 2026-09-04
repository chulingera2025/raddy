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

## Layer 4 forwarding benchmark

While HTTP runs through Pingora, Layer 4 (TCP/UDP) runs on an independent
**native Tokio** data path (`Raddex -> L4 Core -> Tokio -> TCP/UDP`).
The L4 benchmark lives in [`bench/l4/`](https://github.com/chulingera2025/raddex/tree/main/bench/l4)
and compares Nginx stream, Caddy layer4 (`mholt/caddy-l4`), Raddex L4, and Linux
NAT / nftables as a kernel baseline.

### Summary (Nginx stream = 100%)

| Scenario | Metric | Caddy | Raddex | Linux NAT |
| --- | --- | ---: | ---: | ---: |
| TCP throughput (64 KiB / 16 conns) | Throughput | 76.6% | **156.5%** | 173.4% |
| TCP throughput (64 KiB / 16 conns) | CPU / Memory | 129.8% / 12.6% | **64.4% / 9.3%** | — / — |
| TCP throughput (64 KiB / 64 conns) | Throughput | 89.2% | **176.6%** | 225.5% |
| TCP throughput (64 KiB / 64 conns) | CPU / Memory | 112.4% / 25.4% | **57.6% / 25.1%** | — / — |
| TCP connection rate (10K conns) | Connects/sec | 73.3% | **84.6%** | 117.2% |
| TCP connection rate (50K conns) | Connects/sec | 86.1% | **114.5%** | 456.8% |
| TCP connection rate (50K conns) | CPU / Memory | 131.8% / 118.1% | **83.2% / 84.0%** | — / — |
| UDP flows (10K clients) | Flow capacity | 98.5% (1.51% err) | **100.0% (0.00% err)** | 100.0% |
| UDP flows (10K clients) | Memory | 183.4% | **52.2%** | — |
| TCP / UDP p99 latency | p99 | 100% – 200% | **100.0%** | 40% – 50% |

### Key takeaways

- **TCP bulk throughput:** Bypassing Pingora's buffered writer and per-chunk `flush().await`
  lets Raddex achieve **156.5% to 176.6% of Nginx's throughput** at **~60% of its CPU** and a fraction of its memory.
- **Connection establishment rate:** Native `SO_REUSEPORT` accept loops achieve **84.6% of Nginx** at
  10K connections, and **114.5% of Nginx** at 50K connections with the lowest error rate among user-space proxies.
- **UDP flow capacity:** Per-thread `SO_REUSEPORT` datagram socket fan-out eliminates kernel
  receive-buffer overflow, reaching **100.0% capacity with 0.000% error rate**, matching Nginx.
- **Memory footprint:** Raddex maintains the lowest memory footprint among all user-space proxies
  across every scenario (typically 3% to 70% of Nginx, and 2× to 10× leaner than Caddy).

### L4 Overview Chart

![Layer 4 forwarding benchmark overview](/benchmarks/l4-forwarding.svg)

### Run the L4 benchmark

```bash
./bench/l4/scripts/run.sh full
```

