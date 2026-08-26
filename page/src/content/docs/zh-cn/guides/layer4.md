---
title: 四层代理（TCP 与 UDP）
description: 用 tcp 与 udp 监听器做裸 TCP 与 UDP 代理——负载均衡、超时、健康检查、通配符 SNI、TLS 终止、透明路由与 UDP 交接。
---

除 HTTP 之外，raddy 可用 `tcp` 与 `udp` **顶级监听器**代理裸 TCP 连接与 UDP
数据报——它们是 HTTP 站点块的平级，而非其内部指令。它们只拥有传输概念：
上游选择、超时、连接/流上限与中继。普通 TCP 不终止 TLS；可选的 `tls` 配置
会在同一中继前先终止 TLS。

## 裸 TCP

```caddyfile
tcp :3306 {
    to db-1.internal:3306 db-2.internal:3306
    lb_policy round_robin          # round_robin | random | ip_hash
    connect_timeout 3s
    idle_timeout 5m
    max_connections 10000
    health_check {
        interval 10s
        timeout 2s
    }
}
```

- **`to <host>:<port>...`** —— 至少一个上游。主机名在启动时解析，并按周期
  重新解析（默认 60s）；瞬时 DNS 失败会保留最后可用地址。
- **`lb_policy`** —— `round_robin`（默认）、`random` 或 `ip_hash`（源 IP
  粘滞）。
- **`connect_timeout`** 限制单次上游连接；**`idle_timeout`** 是*真正的*空闲
  超时（任一方有流量即重置，长活活跃连接不超时）；**`max_connections`** 限制
  并发连接数（超额被拒并计数）。
- **`health_check { ... }`** —— 主动 TCP 连接探活；不健康上游被跳过，全部
  不健康时拒绝新连接。
- **重载** —— SIGHUP 重载把新的上游集合/策略/限制应用到*新*连接；已有连接
  保持其上游。修改绑定地址属于拓扑变更，会被拒绝（请重启或 `raddy upgrade`）。
- 每条关闭的连接写一条类型化 JSON 访问日志与 `raddy_l4_tcp_*` 指标。

### SNI 路由

`tcp` 监听器可按精确 ClientHello SNI 路由 TLS 连接——不终止 TLS：

```caddyfile
tcp 0.0.0.0:443 {
    sni api.example.com 10.0.0.1:9001
    sni web.example.com  10.0.0.2:9002
    fallback             10.0.0.3:9003
}
```

ClientHello 在有界前缀内检查（从不修改），原始字节原样转发到匹配的上游。
未知/缺失/畸形的 SNI 在设置 `fallback` 时走 fallback，否则关闭连接。精确名称
优先，通配符只匹配一个最左标签。`sni` 与 `to` 互斥；SNI 模式不支持
`health_check`。

### 透明 TCP

```caddyfile
tcp :15000 {
    transparent
    to 127.0.0.1:8080
}
```

Linux 下 `transparent` 启用 `IP_TRANSPARENT`，在 TPROXY 规则提供元数据时使用
原始目的地址，并用原始客户端地址建立出站连接。需要 `CAP_NET_ADMIN`、TPROXY
规则和策略路由。透明监听器由自定义服务持有，升级时必须普通重启。

## UDP

```caddyfile
udp :53 {
    to 1.1.1.1:53 8.8.8.8:53
    lb_policy ip_hash
    idle_timeout 30s
    max_flows 50000
    max_datagram_size 4096
    recv_buffer 4MiB
    send_buffer 4MiB
}
```

- 每个客户端（地址 + 端口）映射为一个 **flow**，各自持有与所选上游相连的
  socket——本地临时端口负责上游响应的多路复用。选择每个 flow 只发生一次；
  `ip_hash` 按客户端 *IP* 钉住。
- **上限** —— `max_flows` 限制流表大小（最旧优先驱逐）、`idle_timeout` 驱逐
  空闲 flow、`max_datagram_size` 丢弃并计数超大报文、`recv_buffer`/`send_buffer`
  设置 socket 缓冲（0 = 系统默认）。IPv4 与 IPv6 上游均支持。
- UDP 与 TCP 可共享同一地址端口。
- 指标：`raddy_l4_udp_*`。
- **Linux UDP 支持零停机升级**：raddy 在替代进程开始接收前交接监听 fd、已连接
  上游 flow fd 和有界 flow 元数据。
- **QUIC 透传**通过 UDP 可用，因为 QUIC 承载于数据报。Pingora 0.8.1 不提供
  QUIC/HTTP/3 终止、HTTP/3 路由或连接迁移；这些需要独立的 QUIC/HTTP/3 sidecar。
