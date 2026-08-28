# 性能对比

正式的 Nginx / Caddy / Raddy 公平对比测试位于 [`bench/`](../bench/)。它
使用同一个 origin、同一组场景和同样的 Docker 资源限制，每次只启动一个
代理，并在每个场景内以 Nginx 作为基线。

## 运行套件

```bash
./bench/scripts/run.sh quick
./bench/scripts/run.sh full
./bench/scripts/run.sh test
```

宿主机只需要 Docker Engine 和 Docker Compose v2。oha、origin、Matplotlib
以及 Raddy 构建都在固定版本的容器中完成。详细拓扑、场景和资源控制见
[`bench/README.md`](../bench/README.md)。

## 如何读取结果

每个场景独立归一化：

```text
Nginx = 1.00x = 100%
```

报告保留独立指标，不计算容易误导的综合分数：

- 吞吐：越高越好；
- p99 延迟：越低越好；
- 单请求 CPU 毫秒：越低越好；
- 峰值内存：越低越好；
- 错误率：显示绝对百分比，不做归一化。

归一化只发生在同一次运行、同一个场景内。不同机器之间不要直接比较
绝对 QPS；图表只表达各自相对于 Nginx 的比例。

每次运行会在 `page/public/benchmarks/` 刷新稳定 SVG 图表，并在
`bench/results/<run-id>/` 保存原始 JSON、Docker stats、汇总 CSV、Markdown
和 HTML 报告。

## 测试范围

完整矩阵覆盖 HTTP/1.1 keep-alive、128 B / 4 KiB / 1 MiB 响应、连接 churn、
HTTPS/HTTP/1.1、HTTPS/HTTP/2 多路复用、路由和并发扫描。

首版不比较 ACME、缓存、TCP/UDP、QUIC/HTTP3 以及代理特有能力；这些能力
需要单独定义公平边界。仓库中的 `examples/loadtest.rs` 仍可用于开发者
smoke test，但不再是三者正式对比基准。
