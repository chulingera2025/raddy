---
title: 可信代理
description: 告诉 raddex 哪些网络可信,使其能从 X-Forwarded-For 推导真实客户端 IP。
---

依赖客户端身份的功能 —— [限流](../directives/#rate_limit)、`ip_hash`
负载均衡、访问日志 —— 需要*真实*客户端 IP,而非中间代理的地址。本页说明
raddex 如何判断这一点。

## 默认行为

默认情况下 raddex **不信任任何东西**:直接使用 TCP 对端地址,忽略任何
`X-Forwarded-For` 头。这是安全默认 —— 除非你显式信任客户端经过的代理,否则
攻击者无法伪造客户端 IP。

## 当你位于代理之后

如果 raddex 位于 CDN 或负载均衡之后,且它们会设置 `X-Forwarded-For`,就把该
代理的网络声明为可信:

```caddyfile
{
    trusted_proxies 10.0.0.0/8 172.16.0.0/12 127.0.0.1
}
```

声明可信网络后,raddex 按如下规则推导真实客户端 IP:

1. 若 TCP 对端**不在**可信列表中,对端地址即客户端(不解析
   `X-Forwarded-For`)。
2. 若对端**可信**,raddex 从右向左遍历 `X-Forwarded-For` 链,取最右侧
   **不是**可信代理的条目(畸形条目跳过)。
3. 若整条链都可信——或头部缺失——则使用可信对端本身。

**语法。**

```caddyfile
trusted_proxies <cidr>...
```

每个 `<cidr>` 是 `<address>/<prefix>` 或裸地址(单个主机)。IPv4 与 IPv6 均
支持。同一作用域内,后出现者覆盖先出现者。

## 按站点覆盖

`trusted_proxies` 可在站点块内设置,**仅对该站点**覆盖全局列表:

```caddyfile
{
    trusted_proxies 10.0.0.0/8
}

api.example.com {
    trusted_proxies 127.0.0.1   # 只有此站点信任回环
    reverse_proxy 127.0.0.1:8080
}
```

## 示例

使用下面的配置,通过你的 CDN(`203.0.113.0/24`)到达、携带
`X-Forwarded-For: 198.51.100.9, 10.0.0.5` 的请求,会被记录与限流为来自
`198.51.100.9` —— 最右侧且不是可信代理的条目:

```caddyfile
{
    trusted_proxies 203.0.113.0/24 10.0.0.0/8
}

api.example.com {
    rate_limit remote_ip 100r/s
    reverse_proxy 127.0.0.1:8080
}
```
