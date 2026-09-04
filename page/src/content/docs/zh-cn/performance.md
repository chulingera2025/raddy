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

## 贡献你的测试结果

上面所有数字都来自同一台主机，而 9 个场景里有 7 个是**固定速率**压测——它们测的
是「在该速率下的开销」，不是容量上限。这两个局限，只有更多人在不同硬件上跑过之后
才会收敛。

如果你跑了这套测试，结果很有价值。欢迎开一个标题为
`benchmark: <CPU> / <发行版>` 的 issue，并附上：

- `bench/results/<run-id>/summary.json`——聚合数据，内含归一化结果、档位和
  Raddex commit；
- 主机信息：CPU 型号与核数、内存总量、内核版本、Docker 版本，以及它是物理机、
  虚拟机还是容器宿主；
- 任何非默认的 `BENCH_CPUS` / `BENCH_MEMORY_LIMIT` / `RADDEX_THREADS`。

具备可比性的前提是：在空闲主机上执行 `run.sh full`，且未改动
`bench/versions.env` 里固定版本的镜像。`quick` 档或繁忙机器上的结果仍有助于发现
大的偏差，但请注明——它们不能与上面的数字直接对比。

**与上述结论相矛盾的结果，是最有价值的。**

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

## 原生四层转发基准测试（Layer 4 Forwarding）

四层代理运行在 **Tokio 原生** 数据面上（`Raddex -> L4 Core -> Tokio -> TCP/UDP`），数据包完全不走 Pingora。[`bench/l4/`](https://github.com/chulingera2025/raddex/tree/main/bench/l4) 提供了独立的 Linux 四层基准测试，对比 Nginx stream、Caddy layer4（`mholt/caddy-l4`）、Raddex L4 以及 Linux NAT / nftables。

### 核心指标概览（以 Nginx stream = 100% 为基线）

| 场景 | 评估指标 | Caddy | Raddex | Linux NAT (内核参考) |
| --- | --- | ---: | ---: | ---: |
| TCP 吞吐 (64 KiB / 16 并发) | 吞吐量 | 76.6% | **156.5%** | 173.4% |
| TCP 吞吐 (64 KiB / 16 并发) | CPU / 内存 | 129.8% / 12.6% | **64.4% / 9.3%** | — / — |
| TCP 吞吐 (64 KiB / 64 并发) | 吞吐量 | 89.2% | **176.6%** | 225.5% |
| TCP 吞吐 (64 KiB / 64 并发) | CPU / 内存 | 112.4% / 25.4% | **57.6% / 25.1%** | — / — |
| TCP 连接速率 (10K 并发建连) | 建连速率 | 73.3% | **84.6%** | 117.2% |
| TCP 连接速率 (50K 并发建连) | 建连速率 | 86.1% | **114.5%** | 456.8% |
| TCP 连接速率 (50K 并发建连) | CPU / 内存 | 131.8% / 118.1% | **83.2% / 84.0%** | — / — |
| UDP 流容量 (10K 客户端) | 建立容量 | 98.5% (1.51% 丢包) | **100.0% (0.00% 丢包)** | 100.0% |
| UDP 流容量 (10K 客户端) | 内存开销 | 183.4% | **52.2%** | — |
| TCP / UDP p99 延迟 | 延迟 | 100% – 200% | **100.0%** | 40% – 50% |

### 核心结论

- **TCP 大数据量吞吐优势显著：** 剔除 Pingora 缓冲写和逐 chunk `flush().await` 后，原生 Tokio 中继达到 **Nginx 的 1.56 倍 ~ 1.77 倍吞吐**，同时 **CPU 仅消耗 Nginx 的约 60%**，内存开销降至几分之一。
- **并发连接建立速率：** 基于多套接字 `SO_REUSEPORT` 原生 accept 循环，10K 速率达到 **Nginx 的 84.6%**（显著高于 Caddy 的 73.3%）；在 50K 极高并发建连下达到 **Nginx 的 114.5%**（反超 Nginx 14.5%），且重试错误率在三家代理中最低。
- **UDP 流容量与丢包彻底根治：** 借助多接收套接字扇出，消除了内核接收队列溢出，10K 场景错误率严格降为 **0.000%**，达到 100% 满容量接入，完全打平 Nginx。
- **内存占用全场最低：** 各场景中 Raddex 内存通常仅为 Nginx 的 3%~70%，且只有 Caddy 的几分之一到几十分之一。

### 四层转发总览图表

![四层转发基准测试概览](/benchmarks/l4-forwarding.svg)

### 运行四层测试

```bash
./bench/l4/scripts/run.sh full
```

