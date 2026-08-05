# Raddy

Raddy 是基于 [Cloudflare Pingora](https://github.com/cloudflare/pingora) 构建的
极简高性能反向代理网关（Rust）。它结合了 Caddy 风格的配置 DSL（Raddyfile）
与 Pingora 引擎：显式书写顺序的配置、ACME 自动 HTTPS、多线程共享连接池。

## 文档

- [安装](install.md) — 安装脚本、手动安装、Docker
- [配置](spec.md) — Raddyfile 规范
- [性能](performance.md) — 可复现的 QPS / P99 基线

## 快速开始

```bash
raddy run -c Raddyfile
```

先写一个 `Raddyfile`，用 `raddy check -c Raddyfile` 校验后再运行。完整语法见
[Raddyfile 规范](spec.md)。
