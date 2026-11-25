# NAOS Device Manager

**English** | [简体中文](README.zh-CN.md)

NAOS device management service and command-line interface for Linux. This branch contains the Rust implementation of `naos-core`.

## Features

- Local CLI, Unix socket API, and optional HTTP API.
- Current firmware version, build ID, and machine queries.
- Container queries and control through `naosd`.
- Signed RAUC system and bootloader updates, hawkBit registration and update scheduling.
- OEM identity, hostname and mDNS management, and hardware watchdog support.
- Device reboot and factory reset.

## Build and test

With a current stable Rust toolchain on Linux:

```sh
cargo build --locked --release
cargo test --locked
```

The native binary is `target/release/naos-core`. Tests use temporary INI files and journals, fake RAUC and hawkBit services, a simulated naosd socket, and a mock watchdog. The daemon integration test starts a separate Core process with a temporary Unix socket and hardware features disabled.

Run the optional native RAUC D-Bus test with `dbus-daemon` installed and permission to bind a temporary Unix socket:

```sh
cargo test --locked -- --include-ignored
```

## ARM64 Alpine build

On an x86-64 Linux host with the AArch64 GNU cross compiler and binutils installed:

```sh
rustup target add aarch64-unknown-linux-musl
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
CC_aarch64_unknown_linux_musl=aarch64-linux-gnu-gcc \
AR_aarch64_unknown_linux_musl=aarch64-linux-gnu-ar \
cargo build --locked --release --target aarch64-unknown-linux-musl
```

The static binary is `target/aarch64-unknown-linux-musl/release/naos-core`.

## Configuration

Use [config/device.ini.example](config/device.ini.example) as `/etc/naos-core/device.ini`. Commands accept `--config PATH` to select another file.

The default Unix socket is `/run/naos/core.sock`; the optional HTTP API listens on `127.0.0.1:8890`. Restart Core after manually editing the configuration. Successful `hawkbit register` applies registration settings to the running service.

To start a configured application at boot, set `[container] enabled=true` and `manifest` to its manifest under `/userdata/` or `/var/lib/naos/containers/<id>/manifest.yml`.

## System information API

`GET /v1/system` reads the current firmware information from `/usr/lib/naos/version.json`. It does not require OTA to be enabled or an OTA server connection.

On the device, use either local HTTP or the Unix socket:

```sh
curl http://127.0.0.1:8890/v1/system
curl --unix-socket /run/naos/core.sock http://localhost/v1/system
```

Example response:

```json
{
  "version": "0.1.0",
  "build_id": "example-build",
  "machine": "generic-board"
}
```

All three fields are strings. `version` must be nonempty; missing `build_id` or `machine` values are returned as empty strings. Additional fields in the version file are ignored.

If the version file is missing, unreadable, invalid, or larger than 64 KiB, the API returns HTTP `503` with `error.code` set to `SYSTEM_INFO_UNAVAILABLE`.

## Commands

```sh
naos-core --help
sudo naos-core serve
sudo naos-core oem
sudo naos-core containers
sudo naos-core containers start myapp
sudo naos-core hawkbit status
sudo naos-core hawkbit check
```

Register a device using the organization's Gateway Token:

```sh
sudo naos-core hawkbit register \
  --server https://ota.example.com \
  --tenant DEFAULT \
  --token GATEWAY_TOKEN
```
