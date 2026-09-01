---
title: 代理 API
description: 跨上游负载均衡 API,带健康检查、限流与真实客户端 IP。
---

## 目标

通过 HTTPS 暴露一个 API:两个后端实例、其中一个掉线时自动故障转移、按客户端
限流,并把客户端套接字地址转发给后端。

## 配置

```caddyfile
{
    acme_email ops@example.com
    trusted_proxies 10.0.0.0/8
}

api.example.com {
    rate_limit remote_ip 100r/s burst=200

    reverse_proxy {
        to 10.0.0.1:8000 10.0.0.2:8000
        lb_policy round_robin
        health_check {
            interval 5s
            timeout 2s
            consecutive_failures 3
            consecutive_successes 2
        }
    }

    header_up X-Real-IP {remote_host}
}
```

每部分的作用:

- **`rate_limit remote_ip 100r/s burst=200`** —— 每个客户端每秒 100 个请求,
  突发 200;超出返回 `429 Too Many Requests`。
- **`to`** —— 把请求分发到两个上游。
- **`lb_policy round_robin`** —— 默认的选择顺序(可选 `round_robin`、
  `random`、`ip_hash`)。
- **`health_check`** —— 每 `5s`(超时 `2s`)对每个上游做一次 TCP 连接探测。
  连续 `3` 次失败才摘除上游,连续 `2` 次成功才恢复 —— 这种 flapping 抑制
  避免在抖动网络上反复横跳。
- **`header_up X-Real-IP {remote_host}`** —— 把客户端套接字地址(直接 TCP
  对端)传给后端。位于可信代理之后时,这是代理的地址,而非 `rate_limit`
  所用的有效客户端 IP。

## 运行

```bash
raddex check -c Raddexfile
raddex run -c Raddexfile
```

## 你能得到什么

- **分发** —— 连续请求交替落在 `10.0.0.1:8000` 与 `10.0.0.2:8000`。
- **故障转移** —— 停掉一个后端;经过 `consecutive_failures` 次探测后它被
  摘除,流量转向健康实例。重启后经过 `consecutive_successes` 次成功自动回流。
- **全挂** —— 若*每个*上游都不健康,raddex 返回 **`502 Bad Gateway`**,而不是
  静默黑洞请求。
- **限流** —— 超限客户端收到 `429`;健康状态与令牌桶跨 SIGHUP 重载存活。

## 变体

**按 IP 粘性** —— 把 `round_robin` 换成 `ip_hash`,客户端会持续命中同一上游
(对有状态后端有用)。

**代理 WebSocket** —— 无需额外配置。`reverse_proxy` 透明转发 HTTP `Upgrade`
请求,因此同一站点同时服务 WebSocket 与普通 HTTP 流量:

```caddyfile
chat.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

上游应答 `101 Switching Protocols` 后,raddex 便隧道该连接;后端本身必须能说
升级后的协议。`encode` 绝不作用于升级响应。协议升级见[指令参考](../../config/directives/#websocket-and-protocol-upgrades)。

**不同路径** —— 与 `handle` 组合,可在同一主机上为 API 限流、负载均衡,同时
托管静态资源:

```caddyfile
api.example.com {
    handle /static/* {
        root /var/www/html
        file_server
    }

    rate_limit remote_ip 100r/s
    reverse_proxy {
        to 10.0.0.1:8000 10.0.0.2:8000
        health_check {
            interval 5s
            timeout 2s
        }
    }
}
```
