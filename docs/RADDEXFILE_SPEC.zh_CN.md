# Raddexfile 设计规范

> Raddexfile 是发布后很难再改的**公共接口**。本文档是唯一事实来源。
>
> **红线**：任何未在本文档明确的语法，实现前必须先补充到本文档，不得"顺手决定"。

状态图例：**可用** = 已实现；**推迟** = 已阻塞或推迟；**远期** = 未排期。

## 1. 设计哲学：显式书写顺序执行

站点块内指令**严格按书写顺序执行**。这是相对 Caddy（内部隐式指令排序表）的核心差异化。代价是用户需自行保证顺序合理（如鉴权指令写在业务路由之前），但这个代价是显式、可预期的，好于一张需要单独查文档的隐式排序表。

## 2. 匹配语义：终端指令 / 修饰指令 / `handle`

站点块在解析期编译为两部分，运行时不是逐行解释：

- **终端指令**（`reverse_proxy`、`file_server`、`redir`、`respond`、`error`）：决定**哪个指令服务**请求。可带内联 matcher（见 第 5.9 节），如 `reverse_proxy /api/* { to ... }`；**不匹配时跳过该指令（no-op），继续执行下一条**；不带 matcher 则始终执行。第一个命中的终端指令结束本站点执行。
- **修饰指令**（`header_up`、`header_down`、`encode`、`rewrite`）：**声明式变换**；`rate_limit`、`basic_auth`、`forward_auth` 为**声明式守卫**（见 第 5.2 / 5.10 节）。均不参与「谁服务」的决策。无论写在块内哪个位置（终端之前或之后），作用于该块内服务的那个终端指令；`handle` 块内的修饰只作用于该块的终端。
- **`handle /path { ... }`**：互斥匹配块。匹配时执行块内指令并**停止后续匹配**（互斥、命中即停）；不匹配则继续执行后续指令。`handle` 用于路径分组与"命中一个即停止"的场景。
- 不引入 Caddy 的 `route`（保序执行所有匹配块）——与默认顺序执行语义重叠，且是 Caddy 用户最大困惑源。

> 推论：修饰指令可写在终端指令之后（如 `reverse_proxy` 之后的 `header_up`），请求头改写仍生效（见第 7 节的示例）。这是声明式语义，不是位置相关的逐行解释。

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
| `reverse_proxy` | `reverse_proxy <target>` 或块形式 `to <upstream>...`；`to` 支持**多目标轮询**；块内可选 `lb_policy` / `health_check`（见 第 5.1 节）；`https://` 上游支持 `tls_servername` / `tls_skip_verify` / `tls_ca` / `tls_cert`（见 第 5.4 节） | 可用 |
| `handle` | 互斥匹配块（见 第 2 节） | 可用 |
| `header_up` / `header_down` | 请求头 / 响应头改写 | 可用 |
| `root` | 已限定路径的块内直接写路径，**不需要** Caddy 的 `root *` 冗余通配符 | 可用 |
| `file_server` | 静态文件托管 | 可用 |
| `encode` | 参数顺序 = 优先级：`encode zstd gzip` 表示客户端同时支持时优先 zstd；`br`（Brotli）也是合法算法 | 可用 |
| `redir` | `redir <target> [code]`，默认 `308`；`code` 为 3xx 数字，或关键字 `permanent`(=308) / `temporary`(=302)；占位符 `{host}`、`{uri}` | 可用 |
| `log_level` | 全局日志级别（`info` / `debug` / `warn` / `error`） | 可用 |
| `acme_email` | ACME 注册邮箱（Let's Encrypt 要求） | 可用 |
| `rate_limit` | `rate_limit <key> <rate> [burst=<n>]`（**单机**限流；见 第 5.2 节）；key 为 `remote_ip` 或 `header <name>` | 可用 |
| `trusted_proxies` | 受信网段列表（见 第 4 节） | 可用 |
| `dns_challenge` | `dns_challenge <provider> <value>`，或块形式 `dns_challenge <provider> { <字段> <值>... }` —— 经 DNS 服务商（目前 Cloudflare）做 DNS-01 签发（见 第 5.3 节） | 可用 |
| `tls_alpn_challenge` | 在 443 端口上以 TLS-ALPN-01 代替 HTTP-01 证明域名控制权（见 第 5.8 节） | 可用 |
| `tls` | 站点级 TLS 来源与选项：`tls [<cert> <key> \| internal]`、`min_version`、`max_version`、`ciphers`、`client_auth`（见 第 5.7 节） | 可用 |
| `rewrite` | `rewrite <to>` —— 转发前改写请求 URI；不带 matcher，始终生效（修饰指令；见 第 5.9 节） | 可用 |
| `handle_path` | 类似 `handle`，但从 URI 中去掉命中的路径前缀（见 第 5.9 节） | 可用 |
| `respond` | `respond <matcher> <status> [<body>]` —— 直接以状态/响应体应答；可带内联 matcher（终端指令；见 第 5.9 节） | 可用 |
| `error` | `error <matcher> [<status>] [<message>]` —— 触发内部错误响应；可带内联 matcher（终端指令；见 第 5.9 节） | 可用 |
| `basic_auth` | `basic_auth <user> <bcrypt-hash>` —— HTTP Basic 认证守卫（见 第 5.10 节） | 可用 |
| `forward_auth` | `forward_auth <target>` —— 将认证委托给上游（见 第 5.10 节） | 可用 |
| `import` / `(name)` | `import <file\|name>` 多文件包含 / 片段，`{$ENV}` 展开（见 第 5.12 节） | 可用 |
| `access_log` | `access_log <path> [format=<json\|common>]` 或 `off`（见 第 5.13 节） | 可用 |

