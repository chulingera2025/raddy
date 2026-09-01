---
title: Metrics
description: Prometheus metrics exposed by raddex on the --metrics-addr listener.
---

Start raddex with `--metrics-addr` to expose Prometheus metrics over HTTP:

```bash
raddex run -c Raddexfile --metrics-addr 127.0.0.1:9100
```

The metrics listener serves Prometheus text format at `/metrics`:

```bash
curl http://127.0.0.1:9100/metrics
```

## Metrics

All metrics are registered on the default Prometheus registry and recorded on
every completed request:

| Metric | Type | Description |
|---|---|---|
| `raddex_requests_total` | Counter | Total HTTP requests served by raddex (the QPS source) |
| `raddex_request_duration_seconds` | Histogram | HTTP request duration in seconds |

```text
# HELP raddex_requests_total Total HTTP requests served by raddex
# TYPE raddex_requests_total counter
raddex_requests_total 123456

# HELP raddex_request_duration_seconds HTTP request duration in seconds
# TYPE raddex_request_duration_seconds histogram
raddex_request_duration_seconds_bucket{le="0.005"} 90000
raddex_request_duration_seconds_bucket{le="0.01"} 120000
raddex_request_duration_seconds_sum 620.5
raddex_request_duration_seconds_count 123456
```

The histogram uses the default Prometheus buckets. Compute latency percentiles
(p50 / p99) from the buckets with the usual `histogram_quantile()` PromQL:

```text
histogram_quantile(0.99, sum by (le) (rate(raddex_request_duration_seconds_bucket[5m])))
```

## What is not measured yet

The metrics set is deliberately minimal. There are no per-site or per-upstream
breakdowns, and no backend connection-pool gauges yet. OpenTelemetry tracing is
on the roadmap.
