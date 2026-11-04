# sekai-build

Sekai 固件构建/烧录编排器：为 Xtensa ESP32 目标（`xtensa-esp32-none-elf`）编译
`sekai-fw` 固件镜像并经串口烧录，服务嵌入式 Worker（`crate/sekai-embedded`，
当前阶段置后）。固件镜像由 `esp32` feature 门控，
`cargo build --workspace` 会完全跳过它。

## 用法

```text
cargo run -p sekai-build -- build [--release]     # 编译固件（推荐 --release）
cargo run -p sekai-build -- flash [--release] [--monitor] [-- <espflash 参数>]
cargo run -p sekai-build -- help
```

- `--monitor`：烧录后保持串口监视器打开（仅 `flash`）。
- `--`：其后的参数原样转发给 `espflash`（仅 `flash`）。
- 工具链：默认用 espup 安装的 `esp` 工具链（esp-rs fork），可用
  `SEKAI_TOOLCHAIN` 改名；core/alloc 用 `-Zbuild-std` 从 rust-src 现场构建
  （Xtensa 没有官方预编译 std）。

## 编译期固件环境变量

固件在编译时用 `option_env!` 读取以下变量，因此**必须在进行 `build` 时设置**
（仅烧录不需要）。必填项缺失只告警不失败（CI 可编译出镜像），但固件启动后会在
串口上报告配置错误并空转：

| 变量 | 必填 | 含义 |
| --- | --- | --- |
| `SEKAI_WIFI_SSID` | ✅ | 2.4GHz SSID |
| `SEKAI_WIFI_PASSWORD` | 可选 | 空 = Open，非空 = WPA2 Personal |
| `SEKAI_MQTT_ADDR` | ✅ | broker 地址，IPv4:port 字面量（如 `192.168.1.10:1883`，无 DNS） |
| `SEKAI_MQTT_USERNAME` / `SEKAI_MQTT_PASSWORD` | 可选 | MQTT 凭据 |
| `SEKAI_DEVICE_ID` | 可选 | worker id（默认 `sekai-esp32`） |
| `SEKAI_TOPIC_ROOT` | 可选 | 主题根（默认 `sekai/v1`） |
| `SEKAI_DEVICES` | 可选 | Jump 目标表 `name=IPv4:port,...` |

## 相关文档

- 完整指南（工具链安装、烧录、最小 WiFi/MQTT 链路、排障）：
  [docs/sekai-esp32.md](../../docs/sekai-esp32.md)。
- esp 工具链诊断记录与恢复步骤：
  [crate/sekai-embedded/README.md](../sekai-embedded/README.md)。