**单机 vs 集群限流**：限流为**单机**（每实例独立计数）；集群级（跨实例共享计数）需外挂 Redis，属后续可选特性，不在本文档文法上预留参数。

**`file_server` 运行时语义**：`file_server` 从 `root` 目录提供**完整请求路径**（含 `handle` 前缀）对应的文件——`handle /static/* { root /var/www; file_server }` 将 `/static/foo` 映射到 `/var/www/static/foo`。支持目录 `index.html`；拒绝 `..` 目录穿越（404）；仅允许 GET/HEAD。**永不服务隐藏文件**：任何以 `.` 开头的路径段（`.env`、`.git/`、`.htaccess`）一律 404，唯一例外是 `.well-known` 目录（RFC 8615 的 well-known URI 是公开发现端点）。`encode` 对 `file_server` 响应同样生效；小于 64 字节的响应体不做压缩（编码框架会使其更大）。

### 5.1 `lb_policy` / `health_check`（`reverse_proxy` 块内子指令）

- 仅出现在 `reverse_proxy` 的**块形式**内；省略时保持默认轮询（`round_robin`），与 v0.1 行为一致。
- `lb_policy round_robin | random | ip_hash`：选择算法。`round_robin`（默认）轮询；`random` 随机选择；`ip_hash` 按客户端 IP 一致哈希（同 IP 会话粘性）。
- `health_check { ... }`：**主动健康检查**（TCP 连接探活）。全部子参数可选，省略用默认值：
  - `interval <dur>`：探活周期，默认 `5s`。
  - `timeout <dur>`：单次探活超时，默认 `2s`。
  - `consecutive_failures <n>`：连续失败 N 次才把上游摘除（flapping 抑制），默认 `3`。
  - `consecutive_successes <n>`：连续成功 M 次才恢复（flapping 抑制），默认 `2`。
  - `<dur>` 形式：数字 + 单位（`ms` / `s` / `m` / `h`），或裸数字表示秒。
- 运行时语义：被标记为不健康的上游不再被选择；恢复后自动回流。**所有上游均不健康时返回 502**。健康状态是进程级生命周期、跨 SIGHUP 重载存活；仅当上游地址、策略或健康检查参数变更时重建。

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
- `<key>` 决定按什么计数，支持两种：
  - `remote_ip` —— 按 第 4 节 信任模型解析的真实客户端 IP（v0.1.2 即支持的 key）。
  - `header <name>` —— 请求头 `<name>` 的值（如 `header X-API-Key`）。不带该头的请求共用一个桶。
