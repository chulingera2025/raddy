# Raddex

[![CI](https://github.com/chulingera2025/raddex/actions/workflows/ci.yml/badge.svg)](https://github.com/chulingera2025/raddex/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/chulingera2025/raddex)](LICENSE)
[![Version](https://img.shields.io/badge/version-v0.3.5-blue)]()
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange)]()
[English](README.md)

基于 Cloudflare [Pingora](https://github.com/cloudflare/pingora) 构建的极简高性能反向代理网关（Rust）。Raddex 结合了 **Caddy 级别的开发者体验**（显式书写顺序的配置 DSL + 原生自动 HTTPS）与 **Pingora 级别的性能与内存安全**（Rust、无 GC、多线程共享连接池）。

## 为什么做 Raddex

Rust 网关生态的现状大致两类：直接手写 Pingora 代码，或者使用现成的发行版。前者灵活但门槛高（无配置模型），后者配置模型各异、扩展受限。Raddex 想在三件事上给出不同的答案：

1. **可预期的配置体验**：指令严格按书写顺序执行，不引入隐式指令排序表。配置结果可预期、无需查表。
2. **自动 HTTPS 作为一等公民**：原生集成 ACME，自动申请证书并支持 SNI 动态证书，开箱即用。
3. **Pingora 引擎的硬核能力**：多线程共享连接池、无 GC，以及零停机二进制升级（`raddex upgrade` 将监听套接字移交给新二进制，不丢弃任何请求）。

## 功能特性

- **显式书写顺序 DSL**（Raddexfile）：终端指令 vs 修饰指令、`handle` 互斥匹配块、无隐藏排序规则。见 [Raddexfile 规范](docs/RADDEXFILE_SPEC.zh_CN.md)。
- **自动 HTTPS**：具名站点通过 ACME 自动签发（默认 HTTP-01，也支持 `dns_challenge cloudflare <token>` 的 DNS-01 和 `tls_alpn_challenge` 的 TLS-ALPN-01），支持 SNI 动态/通配符证书，含 On-Demand `ask` 授权回调。
- **配置热重载**：SIGHUP 原子替换路由快照；上游连接池跨重载存活（零中断）。
- **静态托管 + 压缩**：`file_server` 带目录穿越防护；`encode` 支持 gzip/zstd 并按 `Accept-Encoding` 选择。
- **可观测性**：结构化 JSON 访问日志 + Prometheus `/metrics` 端点（QPS + 延迟）。
- **转发**：`reverse_proxy` 支持 `to` 多目标轮询、上游 HTTP/2/h2c、多域名共享站点块、IPv6 监听、头改写、重定向、干净的 400/404 兜底。
- **边缘防护**：`rate_limit remote_ip` 单机令牌桶限流，配合 `trusted_proxies` 解析真实客户端 IP。
- **迁移**：`raddex import caddyfile|nginx <file>` 将常见 Caddyfile / nginx.conf 子集转换为 Raddexfile（独立转换器，永不改动 Raddexfile 文法）。
- **四层代理**：`tcp <address> { ... }` 与 `udp <address> { ... }` 监听器代理裸 TCP 连接和 UDP 数据报（不做 HTTP 解析），支持负载均衡、通配符 SNI 透传、TLS 终止、Linux 透明路由、空闲/连接超时、连接/flow 上限、健康检查、DNS 刷新、Linux UDP flow 无损交接与指标。

## 快速开始

安装最新发布（校验过的安装脚本，非 `curl | sudo bash`）：

```bash
curl -fsSL -O https://github.com/chulingera2025/raddex/releases/latest/download/install.sh
./install.sh
raddex --version
```

或从源码构建：

```bash
cargo build --release
./target/release/raddex --version
```

写一个 `Raddexfile`：

```
:8080 {
    reverse_proxy 127.0.0.1:9000
}
```

运行：

```bash
raddex run -c Raddexfile
curl http://127.0.0.1:8080/
```

自动 HTTPS：配置具名站点并用 ACME 运行（需公网可达的域名与 80/443 端口）：

```
raddex.test {
    reverse_proxy 127.0.0.1:9000
}
```

```bash
raddex run -c Raddexfile --acme-directory https://acme-v02.api.letsencrypt.org/directory
```

80 端口不可达？在全局块加 `dns_challenge cloudflare <token>`，改用 DNS-01
证明控制权（见 [Raddexfile 规范](docs/RADDEXFILE_SPEC.zh_CN.md)）。

## 配置

完整语法与语义见 [Raddexfile 规范](docs/RADDEXFILE_SPEC.zh_CN.md)：站点、`handle`、头改写、静态文件、压缩、重定向、全局块。

## 文档

- [安装](docs/INSTALL.zh_CN.md) — 安装脚本、手动安装、Docker
- [Raddexfile 规范](docs/RADDEXFILE_SPEC.zh_CN.md) — 配置语言
- [性能](docs/PERFORMANCE.zh_CN.md) — 可复现的 QPS / P99 基线
- [English](README.md)

## 开发状态

核心实现已完成：转发 + 热重载、Raddexfile 解析器（fuzz 验证、带行列号错误）、ACME 自动 HTTPS（HTTP-01、DNS-01、TLS-ALPN-01，自动续期）、站点级 TLS（静态/internal 证书、mTLS、上游 TLS）、上游 HTTP/2/h2c、多域名与通配符路由、IPv6 监听、健康检查与负载均衡策略、单机限流与可信客户端 IP 模型、gzip/zstd/brotli 压缩（流式、感知 Range）、结构化 JSON 与 common 格式访问日志、Caddyfile/nginx 迁移工具、零停机二进制升级，以及已通过集成测试的 L4 TCP/SNI/UDP 代理（含 TLS 终止、Linux 透明路由与 UDP flow 交接）。QUIC/HTTP/3 终止因 Pingora 0.8.1 没有 QUIC transport，仍保持为独立 sidecar 边界。发布工具链提供 Linux x86_64/aarch64 的校验和安装脚本与 Docker。

## License

[Apache-2.0](LICENSE) —— 与 Pingora 一致。
