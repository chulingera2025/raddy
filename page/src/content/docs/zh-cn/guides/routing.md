---
title: 路由与匹配器
description: 按路径、主机、方法、请求头、查询参数、客户端 IP 或协议路由。
---

raddy 的路由建立在**匹配器**之上——即选择哪些请求由某条指令服务的匹配项。
本指南展示如何组合它们来路由流量。

## 匹配项

匹配器是一串匹配项;**所有项都必须匹配**(AND)。每项有一个类型:

| 匹配项 | 匹配条件 |
|---|---|
| `path <prefix>` | 请求路径等于前缀或位于其下(`/api` 匹配 `/api` 与 `/api/...`,不匹配 `/apix`);末尾 `*` 会被去除 |
| `host <host>` | 规范化后的 Host 头等于该值 |
| `method <method>` | 请求方法等于该值(如 `GET`) |
| `header <name> <value>` | 请求头 `name` 等于 `value` |
| `query <key> <value>` | 查询参数 `key` 等于 `value` |
| `remote_ip <cidr>...` | 真实客户端 IP 位于列出的网络内 |
| `protocol <http\|https>` | 接收请求的监听器的传输协议 |

以 `/` 开头的裸值是 `path` 的简写,所以 `handle /static/*` 即
`handle path /static/*`。

- **取反** —— 以 `!` 前缀: `!path /admin/*` 匹配除 `/admin/...` 之外的一切。
- **没有括号、没有 `&&`** —— 各项以空格分隔。`handle path /status method GET`
  是语法;`handle (path /status && method GET)` 不是。

## 匹配器附着在哪

匹配器附着在 `handle` / `handle_path` 块上,也可内联附着在终端指令
`reverse_proxy`、`respond` 与 `error` 上:

```caddyfile
reverse_proxy path /api/* { to 127.0.0.1:8080 }
respond method OPTIONS 204
error !path /assets/* 503
```

内联匹配器不匹配的终端是**空操作**——执行继续到下一行指令。没有匹配器的
终端总是匹配。

## 用 `handle` 分组请求

`handle <matcher> { ... }` 为匹配的请求运行块内内容,然后**停止**;不匹配的
请求继续越过它。这是标准的"某个路径给这个终端,其余给另一个"模式:

```caddyfile
example.com {
    handle /static/* {
        root /var/www/html
        file_server
    }

    reverse_proxy 127.0.0.1:8080
}
```

## 用 `handle_path` 剥离前缀

`handle_path <matcher> { ... }` 行为类似 `handle`,但被匹配的路径前缀会从 URI
中剥离,然后块内终端才运行——后端无需知道自己挂载在 `/api` 下:

```caddyfile
example.com {
    handle_path /api/* {
        reverse_proxy 127.0.0.1:8080
    }

    reverse_proxy 127.0.0.1:9000
}
```

`GET /api/users/1` 以 `/users/1` 转发给第一个后端。

## 用 `rewrite` 改写 URI

`rewrite <to>` 是**修饰指令**,在终端服务前改写请求 URI。可使用占位符
`{host}`、`{uri}` 与 `{remote_host}`。与 `handle` 配合可做条件改写:

```caddyfile
example.com {
    handle path /docs/* {
        rewrite /v2/{uri}
        reverse_proxy 127.0.0.1:8080
    }

    reverse_proxy 127.0.0.1:8080
}
```

## 用 `respond` 与 `error` 直接应答

`respond <status> [<body>]` 直接应答请求——不经过上游,也不读文件。用于健康
检查、CORS 预检应答与固定的维护响应:

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

`error [<status>] [<message>]` 以选定状态码(默认 `500`)提供 raddy 的内部
错误响应——可用匹配器拦截某个区域:

```caddyfile
example.com {
    handle /internal/* {
        error 404 not here
    }

    reverse_proxy 127.0.0.1:8080
}
```

## 一个完整的路由示例

```caddyfile
api.example.com {
    # 健康检查与 CORS 在本地应答,先于任何代理。
    handle path /health {
        respond 200 ok
    }

    # API 挂载在 /api 下,在两个后端间负载均衡。
    handle_path /api/* {
        reverse_proxy {
            to 10.0.0.1:8000 10.0.0.2:8000
            health_check { interval 5s }
        }
    }

    # 静态资源来自磁盘并压缩。
    handle /static/* {
        root /var/www/html
        file_server
        encode zstd gzip
    }

    # 其余全部——代理给应用。
    reverse_proxy 127.0.0.1:8080
}
```

> 书写顺序在**终端之间**有意义:第一个匹配的终端结束站点执行。`respond`、
> `handle` 与 `reverse_proxy` 按你书写的顺序竞争。
