---
title: 性能对比
description: 用可复现的 Docker 基准测试，说明 Raddex 相对 Nginx 与 Caddy 的真实位置。
---

仓库的 [`bench/`](https://github.com/chulingera2025/raddex/tree/main/bench)
提供了 Docker 基准测试：一次只启动一个代理，使用同一个源站、同一套场景矩阵、
同一张 TLS 证书和同样的资源限额，并在每个场景内以 Nginx 为基线归一化。

## 结论

**Raddex 位于 Caddy 与 Nginx 之间。** 在所有被测维度上都明显比 Caddy 精简，
但在峰值吞吐和单请求 CPU 上达不到 Nginx。如果你只为了更快而想替换 Nginx，
这组数据不支持这个决定；如果你在「Caddy 式的配置体验」和「Nginx 式的效率」
之间权衡，Raddex 瞄准的正是这段空隙。

| 指标（Nginx = 100%） | Caddy | Raddex | 方向 |
| --- | ---: | ---: | --- |
| 最大稳定吞吐（并发扫描） | 39.7% | **59.6%** | 越高越好 |
| p99 延迟（9 个场景中位数） | 154.5% | **116.6%** | 越低越好 |
| 单请求 CPU（9 个场景中位数） | 274.7% | **146.5%** | 越低越好 |
| 峰值内存（9 个场景中位数） | 306.2% | **174.0%** | 越低越好 |

测试机绝对峰值：Nginx 15 603 QPS、Raddex 9 297、Caddy 6 201。所有场景中三个
目标的错误率均为 0。

- **HTTPS + HTTP/2 多路复用。** 4 000 QPS 目标下，Nginx 与 Raddex 都扛住了
  （4 008 QPS）；Caddy 只到 1 008 QPS（25.1%），单请求 CPU 是 Nginx 的 6.5 倍。
- **连接频繁新建是 Raddex 最弱的场景。** p99 为 Nginx 的 361%（5.08 ms vs
  1.41 ms），比 Caddy 的 116% 还差。长连接场景没问题，但「每请求新建连接」的
  负载目前不是 Raddex 的强项。
- **大响应体是 Raddex 最好的场景。** 1 MiB 下 p99 为 Nginx 的 45.8%、单请求
  CPU 为 70.8%——这是唯一一个胜过 Nginx 的场景。

## 怎么读这组数字

9 个场景中有 7 个是**固定速率**压测，因此三家在那里都报 `100.0%` 吞吐——那表示
三家都打满了目标速率，**不是**容量打平。只有并发扫描在寻找最大稳定吞吐，所以它
是唯一有意义的吞吐数字。错误率不高于 0.1% 的点才算稳定点。

比值只在同一次运行、同一个场景的目标之间有效，不要跨机器比较绝对 QPS。

## 测试环境

上述数字来自一台 10 核 / 7 GiB 的 Debian 12 主机（Docker 29.7）：每个代理限制
2 CPU、1 GiB，Nginx 2 workers、Raddex 2 个 Pingora 线程，压测工具 `oha` 1.16.0，
10 s 预热 + 30 s 测量 + 3 次重复取中位数。

**三个目标都关闭了访问日志。** 对比是公平的，但 Raddex 的访问日志路径没有被测到
——它在单个 mutex 上串行、每请求 flush 一次，开启后性能画像与这组数字不同。

该套件不比较 ACME、缓存、TCP/UDP、QUIC/HTTP3 以及各家特有功能。

## 图表

每个场景独立以 Nginx 为基线归一化（`1.00x = 100%`）。

![相对最大稳定吞吐](/benchmarks/throughput.svg)

![相对 p99 延迟](/benchmarks/latency-p99.svg)

![相对单请求 CPU 开销](/benchmarks/cpu-per-request.svg)

![相对峰值内存](/benchmarks/memory.svg)

## 运行方式

```bash
./bench/scripts/run.sh full     # 上述数字来自这个档位
./bench/scripts/run.sh quick    # 3 秒冒烟测试，不具可比性
./bench/scripts/run.sh test     # 只跑报告脚本单测
```

主机只需要 Docker Engine 与 Docker Compose v2。场景矩阵与调节参数见
[benchmark README](https://github.com/chulingera2025/raddex/tree/main/bench)，
完整的逐场景表格见
[docs/PERFORMANCE.zh_CN.md](https://github.com/chulingera2025/raddex/blob/main/docs/PERFORMANCE.zh_CN.md)。
