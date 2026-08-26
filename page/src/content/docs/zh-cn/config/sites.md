---
title: 站点 · 端口 · HTTPS
description: 请求如何匹配站点、端口与 catch-all 如何工作、自动 HTTPS 如何签发证书。
---

本页说明 raddy 如何决定*哪个*站点服务请求、端口与 catch-all 如何工作,以及
自动 HTTPS 如何为你签发证书。

## 全局块

文件顶部的裸 `{ ... }` 是**全局块**,承载站点级设置:

```caddyfile
{
    acme_email ops@example.com
    log_level info
}
```

开头的 `{` 令牌(不是主机名)即全局块。签发证书前,Let's Encrypt 要求
`acme_email`。

## 站点与端口

每个站点块命名一个主机,可选带端口:

```caddyfile
api.example.com { ... }          # 默认端口:443(TLS)
api.example.com:8081 { ... }     # 显式端口,纯 HTTP
:80 { ... }                      # 端口 80 上每个请求的 catch-all
```

- **不带端口的具名站点**绑定 **443** 并启用自动 HTTPS。
- **显式端口**(`:8081`)以纯 HTTP 绑定该端口 —— 适合本地多端口部署与测试。
- **catch-all**(`:80`、`:8443`,… )服务该监听器上不匹配任何具名站点的每个
  请求。
- 带 [`tls` 指令](../directives/#tls) 的具名站点,即使端口不是 443,也会把该
  端口作为 **TLS 监听器** —— `intranet.example.com:8443 { tls internal }` 在
  8443 上提供 TLS。

## 请求如何匹配站点

选择**按监听器进行**:请求只与它到达的端口上的站点匹配。在纯 HTTP 监听器上,
raddy 比较规范化后的 `Host` 头 —— 去端口、去末尾点、转小写。在 TLS 监听器
(443)上,用 **SNI** 名称匹配。

每种情况的处理:

| 情况 | 结果 |
|---|---|
| `Host` 匹配具名站点 | 该站点服务 |
| `Host` 缺失或畸形 | `400 Bad Request` |
| `Host` 合法但不匹配任何站点 | `404 Not Found` |
| `Host` 不匹配且该端口存在 catch-all | catch-all 服务 |

> 因为按监听器匹配,不同端口的站点互不干扰,`:80` 的 catch-all 也不会截获
> 443 上的 HTTPS 流量。

## 自动 HTTPS

443 端口上的具名站点自动获得证书:

1. **签发** —— raddy 向 ACME 目录(默认 Let's Encrypt)注册,并证明域名
   控制权——默认在纯 HTTP 监听器上应答 **HTTP-01** 挑战
   (`/.well-known/acme-challenge/…`);当 80 端口不可达时,也可通过
   [全局块](#全局块)中的 `dns_challenge` 走 **DNS-01**(见
   [指令参考](../directives/#dns_challenge))。由于 HTTP-01 在纯 HTTP 监听器
   上应答,配置含具名站点但没有任何站点绑定端口 80 时,raddy 会自动绑定一个
   仅服务 ACME 挑战的隐式 `:80` 监听器;`dns_challenge` 则跳过它。
2. **SNI** —— 每个 HTTPS 请求按请求的主机名选择证书,因此多站点服务器按
   站点下发正确证书。
3. **缓存** —— 证书与账户凭据存放在 `raddy_certs/`(可用 `--cert-dir`
   配置),重启复用,无需重新签发。
4. **续期** —— 证书在到期前 30 天内自动续期;续期失败时沿用现有证书继续
   服务。

TLS 监听器通过 ALPN 宣告 HTTP/2(`h2`),为支持的客户端提供 HTTP/2,否则回退
HTTP/1.1。来源为静态或 internal 的 `tls` 站点(见[`tls` 指令](../directives/#tls))
不参与该主机名的 ACME 签发。

在[全局块](#全局块)设置联系邮箱:

```caddyfile
{
    acme_email ops@example.com
}

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

常见搭配是在 80 端口放一个 HTTP→HTTPS 重定向,把纯 HTTP 访客引向安全站点:

```caddyfile
:80 {
    redir https://{host}{uri} permanent
}
```

## 一台服务器,多个站点

每个站点一个块 —— 它们共享进程、连接池与证书:

```caddyfile
api.example.com {
    reverse_proxy 127.0.0.1:8080
}

static.example.com {
    root /var/www/html
    file_server
}
```

> 多域名共享站点块(`a.com, b.com { ... }`)暂不支持 —— 请为主机各写一个块。

## 暂不支持

- 站点名或上游中的 IPv6 字面量地址(`[::1]:8080`)。
- 多域名共享站点块。
- 站点选择回退(缺失或未匹配的 Host → 400 / 404)的可配置响应——这两者是固定
  文案。在站点内部,[`respond`](../directives/#respond) 与
  [`error`](../directives/#error) 终端让你完全掌控自定义响应。
