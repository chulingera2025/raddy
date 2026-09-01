---
title: 访问日志
description: raddex 经 --access-log 或 access_log 指令写出的 JSON 与 Common Log Format 访问日志。
---

raddex 为每个完成的请求写一行访问日志。你可以从 CLI 或从 Raddexfile 配置它。

## 经 CLI:`--access-log`

传入 `--access-log <file>`,把每个完成的请求以一行结构化 JSON 追加到指定
文件:

```bash
raddex run -c Raddexfile --access-log /var/log/raddex/access.jsonl
```

每行是一个独立的 JSON 对象(JSON Lines)。**追加**(绝不截断)。文件在启动时
打开一次,句柄在整个进程生命周期内保持不变,因此轮转请使用 logrotate 的
**`copytruncate`** 模式(raddex 持续追加到同一个 inode);基于重命名的轮转会
让 raddex 继续写入被改名的旧文件,而非新路径。SIGHUP 重载不会重新打开日志。

## 经 Raddexfile:`access_log`

[`access_log` 指令](../../config/directives/#access_log) 在配置中配置日志,支持
两种格式:

```caddyfile
{
    access_log /var/log/raddex/access.log format=json   # 或 format=common
}

api.example.com {
    access_log off        # 仅关闭该站点的日志
    reverse_proxy 127.0.0.1:8080
}
```

- 全局块:`access_log <path> [format=<json|common>]` 设置实例日志文件与格式;
  `access_log off` 关闭整个实例的日志。
- 站点块:`access_log off` 仅关闭该站点的日志。
- Raddexfile 与 `--access-log` 同时设置时,**标志胜出**。

`format=common` 写经典 combined 日志行
(`%h %l %u %t "%r" %>s %b "%{Referer}i" "%{User-Agent}i"`);`json`(默认)写
下面的结构化行。

## JSON 字段

| 字段 | 类型 | 含义 |
|---|---|---|
| `ts` | integer | 请求开始时间,**epoch 毫秒** |
| `client` | string | **真实客户端 IP** —— TCP 对端,或配置了 `trusted_proxies` 时 `X-Forwarded-For` 中不可信的那一项(见[可信代理](../../config/trusted-proxies/)) |
| `method` | string | HTTP 方法(`GET`、`POST`,…) |
| `path` | string | 请求路径,**含查询字符串** |
| `status` | integer | HTTP 响应状态码 |
| `duration_ms` | integer | 请求耗时(毫秒) |

```json
{"ts":1760850000123,"client":"203.0.113.7","method":"GET","path":"/","status":200,"duration_ms":4}
{"ts":1760850000456,"client":"203.0.113.7","method":"GET","path":"/search?q=raddex&page=2","status":200,"duration_ms":7}
```

`client` 字段按[信任模型](../../config/trusted-proxies/)取值:未配置
`trusted_proxies` 时是 TCP 对端;配置后是最右侧不可信的 `X-Forwarded-For`
条目。这与头改写中使用的 `{remote_host}` 占位符不同:后者始终展开为 TCP
对端地址——即使该对端是可信代理。
