---
title: 指令参考
description: 每一条 Raddyfile 指令的语法、参数与可运行示例。
---

这是 Raddyfile 的完整参考。每条指令采用统一结构:作用、语法、参数、示例。
如果你刚接触这门语言,请先读[核心概念](../)建立心智模型。

## 一览

| 指令 | 作用 | 类型 |
|---|---|---|
| [`reverse_proxy`](#reverse_proxy) | 将请求代理到一个或多个上游 | 终端 |
| [`handle`](#handle) | 匹配一个路径并用一个终端服务 | 终端(作用域) |
| [`file_server`](#file_server) | 提供静态文件 | 终端 |
| [`redir`](#redir) | 重定向客户端 | 终端 |
| [`header_up` / `header_down`](#header_up--header_down) | 改写请求 / 响应头 | 修饰 |
| [`encode`](#encode) | 压缩响应(gzip / zstd) | 修饰 |
| [`rate_limit`](#rate_limit) | 拒绝超过速率限制的请求 | 修饰 |
| [`root`](#root) | 为块设置静态文件根目录 | 辅助 |
| [`trusted_proxies`](#trusted_proxies) | 推导真实客户端 IP 的可信网络 | 配置 |
| [`dns_challenge`](#dns_challenge) | 经 DNS 服务商(Cloudflare)做 DNS-01 签发 | 配置 |
| [`log_level`](#log_level) | 全局日志级别 | 配置 |
| [`acme_email`](#acme_email) | ACME 注册邮箱 | 配置 |
| [`snippet` / `import`](#snippet--import) | 可复用片段与包含 | *规划中* |

## `reverse_proxy`

**作用。** 把请求转发给一个上游服务 —— 反向代理的核心。

**语法。**

```caddyfile
reverse_proxy <target>

reverse_proxy {
    to <upstream>...
    lb_policy round_robin|random|ip_hash
    health_check { ... }
}
```

**参数。**

- `<target>` / `<upstream>` —— 上游地址,例如 `127.0.0.1:8080`。
- `to` —— 列出多个上游以轮询分发。
- `lb_policy` —— 选择算法,默认 `round_robin`。见[负载均衡](#lb_policy-and-health_check)。
- `health_check { ... }` —— 对上游的主动健康检查。见[健康检查](#lb_policy-and-health_check)。

**示例。**

```caddyfile
api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

## `handle`

**作用。** 为某个路径前缀运行一个终端,然后停止。`handle` 块内的一切只作用
于路径匹配的请求。

**语法。**

```caddyfile
handle /path/* {
    # 此路径的终端(及修饰指令)
}
```

**参数。** 一个路径匹配器 —— `/static/*` 匹配 `/static/` 下的任意路径。

**示例。** 静态资源走磁盘,其余请求代理给应用:

```caddyfile
example.com {
    handle /static/* {
        root /var/www/html
        file_server
    }

    reverse_proxy 127.0.0.1:8080
}
```

## `file_server`

**作用。** 从磁盘提供文件。

**语法。**

```caddyfile
file_server
```

**参数。** 无。根目录来自同一作用域块内的 [`root`](#root)。

**行为。** 提供 `root` + 完整请求路径,含任何 `handle` 前缀 ——
`handle /static/* { root /var/www; file_server }` 将 `/static/foo` 映射到
`/var/www/static/foo`。目录提供其 `index.html`。路径穿越(`..`)被 `404`
拒绝。仅允许 `GET` 与 `HEAD`。

**示例。**

```caddyfile
static.example.com {
    root /var/www/html
    file_server
}
```

## `redir`

**作用。** 向客户端发送 HTTP 重定向。

**语法。**

```caddyfile
redir <target> [code]
```

**参数。**

- `<target>` —— 重定向目标。占位符:`{host}`、`{uri}`。
- `<code>` —— 3xx 状态码或关键字,默认 `308`。`permanent` = `308`,
  `temporary` = `302`。

**示例。** 把所有请求重定向到 HTTPS,保留主机与路径:

```caddyfile
:80 {
    redir https://{host}{uri} permanent
}
```

## `header_up` / `header_down`

**作用。** 在上游请求上(`header_up`)与发回客户端的响应上(`header_down`)
添加、设置或移除头。

**语法。**

```caddyfile
header_up <name> <value>
header_down <name> <value>
```

**参数。** `<name>` 是头名;`<value>` 是值或占位符:`{remote_host}`(TCP
客户端套接字地址——直接对端 IP,而非可信代理推导的有效客户端 IP)、
`{host}`、`{uri}`。

**示例。** 把客户端套接字地址透传给后端:

```caddyfile
api.example.com {
    reverse_proxy 127.0.0.1:8080
    header_up X-Real-IP {remote_host}
}
```

## `encode`

**作用。** 压缩响应。**参数顺序即优先级** —— raddy 选用客户端也支持的
第一个算法。

**语法。**

```caddyfile
encode <algorithm>...
```

**参数。** `gzip`、`zstd` —— 按优先级排列。

**示例。** 优先 zstd,回退 gzip:

```caddyfile
example.com {
    encode zstd gzip
    reverse_proxy 127.0.0.1:8080
}
```

`encode` 同样作用于 `file_server` 的响应:

```caddyfile
static.example.com {
    root /var/www/html
    file_server
    encode zstd gzip
}
```

## `rate_limit`

**作用。** 以 `429 Too Many Requests` 拒绝超过速率限制的请求。

**语法。**

```caddyfile
rate_limit <key> <rate> [burst=<n>]
```

**参数。**

- `<key>` —— `remote_ip`,真实客户端 IP(见[可信代理](../trusted-proxies/))。
- `<rate>` —— `<count>r/<unit>`,单位是 `s` / `m` / `h` / `d`,例如 `50r/s`、
  `1200r/m`。count 至少为 1。
- `burst=<n>` —— 令牌桶容量,默认等于 rate 的 count。

**行为。** 按 (站点, 终端, 客户端 IP) 计令牌桶。桶按 `<rate>` 持续补充;
取不到令牌的请求被 `429` 拒绝。`rate_limit` 是修饰指令:守卫服务该块的任意
终端,状态跨 SIGHUP 重载存活。

**示例。**

```caddyfile
api.example.com {
    rate_limit remote_ip 100r/s burst=200
    reverse_proxy 127.0.0.1:8080
}
```

## `root`

**作用。** 设置 [`file_server`](#file_server) 服务的文件系统根目录。

**语法。**

```caddyfile
root <path>
```

**参数。** `<path>` —— 文件系统路径,直接写在作用域块内。没有 `root *`
通配符。

**示例。**

```caddyfile
static.example.com {
    root /var/www/html
    file_server
}
```

## `trusted_proxies`

**作用。** 声明哪些网络是可信代理,使 raddy 能从 `X-Forwarded-For` 推导真实
客户端 IP。见[可信代理](../trusted-proxies/)。

**语法。**

```caddyfile
trusted_proxies <cidr>...
```

**参数。** 一个或多个网络 —— `<address>/<prefix>` 或裸地址。IPv4 与 IPv6 均
支持。站点块的值仅对该站点覆盖全局列表。

## `dns_challenge`

**作用。** 用 **DNS-01** 代替 HTTP-01 签发证书,通过 DNS 服务商发布 TXT
记录证明域名控制权。适合 80 端口不可达的场景。见[站点 · 端口 ·
HTTPS](../sites/)。

**语法。**

```caddyfile
dns_challenge cloudflare <api_token>
```

**参数。** 服务商(`cloudflare`——目前唯一)与服务商的 API 令牌,令牌需要
**Zone: DNS: Edit** 权限。位于[全局块](../sites/#全局块)。

**行为。** 配置后,本实例上所有证书走 DNS-01 签发:raddy 在校验订单期间发布
`_acme-challenge.<host>` TXT 记录,完成后移除。未配置 `dns_challenge` 时
沿用 HTTP-01。

> **安全:** API 令牌是机密——注意不要让 Raddyfile 落入版本控制。

**示例。**

```caddyfile
{
    acme_email ops@example.com
    dns_challenge cloudflare <api_token>
}

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

## `log_level`

**作用。** 设置全局日志级别。

**语法。**

```caddyfile
log_level <level>
```

**参数。** `info`(默认) | `debug` | `warn` | `error`。

## `acme_email`

**作用。** 设置 ACME 注册邮箱(Let's Encrypt 必填)。

**语法。**

```caddyfile
acme_email <address>
```

**参数。** 一个邮箱地址。位于[全局块](../sites/#全局块)。

## `snippet` / `import`

**作用。** 可复用片段与多文件包含。目前**规划中、尚未实现** —— 语法槽位
已预留,但不要在你交付的配置中使用。

## `lb_policy` 与 `health_check`

[`reverse_proxy`](#reverse_proxy) 块的子指令。

**`lb_policy`** —— 上游选择算法。

- `round_robin`(默认)—— 依次轮转上游。
- `random` —— 均匀随机选择。
- `ip_hash` —— 按客户端 IP 一致性哈希(按 IP 的会话粘性)。

**`health_check { ... }`** —— 主动健康检查(TCP 连接探测)。每个参数均可选:

| 参数 | 默认 | 含义 |
|---|---|---|
| `interval <dur>` | `5s` | 探测周期 |
| `timeout <dur>` | `2s` | 单次探测超时 |
| `consecutive_failures <n>` | `3` | 连续 N 次失败才摘除上游 |
| `consecutive_successes <n>` | `2` | 连续 M 次成功才恢复上游 |

时长是数字加单位(`ms` / `s` / `m` / `h`),或裸数字表示秒。

**行为。** 不健康的上游永远不会被选中,恢复后自动回流。若**所有**上游都不
健康,raddy 返回 `502`。健康状态跨 SIGHUP 重载存活,仅在上游列表、策略或
健康检查参数变化时才重建。

**示例。**

```caddyfile
api.example.com {
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
}
```

## 完整示例

```caddyfile
{
    acme_email ops@example.com
    log_level info
    trusted_proxies 127.0.0.1
}

# HTTP → HTTPS 重定向
:80 {
    redir https://{host}{uri} permanent
}

api.example.com {
    rate_limit remote_ip 50r/s burst=100

    handle /static/* {
        root /var/www/html
        file_server
        encode zstd gzip
    }

    reverse_proxy 127.0.0.1:8080
    header_up X-Real-IP {remote_host}
}
```

> `reverse_proxy` 之后的 `header_up` 仍然生效 —— 它是修饰指令。`handle` 块
> 内的 `encode` 只作用于该块的 `file_server`。`rate_limit` 守卫服务该站点的
> 任意终端。
