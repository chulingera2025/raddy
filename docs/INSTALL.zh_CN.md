# 安装

Raddy 的发布提供：**校验过的安装脚本** 与 **手动安装路径**，二者都不依赖 `curl | sudo bash`——脚本先下载、校验 sha256，再安装。

## 方式一：安装脚本（推荐）

发布资产包含 `install.sh`（可先下载并审查）：

```bash
# 下载脚本并审查，然后运行
curl -fsSL -O https://github.com/chulingera2025/raddy/releases/latest/download/install.sh
# 可选：核对脚本自身的 sha256（见发布页 SHA256SUMS）
shasum -a 256 -c SHA256SUMS   # 需已下载 SHA256SUMS
./install.sh                  # 安装到 /usr/local/bin/raddy
./install.sh v0.1.2 ~/.local  # 指定版本与前缀
```

脚本行为：按 `uname -m` 选择 `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu`，下载对应 tarball 与 `SHA256SUMS`，用 `shasum -a 256 -c` 校验通过后才 `install` 到前缀。校验失败即退出、不安装。

## 方式二：手动安装

1. 在 [Releases](https://github.com/chulingera2025/raddy/releases) 下载对应架构的 `raddy-<arch>.tar.gz` 与 `SHA256SUMS`。
2. 校验：
   ```bash
   shasum -a 256 -c SHA256SUMS
   ```
   输出须包含 `<文件名>: OK`。
3. 解压安装：
   ```bash
   tar -xzf raddy-<arch>.tar.gz -C /usr/local
   raddy --version
   ```

## 方式三：从源码构建

```bash
cargo build --release
./target/release/raddy --version
```

系统依赖：Rust stable、OpenSSL 开发库（`libssl-dev` / `openssl`）、`cmake`（pingora 的 `libz-ng-sys` 需要）。

## Docker

```bash
docker build -t raddy .
docker run --rm -p 8080:8080 raddy run -c /etc/raddy/Raddyfile
```

> 签名说明：用 **sha256 checksum** 保证完整性（安装脚本内置校验）。基于发布密钥的代码签名（如 minisign/cosign）作为后续增强，密钥与流程定稿后补充。

## 验证安装

```bash
raddy check -c <你的 Raddyfile>   # 校验配置
raddy run -c <你的 Raddyfile>     # 运行
```
