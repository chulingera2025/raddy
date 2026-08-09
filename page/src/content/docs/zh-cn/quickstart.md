---
title: 快速上手
description: 写出第一个 Raddyfile 并在约五分钟内跑通流量。
---

本教程带你从空目录到一个运行中的反向代理:写 Raddyfile、校验、运行、观察
流量流动。大约需要五分钟和一个终端。

## 开始之前

先[安装 raddy](../install/)。你还需要一个本地 HTTP 服务作为代理目标 —— 如果
`127.0.0.1:8080` 上没跑任何东西,启动一个简易的:

```bash
python3 -m http.server 8080 --bind 127.0.0.1
```

让它保持运行;raddy 会把请求转发给它。

## 1. 写你的第一个 Raddyfile

创建一个名为 `Raddyfile` 的文件,包含一个站点。这个站点服务 `example.local`
主机上的 8090 端口,并把每个请求代理给你的本地服务:

```caddyfile
example.local:8090 {
    reverse_proxy 127.0.0.1:8080
}
```

> **为什么要显式端口?** 不带端口的具名站点绑定 **443(TLS)** 并启用自动
> HTTPS。写成 `:8090` 让这个第一个示例保持纯 HTTP,无需真实域名即可在笔记本
> 上运行。端口与 HTTPS 见[站点 · 端口 · HTTPS](../config/sites/)。

## 2. 校验配置

`raddy check` 执行**与重载完全相同的校验** —— 若此处通过,raddy 即可干净
启动:

```bash
raddy check -c Raddyfile
```

预期输出:

```
Raddyfile: ok
```

## 3. 运行 raddy

```bash
raddy run -c Raddyfile
```

raddy 在前台启动,绑定 8090 端口,等待请求。

## 4. 发送请求

在另一个终端里,经由 raddy 请求该站点:

```bash
curl -H 'Host: example.local' http://127.0.0.1:8090/
```

`Host` 头匹配该站点,于是 raddy 把请求代理到 `127.0.0.1:8080`,你会看到本地
服务的响应。

再看站点选择的实际效果。与单纯的端口转发不同,raddy 按主机路由 —— 试试:

```bash
curl http://127.0.0.1:8090/                             # 缺失 Host → 400
curl -H 'Host: unknown.example' http://127.0.0.1:8090/  # 无匹配站点 → 404
```

## 5. 添加第二个站点

建一个含文件的目录,然后在你的代理旁加一个静态站点:

```bash
mkdir public && echo 'hello from raddy' > public/hello.txt
```

```caddyfile
example.local:8090 {
    reverse_proxy 127.0.0.1:8080
}

static.local:8090 {
    root ./public
    file_server
}
```

停止 raddy(Ctrl-C),用更新后的文件重新启动:

```bash
raddy run -c Raddyfile
```

然后通过静态站点取文件:

```bash
curl -H 'Host: static.local' http://127.0.0.1:8090/hello.txt
# → hello from raddy
```

> raddy 还能**无停机重载**配置:给运行中的进程发 `SIGHUP`
> (`kill -HUP <raddy pid>`)。重载会原子替换路由快照,并保持既有连接不断。

## 下一步

- [静态托管](../guides/static-files/) —— `file_server` 详解
- [HTTP → HTTPS 重定向](../guides/http-to-https/) —— `:80` catch-all 模式
- [代理 API](../guides/api-proxy/) —— 负载均衡、健康检查、限流
- [站点 · 端口 · HTTPS](../config/sites/) —— 匹配与证书如何工作
- [指令参考](../config/directives/) —— 每条指令,含示例
