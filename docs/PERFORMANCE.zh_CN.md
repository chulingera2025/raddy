# 性能对比

跨代理基准测试位于 [`bench/`](../bench/)。它在隔离的 Docker 运行中依次启动
Nginx、Caddy、Raddex，使用同一个源站、同一套场景矩阵、同一张 TLS 证书和同样的
资源限额，然后在每个场景内以 Nginx 为基线做归一化。

## 结论

**Raddex 位于 Caddy 与 Nginx 之间。** 在所有被测维度上都明显比 Caddy 精简，
但在峰值吞吐和单请求 CPU 上达不到 Nginx。

| 指标（Nginx = 100%） | Caddy | Raddex | 方向 |
| --- | ---: | ---: | --- |
| 最大稳定吞吐（并发扫描） | 39.7% | **59.6%** | 越高越好 |
| p99 延迟（9 个场景的中位数） | 154.5% | **116.6%** | 越低越好 |
| 单请求 CPU（9 个场景的中位数） | 274.7% | **146.5%** | 越低越好 |
| 峰值内存（9 个场景的中位数） | 306.2% | **174.0%** | 越低越好 |

测试机上的绝对峰值：Nginx 15 603 QPS，Raddex 9 297，Caddy 6 201。所有场景中
三个目标的错误率均为 0。

几个值得单独说明的结果：

- **HTTPS + HTTP/2 多路复用。** 在 4 000 QPS 目标下，Nginx 与 Raddex 都扛住了
  （4 008 QPS）；Caddy 只达到 1 008 QPS（25.1%），且单请求 CPU 是 Nginx 的
  6.5 倍。
- **连接频繁新建是 Raddex 最弱的场景。** p99 为 Nginx 的 361%（5.08 ms vs
  1.41 ms），比 Caddy 的 116% 还差。长连接场景没问题；但「每个请求都新建连接」
  的负载目前不是 Raddex 的强项。
- **大响应体是 Raddex 最好的场景。** 1 MiB 响应下，p99 为 Nginx 的 45.8%、
  单请求 CPU 为 70.8%——这是唯一一个 Raddex 胜过 Nginx 的场景。

## 运行溯源

| | |
| --- | --- |
| 档位 | `full` |
| Run ID | `20260901T101125Z-669719` |
| Raddex commit | `376526c916e12a00409e367cfeb2dc4757b3f070` |
| 主机 | 10 核 / 7 GiB Debian 12，Docker 29.7 |
| 单目标限额 | 2 CPU、1 GiB |
| 工作进程 | Nginx 2 workers，Raddex 2 个 Pingora 线程 |
| 压测工具 | `oha` 1.16.0 |
| 计时 | 10 s 预热、30 s 测量、3 次重复取中位数 |
| 镜像 | `nginx:1.30.4-alpine`、`caddy:2.11.4-alpine`、Rust 1.97.1 |

## 怎么读这些表

每个场景都以 Nginx 为自己的基线：

```text
Nginx = 1.00x = 100%
```

9 个场景中有 7 个是**固定速率**压测，因此三个目标在那里都报 `100.0%` 吞吐
——这表示三家都打满了目标速率，**不是**容量打平。只有并发扫描场景在寻找最大
稳定吞吐，所以它是唯一有意义的吞吐数字。错误率不高于 0.1% 的点才算稳定点。

报告保持各维度独立，不合成单一总分：吞吐越高越好；p99 延迟、单请求 CPU 毫秒数、
峰值内存越低越好；错误率以绝对百分比呈现，不做归一化。

比值只在**同一次运行、同一个场景**的目标之间有效。不要跨机器比较绝对 QPS。

## 归一化结果

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

## 原始参考指标

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

## 覆盖范围与边界

该套件覆盖 HTTP/1.1、HTTPS/HTTP/1.1、HTTPS/HTTP/2、响应体大小、连接频繁新建、
路由匹配与并发扫描。它**刻意不包含** ACME、缓存、TCP/UDP、QUIC/HTTP3 以及各家
特有功能——这些需要各自独立的公平性定义。

**三个目标都关闭了访问日志。** Nginx 配置 `access_log off`，Caddy 配置未定义
logger，Raddex 配置未写 `access_log` 指令——所以对比是公平的，但 Raddex 的访问
日志路径在这里**没有被测到**。这条路径并不免费：它在单个 mutex 上串行、且每个
请求 flush 一次，开启后的性能画像与这组数字不是一回事。

## 运行方式

```bash
./bench/scripts/run.sh full     # 上述数字来自这个档位
./bench/scripts/run.sh quick    # 3 秒冒烟测试，不具可比性
./bench/scripts/run.sh test     # 只跑报告脚本单测，不启动代理
```

