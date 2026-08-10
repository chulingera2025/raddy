---
title: 压缩
description: 用 gzip、zstd 与 Brotli 压缩响应。
---

`encode` 指令在响应离开 raddy 之前压缩它。它支持三种算法——**gzip**、
**zstd** 与 **Brotli**(`br`)——并同时作用于代理响应与静态文件。

## 工作原理

`encode` 按**优先级顺序**接受算法。raddy 对照客户端的 `Accept-Encoding` 头
协商,使用客户端也支持的第一个算法:

```caddyfile
example.com {
    encode br zstd gzip
    reverse_proxy 127.0.0.1:8080
}
```

这里接受 Brotli 的客户端得到 `br`;不接受的依次回退到 `zstd`,再 `gzip`。
三种算法都不支持的客户端得到未压缩的响应。

该指令作用于服务该块的任意终端——代理的 API 与静态文件皆然:

```caddyfile
static.example.com {
    root /var/www/html
    file_server
    encode zstd gzip
}
```

## 选择算法

| 算法 | 取舍 |
|---|---|
| `br`(Brotli) | 压缩率最佳;现代浏览器普遍支持 |
| `zstd` | 快速、压缩率强;当前浏览器与许多 HTTP 客户端支持 |
| `gzip` | 基线;处处支持 |

对 Web 流量,`encode br zstd gzip` 是不错的默认。`encode` 绝不作用于 `101`
(升级,如 WebSocket)响应。

## 你能得到什么

```bash
curl -H 'Host: static.example.com' -H 'Accept-Encoding: br' \
     http://127.0.0.1:8090/app.js -sD - -o /dev/null
```

```http
HTTP/1.1 200 OK
content-encoding: br
```

`file_server` 遵循同样的协商,因此它压缩提供 `index.html`,并对不要求压缩的
客户端跳过压缩。
