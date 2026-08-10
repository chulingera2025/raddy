---
title: 认证
description: 用 HTTP Basic 认证或委托给认证上游,为你的站点把关。
---

两个守卫指令控制谁可以被服务:`basic_auth`(HTTP Basic 认证)与 `forward_auth`
(委托给专用认证服务)。两者都是守卫,因此作用于服务该块的任意终端;在
`handle` 块内只作用于该块的终端。

## 用 `basic_auth` 做 HTTP Basic 认证

`basic_auth <user> <bcrypt-hash>` 要求 HTTP Basic 认证。先生成密码的 bcrypt
哈希:

```bash
htpasswd -Bbn admin 's3cret'
# → admin:$2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
```

然后保护一个站点:

```caddyfile
admin.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:8080
}
```

多行 `basic_auth` 构成**用户表**——请求可为其中任意一个出示凭据:

```caddyfile
admin.example.com {
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    basic_auth jane $2b$12$7r8...HrH9
    reverse_proxy 127.0.0.1:8080
}
```

无有效凭据的请求返回 **401 Unauthorized**,并带 `WWW-Authenticate: Basic`
挑战,因此浏览器会弹出登录提示。

## 用 `forward_auth` 委托给认证服务

`forward_auth <host:port>` 把每个请求转发给专用认证上游(例如 oauth2-proxy,
或你自己的认证服务):

```caddyfile
app.example.com {
    forward_auth auth.example.com:4181
    reverse_proxy 127.0.0.1:8080
}
```

它的判定:

- 认证上游返回 **2xx** —— 放行,转发给真实上游。
- **403** —— 原样透传给客户端。
- 其他情况 —— **401 Unauthorized**。

发给认证上游的请求携带原始 `Authorization` 与 `X-Forwarded-For` 头,因此认证
服务看到与 raddy 相同的凭据与客户端。认证上游的**响应头**——例如
`X-Auth-User` 这样的身份头——会在真实上游看到请求之前被复制到请求上,你的
后端可以信任它们。

## 把守卫限定在某个路径

把守卫放进 `handle` 块,只保护站点的部分区域:

```caddyfile
example.com {
    handle /admin/* {
        basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
        reverse_proxy 127.0.0.1:9000
    }

    reverse_proxy 127.0.0.1:8080
}
```

这里 `/admin/...` 经 Basic 认证后代理给管理后端;其余请求不加守卫直达主应用。

## 与 mTLS 组合

HTTP 层认证与[双向 TLS](../https-tls/#双向-tls客户端证书)相互独立、
可以组合:mTLS 回答"这个客户端的证书是否由我们的 CA 签发?",而
`basic_auth` / `forward_auth` 回答"这个客户端是谁,允许吗?"。

```caddyfile
secure.example.com {
    tls client_auth require /etc/certs/clients-ca.pem
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:9000
}
```
