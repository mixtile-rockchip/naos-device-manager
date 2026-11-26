# NAOS Device Manager

[English](README.md) | **简体中文**

NAOS 设备管理服务与命令行工具。本分支提供 `naos-core` 的 Rust 实现。

## 功能

- 本地 CLI、Unix socket API 和可选 HTTP API。
- 当前固件版本、构建标识和板型查询。
- 通过 `naosd` 查询和控制容器。
- RAUC 签名系统包和 bootloader 包升级、hawkBit 注册与升级调度。
- OEM 身份读取、主机名和 mDNS 管理、硬件看门狗。
- 设备重启与恢复出厂设置。

## 构建和测试

在 Linux 主机上使用稳定版 Rust 工具链运行以下命令。

```sh
cargo build --locked --release
cargo test --locked
```

本机可执行文件位于 `target/release/naos-core`。测试使用临时配置和升级记录，并模拟 RAUC、hawkBit、naosd 和看门狗。集成测试会启动独立的 Core 进程并通过 Unix socket 调用服务。

主机安装 `dbus-daemon` 后，可一并运行 RAUC D-Bus 测试。

```sh
cargo test --locked -- --include-ignored
```

## 为 ARM64 Alpine 构建

在 x86-64 Linux 主机上安装 AArch64 GNU 交叉编译器和 binutils 后，执行以下命令。

```sh
rustup target add aarch64-unknown-linux-musl
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc \
AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar \
cargo build --locked --release --target aarch64-unknown-linux-musl
```

静态可执行文件位于 `target/aarch64-unknown-linux-musl/release/naos-core`。

## 发布

在 Rust 分支推送形如 `YYYY.MM.DD.1` 的版本标签后，GitHub Actions 会运行测试，并发布 ARM64 musl 安装包 `naos-core-linux-arm64.tar.gz`、SPDX 2.3 格式的 SBOM 和 `SHA256SUMS`。安装包内含可执行文件、OpenRC 脚本、配置模板、构建信息、许可证和一份 SBOM。

## 配置

以 [config/device.ini.example](config/device.ini.example) 为模板配置 `/etc/naos-core/device.ini`。命令可使用 `--config PATH` 指定其他配置文件。

默认 Unix socket 为 `/run/naos/core.sock`，可选 HTTP API 监听 `127.0.0.1:8890`。手动修改配置后重启 Core；`hawkbit register` 成功后会向运行中的服务应用注册配置。

需要开机启动业务时，在 `[container]` 中设置 `enabled=true`，并将 `manifest` 指向 `/userdata/` 下的业务清单或 `/var/lib/naos/containers/<id>/manifest.yml`。

## 系统信息 API

`GET /v1/oem` 和 `naos-core oem` 根据 `device.ini` 中的 `[platform] soc` 读取设备 SN。

| 平台 | SN 来源 |
| --- | --- |
| `rk3568` | 读取 `/proc/device-tree/serial-number` 中的 16 位 CPU 序列号。 |
| `rk3588` | 优先读取烧录的 OEM SN，缺失时从同一设备树属性读取 CPU 序列号。 |

使用 CPU 序列号时，主机名也使用该值。没有有效序列号时，SN 和主机名均为 `XXXXXXXXXXXXXXXX`。RK3568 的 `mac` 字段暂为空字符串。

新生成的 OTA 设备 ID 用 `SN--` 加 OEM SN，或 `CPU-` 加 CPU 序列号。两者都缺失时，Core 生成以 `NN` 开头的随机设备 ID。

`GET /v1/system` 从 `/usr/lib/naos/version.json` 读取当前固件信息，无需启用 OTA 或连接 OTA 服务器。

在设备本机，通过 HTTP 或 Unix socket 调用：

```sh
curl http://127.0.0.1:8890/v1/system
curl --unix-socket /run/naos/core.sock http://localhost/v1/system
```

响应示例：

```json
{
  "version": "0.1.0",
  "build_id": "example-build",
  "machine": "generic-board"
}
```

三个字段均为字符串。`version` 必须非空；文件中缺少 `build_id` 或 `machine` 时，对应字段返回空字符串。版本文件中的额外字段会被忽略。

版本文件缺失、不可读、无效或超过 64 KiB 时，接口返回 HTTP `503`，`error.code` 为 `SYSTEM_INFO_UNAVAILABLE`。

## 命令

```sh
naos-core --help
sudo naos-core serve
sudo naos-core oem
sudo naos-core containers
sudo naos-core containers start myapp
sudo naos-core hawkbit status
sudo naos-core hawkbit check
```

使用组织的 Gateway Token 注册设备：

```sh
sudo naos-core hawkbit register \
  --server https://ota.example.com \
  --tenant DEFAULT \
  --token GATEWAY_TOKEN
```
