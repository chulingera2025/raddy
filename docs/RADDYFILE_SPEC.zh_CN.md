# Raddyfile 设计规范

> Raddyfile 是发布后很难再改的**公共接口**。本文档是唯一事实来源。
>
> **红线**：任何未在本文档明确的语法，实现前必须先补充到本文档，不得"顺手决定"。

状态图例：**可用** = 已实现；**计划中** = 已排期；**远期** = 未排期。

## 1. 设计哲学：显式书写顺序执行

站点块内指令**严格按书写顺序执行**。这是相对 Caddy（内部隐式指令排序表）的核心差异化。代价是用户需自行保证顺序合理（如鉴权指令写在业务路由之前），但这个代价是显式、可预期的，好于一张需要单独查文档的隐式排序表。

## 2. 匹配语义：终端指令 / 修饰指令 / `handle`

站点块在解析期编译为两部分，运行时不是逐行解释：

- **终端指令**（`reverse_proxy`、`file_server`、`redir`）：决定**哪个指令服务**请求。可带内联 matcher，如 `reverse_proxy /api/* { to ... }`；**不匹配时跳过该指令（no-op），继续执行下一条**；不带 matcher 则始终执行。命中即停——一旦命中，本站点执行结束。
- **修饰指令**（`header_up`、`header_down`、`encode`）：**声明式变换**；`rate_limit` 为**声明式守卫**（见 §5.2）。均不参与「谁服务」的决策。无论写在块内哪个位置（终端之前或之后），作用于该块内服务的那个终端指令；`handle` 块内的修饰只作用于该块的终端。
- **`handle /path { ... }`**：互斥匹配块。匹配时执行块内指令并**停止后续匹配**（互斥、命中即停）；不匹配则继续执行后续指令。`handle` 用于路径分组与"命中一个即停止"的场景。
- 不引入 Caddy 的 `route`（保序执行所有匹配块）——与默认顺序执行语义重叠，且是 Caddy 用户最大困惑源。

> 推论：修饰指令可写在终端指令之后（如 `reverse_proxy` 之后的 `header_up`），请求头改写仍生效（见 §7 示例）。这是声明式语义，不是位置相关的逐行解释。

```caddyfile
handle /admin/* {
    # 鉴权拦截；命中后不再匹配后面的块
}

handle /static/* {
    root /var/www/html
    file_server
}
```

## 3. 全局配置块

文件开头的裸 `{ ... }` 为全局块，承载全局项（ACME 邮箱、日志级别等）：