- `<rate>`：`<count>r/<unit>`，单位为 `s`（秒）、`m`（分）、`h`（时）、`d`（天）——如 `50r/s`、`1200r/m`。count 必须 ≥ 1。
- `burst=<n>`：令牌桶容量，`n ≥ 1`；**默认 = rate 的 count 值**。可省略或显式指定。
- 语义：**单机、内存内令牌桶**，按 (站点, 终端, key 值) 分别计数。桶以 `<rate>` 持续补充令牌、容量上限为 `burst`；请求到达时若无令牌可用，返回 **429 Too Many Requests**。状态是进程级生命周期、跨 SIGHUP 重载存活。
- 它是**修饰指令**（守卫）：站点级 `rate_limit` 守卫该块内服务的任意终端；位于 `handle` 块内时只守卫该块的终端。未匹配到任何终端的请求（404）不受限流。作用域内存在多条 `rate_limit` 时各自独立计数。

```caddyfile
api.example.com {
    rate_limit remote_ip 100r/s burst=200
    reverse_proxy 127.0.0.1:8080
}
```

### 5.3 `dns_challenge`（经 DNS 服务商的 DNS-01）

默认情况下，raddex 在纯 HTTP 监听器上用 **HTTP-01** 证明域名控制权。当 80
端口不可达（网络屏蔽，或纯 DNS 部署）时，改用 `dns_challenge` 通过发布 DNS
TXT 记录证明控制权。

**语法**——两种写法，都位于**全局块**：

```caddyfile
dns_challenge <provider> <value>        # 简写：单个凭证
dns_challenge <provider> { ... }        # 块形式：任意数量凭证
```

简写只对「恰好需要一个凭证」的服务商可用，填入的就是那个凭证。块形式每行一条
`<字段> <值>`，适用于所有服务商。

- **语义**：配置后，本实例上所有证书签发走 **DNS-01**——raddex 在校验订单
  期间通过服务商 API 发布 `_acme-challenge.<host>` TXT 记录，完成后移除。未
  配置 `dns_challenge` 时行为不变（HTTP-01）。
- **校验**：服务商关键字及其凭证字段由 `raddex check` 检查。必填凭证缺失或为
  空、字段名未知、字段重复，都是配置错误。
- **安全**：所有凭证值都是机密。raddex 会在诊断输出中脱敏，但 Raddexfile 本身
  以明文保存它们——不要让它落入版本控制，或改用 `{$ENV}` 占位符注入
  （见 第 5.12 节）。

**服务商**

| 服务商 | 凭证 | 必填 | 说明 |
|---|---|---|---|
| `cloudflare` | `api_token` | 是 | 需要该 zone 的 **Zone: DNS: Edit** 权限。 |

新增服务商不会改动这套语法：服务商是 `src/server/dns/` 里的注册表条目，解析、
校验和错误信息全部由服务商自身声明的字段推导而来。参见 `CONTRIBUTING.md`。

```caddyfile
{
    acme_email ops@example.com
    dns_challenge cloudflare {$CLOUDFLARE_API_TOKEN}
}

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

块形式等价，也是多凭证服务商所用的写法：

```caddyfile
{
    acme_email ops@example.com
    dns_challenge cloudflare {
        api_token {$CLOUDFLARE_API_TOKEN}
    }
}
```

### 5.4 上游 TLS（`reverse_proxy` 到 HTTPS 后端）

**状态：可用。**

上游默认走明文 HTTP/1.1。`https://` 前缀表示到后端使用 TLS HTTP/1.1；
`h2://` 表示 TLS HTTP/2，`h2c://` 表示明文先验 HTTP/2：

```caddyfile
reverse_proxy https://127.0.0.1:8443
reverse_proxy h2://127.0.0.1:9443
reverse_proxy h2c://127.0.0.1:9080

reverse_proxy {
    to https://10.0.0.1:8443 https://10.0.0.2:8443
    tls_servername api.internal
    tls_ca /etc/raddex/root-ca.pem
}
```

