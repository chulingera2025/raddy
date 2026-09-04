---
title: 能力矩阵
description: 弄清 Raddex 的哪些能力是完整支持、仅限 Linux、仅透传，还是需要独立的 sidecar。
---

本页是面向部署的协议边界总览。配置参考讲的是**语法**，本页讲的是运行时**实际
终止了什么、转发了什么、以及把什么留给了别的服务**。

## 状态词汇表

- **支持** —— 已实现，且被 v0.3.6 的验证套件覆盖。
- **仅限 Linux** —— 支持，但依赖 Linux 内核行为、特权，或文件描述符交接。
- **透传** —— Raddex 只转发字节或数据报，不终止上层协议。
- **需要 sidecar** —— 该能力所需的协议栈不在 Pingora 0.8.1 或 Raddex 当前运行时
  之内。

Raddex 尚未到 1.0。"支持"是当前版本的发布契约，而
[Raddexfile 规范](../../config/directives/)始终是配置兼容性的唯一事实来源。

## HTTP 与 TLS

| 能力 | 状态 | 说明 |
| --- | --- | --- |
| HTTP/1.1 反向代理 | 支持 | 明文 HTTP 监听器与 HTTP/1.1 上游 |
| 下游 HTTP/2 | 支持 | TLS 监听器通过 ALPN 通告 `h2`，可回退到 HTTP/1.1 |
| 上游 TLS | 支持 | `https://`，可配 SNI、CA 与客户端证书 |
| 上游 HTTP/2 | 支持 | 使用 `h2://`；上游的 HTTP/2 必须显式声明 |
| 上游 h2c | 支持 | 使用 `h2c://` 走明文 prior-knowledge HTTP/2 |
| WebSocket 升级 | 支持 | 由 `reverse_proxy` 透明转发 |
| 自动 HTTPS | 支持 | 默认 HTTP-01，也支持 Cloudflare DNS-01 与 TLS-ALPN-01 |
| TLS 终止 | 支持 | 站点级证书、internal 证书、版本/加密套件限制与 mTLS |

## 站点匹配

| 能力 | 状态 | 说明 |
| --- | --- | --- |
| 一个站点块写多个域名 | 支持 | 块体被编译为多个可独立寻址的站点 |
| IPv4 与 IPv6 监听器 | 支持 | HTTP/TLS 监听器使用显式的双栈行为 |
| 精确 host 与 SNI 匹配 | 支持 | 归一化后的域名按监听器匹配 |
| 通配 host 与 SNI 匹配 | 支持 | 仅最左一级标签；精确名优先 |
| 通配匹配裸域 | 不支持 | `*.example.com` 不匹配 `example.com` |
| 跨多级标签通配 | 不支持 | `*.example.com` 不匹配 `a.b.example.com` |

## 四层

四层运行在**原生 Tokio** 数据面上：Raddex 自己绑定套接字、自己 accept、自己终止
TLS、自己做上游选择与健康检查，并用 `tokio::io` 中继。**它转发的任何一个字节都
不经过 Pingora**——Pingora 负责承载进程并运行 HTTP 核心。两个核心为什么要拆开，
见[架构记录](https://github.com/chulingera2025/raddex/blob/main/docs/PINGORA_CAPABILITY_RESEARCH.md)。

| 能力 | 状态 | 说明 |
| --- | --- | --- |
| 裸 TCP 代理 | 支持 | 不解析 HTTP；连接数与超时均有上界 |
| TCP SNI 透传 | 支持 | 检查 ClientHello 后原样转发 |
| TCP TLS 终止 | 支持 | 先终止 TLS，再走裸字节中继 |
| UDP 数据报代理 | 支持 | 每客户端 flow 状态，资源占用有界 |
| UDP IPv6 上游 | 支持 | flow socket 保持地址族一致 |
| 透明 TCP | 仅限 Linux | 需要 TPROXY 规则、策略路由，以及等价于 `CAP_NET_ADMIN` 的权限 |
| 四层负载均衡 | 支持 | 在健康上游中做 round-robin、random 与一致性哈希 `ip_hash` |
| 四层主动健康检查 | 支持 | TCP 连接探活，带连续失败/连续成功的抖动抑制 |
| TCP 监听器升级交接 | 仅限 Linux | 显式交接监听描述符；交接失败即升级失败 |
| UDP 无损升级 | 仅限 Linux | 交接监听器、已连接的 flow socket 与有界元数据 |
| QUIC 数据报透传 | 透传 | QUIC 包被当作普通 UDP 数据报处理 |
| QUIC / HTTP/3 终止 | 需要 sidecar | Raddex 不实现 QUIC 握手、HTTP/3 路由或连接迁移 |

## 运维边界

| 行为 | 预期 |
| --- | --- |
| `raddex check` | 与重载使用完全相同的配置校验规则 |
| SIGHUP 重载 | 为**新**请求替换路由与运行时策略；已有连接保持其已选上游 |
| 监听器拓扑变更 | 重载与升级预检都会拒绝；必要时请正常重启 |
| 透明 TCP 升级 | 标准交接路径不支持；请正常重启 |
| 限流 | 内存内、按进程计数，**不是**集群级 |
| DNS-01 服务商 | v0.3.6 已实现的是 Cloudflare |

## 怎么选边界

需要终止 HTTP、TLS、TCP 或 UDP 时，直接用 Raddex。当部署需要 HTTP/3 终止、
HTTP/3 路由、连接迁移或 QUIC 感知的负载均衡时，请在 Raddex 前面或旁边放一个专门
的 QUIC/HTTP/3 服务。**不要因为 UDP 透传能跑通，就推断这些能力存在。**
