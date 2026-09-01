---
title: HTTP → HTTPS 重定向
description: 用一个 catch-all 站点把每个纯 HTTP 访客强制转到 HTTPS。
---

## 目标

输入 `http://example.com` 的访客应落到 `https://example.com` —— 同样的主机、
同样的路径。你的安全站点在 443(见[站点 · 端口 · HTTPS](../../config/sites/)),
你希望纯 HTTP 监听器负责重定向。

## 配置

`:80` 的 catch-all 站点重定向到达端口 80 的每个请求。因为 catch-all 匹配该
端口上未被具名站点认领的任何请求,这就是完整的 HTTP→HTTPS 方案:

```caddyfile
{
    acme_email ops@example.com
}

# HTTP → HTTPS 重定向
:80 {
    redir https://{host}{uri} permanent
}

example.com {
    reverse_proxy 127.0.0.1:8080
}
```

- `{host}` 与 `{uri}` 占位符保留主机名与完整路径(含查询字符串)。
- `permanent` 发送 **308 Permanent Redirect**,客户端与搜索引擎会记住新地址。
- 不想让重定向被缓存时,可用 `temporary`(302)。

## 运行

```bash
raddex check -c Raddexfile
raddex run -c Raddexfile
```

## 你能得到什么

```bash
curl -sI http://localhost/
```

```http
HTTP/1.1 308 Permanent Redirect
location: https://localhost/
```

请求路径会被透传:

```
http://example.com/posts/1?ref=home  →  308  →  https://example.com/posts/1?ref=home
```

## 不只 80 端口

同样的 catch-all 模式可用于任何纯 HTTP 端口,例如把旧端口重定向到当前端口:

```caddyfile
:8080 {
    redir https://{host}{uri} permanent
}
```
