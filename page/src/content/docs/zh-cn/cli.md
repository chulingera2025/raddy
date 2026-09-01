---
title: CLI 参考
description: raddex 的每个子命令及其选项。
---

`raddex` 二进制有三个主要子命令,外加一个迁移辅助命令。

## `raddex run`

前台运行反向代理服务器。

| 选项 | 默认值 | 说明 |
|---|---|---|
| `-c, --config <file>` | `Raddexfile` | Raddexfile 路径 |
| `--cert-dir <dir>` | `raddex_certs` | ACME 证书与账户凭据目录 |
| `--acme-directory <url>` | Let's Encrypt 生产环境 | ACME 目录 URL |
| `--acme-root-pem <file>` | — | 信任 ACME 服务器的 PEM 根 CA(测试服务器如 Pebble 必需) |
| `--access-log <file>` | — | 将结构化 JSON 访问日志追加到此文件 |
| `--metrics-addr <addr>` | — | 在此地址暴露 Prometheus `/metrics`(例如 `127.0.0.1:9100`) |
| `--pidfile <file>` | — | 将本进程 PID 写入此文件,供 `raddex upgrade` 定位 |
| `--upgrade-sock <sock>` | `/tmp/raddex_upgrade.sock` | 升级期间移交监听 fd 的 Unix 套接字 |
| `-u, --upgrade` | — | 以零停机升级的*新*一侧启动(通常由 `raddex upgrade` 派生) |
| `-t, --test` | — | 校验配置与构造后退出 0/1,不绑定任何监听器(`raddex upgrade` 的预检) |

## `raddex upgrade`

零停机二进制升级(需要 `--pidfile`):预检新二进制,以 `-u` 派生替代进程,然后
向运行中实例发送 SIGQUIT。与 `raddex run` 共享相同的选项。

## `raddex check`

校验 Raddexfile 并退出——与**重载执行的校验完全相同**。通过 `check` 的配置能
干净重载,反之亦然。

```bash
raddex check -c Raddexfile   # 输出 "Raddexfile: ok",退出 0;或输出错误并退出 1
```

## `raddex import`

将 Caddyfile 或 nginx.conf 子集转换为 Raddexfile。**独立转换器**:绝不改动
Raddexfile 文法,并在打印前(通过重载所用的同一管道)校验自身输出。

```bash
raddex import caddyfile <source> [-o <output>]
raddex import nginx    <source> [-o <output>]
```

省略 `-o` 则把 Raddexfile 打印到 stdout。

## 退出行为

`check` 对合法配置退出 0,否则退出 1。`run` 与 `upgrade` 在启动错误时退出 1
(例如非法配置,因此非法配置绝不会启动进程)。`import` 在无可转换内容或生成的
Raddexfile 校验失败时退出 1。
