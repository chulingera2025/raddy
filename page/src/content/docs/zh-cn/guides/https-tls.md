---
title: HTTPS 与 TLS
description: 自动 HTTPS、tls 指令、上游 TLS、双向 TLS 与 HTTP/2。
---

本指南涵盖 raddy 中所有与 TLS 相关的内容:基于 ACME 的自动 HTTPS、按站点
掌控的 `tls` 指令(自签或静态证书、协议版本、密码套件、双向 TLS)、到上游
后端的 TLS,以及下游 HTTP/2。

## 一行自动 HTTPS

不带端口的具名站点绑定 **443** 并自动获得 ACME 证书:

```caddyfile
example.com {
    reverse_proxy 127.0.0.1:8080
}
```

raddy 向 ACME 目录注册,证明域名控制权——默认在其纯 HTTP 监听器上用
**HTTP-01**,当 80 端口不可达时经 [`dns_challenge`](../../config/directives/#dns_challenge)
用 **DNS-01**,或者设置 `tls_alpn_challenge` 使用 TLS-ALPN-01——并在到期前
30 天内自动续期。在[全局块](../../config/sites/#the-global-block)
设置联系邮箱;如需把 80 端口访客引向安全站点,再补一个 HTTP→HTTPS 重定向。
完整的匹配模型见[站点 · 端口 · HTTPS](../../config/sites/)。

## `tls` 指令

站点块内的 [`tls` 指令](../../config/directives/#tls) 定制该站点的 TLS。它有三
种证书**来源**:

| 来源 | 适用场景 |
|---|---|
| *(省略)* | ACME,默认 |
| `tls internal` | 启动时生成的自签证书——仅开发;客户端须信任它 |
| `tls <cert-file> <key-file>` | 静态 PEM 证书链与私钥;续期由你负责 |

```caddyfile
dev.example.com {
    tls internal
    reverse_proxy 127.0.0.1:8080
}

intranet.example.com {
    tls /etc/certs/intranet.pem /etc/certs/intranet.key
    reverse_proxy 127.0.0.1:9000
}
```

带 `tls` 指令的具名站点会将其端口绑定为 **TLS 监听器**,因此静态或自签站点
可以在非 443 端口提供 TLS:

```caddyfile
dev.local:8443 {
    tls internal
    reverse_proxy 127.0.0.1:8080
}
```

```bash
curl -k -H 'Host: dev.local' https://127.0.0.1:8443/
```

### 协议版本与密码套件

按站点限制协商的 TLS 版本与密码套件。每个选项单独一行 `tls`:

```caddyfile
secure.example.com {
    tls min_version 1.2
    tls max_version 1.3
    tls ciphers ECDHE-ECDSA-AES128-GCM-SHA256
    reverse_proxy 127.0.0.1:9000
}
```

`min_version` / `max_version` 接受 `1.2` 或 `1.3`。`ciphers` 接受 OpenSSL 密码
套件列表;空格分隔的名称以 `:` 连接。

### 双向 TLS(客户端证书)

要求——或可选请求——由你信任的 CA 签发的客户端证书:

```caddyfile
secure.example.com {
    tls client_auth require /etc/certs/clients-ca.pem
    reverse_proxy 127.0.0.1:9000
}
```

- `client_auth require <ca-file>` —— 拒绝无有效证书的客户端。
- `client_auth optional <ca-file>` —— 请求证书,但接受没有证书的客户端。

同一个 CA 文件可复用于多个站点。HTTP 层的认证守卫见[认证指南](../auth/)。

## 到后端的 TLS(上游 TLS)

上游默认是纯 HTTP。以 `https://` 前缀启用到后端的 TLS:

```caddyfile
api.example.com {
    reverse_proxy https://127.0.0.1:8443
}
```

对需要特定 SNI 名称、私有 CA 或客户端证书的后端,使用块形式配合[上游 TLS
选项](../../config/directives/#upstream-tls-options):

```caddyfile
api.example.com {
    reverse_proxy {
        to https://10.0.0.1:8443 https://10.0.0.2:8443
        tls_servername api.internal
        tls_ca /etc/raddy/root-ca.pem
        tls_cert /etc/raddy/client.pem /etc/raddy/client.key
    }
}
```

- `tls_servername` —— 发给上游的 SNI(默认:上游主机)。地址是 IP 但证书签给
  某个名字时必需。
- `tls_ca` —— 校验上游证书所用的额外根 CA;系统根始终额外信任。
- `tls_cert <cert-file> <key-file>` —— 用于上游 mTLS 的客户端证书。
- `tls_skip_verify` —— 关闭校验;生产环境切勿使用。

上游证书校验失败表现为 `502 Bad Gateway`,因此 `tls_servername` 不匹配或
`tls_ca` 缺失是响亮报错,而非静默吞掉。

## 下游 HTTP/2

TLS 监听器通过 ALPN 宣告 HTTP/2(`h2`),为支持的客户端提供 HTTP/2,否则回退
HTTP/1.1——无需任何配置。纯 HTTP 监听器保持 HTTP/1.1。上游 HTTP/2 需显式
指定：`h2://host:port` 表示 TLS HTTP/2，`h2c://host:port` 表示明文先验
HTTP/2。

## TLS-ALPN-01

80 端口不可用时，在全局块启用：

```caddyfile
{
    tls_alpn_challenge
}
```

挑战在 TCP 443 上提供带 RFC 8737 `acmeIdentifier` 扩展和 `acme-tls/1`
ALPN 的临时证书。它与 DNS-01 互斥，并且只适用于 443 端口的 ACME 站点。

## 基于 TLS 的 WebSocket

WebSocket 升级在 HTTP 与 HTTPS 监听器上均可工作——`reverse_proxy` 透明转发
`Upgrade` 请求。见 [WebSocket 与协议升级](../../config/directives/#websocket-and-protocol-upgrades)
与 [API 代理指南](../api-proxy/) 中的示例。