- **语法**：上游目标可带 `https://`（或 `http://`）协议前缀；协议决定上游
  连接是否走 TLS。裸 `host:port` 仍是明文 HTTP（向后兼容）。
- TLS 选项只出现在 `reverse_proxy` 的**块形式**内，作为子指令：
  - `tls_servername <host>`：发往上游的 SNI/主机名（默认：上游主机）。上游
    地址是 IP 但证书签发给域名时必须使用。
  - `tls_skip_verify`：关闭上游证书*与*主机名校验（切勿用于生产）。
  - `tls_ca <pem-file>`：用于校验上游证书的根 CA，可重复。设置后校验**只
    信任列出的 CA**——不再参考系统信任根，如仍需系统根请一并写入该文件。
  - `tls_cert <cert-file> <key-file>`：向上游出示的客户端证书（到后端的
    双向 TLS）。
- **语义**：默认情况下，上游证书按系统信任根校验，且主机名必须匹配
  `tls_servername`（或上游主机）。校验失败表现为 **502 Bad Gateway**。

### 5.5 WebSocket 与协议升级

**状态：可用。**

`reverse_proxy` 透明转发 HTTP `Upgrade` 请求（WebSocket 及类似的
`Connection: upgrade` 协议）：客户端的升级请求原样发往上游，上游应答
`101 Switching Protocols` 后 raddex 双向隧道转发该连接。

- 无需任何指令——这是 `reverse_proxy` 对 HTTP/1.1 升级请求的默认行为。
- 升级是端到端的：raddex 不终结升级后的协议，后端必须自行实现该协议。
- `header_up` / `header_down` 对升级请求/响应头仍然生效；`encode` 绝不作用
  于 `101`（已升级）响应。

### 5.6 HTTP/2

**状态：可用。**

- **下游**：TLS 监听器（443 端口）通过 ALPN 通告 `h2`，对支持的客户端提供
  HTTP/2，其余回退到 HTTP/1.1。这是默认行为。
- **明文（h2c）**：下游纯 HTTP 监听器仍提供 HTTP/1.1；上游可用显式的
  `h2c://` 方案启用先验 HTTP/2。
- **上游**：`h2://host:port` 表示带 TLS、使用 ALPN `h2` 的 HTTP/2；
  `h2c://host:port` 表示明文先验 HTTP/2；`https://` 与裸地址仍保持
  原有 HTTP/1.1 行为。`h2c://` 不使用过时的 HTTP/1.1 Upgrade，
  上游必须直接接受 HTTP/2 connection preface。
- 所有 TLS 监听器通告的 ALPN 集合固定为（优先 `h2`，回退 `http/1.1`），
  不可按站点配置。

### 5.7 `tls` 指令（站点级 TLS 选项、手动证书、mTLS）

**状态：可用。**

具名站点默认自动从 ACME 获取证书。站点块内的 `tls` 指令定制该站点的 TLS；
带 `tls` 指令的具名站点，其端口以 **TLS** 提供（默认 443，指定显式端口时用
该端口）：

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

- **来源**（至多一个）：
  - *（省略）* —— ACME（默认；不变）。
  - `tls internal` —— 启动时为开发环境生成的自签名证书；客户端需自行配置
    信任。不尝试 ACME。
  - `tls <cert-file> <key-file>` —— 该站点改用手动静态 PEM 证书链 + 私钥，
    不再走 ACME。续期由运维负责。
- **选项**（全部可选；可自由组合；每个选项独占一行 `tls`）：
  - `tls min_version <1.2|1.3>` / `tls max_version <1.2|1.3>` —— 限制该站点
    协商的 TLS 协议版本。
  - `tls ciphers <list>` —— OpenSSL 密码套件列表（如
    `ECDHE-ECDSA-AES128-GCM-SHA256`）；空格分隔的名称以 `:` 连接。
  - `tls client_auth <optional|require> <ca-file>` —— 双向 TLS：按 `ca-file`
    校验客户端证书。`require` 拒绝无有效证书的客户端；`optional` 请求证书
    但接受无证书客户端。
