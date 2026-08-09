---
title: 访问日志
description: raddy 用 --access-log 写入的结构化 JSON 访问日志。
---

传入 `--access-log <file>`,把每个完成的请求以一行结构化 JSON 追加到指定
文件:

```bash
raddy run -c Raddyfile --access-log /var/log/raddy/access.jsonl
```

每行是一个独立的 JSON 对象(JSON Lines)。**追加**(绝不截断),因此你可以在
raddy 运行期间轮转文件,它会继续写入新路径。

## 字段

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
{"ts":1760850000456,"client":"203.0.113.7","method":"GET","path":"/search?q=raddy&page=2","status":200,"duration_ms":7}
```

`client` 字段按[信任模型](../../config/trusted-proxies/)取值:未配置
`trusted_proxies` 时是 TCP 对端;配置后是最右侧不可信的 `X-Forwarded-For`
条目。这与头改写中使用的 `{remote_host}` 占位符不同:后者始终展开为 TCP
对端地址——即使该对端是可信代理。
