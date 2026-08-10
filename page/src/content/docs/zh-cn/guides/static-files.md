---
title: 静态托管
description: 用 raddy 从磁盘托管一个静态站点,并启用压缩。
---

## 目标

通过 HTTP 提供一目录的静态文件(HTML、CSS、JS、图片)—— 带压缩,无需应用
服务器。

## 配置

把文件放进一个目录,用 `file_server` 指向一个站点:

```caddyfile
static.example.com {
    root /var/www/html
    file_server
}
```

`root` 设置目录;`file_server` 提供它。像 `./public` 这样的相对路径从 raddy
的工作目录解析。

要发送压缩响应,加 `encode` —— 客户端也支持的第一个算法胜出。可使用
`gzip`、`zstd` 与 Brotli(`br`):

```caddyfile
static.example.com {
    root /var/www/html
    file_server
    encode br zstd gzip
}
```

协商如何工作、如何选择算法,见[压缩指南](../compression/)。

## 运行

```bash
raddy check -c Raddyfile
raddy run -c Raddyfile
```

## 你能得到什么

```bash
curl -H 'Host: static.example.com' http://127.0.0.1:8090/            # index.html
curl -H 'Host: static.example.com' http://127.0.0.1:8090/app.js      # 一个文件
curl -H 'Host: static.example.com' -H 'Accept-Encoding: gzip' \
     http://127.0.0.1:8090/app.js -sD - -o /dev/null                # Content-Encoding: gzip
```

可依赖的行为:

- **目录提供其 `index.html`** —— `/` 映射到 `index.html`。
- **仅允许 `GET` 与 `HEAD`**;其他方法被拒绝。
- **路径穿越被阻止** —— `/../etc/passwd` 返回 `404`,而不是你的文件。
- **`encode` 同样压缩 `file_server` 的响应**,遵循客户端的
  `Accept-Encoding`。
- `file_server` 提供 `root` + 完整请求路径,含任何 `handle` 前缀(见下)。

## 提供子路径

用 `handle` 在某个路径下提供静态文件,其余代理给应用:

```caddyfile
example.com {
    handle /static/* {
        root /var/www/html
        file_server
        encode zstd gzip
    }

    reverse_proxy 127.0.0.1:8080
}
```

这里 `/static/app.js` 映射到 `/var/www/html/static/app.js` —— `handle`
前缀保留在路径中。