- 选项按 **(host, port)** 键控，客户端连接到该站点时在握手期间应用；重载
  更新它们而无需重建监听器。通告的 ALPN 集合不可按站点区分——第 5.6 节的
  `h2`/`http/1.1` 默认适用于每个 TLS 监听器。
- `tls` 来源为静态或 internal 的站点，其主机名被排除在 ACME 签发之外（启动
  签发与按需签发均是）。

### 5.8 TLS-ALPN-01 挑战

**状态：可用。** `tls_alpn_challenge` 使用 OpenSSL 监听器的底层
ClientHello/ALPN 回调和临时 RFC 8737 challenge 证书。它与
`dns_challenge` 互斥，并要求 ACME 站点使用标准 TLS 端口 443。

否则，当 HTTP-01（80 端口）被屏蔽但 443 端口可达时，本可通过在 443 端口的
`acme-tls/1` ALPN 协议上应答 ACME 挑战来证明域名控制权：

- **语法**：`tls_alpn_challenge`，位于**全局块**。
- **语义**：设置后，证书签发使用 **TLS-ALPN-01**：ACME 服务器向 443 端口
  发起只携带 `acme-tls/1` 的 TLS 连接，raddex 返回带有
  `acmeIdentifier` 扩展的短期校验证书。不会回退到 HTTP-01。

### 5.9 Matchers、`rewrite`、`handle_path`、`respond`、`error`

**状态：可用。**

**Matchers** 泛化了仅路径的内联 matcher。一个 matcher 是一串 matcher 项；所有
项必须全部匹配（AND）。以 `/` 开头的裸值视为 `path` 的简写：

- `path <prefix>` —— 请求路径等于该前缀或位于其下（`/api` 匹配 `/api` 与
  `/api/...`，不匹配 `/apix`）。尾部 `*` 被剥掉（`/api/*` ≡ `/api`）；前缀
  `/` 匹配所有路径。
- `host <host>` —— 规范化 Host 头（去端口、去尾点、ASCII 小写）等于该值。
- `method <method>` —— 请求方法等于该值（如 `GET`）。
- `header <name> <value>` —— 请求头 `name` 等于 `value`（名称大小写不敏感；
  值精确匹配）。
- `query <key> <value>` —— 查询参数 `key` 的值等于 `value`。
- `remote_ip <cidr>...` —— 真实客户端 IP（按 第 4 节 信任模型）位于所列网段内。
- `protocol <http|https>` —— 接收请求的监听器的传输类型。
- 以 `!` 开头的项为取反（如 `!path /admin/*`）。

Matcher 附加到指令或 `handle` 块：`handle <matcher> { ... }`、
`reverse_proxy <matcher> { to ... }`。一个指令后可直接跟多个 matcher 项：
`handle path /a/* host example.com { ... }`。

随 matchers 引入的新指令：

- `rewrite <to>`：**修饰指令**，转发前改写请求 URI。它不带 matcher，**始终
  作用于**服务本块的终端。终端仍服务该请求，但上游看到的是改写后的路径
  （支持占位符 `{host}`、`{uri}`、`{remote_host}`）。条件改写应放入 `handle`
  块内。
- `handle_path <matcher> { ... }`：类似 `handle`，但块内终端运行前先把命中
  的路径前缀从 URI 中剥掉——所以 `handle_path /api/* { reverse_proxy }`
  转发 `/users/1`，而非 `/api/users/1`。
- `respond <matcher> <status> [<body>]`：**终端指令**，直接以给定状态与可选
  响应体应答（matcher 可选——省略即始终匹配）。
- `error <matcher> [<status>] [<message>]`：**终端指令**，以给定状态（默认
  **500**）与可选消息触发 raddex 的内部错误响应。

```caddyfile
api.example.com {
    handle_path /api/* {
        reverse_proxy 127.0.0.1:8080
    }
    handle path /status method GET {
        respond 200 ok
    }
    rewrite /app/{uri}
    reverse_proxy 127.0.0.1:9000
}
```

> Matcher 项以空格分隔并 AND 连接——没有括号或 `&&` 运算符（`handle path
> /status method GET`，而非 `handle (path /status && method GET)`）。
> `reverse_proxy`、`respond`、`error` 同样支持内联 matcher。