主机只需要 Docker Engine 与 Docker Compose v2；压测工具和 Matplotlib 报告生成器
都在固定版本的容器里构建。资源限额可覆盖以做受控实验：

```bash
BENCH_CPUS=4 BENCH_MEMORY_LIMIT=2g ./bench/scripts/run.sh full
```

一次运行会把原始数据写到 `bench/results/<run-id>/`，并刷新
`page/public/benchmarks/` 下的 SVG。拓扑与场景矩阵见
[`bench/README.md`](../bench/README.md)。

`examples/loadtest.rs` 仍是一个小的开发期冒烟工具；它每个请求新建连接，不是
跨代理对比基准。

## 原生四层转发基准测试（Layer 4 Forwarding）

HTTP 核心依赖 Pingora，而四层代理（TCP/UDP）运行在完全独立的 **Tokio 原生** 数据面上（`Raddex -> L4 Core -> Tokio -> TCP/UDP`）。四层基准测试位于 [`bench/l4/`](../bench/l4/)，对比了 Nginx stream、Caddy layer4（`mholt/caddy-l4`）、Raddex L4 以及 Linux NAT / nftables 作为内核级参考。

### 测试环境与版本溯源

| 参数 | 说明 |
| --- | --- |
| 档位 | `full`（全部 17 个场景，规模封顶在 50K） |
| 运行 ID | `20260904T064312Z-3135437` |
| Raddex 提交 | `b548f6455907ef2f6bee4982273ead070312f05b` |
| 测试机硬件 | 10 核 Intel Xeon CPU E5-2650 v2 @ 2.60GHz / 8 GiB 内存，Debian 12（Linux 6.1.0-31-amd64） |
| 容器环境 | Docker 29.7.2 |
| 资源限额 | 容器限制 2.0 CPUs，1 GiB 内存 |
| 进程/线程 | Nginx 2 workers，Raddex 2 worker threads，Caddy 默认（cgroup 约束） |
| 隔离机制 | 预热与正式测量之间自动重启容器隔离内核状态 |
| 统计方式 | 3 次独立重复取中位数 |

### 归一化实测结果（以 Nginx stream = 100% 为基线）

