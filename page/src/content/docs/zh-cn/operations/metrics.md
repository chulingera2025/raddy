---
title: 指标
description: raddex 在 --metrics-addr 监听器上暴露的 Prometheus 指标。
---

用 `--metrics-addr` 启动 raddex,即可通过 HTTP 暴露 Prometheus 指标:

```bash
raddex run -c Raddexfile --metrics-addr 127.0.0.1:9100
```

指标监听器在 `/metrics` 提供 Prometheus 文本格式:

```bash
curl http://127.0.0.1:9100/metrics
```

## 指标

所有指标都注册在默认 Prometheus 注册表上,并在每个完成的请求上记录:

| 指标 | 类型 | 说明 |
|---|---|---|
| `raddex_requests_total` | Counter | raddex 服务的 HTTP 请求总数(QPS 来源) |
| `raddex_request_duration_seconds` | Histogram | HTTP 请求耗时(秒) |

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

直方图使用默认 Prometheus 桶。用惯用的 `histogram_quantile()` PromQL 从桶中
计算延迟百分位(p50 / p99):

```text
histogram_quantile(0.99, sum by (le) (rate(raddex_request_duration_seconds_bucket[5m])))
```

## 尚未覆盖的测量

指标集合刻意保持精简。目前没有按站点或按上游的细分,也没有后端连接池的
gauge。OpenTelemetry 追踪已在路线图上。