### 5.10 `basic_auth` / `forward_auth`

**状态：可用。**

- `basic_auth <user> <bcrypt-hash>`：要求 HTTP Basic 认证的**守卫**。多条
  `basic_auth` 指令构成用户表；请求须提供其中某个用户且密码与 bcrypt 哈希
  校验通过，否则返回 **401** 并带 `WWW-Authenticate: Basic`。用
  `htpasswd -B` 生成哈希。
- `forward_auth <target>`：把认证委托给上游 `target`（`host:port`）的
  **守卫**：raddex 转发请求（携带原始 `Authorization` 与 `X-Forwarded-For`），
  仅在 **2xx** 响应时放行；**403** 原样透传，其余返回 **401**。来自认证上游
  的响应头（如身份头）会在真实上游看到请求前复制到请求上。

两者与 `rate_limit` 一样是守卫：作用于本块服务的任意终端；位于 `handle` 块内
时只作用于该块的终端。

```caddyfile
api.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:8080
}
```

### 5.11 `encode` 算法

**状态：可用。**

`encode` 除 `gzip`、`zstd` 外还接受 `br`（Brotli）；参数顺序仍是服务端
偏好——`encode br zstd gzip` 优先 Brotli。算法仅在列出时才会被使用；再与
客户端的 `Accept-Encoding` 协商。

### 5.12 `import`、片段与环境变量

**状态：可用。**

- **`import <file>`**：在该位置拼入另一个 Raddexfile 的内容。路径相对于导入方
  文件。import 只能嵌套到有界深度；import 环（按规范化路径检测）与超过大小
  限制的导入文件都是**错误**——绝不会静默截断。站点块可以导入属于该站点的
  指令文件。
- **片段（snippets）**：顶层命名为 `(name) { ... }` 的块定义可复用片段；
  `import name` 在该位置拼入该片段。片段仅对定义它的文件可见。
- **环境变量**：指令参数中的 `{$ENV_VAR}` 在解析期被替换为 `ENV_VAR` 的值；
  变量缺失是校验错误。任何出现参数的地方都适用（上游目标、`root`、`tls`
  路径等）。展开是 **token 级**的：值成为单个参数，因此含空格、`#`、花括号
  或换行的值无法改变配置结构。

```caddyfile
(base) {
    rate_limit remote_ip 100r/s
    header_up X-Raddex true
}

{
    acme_email ops@example.com
}

api.example.com {
    import base
    reverse_proxy https://{$BACKEND_HOST}:8443
}
```

### 5.13 访问日志配置

**状态：可用。**

`--access-log` CLI 参数追加 JSON 访问日志。Raddexfile 可以更精细地配置访问
日志：

- 全局块：`access_log <path> [format=<json|common>]` 设置日志文件与格式
  （默认 `json`）；`access_log off` 对整个实例关闭。两者同时设置时
  `--access-log` 参数仍然优先。
- 站点块：`access_log off` 仅关闭该站点的访问日志——所有终端类型
  （`reverse_proxy`、`file_server`、`redir`、`respond`、`error`）都被排除。
- `common` 格式为经典 combined 日志行（`%h %l %u %t "%r" %>s
  %b "%{Referer}i" "%{User-Agent}i"`）。

## 6. 站点选择、端口、catch-all 与多站点

