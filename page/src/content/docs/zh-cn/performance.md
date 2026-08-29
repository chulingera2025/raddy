---
title: 性能对比
description: 使用可复现的 Docker 测试套件比较 Nginx、Caddy 与 Raddy,图表以 Nginx 为基准。
---

仓库在 [`bench/`](https://github.com/chulingera2025/raddy/tree/main/bench) 中提供
独立的 Docker 性能对比套件。它使用同一个 origin、同一组场景和相同的资源
限制，每次只启动一个代理，并在每个场景内以 Nginx 作为基线。

## 运行测试

在仓库根目录执行：

```bash
./bench/scripts/run.sh quick
./bench/scripts/run.sh full
```

运行报告脚本单元测试：

```bash
./bench/scripts/run.sh test
```

宿主机只需要 Docker Engine 和 Docker Compose v2。负载生成器与 Matplotlib
均在固定版本的测试容器中运行。完整场景矩阵和调节项见
[测试套件说明](https://github.com/chulingera2025/raddy/tree/main/bench)。

## 相对图表

下面的图表来自最近一次提交的 benchmark snapshot。每个场景独立归一化：

```text
Nginx = 1.00x = 100%
```

![相对最大稳定吞吐](/benchmarks/throughput.svg)

![相对 p99 延迟](/benchmarks/latency-p99.svg)

![相对单请求 CPU 成本](/benchmarks/cpu-per-request.svg)

![相对峰值内存](/benchmarks/memory.svg)

吞吐越高越好；延迟、CPU 与内存越低越好。错误率显示绝对百分比，不做
归一化。报告不计算综合分数。

## 场景与边界

完整矩阵覆盖 HTTP/1.1 keep-alive、不同响应大小、连接 churn、HTTPS/HTTP/1.1、
HTTPS/HTTP/2 多路复用、路由和 HTTP/1.1 并发扫描。每个目标会预热、测量并
重复运行，结果取中位数；错误率不超过 0.1% 的点才算稳定吞吐。

首版不比较 ACME、缓存、TCP/UDP、QUIC/HTTP3 或代理特有能力。现有的
`examples/loadtest.rs` 仍可用于开发者快速 smoke test，但不是正式的跨代理
基准。
