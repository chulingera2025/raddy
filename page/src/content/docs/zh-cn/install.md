---
title: 安装
description: 使用校验和验证的安装脚本、手动方式或源码方式安装 raddex。
---

发布包附赠**经校验和验证的安装脚本**与**手动安装路径**——两者都不依赖
`curl | sudo bash`。安装脚本会先下载、校验 sha256,再执行安装。

## 安装脚本(推荐)

发布资源中包含 `install.sh`(先下载并审阅脚本):

```bash
# 下载并审阅脚本,然后执行
curl -fsSL -O https://github.com/chulingera2025/raddex/releases/latest/download/install.sh
# 可选:对照发布 SHA256SUMS 校验脚本自身的校验和
shasum -a 256 -c SHA256SUMS
./install.sh                  # 安装到 /usr/local/bin/raddex
./install.sh v0.1.2 ~/.local  # 指定版本与前缀
```

脚本根据 `uname -m` 选择 `x86_64-unknown-linux-gnu` 或
`aarch64-unknown-linux-gnu`,下载对应的压缩包与 `SHA256SUMS`,只有在
`shasum -a 256 -c` 通过后才执行 `install`。校验失败则中止,不进行安装。

## 手动安装

1. 在 [Releases](https://github.com/chulingera2025/raddex/releases) 下载对应
   架构的 `raddex-<arch>.tar.gz` 与 `SHA256SUMS`。
2. 校验:
   ```bash
   shasum -a 256 -c SHA256SUMS
   ```
   输出必须包含 `<filename>: OK`。
3. 解压并安装:
   ```bash
   tar -xzf raddex-<arch>.tar.gz -C /usr/local
   raddex --version
   ```

## 从源码构建

```bash
cargo build --release
./target/release/raddex --version
```

系统依赖:稳定版 Rust、OpenSSL 开发库(`libssl-dev` / `openssl`)、以及
`cmake`(pingora 的 `libz-ng-sys` 需要)。

## Docker

镜像**不会**内置 Raddexfile,因此需要把宿主机上的 Raddexfile 以只读方式挂载
进去。镜像的 `ENTRYPOINT` 是 `raddex`,所以容器命令直接写 `run` 子命令即可。
请在包含你 `Raddexfile` 的目录下运行:

```bash
docker build -t raddex .
docker run --rm -p 8080:8080 \
  -v "$PWD/Raddexfile:/etc/raddex/Raddexfile:ro" \
  raddex run -c /etc/raddex/Raddexfile
```

如需在容器重启后保留 ACME 证书,挂载一个证书目录并用 `--cert-dir` 指向它:

```bash
docker run --rm -p 80:80 -p 443:443 \
  -v "$PWD/Raddexfile:/etc/raddex/Raddexfile:ro" \
  -v raddex_certs:/etc/raddex/certs \
  raddex run -c /etc/raddex/Raddexfile --cert-dir /etc/raddex/certs
```

## 验证安装

```bash
raddex check -c <你的 Raddexfile>   # 校验配置
raddex run -c <你的 Raddexfile>     # 运行
```
