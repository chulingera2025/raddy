---
title: 指令参考
description: 每一条 Raddyfile 指令的语法、参数与可运行示例。
---

这是 Raddyfile 的完整参考。每条指令采用统一结构:作用、语法、参数、示例。
如果你刚接触这门语言,请先读[核心概念](../)建立心智模型;[路由与
匹配器](../../guides/routing/)介绍匹配器与路由指令如何配合使用。

## 一览

| 指令 | 作用 | 类型 |
|---|---|---|
| [`reverse_proxy`](#reverse_proxy) | 将请求代理到一个或多个上游(含 TLS 后端) | 终端 |
| [`handle`](#handle) | 匹配一个匹配器并用一个终端服务 | 终端(作用域) |
| [`handle_path`](#handle_path) | 类似 `handle`,但剥离匹配的路径前缀 | 终端(作用域) |
| [`respond`](#respond) | 直接以状态码与可选响应体应答 | 终端 |
| [`error`](#error) | 触发内部错误响应 | 终端 |
| [`file_server`](#file_server) | 提供静态文件 | 终端 |
| [`redir`](#redir) | 重定向客户端 | 终端 |
| [`rewrite`](#rewrite) | 转发前改写请求 URI | 修饰 |
| [`header_up` / `header_down`](#header_up--header_down) | 改写请求 / 响应头 | 修饰 |
| [`encode`](#encode) | 压缩响应(gzip / zstd / br) | 修饰 |
| [`rate_limit`](#rate_limit) | 拒绝超过速率限制的请求(`remote_ip` 或 `header <name>`) | 守卫 |
| [`basic_auth`](#basic_auth) | 要求 HTTP Basic 认证 | 守卫 |
| [`forward_auth`](#forward_auth) | 把认证委托给上游 | 守卫 |
| [`root`](#root) | 为块设置静态文件根目录 | 辅助 |
| [`tls`](#tls) | 按站点的 TLS 来源、选项与 mTLS | 配置(站点) |
| [`access_log`](#access_log) | 配置访问日志(全局),或按站点关闭 | 配置 |
| [`trusted_proxies`](#trusted_proxies) | 推导真实客户端 IP 的可信网络 | 配置 |
| [`dns_challenge`](#dns_challenge) | 经 DNS 服务商(Cloudflare)做 DNS-01 签发 | 配置 |
| [`tls_alpn_challenge`](#tls_alpn_challenge) | 在 443 上使用 TLS-ALPN-01 签发 | 配置 |
| [`log_level`](#log_level) | 全局日志级别 | 配置 |
| [`acme_email`](#acme_email) | ACME 注册邮箱 | 配置 |
| [`import` / `(name)`](#import-and-snippets) | 多文件包含 / 可复用片段 | 配置(DX) |
| `{$ENV}` | 任意参数中的环境变量替换 | 令牌 |

## 匹配器

**作用。** 选择哪些请求由某条指令或 `handle` 块处理。匹配器将早期版本中仅限
路径的内联匹配器推广为通用形式。

**语法。** 匹配器是一串**匹配项**;所有项都必须匹配(AND)。以 `/` 开头的裸
值即 `path` 的简写:

```caddyfile
handle path /status method GET { ... }        # AND 条件,无括号
handle /static/* { ... }                       # 裸前缀 = path 简写
reverse_proxy !path /admin/* { to 127.0.0.1:8080 }
```

**匹配项。**

| 匹配项 | 匹配条件 |
|---|---|
| `path <prefix>` | 请求路径等于前缀或位于其下(`/api` 匹配 `/api` 与 `/api/...`,不匹配 `/apix`)。末尾 `*` 会被去除(`/api/*` ≡ `/api`);前缀 `/` 匹配所有路径。 |
| `host <host>` | 规范化后的 Host 头(去端口、去末尾点、转小写)等于该值。 |
| `method <method>` | 请求方法等于该值(如 `GET`)。 |
| `header <name> <value>` | 请求头 `name` 等于 `value`(名字不区分大小写;值精确匹配)。 |
| `query <key> <value>` | 查询参数 `key` 的值等于 `value`。 |
| `remote_ip <cidr>...` | **真实客户端 IP**(见[可信代理](../trusted-proxies/))位于列出的网络内。 |
| `protocol <http\|https>` | 接收请求的监听器的传输协议。 |

以 `!` 前缀的匹配项表示取反(`!path /admin/*`)。各项**以空格分隔并 AND**——
没有括号或 `&&` 运算符:

```caddyfile
handle path /status method GET { ... }   # 正确
handle (path /status && method GET) { ... }  # 非法——不支持括号 / && 语法
```

**匹配器附着在哪。** 匹配器附着在 `handle` / `handle_path` 块上,也可作为
内联匹配器附着在终端指令上——`reverse_proxy`、`respond` 与 `error`:

```caddyfile
reverse_proxy path /api/* { to 127.0.0.1:8080 }
respond method OPTIONS 204
error !path /assets/* 503
```

内联匹配器不匹配时,该终端是**空操作**:执行继续到下一行指令。没有匹配器的
终端总是匹配。

## `reverse_proxy`

**作用。** 把请求转发给一个上游服务 —— 反向代理的核心。

**语法。**

```caddyfile
reverse_proxy <target>

reverse_proxy [<matcher>] {
    to <upstream>...
    lb_policy round_robin|random|ip_hash
    health_check { ... }
}
```

**参数。**

- `<target>` / `<upstream>` —— 上游地址。上游默认是纯 HTTP;以 `https://`
  前缀启用到后端的 TLS(见[上游 TLS 选项](#upstream-tls-options))。裸的
  `host:port` 保持纯 HTTP。
- `to` —— 列出多个上游以轮询分发。
- `lb_policy` —— 选择算法,默认 `round_robin`。见[负载均衡](#lb_policy-and-health_check)。
- `health_check { ... }` —— 对上游的主动健康检查。见[健康检查](#lb_policy-and-health_check)。
- `<matcher>` —— 可选的内联[匹配器](#matchers)。

**行为。** 上游默认使用 HTTP/1.1；`h2://host:port` 启用带 TLS 的 HTTP/2，
`h2c://host:port` 启用明文先验 HTTP/2。`h2c://` 要求上游直接接受
HTTP/2 connection preface，不使用 HTTP/1.1 Upgrade。WebSocket 及其他
HTTP `Upgrade` 请求会被透明转发——见 [WebSocket 与协议升级](#websocket-and-protocol-upgrades)。

**示例。**

```caddyfile
api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

## `handle`

**作用。** 为匹配的请求运行一个终端,然后停止。`handle` 块内的一切只作用
于匹配器匹配的请求。

**语法。**

```caddyfile
handle <matcher> {
    # 匹配请求的终端(及修饰指令)
}
```

**参数。** 一个[匹配器](#matchers)。`handle /static/*` 是
`handle path /static/*` 的简写。

**行为。** 若匹配器匹配,块内指令运行并**停止匹配**——站点其余部分不再被
考虑。若不匹配,执行继续越过该块。`handle` 是把路径(或任意匹配器)与终端
分组的方式。

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

## `handle_path`

**作用。** 类似 `handle`,但被匹配的路径前缀会从 URI 中**剥离**,然后块内
终端才运行。

**语法。**

```caddyfile
handle_path <matcher> {
    # 终端(及修饰指令)
}
```

**参数。** 一个[匹配器](#matchers)。

**行为。** 块内终端看到的请求不含匹配前缀,因此后端无需知道自己挂载在哪个
前缀下。路径匹配器末尾的 `*` 会先被去除。

**示例。** 把 `/api/users/1` 以 `/users/1` 转发给后端:

```caddyfile
example.com {
    handle_path /api/* {
        reverse_proxy 127.0.0.1:8080
    }

    reverse_proxy 127.0.0.1:9000
}
```

## `respond`

**作用。** 直接以状态码与可选响应体应答请求——不经过上游,也不读文件。

**语法。**

```caddyfile
respond [<matcher>] <status> [<body>]
```

**参数。**

- `<matcher>` —— 可选的内联[匹配器](#matchers)。
- `<status>` —— 三位 HTTP 状态码(100–599)。
- `<body>` —— 可选的响应体。

**行为。** 终端指令:第一个匹配的终端结束站点执行。可用于健康检查端点、CORS
预检应答、维护横幅等固定响应。

**示例。** 一个站点上同时提供健康端点与 CORS 预检应答:

```caddyfile
api.example.com {
    handle path /health {
        respond 200 ok
    }

    handle method OPTIONS {
        respond 204
    }

    reverse_proxy 127.0.0.1:8080
}
```

## `error`

**作用。** 以选定的状态码与可选消息触发 raddy 的内部错误响应。

**语法。**

```caddyfile
error [<matcher>] [<status>] [<message>]
```

**参数。**

- `<matcher>` —— 可选的内联[匹配器](#matchers)。
- `<status>` —— 错误响应的状态码;默认 `500`。
- `<message>` —— 可选的、包含在响应中的消息。

**行为。** 终端指令:服务该错误并结束站点执行。可用匹配器配合拦截某个路径并
返回特定状态码,例如为要屏蔽的区域返回 `403`。

**示例。**

```caddyfile
example.com {
    handle /internal/* {
        error 404 not here
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
拒绝。隐藏文件永不服务——任何以 `.` 开头的路径段(`.env`、`.git/`、
`.htaccess`)一律 `404`,唯一例外是 `.well-known` 目录(RFC 8615 的
well-known URI 天生公开)。仅允许 `GET` 与 `HEAD`。`encode` 同样作用于
`file_server` 的响应;小于 64 字节的响应体不做压缩。

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

## `rewrite`

**作用。** 在转发前改写请求 URI。修饰指令:终端仍服务请求,但上游看到的是
改写后的路径。

**语法。**

```caddyfile
rewrite <to>
```

**参数。** `<to>` —— 改写后的 URI。占位符:`{host}`、`{uri}`、`{remote_host}`。

**行为。** 改写发生在终端运行之前,因此代理与文件服务都会看到新路径。条件
改写请放在 `handle` 块内。

**示例。** 为每个请求加上版本前缀:

```caddyfile
example.com {
    rewrite /v1/{uri}
    reverse_proxy 127.0.0.1:8080
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

**参数。** `gzip`、`zstd`、`br`(Brotli)—— 按优先级排列。

**行为。** 只有列出的算法才会被使用,并按客户端的 `Accept-Encoding` 协商;若
客户端一个都不支持,响应不压缩发送。`encode` 作用于 `reverse_proxy` 与
`file_server` 的响应,绝不作用于 `101`(升级,如 WebSocket)响应。

**示例。** 优先 Brotli,回退 zstd,再 gzip:

```caddyfile
example.com {
    encode br zstd gzip
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

- `<key>` —— 统计的对象:
  - `remote_ip` —— 真实客户端 IP(见[可信代理](../trusted-proxies/))。
  - `header <name>` —— 请求头 `<name>` 的值(如 `header X-API-Key`)。
    不带该请求头的请求共享一个桶。
- `<rate>` —— `<count>r/<unit>`,单位是 `s` / `m` / `h` / `d`,例如 `50r/s`、
  `1200r/m`。count 至少为 1。
- `burst=<n>` —— 令牌桶容量,默认等于 rate 的 count。

**行为。** 按 (站点, 终端, key 值) 计内存令牌桶。桶按 `<rate>` 持续补充,
最多持有 `burst` 个令牌;取不到令牌的请求被 `429` 拒绝。`rate_limit` 是守卫
(修饰指令):守卫服务该块的任意终端,状态跨 SIGHUP 重载存活。未匹配任何终端
的请求(404)不参与限流。作用域内有多个 `rate_limit` 时,各自独立计数。
限流按实例计——不跨集群共享。

**示例。** 按 API 密钥限流:

```caddyfile
api.example.com {
    rate_limit header X-API-Key 100r/s burst=200
    reverse_proxy 127.0.0.1:8080
}
```

或按客户端 IP:

```caddyfile
api.example.com {
    rate_limit remote_ip 100r/s burst=200
    reverse_proxy 127.0.0.1:8080
}
```

## `basic_auth`

**作用。** 要求块内请求通过 HTTP Basic 认证。

**语法。**

```caddyfile
basic_auth <user> <bcrypt-hash>
```

**参数。** `<user>` —— 用户名;`<bcrypt-hash>` —— 密码的 bcrypt 哈希。多个
`basic_auth` 指令构成用户表:请求必须为其中一个出示密码能通过哈希校验的
凭据。

**行为。** 守卫指令:无有效凭据的请求返回 **401 Unauthorized**,并带
`WWW-Authenticate: Basic` 挑战。与 `rate_limit` 一样被守卫——作用于服务该块
的任意终端。用 `htpasswd -B` 生成哈希:

```bash
htpasswd -Bbn admin 's3cret'
```

**示例。**

```caddyfile
admin.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:8080
}
```

## `forward_auth`

**作用。** 把认证委托给专用上游服务。

**语法。**

```caddyfile
forward_auth <host:port>
```

**参数。** `<host:port>` —— 认证上游,如 `auth.example.com:4181`。

**行为。** 守卫指令:raddy 把请求转发给认证上游,携带原始 `Authorization` 与
`X-Forwarded-For` 头,仅在 **2xx** 响应时放行。认证上游返回 **403** 时原样
透传给客户端;其他情况一律返回 **401**。认证上游的响应头(例如身份头)会在
真实上游看到请求之前被复制到请求上。

**示例。**

```caddyfile
app.example.com {
    forward_auth 127.0.0.1:4181
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

## `tls`

**作用。** 定制站点的 TLS:选择证书来源、限制协议版本与密码套件、要求客户端
证书(mTLS)。

**语法。**

```caddyfile
tls                              # ACME(默认;可省略)
tls internal                     # 自签证书,用于开发
tls <cert-file> <key-file>       # 静态 PEM 证书 + 私钥
tls min_version <1.2|1.3>
tls max_version <1.2|1.3>
tls ciphers <cipher-list>
tls client_auth <optional|require> <ca-file>
```

**参数。**

- **来源**(至多一个):
  - *(省略)* —— ACME,默认。具名站点自动获得证书(见[站点 · 端口 ·
    HTTPS](../sites/))。
  - `tls internal` —— 启动时生成的自签证书,用于开发;客户端需配置信任。
    不尝试 ACME。
  - `tls <cert-file> <key-file>` —— 为该站点提供静态 PEM 证书链与私钥,替代
    ACME。续期由运维负责。
- **选项**(全部可选;自由组合;每个选项单独一行 `tls`):
  - `min_version <1.2|1.3>` / `max_version <1.2|1.3>` —— 限制该站点协商的
    TLS 协议版本。
  - `ciphers <list>` —— OpenSSL 密码套件列表,如
    `ECDHE-ECDSA-AES128-GCM-SHA256`。空格分隔的名称以 `:` 连接。
  - `client_auth <optional|require> <ca-file>` —— 双向 TLS:按 `ca-file` 校验
    客户端证书。`require` 拒绝无有效证书的客户端;`optional` 请求证书但接受
    没有证书的客户端。

**行为。** 带 `tls` 指令的具名站点会将其端口绑定为 **TLS 监听器**(在默认
443 之外),因此 `intranet.example.com:8443 { tls internal }` 会在 8443 上提供
TLS。来源为静态或 internal 的站点,该主机名不会参与 ACME 签发。选项在握手
期间按 SNI 应用,并在重载时更新而无需重建监听器。

**示例。**

```caddyfile
api.example.com {
    tls internal
    reverse_proxy 127.0.0.1:8080
}

intranet.example.com {
    tls /etc/certs/intranet.pem /etc/certs/intranet.key
    reverse_proxy 127.0.0.1:9000
}

secure.example.com {
    tls client_auth require /etc/certs/clients-ca.pem
    reverse_proxy 127.0.0.1:9000
}
```

## `access_log`

**作用。** 在 Raddyfile 中配置访问日志,替代(或补充)`--access-log` CLI 标志。

**语法。**

```caddyfile
# 全局块:以路径与格式启用,或关闭
access_log <path> [format=<json|common>]
access_log off

# 站点块:仅关闭该站点的访问日志
access_log off
```

**参数。**

- `<path>` —— 追加写入的日志文件。
- `format=<json|common>` —— `json`(默认)每个请求写一个 JSON 对象;
  `common` 写经典 combined 日志行
  (`%h %l %u %t "%r" %>s %b "%{Referer}i" "%{User-Agent}i"`)。

**行为。** 在全局块中,`access_log <path>` 设置实例日志文件与格式;`access_log
off` 关闭整个实例的访问日志。在站点块中,`access_log off` 仅关闭该站点的
访问日志。当 Raddyfile 与 `--access-log` 标志同时设置时,标志胜出。JSON 字段
见[访问日志](../../operations/access-log/)。

**示例。**

```caddyfile
{
    access_log /var/log/raddy/access.log format=common
}

api.example.com {
    access_log off        # 该站点保持安静
    reverse_proxy 127.0.0.1:8080
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
**Zone: DNS: Edit** 权限。位于[全局块](../sites/#the-global-block)。

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

## `tls_alpn_challenge`

**作用。** 当 80 端口不可达时，使用 ACME TLS-ALPN-01 替代 HTTP-01。

**语法。**

```caddyfile
{
    tls_alpn_challenge
}
```

**行为。** 挑战在标准 TLS 443 上提供带 RFC 8737 `acmeIdentifier` 扩展和
`acme-tls/1` ALPN 的临时证书。它与 `dns_challenge` 互斥，并要求
ACME 站点使用 443 端口；不会回退到 HTTP-01。

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

**参数。** 一个邮箱地址。位于[全局块](../sites/#the-global-block)。

## `import` 与片段

**作用。** 跨文件拆分配置,并用命名块复用片段。

**语法。**

```caddyfile
import <file|name>

(name) {
    # 可复用指令
}
```

**行为。**

- `import <file>` 在该位置拼入另一个 Raddyfile 的内容。路径相对于导入文件。
  导入可以嵌套(有深度限制)。站点块可以导入其指令归属于该站点的文件。
- 顶层命名为 `(name) { ... }` 的块定义一个可复用的**片段**;`import name`
  在该位置将其拼入。片段仅对定义它的文件可见。

**示例。** 一个携带共享守卫的片段,导入到两个站点:

```caddyfile
(base) {
    rate_limit remote_ip 100r/s
    header_up X-Raddy true
}

api.example.com {
    import base
    reverse_proxy 127.0.0.1:8080
}

admin.example.com {
    import base
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:9000
}
```

## 环境变量

**作用。** 在解析时把环境变量的值注入 Raddyfile。

**语法。** 形如 `{$ENV_VAR}` 的指令参数会在解析时被 `ENV_VAR` 的值替换。
变量缺失是校验错误,因此写错的变量名会让 `raddy check` 失败,而不是带着错误
值启动。

**行为。** 适用于出现参数的任何位置 —— 上游目标、`root` 路径、`tls` 证书
路径等。

**示例。**

```caddyfile
api.example.com {
    reverse_proxy https://{$BACKEND_HOST}:8443
}
```

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

## 上游 TLS 选项

`reverse_proxy` 块的子指令,用于配置到 `https://` 后端的 TLS。它们要求至少
有一个 `https://` 上游,否则配置会被拒绝(视为无意义)。

```caddyfile
reverse_proxy {
    to https://10.0.0.1:8443 https://10.0.0.2:8443
    tls_servername api.internal
    tls_ca /etc/raddy/root-ca.pem
}
```

| 子指令 | 含义 |
|---|---|
| `tls_servername <host>` | 发给上游的 SNI / 主机名(默认:上游主机)。上游地址是 IP 但证书签给某个名字时必需。 |
| `tls_skip_verify` | 关闭上游证书*与*主机名校验(生产环境切勿使用)。 |
| `tls_ca <pem-file>` | 校验上游证书所用的根 CA;可重复。设置后仅信任列出的 CA,不再参考系统信任根。 |
| `tls_cert <cert-file> <key-file>` | 出示给上游的客户端证书(到后端的双向 TLS)。 |

默认情况下,上游证书按系统信任根校验,且其主机名必须匹配 `tls_servername`
(或上游主机)。校验失败表现为 **502 Bad Gateway**。

**示例。** 代理到提供内部证书的 HTTPS 后端:

```caddyfile
api.example.com {
    reverse_proxy {
        to https://10.0.0.1:8443
        tls_servername api.internal
        tls_ca /etc/raddy/root-ca.pem
    }
}
```

## WebSocket 与协议升级

**作用。** `reverse_proxy` 透明转发 HTTP `Upgrade` 请求(WebSocket 及类似的
`Connection: upgrade` 协议)。

**行为。** 客户端的升级请求发往上游;一旦上游应答 `101 Switching
Protocols`,raddy 便双向隧道该连接。无需任何指令——这是 `reverse_proxy`
对 HTTP/1.1 升级请求的默认行为。

- 升级是**端到端**的:raddy 不终止升级后的协议;后端必须能说该协议。
- `header_up` / `header_down` 仍作用于升级请求 / 响应头;`encode` 绝不作用于
  `101`(升级)响应。

**示例。**

```caddyfile
chat.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

同一站点同时服务 WebSocket 与普通 HTTP 流量;raddy 只为携带 `Upgrade` 头的
请求升级。

## 完整示例

```caddyfile
{
    acme_email ops@example.com
    log_level info
    trusted_proxies 127.0.0.1
    access_log /var/log/raddy/access.jsonl format=json
}

(base) {
    rate_limit remote_ip 50r/s burst=100
    header_up X-Raddy true
}

# HTTP → HTTPS 重定向
:80 {
    redir https://{host}{uri} permanent
}

api.example.com {
    import base

    handle /health {
        respond 200 ok
    }

    handle_path /api/* {
        reverse_proxy https://{$API_BACKEND}:8443
        tls_servername api.internal
    }

    handle /static/* {
        root /var/www/html
        file_server
        encode br zstd gzip
    }

    reverse_proxy 127.0.0.1:8080
    header_up X-Real-IP {remote_host}
}

admin.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    tls client_auth require /etc/certs/clients-ca.pem
    reverse_proxy 127.0.0.1:9000
}
```

> `reverse_proxy` 之后的 `header_up` 仍然生效 —— 它是修饰指令。`handle` 块
> 内的 `encode` 只作用于该块的 `file_server`。`rate_limit`(从 `base` 片段
> 导入)守卫服务该站点的任意终端。