- **站点选择按监听器收敛**：请求到达某监听器后，仅在该监听器的候选站点集合内匹配——TLS 监听器按 SNI、纯 HTTP 监听器按规范化 Host（去端口、去尾点、ASCII 小写）。候选集合 = 地址落在该端口的具名站点 + 该端口的 `:port` 兜底块。
- **具名站点默认端口 443**：`api.example.com`（不带端口）默认绑 443（TLS）。自动 HTTPS 生效：具名站点通过 ACME 签发证书——默认 HTTP-01，配置 `dns_challenge` 后走 DNS-01（见 第 5.3 节），配置 `tls_alpn_challenge` 后走 TLS-ALPN-01（见 第 5.8 节）。`tls` 来源为静态或 internal 证书的站点（见 第 5.7 节）被排除在 ACME 之外。SNI 按域名返回对应证书；端口 443 监听器使用 SNI 动态证书（`raddex_certs/` 目录缓存，重启复用）。证书在到期前 30 天内自动续期。
- **隐式 HTTP-01 监听器（:80）**：HTTP-01 挑战在明文 HTTP 监听器上应答，因此当配置含具名站点但没有任何站点绑定端口 80 时，raddex 会隐式绑定一个仅服务 ACME 挑战的明文 `:80` 监听器（其余请求返回 404）。没有它，ACME 服务器永远无法触达挑战，签发会一直挂起。配置 `dns_challenge`（DNS-01）时跳过该隐式监听器——选择 DNS 部署正是因为端口 80 不可用。显式配置 `:80` catch-all 已能应答挑战，因此不会重复绑定。
- **具名站点显式端口**：`api.example.com:8081 { ... }` 将具名站点绑定到非标端口（用于本地多端口部署与测试）；省略端口时默认 443。IPv6 字面量地址（`[::1]:8080`）已支持，Host 头也必须使用方括号形式。带 `tls` 指令的具名站点（见 第 5.7 节）即使端口不是 443 也以 TLS 提供。
- **选不中兜底**：Host 缺失或畸形 → `400 Bad Request`；Host 合法但不匹配任何站点、且无兜底块 → `404 Not Found`。不提供可配置错误页。
- **非标端口**：`:8443`。
- **Catch-all**：`:80` 捕获该监听器上所有未匹配到具名站点的请求，常用于 HTTP→HTTPS 跳转（自动 HTTPS UX 的组成部分）。
- **多域名共享站点块**（`a.example.com, b.example.com { ... }`）：已可用。块体会复制为每个主机一个独立站点，重复的主机/端口组合会被拒绝。
- **通配符站点名**（`*.example.com`）：只匹配一个最左标签，不匹配顶级域名本身或多级前缀。精确名称优先于通配符，更具体的通配符后缀优先。

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

> 示例保持默认的 HTTP-01。端口 80 不可用时，可在全局块启用
> `tls_alpn_challenge`。

## 8. 四层监听器（TCP、SNI 透传与 UDP）

**状态：可用（TCP/SNI/UDP/TLS 终止/透明 TCP），包含在
`v0.3.6`。**

`tcp` 块是**顶级监听器**，与 HTTP 站点块平级。它将裸 TCP 连接（不做 HTTP
解析）转发到一个或多个上游。默认透传 TLS；配置可选的 `tls` 指令后会在中继前终止 TLS：

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
        consecutive_failures 3
        consecutive_successes 2
    }
}