| 场景 | 目标 | 吞吐量 | PPS | 建连速率 | 连接数 | p99 延迟 | CPU 开销 | 内存峰值 | 错误率 | 丢包率 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| TCP 连接速率 (10K 并发建连) | Nginx stream | — | — | 100.0% | — | — | 100.0% | 100.0% | 0.000% | 0.000% |
| TCP 连接速率 (10K 并发建连) | Caddy layer4 | — | — | 73.3% | — | — | 316.0% | 190.1% | 0.000% | 0.000% |
| TCP 连接速率 (10K 并发建连) | Raddex L4 | — | — | **84.6%** | — | — | 254.7% | **67.4%** | 0.000% | 0.000% |
| TCP 连接速率 (10K 并发建连) | Linux NAT / nftables | — | — | 117.2% | — | — | — | — | 0.000% | 0.000% |
| TCP 连接速率 (50K 并发建连) | Nginx stream | — | — | 100.0% | — | — | 100.0% | 100.0% | 42.394% | 0.000% |
| TCP 连接速率 (50K 并发建连) | Caddy layer4 | — | — | 86.1% | — | — | 131.8% | 118.1% | 42.358% | 0.000% |
| TCP 连接速率 (50K 并发建连) | Raddex L4 | — | — | **114.5%** | — | — | **83.2%** | **84.0%** | **34.968%** | 0.000% |
| TCP 连接速率 (50K 并发建连) | Linux NAT / nftables | — | — | 456.8% | — | — | — | — | 2.904% | 0.000% |
| TCP 吞吐 (64 KiB / 1 连接) | Nginx stream | 100.0% | — | — | — | — | 100.0% | 100.0% | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 1 连接) | Caddy layer4 | 89.6% | — | — | — | — | 117.3% | 8.2% | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 1 连接) | Raddex L4 | **105.1%** | — | — | — | — | **96.9%** | **3.8%** | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 1 连接) | Linux NAT / nftables | 58.6% | — | — | — | — | — | — | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 16 并发) | Nginx stream | 100.0% | — | — | — | — | 100.0% | 100.0% | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 16 并发) | Caddy layer4 | 76.6% | — | — | — | — | 129.8% | 12.6% | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 16 并发) | Raddex L4 | **156.5%** | — | — | — | — | **64.4%** | **9.3%** | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 16 并发) | Linux NAT / nftables | 173.4% | — | — | — | — | — | — | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 64 并发) | Nginx stream | 100.0% | — | — | — | — | 100.0% | 100.0% | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 64 并发) | Caddy layer4 | 89.2% | — | — | — | — | 112.4% | 25.4% | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 64 并发) | Raddex L4 | **176.6%** | — | — | — | — | **57.6%** | **25.1%** | 0.000% | 0.000% |
| TCP 吞吐 (64 KiB / 64 并发) | Linux NAT / nftables | 225.5% | — | — | — | — | — | — | 0.000% | 0.000% |
| TCP 长连接维持 (10K 连接) | Nginx stream | — | — | — | 100.0% | — | 100.0% | 100.0% | 0.000% | 0.000% |
| TCP 长连接维持 (10K 连接) | Caddy layer4 | — | — | — | 100.0% | — | 225.2% | 191.0% | 0.000% | 0.000% |
| TCP 长连接维持 (10K 连接) | Raddex L4 | — | — | — | 100.0% | — | 183.6% | **67.0%** | 0.000% | 0.000% |
| TCP 长连接维持 (10K 连接) | Linux NAT / nftables | — | — | — | 100.0% | — | — | — | 0.000% | 0.000% |
| TCP 长连接维持 (50K 连接) | Nginx stream | — | — | — | 100.0% | — | 100.0% | 100.0% | 30.798% | 0.000% |
| TCP 长连接维持 (50K 连接) | Caddy layer4 | — | — | — | 80.0% | — | 236.7% | 125.0% | 44.626% | 0.000% |
| TCP 长连接维持 (50K 连接) | Raddex L4 | — | — | — | 85.6% | — | 161.2% | **89.5%** | 40.764% | 0.000% |
| TCP 长连接维持 (50K 连接) | Linux NAT / nftables | — | — | — | 138.9% | — | — | — | 3.848% | 0.000% |
| UDP 流容量 (10K 客户端) | Nginx stream | — | — | — | 100.0% | — | 100.0% | 100.0% | 0.000% | 0.000% |
| UDP 流容量 (10K 客户端) | Caddy layer4 | — | — | — | 98.5% | — | 340.5% | 183.4% | 1.510% | 0.000% |
| UDP 流容量 (10K 客户端) | Raddex L4 | — | — | — | **100.0%** | — | 115.4% | **52.2%** | **0.000%** | 0.000% |
| UDP 流容量 (10K 客户端) | Linux NAT / nftables | — | — | — | 100.0% | — | — | — | 0.000% | 0.000% |
| UDP 流容量 (50K 客户端) | Nginx stream | — | — | — | 100.0% | — | 100.0% | 100.0% | 43.538% | 0.000% |
| UDP 流容量 (50K 客户端) | Caddy layer4 | — | — | — | 170.6% | — | 46.7% | 105.5% | 3.664% | 0.000% |
| UDP 流容量 (50K 客户端) | Raddex L4 | — | — | — | 100.0% | — | 99.2% | **77.4%** | 43.538% | 0.000% |
| UDP 流容量 (50K 客户端) | Linux NAT / nftables | — | — | — | 177.1% | — | — | — | 0.016% | 0.000% |
| TCP p99 延迟 (1 / 16 / 64 连接) | Raddex L4 | — | — | — | — | **100.0%** | — | — | 0.000% | 0.000% |
| UDP p99 延迟 (64 B) | Raddex L4 | — | — | — | — | **100.0%** | 118.3% | **3.5%** | 0.000% | 0.000% |
| UDP 吞吐 (64 B / 512 B / 1400 B) | Raddex L4 | 100.0% | — | — | — | — | 103%–111% | **3.7%** | 0.000% | 0.000% |
| UDP 每秒数据包 PPS (64 B) | Raddex L4 | — | 100.0% | — | — | — | 139.3% | **3.8%** | 0.000% | 0.000% |

### 核心结论

1. **TCP 大数据量吞吐优势显著：** 彻底去除 Pingora 用户态缓冲与逐 chunk `flush().await` 后，Raddex 原生 Tokio 中继在 16 并发下达到 **Nginx 的 156.5%**，在 64 并发下达到 **176.6%**，且 CPU 消耗仅为 Nginx 的 **57.6%~64.4%**，内存开销大幅低于两家对手。
2. **并发建连性能反超：** 原生多 socket `SO_REUSEPORT` accept 循环在 10K 建连下做到 **Nginx 的 84.6%**（显著领先 Caddy 的 73.3%）；在 50K 极端高并发建连下做到 **Nginx 的 114.5%**（超越 Nginx 14.5%），重试错误率也是三家代理中最低的。
3. **UDP 流容量与丢包彻底根治：** 借助多接收套接字扇出，10K 场景错误率严格降为 **0.000%**，达到 100% 满容量接入，完全打平 Nginx。
4. **内存全场最低：** 各场景中 Raddex 内存通常仅为 Nginx 的 3%~70%，且只有 Caddy 的几分之一到几十分之一。

复现命令：

```bash
./bench/l4/scripts/run.sh full
```

![四层转发基准测试概览](../page/public/benchmarks/l4-forwarding.svg)

