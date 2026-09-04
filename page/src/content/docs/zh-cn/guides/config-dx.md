---
title: 配置复用
description: 跨文件拆分配置、复用片段,并在解析时注入环境变量。
---

随着 Raddexfile 增长,三个特性让配置保持可读、可部署:`import` 做多文件包含,
`(name)` 片段在同一文件内复用,`{$ENV}` 在解析时注入环境变量。

## 用 `import` 包含其他文件

`import <file>` 在该位置拼入另一个 Raddexfile 的内容。路径相对于导入文件,
导入可以嵌套(有深度限制):

```caddyfile
import common/headers.conf

api.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

站点块可以导入其指令归属于该站点的文件:

```caddyfile
# common/proxy-settings.conf
rate_limit remote_ip 100r/s
header_up X-Raddex true
```

```caddyfile
api.example.com {
    import common/proxy-settings.conf
    reverse_proxy 127.0.0.1:8080
}
```

## 片段:一个文件内的可复用块

顶层命名为 `(name) { ... }` 的块定义**片段**;`import name` 在该位置将其拼入。
片段仅对定义它的文件可见:

```caddyfile
(base) {
    rate_limit remote_ip 100r/s
    header_up X-Raddex true
}

api.example.com {
    import base
    reverse_proxy 127.0.0.1:8080
}

admin.example.com {
    import base
    basic_auth admin $2b$12$C6UzMDM.H6dfI/f/IKcEeO7Q7gW1wjQyG9Q8wK7BZ3v2pG5YzF5qO
    reverse_proxy 127.0.0.1:9000
}
```

两个站点共享同样的守卫,`admin.example.com` 再加上自己的认证。

## 注入环境变量

形如 `{$ENV_VAR}` 的指令参数在解析时被 `ENV_VAR` 的值替换。它适用于出现参数
的任何位置——上游目标、`root` 路径、`tls` 证书路径:

```caddyfile
api.example.com {
    reverse_proxy https://{$BACKEND_HOST}:8443
}
```

```bash
BACKEND_HOST=10.0.0.5 raddex run -c Raddexfile
```

**缺失**的变量是校验错误,因此引用了写错或未设置变量的配置会让
`raddex check` 失败,而不是带着错误值启动——部署时的失误在流量之前就被抓住。

## 组合使用

片段 + 环境变量是用环境专属值共享站点模板的干净方式:

```caddyfile
(api_site) {
    rate_limit remote_ip 50r/s
    reverse_proxy https://{$API_BACKEND}:8443
}

api.example.com {
    import api_site
}
```

运行前先校验整体:

```bash
API_BACKEND=10.0.0.1 raddex check -c Raddexfile
API_BACKEND=10.0.0.1 raddex run -c Raddexfile
```