tcp :8443 {
    tls internal
    to 127.0.0.1:9443
}
```

- **监听地址**：IP 字面量，或 `:port` 表示所有接口；IPv6 需加方括号
  （`tcp [::1]:8080`、`udp [::1]:53`）。TCP 与 UDP 可共享同一地址端口，但两个 TCP 监听器
  若绑定重叠（通配符与任意具体绑定重叠）会被拒绝，与 HTTP 站点端口冲突的
  裸 TCP 监听器同样被拒绝。
- **`to <host>:<port>...`**：至少一个上游。主机名在启动时解析，并按周期
  重新解析（默认 60s；新的地址集合仅对*新*连接生效）；瞬时刷新失败会保留
  最后可用地址（`raddex_l4_tcp_dns_refresh_failures_total` 计数）。启动时无法
  解析的上游是错误。
- **SNI 路由**（`sni <name> <host:port>` + 可选 `fallback <host:port>`，L4 P1）：
  含 `sni` 行的 `tcp` 监听器按 ClientHello 的精确或单标签通配符 SNI 路由
  TLS 连接——不终止 TLS（在有界前缀内检查 ClientHello 并原样转发）。精确名称优先，
  通配符只匹配一个最左标签；未知/缺失/畸形/超大的 SNI 在设置 `fallback` 时走
  fallback，否则关闭连接。`sni` 与 `to` 互斥，`health_check` 不适用于
  SNI 模式。
- **L4 TLS 终止**：在 `tcp` 块内加入 `tls internal` 或
  `tls <cert-file> <key-file>`。Raddex 原生基于 OpenSSL 完成 TLS 握手，将解密后的裸字节流送入原生中继；
  该模式使用共享的 `to` 上游，不能与 SNI 透传或 `transparent` 同时使用。
- **透明 TCP**：在 `tcp` 块内加入 `transparent` 并保留 `to` 兜底。
  Linux 下监听 socket 设置 `IP_TRANSPARENT`，通过 `SO_ORIGINAL_DST` 读取原始目的地址，
  并用原始客户端地址建立出站连接。需要 `CAP_NET_ADMIN`、TPROXY 规则和策略路由，
  Windows 不支持。由于监听器由自定义服务持有，透明 TCP 配置必须使用普通重启，
  不能使用 `raddex upgrade`。
- **`lb_policy`** 复用 HTTP 策略：`round_robin`（默认）、`random`、`ip_hash`
  （源 IP 粘滞——同一客户端固定到同一上游）。
- **`connect_timeout`** 限制单次上游连接的时长（默认 `5s`）；**`idle_timeout`**
  是*真正的*空闲超时，任一方有流量即重置（默认 `5m`，长存活活跃连接不会
  超时）；**`max_connections`** 限制并发连接数（默认 `10000`，被拒绝的连接
  计入指标）。
- **`health_check { ... }`** 运行主动 TCP 连接探活，默认值与 HTTP 相同
  （`5s` 间隔、`2s` 超时、连续 `3` 次失败 / `2` 次成功）。不健康的上游会被
  跳过；全部不健康时连接被拒绝。
- 每条关闭的连接输出一条类型化访问日志行（JSON，与 HTTP 访问日志相互独立）
  及 Prometheus 指标（`raddex_l4_tcp_*`，按监听器打标签）。
- **UDP 代理**（`udp <address> { to ... lb_policy idle_timeout max_flows
  max_datagram_size recv_buffer send_buffer }`，L4 P2）：代理数据报。每个客户端
  （地址 + 端口）映射为一个 **flow**，各自持有与所选上游相连的 socket（本地
  临时端口负责响应多路复用）。选择每个 flow 只发生一次——`ip_hash` 按客户端
  *IP* 钉住，flow 身份仍含端口。上限：`max_flows` 限制表大小（最旧优先驱逐）、
  `idle_timeout` 驱逐空闲 flow、`max_datagram_size` 丢弃并计数超大报文、
  `recv_buffer`/`send_buffer` 设置 socket 缓冲（0 = 系统默认）。IPv4 与 IPv6
  上游均支持。UDP 与 TCP 可共享同一地址端口。指标：`raddex_l4_udp_*`。Linux
  下 UDP 支持零停机升级：
  raddex 通过私有 handoff manifest 交接监听 fd、每个已连接上游 flow fd 以及有界的 flow 元数据，
  因而内核接收队列不会因重新 bind 而丢失。交接失败时升级不会报告成功。
- **QUIC 透传**：UDP 代理可以把 QUIC 包当作普通数据报转发，但 Pingora 0.8.1 没有原生
  QUIC/HTTP/3 协议栈。这不提供 QUIC 终止、HTTP/3 请求路由或 QUIC 连接迁移；这些功能
  需要独立的 QUIC/HTTP/3 sidecar。
- **重载语义**：SIGHUP 重载会把新的上游集合、策略、限制与超时应用到*新*连接；
  已有连接保持其选定的上游。修改监听器的绑定地址属于**拓扑变更**，会被拒绝
  并提示使用普通重启。零停机升级要求新旧进程的监听器拓扑一致。

## 9. 待办

- 任何本规范未覆盖的语法细节，实现时**先补文档再动手**。
- Cloudflare 之外的 DNS-01 服务商**推迟**——每个服务商开一个 GitHub issue（欢迎社区贡献）。
- QUIC/HTTP-3 终止仍是独立 sidecar 边界，因为 Pingora 0.8.1 没有 QUIC transport。