```caddyfile
{
    acme_email ops@example.com
    log_level info
}

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

- **判定规则**：文件首 token 为裸 `{`（非域名）即视为全局块。
- `admin 127.0.0.1:2019 { ... }` 子块为占位语法（依赖未来的 admin API），本阶段仅预留文法、不实现。

## 4. 可信代理与真实客户端 IP

限流、访问日志、WAF 全部依赖客户端 IP 是否可信，默认行为必须在规范中定死：

- 默认**不信任**任何上游 `X-Forwarded-For`，直接使用 TCP 连接的对端地址。
- 显式配置 `trusted_proxies` 网段后，才从最右侧非受信地址开始解析 `X-Forwarded-For` 链。
- 全局块与站点块均可覆盖，站点块优先。

**语法**（全局块或站点块）：

```caddyfile
{
    trusted_proxies 10.0.0.0/8 172.16.0.0/12 127.0.0.1
}
```

- `trusted_proxies <cidr>...`；每个 `<cidr>` 为 `<地址>/<前缀>` 或裸地址（单主机）。IPv4 与 IPv6 均接受。同作用域内后出现的覆盖先前的。
- 站点块 `trusted_proxies` **仅对本站点**覆盖全局列表；其他站点保留全局列表。
- **语义**：真实客户端 IP 为 TCP 对端，除非对端是受信代理；此时取 `X-Forwarded-For` 链中**最右侧非受信**的条目（畸形条目跳过）。整条链均受信或缺失时，使用受信对端本身。

## 5. 指令集与参数语义

| 指令 | 语法要点 | 状态 |
|---|---|---|
| `reverse_proxy` | `reverse_proxy <target>` 或块形式 `to <upstream>...`；`to` 支持**多目标轮询**；块内可选 `lb_policy` / `health_check`（见 §5.1） | 可用 |
| `handle` | 互斥匹配块（见 §2） | 可用 |
| `header_up` / `header_down` | 请求头 / 响应头改写 | 可用 |
| `root` | 已限定路径的块内直接写路径，**不需要** Caddy 的 `root *` 冗余通配符 | 可用 |
| `file_server` | 静态文件托管 | 可用 |
| `encode` | 参数顺序 = 优先级：`encode zstd gzip` 表示客户端同时支持时优先 zstd | 可用 |
| `redir` | `redir <target> [code]`，默认 `308`；`code` 为 3xx 数字，或关键字 `permanent`(=308) / `temporary`(=302)；占位符 `{host}`、`{uri}` | 可用 |
| `log_level` | 全局日志级别（`info` / `debug` / `warn` / `error`） | 可用 |
| `acme_email` | ACME 注册邮箱（Let's Encrypt 要求） | 可用 |
| `rate_limit` | `rate_limit remote_ip <rate> [burst=<n>]`（**单机**限流；见 §5.2） | 可用 |
| `jwt` | `jwt { issuer <url> audience <name> }` | 计划中 |
| `trusted_proxies` | 受信网段列表（见 §4） | 可用 |
| `snippet` / `import` | 复用片段 `(name) { ... }` / 多文件拆分 | 远期 |

**单机 vs 集群限流**：限流为**单机**（每实例独立计数）；集群级（跨实例共享计数）需外挂 Redis，属后续可选特性，不在本文档文法上预留参数。

**`file_server` 运行时语义**：`file_server` 从 `root` 目录提供**完整请求路径**（含 `handle` 前缀）对应的文件——`handle /static/* { root /var/www; file_server }` 将 `/static/foo` 映射到 `/var/www/static/foo`。支持目录 `index.html`；拒绝 `..` 目录穿越（404）；仅允许 GET/HEAD。`encode` 对 `file_server` 响应同样生效。

### 5.1 `lb_policy` / `health_check`（`reverse_proxy` 块内子指令）

- 仅出现在 `reverse_proxy` 的**块形式**内；省略时保持默认轮询（`round_robin`），与 v0.1 行为一致。
- `lb_policy round_robin | random | ip_hash`：选择算法。`round_robin`（默认）轮询；`random` 随机选择；`ip_hash` 按客户端 IP 一致哈希（同 IP 会话粘性）。
- `health_check { ... }`：**主动健康检查**（TCP 连接探活）。全部子参数可选，省略用默认值：
  - `interval <dur>`：探活周期，默认 `5s`。
  - `timeout <dur>`：单次探活超时，默认 `2s`。
  - `consecutive_failures <n>`：连续失败 N 次才把上游摘除（flapping 抑制），默认 `3`。
  - `consecutive_successes <n>`：连续成功 M 次才恢复（flapping 抑制），默认 `2`。
  - `<dur>` 形式：数字 + 单位（`ms` / `s` / `m` / `h`），或裸数字表示秒。
- 运行时语义：被标记为不健康的上游不再被选择；恢复后自动回流。**所有上游均不健康时返回 502**。健康状态是进程级生命周期、跨 SIGHUP 重载存活（ADR-011）；仅当上游地址、策略或健康检查参数变更时重建。

```caddyfile
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
```

### 5.2 `rate_limit`（声明式守卫）

- 语法：`rate_limit <key> <rate> [burst=<n>]`。
- `<key>`：`remote_ip`——按 §4 信任模型解析的真实客户端 IP。v0.1.2 仅支持这一种 key。
- `<rate>`：`<count>r/<unit>`，单位为 `s`（秒）、`m`（分）、`h`（时）、`d`（天）——如 `50r/s`、`1200r/m`。count 必须 ≥ 1。
- `burst=<n>`：令牌桶容量，`n ≥ 1`；**默认 = rate 的 count 值**。可省略或显式指定。
- 语义：**单机、内存内令牌桶**，按 (站点, 终端, 客户端 IP) 分别计数。桶以 `<rate>` 持续补充令牌、容量上限为 `burst`；请求到达时若无令牌可用，返回 **429 Too Many Requests**。状态是进程级生命周期、跨 SIGHUP 重载存活（ADR-011）。
- 它是**修饰指令**（守卫）：站点级 `rate_limit` 守卫该块内服务的任意终端；位于 `handle` 块内时只守卫该块的终端。未匹配到任何终端的请求（404）不受限流。作用域内存在多条 `rate_limit` 时各自独立计数。

```caddyfile
api.example.com {
    rate_limit remote_ip 100r/s burst=200
    reverse_proxy 127.0.0.1:8080
}
```

## 6. 站点选择、端口、catch-all 与多站点

- **站点选择按监听器收敛**：请求到达某监听器后，仅在该监听器的候选站点集合内匹配——TLS 监听器按 SNI、纯 HTTP 监听器按规范化 Host（去端口、去尾点、ASCII 小写）。候选集合 = 地址落在该端口的具名站点 + 该端口的 `:port` 兜底块。
- **具名站点默认端口 443**：`api.example.com`（不带端口）默认绑 443（TLS）。自动 HTTPS 生效：具名站点通过 ACME（HTTP-01）自动签发证书，SNI 按域名返回对应证书；端口 443 监听器使用 SNI 动态证书（`raddy_certs/` 目录缓存，重启复用）。证书续期暂缓；磁盘缓存跨重启复用。
- **具名站点显式端口**：`api.example.com:8081 { ... }` 将具名站点绑定到非标端口（用于本地多端口部署与测试）；省略端口时默认 443。IPv6 字面量地址（`[::1]:8080`）暂不支持，待真实用例补充。
- **选不中兜底**：Host 缺失或畸形 → `400 Bad Request`；Host 合法但不匹配任何站点、且无兜底块 → `404 Not Found`。不提供可配置错误页。
- **非标端口**：`:8443`。
- **Catch-all**：`:80` 捕获该监听器上所有未匹配到具名站点的请求，常用于 HTTP→HTTPS 跳转（自动 HTTPS UX 的组成部分）。
- **多域名共享站点块**（`a.example.com, b.example.com { ... }`）：推迟到首个真实用例。

## 7. 示例（完整配置）

```caddyfile
{
    acme_email ops@example.com
    log_level info
    trusted_proxies 127.0.0.1
}

# HTTP → HTTPS 跳转
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

> 注：`header_up` 写在 `reverse_proxy` 之后仍对请求头生效——它是修饰指令，作用于本块服务的终端（此处为 `reverse_proxy`）。`encode zstd gzip` 位于 `handle` 块内，只作用于该块的 `file_server`。`rate_limit` 是声明式守卫（修饰指令）：作用于本块服务的任意终端。

> 其余规划能力（`jwt` 等）不在示例中出现，避免读者照抄无法解析的配置。

## 8. 待办

- `jwt` 子指令文法在实现前必须在本文档定稿（`lb_policy` / `health_check` 已于 v0.1.1 定稿，见 §5.1；`rate_limit` / `trusted_proxies` 已于 v0.1.2 定稿，见 §4 / §5.2）。
- 任何本规范未覆盖的语法细节，实现时**先补文档再动手**。
