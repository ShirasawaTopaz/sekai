# Sekai ESP32 固件：构建、烧录与最小 WiFi/MQTT 链路

本文说明如何把 Sekai 嵌入式 Worker 运行时跑在 **ESP32 classic** 上：安装 Xtensa 工具链、用 `sekai-build` 构建/烧录固件、
配置编译期 WiFi/MQTT 参数，以及最小 WiFi/TCP/MQTT 链路的启动顺序、限制与故障排查。

- **读者**：要在 ESP32 开发板上运行 Sekai 的固件开发者、部署/调试 MQTT 链路的工程师。
- **实现基线**：`crate/sekai-embedded/src/driver/esp32/mod.rs`、`crate/sekai-embedded/src/driver/esp32/net.rs`、
  `crate/sekai-build/src/main.rs`、`crate/sekai-embedded/Cargo.toml`。
- **相关文档**：`docs/sekai-gpio.md`（GPIO 工具与 AI 调用）、`docs/sekai-protocol.md`
  （MQTT 主题、帧格式、错误码、Jump 与 broker ACL）。
- **当前芯片**：仅 ESP32 classic（Xtensa，非 S3/C3/C6）；本轮不做 TLS、DNS、静态 IP、NVS 配网、AP/BLE。

---

## 0. 范围与前提

### 0.1 最小链路包含什么

- **WiFi**：2.4GHz Station 模式，WPA2 Personal 或 Open（由密码是否为空决定）。
- **网络**：`embassy-net` + DHCPv4；只接受 **IPv4 字面量** 的 broker 地址（本轮无 DNS）。
- **MQTT**：MQTT 5.0 over **明文 TCP**；由 `sekai` 运行时负责 CONNECT/SUBSCRIBE/PUBLISH 与 LWT。
- **GPIO**：注册 4 个 `gpio.*` 用户工具，AI 可以通过 MQTT ToolCall 操作真实引脚。
- **运行时**：`sekai::sekai_runtime::Runtime` 在 Embassy 异步执行器上运行，发布 retained Hello、处理 ToolCall。

### 0.2 硬件与网络前提

| 项 | 要求 |
| --- | --- |
| 开发板 | ESP32 classic（Xtensa），带 USB 串口（CP210x/CH340 等）；建议 4MB flash。 |
| 供电 | 稳定 5V/3.3V；WiFi 发射瞬间电流较大，供电不足会导致重启/掉线。 |
| WiFi | 2.4GHz SSID（ESP32 不支持 5GHz）；WPA2 Personal 或 Open。 |
| MQTT broker | 支持 MQTT 5；地址为 **IPv4:port**（例如 `192.168.1.10:1883`）；ESP32 与 broker 在同一可信 LAN。 |
| 串口工具 | `espflash` 或任意串口监视器；默认 115200 baud。 |
| 主机 | 能运行 `cargo`、`rustup` 与 `espflash`；已安装 Espressif 工具链（见 §1）。 |

### 0.3 不在本轮

- TLS/加密 MQTT、DNS 主机名、静态 IP、NVS/WiFi 配网、AP 模式、BLE、企业级 WiFi。
- ESP32-S3/C3/C6 等变体（需要新的家族表与驱动）。
- STM32 的网络链路（F1/F4 无内置 WiFi，另行设计）。
- OTA 升级、双分区、功耗管理（light sleep/deep sleep）。

---

> **状态注记**：嵌入式 Worker 为置后阶段：当前宿主构建保绿，固件构建由 `esp32` feature 门控；xtensa 工具链状态见该文档下文/README。

---

## 1. 工具链与目标安装

### 1.1 目标与工具链概览

| 项 | 值 |
| --- | --- |
| 目标 triple | `xtensa-esp32-none-elf` |
| Rust 工具链 | Espressif 维护的 Xtensa fork，工具链名 `esp`（由 `espup` 或手工安装） |
| 标准库 | 需要 `-Zbuild-std=core,alloc`（Xtensa 没有预构建的 `core`/`alloc`） |
| 链接 | `-C link-arg=-Tlinkall.x`（`esp-hal` 提供 `linkall.x`） |
| 烧录 | `espflash`（`cargo install espflash`） |

> 为什么不能用上游 nightly：上游 Xtensa LLVM 后端在编译 `compiler_builtins` 时会崩溃，
> 必须使用 esp-rs 打过补丁的 fork。`sekai-build` 默认使用工具链 `esp`，可用 `SEKAI_TOOLCHAIN` 覆盖。

### 1.2 路线 A：espup（需要能访问 GitHub API）

标准做法：

```bash
cargo install espup
espup install -t esp32        # 安装 Rust 工具链 `esp`（以及配套组件）
# Unix：加载环境变量脚本
source "$HOME/export-esp.sh"
# Windows PowerShell：
# . "$HOME\export-esp.ps1"
```

验证：

```bash
rustup toolchain list          # 应包含 `esp`
rustc +esp --version           # 1.97.0-nightly（具体版本以 espup 安装为准）
rustup run esp rustc --print target-list | grep xtensa-esp32-none-elf
```

注意：`espup` 依赖 `api.github.com` 与 GitHub 下载。如果网络受限（例如本环境），使用路线 B。

### 1.3 路线 B：离线手工安装（gh-proxy 镜像，本环境验证过）

当 `espup` 不可用时，可以手工把 esp-rs 的预编译工具链放进 rustup 工具链目录：

1. **下载资产**：从 `esp-rs/rust-build` 的 release **v1.97.0.0** 下载 Windows 资产（GitHub 直连被墙时
   使用 `https://gh-proxy.com/` + 原始 GitHub URL 前缀）：

   | 资产 | 本环境文件名 | 用途 |
   | --- | --- | --- |
   | Rust 工具链 | `rust-1.97.0.0-x86_64-pc-windows-msvc.zip` | `rustc`/`cargo`/`rust-lld` 等，解压后即工具链 `esp` |
   | clang/LLVM 运行库 | `libs-clang-esp-21.1.3_20260408-x86_64-w64-mingw32.tar.xz` | `xtensa-esp32-elf-clang/esp-clang`（esp-hal 构建/链接需要） |

大文件建议用支持断点续传/分块的方式下载（`curl -L -r` 分块、校验总字节数），gh-proxy 支持 range 请求。

   Windows PowerShell 下载示例（URL 以 release 页面为准；gh-proxy 的用法是把原始 GitHub URL 放在路径里）：

   ```powershell
   curl.exe -L -o rust-toolchain.zip "https://gh-proxy.com/https://github.com/esp-rs/rust-build/releases/download/v1.97.0.0/rust-1.97.0.0-x86_64-pc-windows-msvc.zip"
   curl.exe -L -o clang-libs.tar.xz "https://gh-proxy.com/https://github.com/esp-rs/rust-build/releases/download/v1.97.0.0/libs-clang-esp-21.1.3_20260408-x86_64-w64-mingw32.tar.xz"
   ```

2. **解压 Rust 工具链**到 rustup 工具链目录（Windows 示例；若压缩包带顶层目录，把顶层目录的内容上移，保证存在 `esp\bin\rustc.exe`）：

   ```powershell
   Expand-Archive rust-toolchain.zip -DestinationPath "$env:USERPROFILE/.rustup/toolchains/esp" -Force
   tar -xJf clang-libs.tar.xz -C "$env:USERPROFILE/.rustup/toolchains/esp"
   ```

   解压后的目录结构应为（`esp` 即工具链根目录；若压缩包带顶层目录，把内容上移）：

   ```text
   esp\
     bin\            (cargo.exe、rustc.exe、rust-lld 等)
     lib\
     libexec\
     share\
   ```

3. **解压 clang 库**到同一目录，确保存在：

```text
%USERPROFILE%\.rustup\toolchains\esp\xtensa-esp32-elf-clang\esp-clang\bin\
```

4. **安装 GCC 工具链**（链接/汇编工具，例如 Espressif 的 `xtensa-esp-elf` 发行包）。
   本环境的 GCC 位于 `C:\Espressif\tools\xtensa-esp-elf\esp-14.2.0_20241119\xtensa-esp-elf\bin`。

5. **把以下目录加入 PATH**（顺序不敏感，但必须都可见）：

```text
%USERPROFILE%\.rustup\toolchains\esp\bin
%USERPROFILE%\.rustup\toolchains\esp\xtensa-esp32-elf-clang\esp-clang\bin
C:\Espressif\tools\xtensa-esp-elf\esp-14.2.0_20241119\xtensa-esp-elf\bin
```

6. **验证**：

```bash
rustc +esp --version
# 期望类似：rustc 1.97.0-nightly (...) (1.97.0.0)
rustup run esp rustc --print target-list | grep -c xtensa       # 应为 7（包含 xtensa-esp32-none-elf）
```

> 版本号、目录名与资产名以实际 release 页面为准；上表是本环境验证过的组合（v1.97.0.0）。
> 路线 A 与路线 B **二选一**即可，不要混用两个不同版本的工具链。

### 1.4 espflash（烧录工具）

```bash
cargo install espflash
espflash --version
```

`sekai-build flash` 会调用 `espflash flash --chip esp32`；未安装时 `sekai-build` 会给出安装提示。
本仓库未在真实硬件上验证刷机，只验证了固件构建。

---

## 2. sekai-build 命令

### 2.1 用法

```text
cargo run -p sekai-build -- build [--release]
cargo run -p sekai-build -- flash [--release] [--monitor] [-- <espflash args>]
cargo run -p sekai-build -- help
```

| 命令/选项 | 说明 |
| --- | --- |
| `build` | 编译 `sekai-fw` 固件镜像（默认 debug）。 |
| `build --release` | release 构建；**真正上板应使用 release**（debug 镜像显著更大，仅用于编译验证）。 |
| `flash` | 先构建，再用 `espflash` 写入开发板。 |
| `flash --monitor` | 刷写后保持串口监视器打开。 |
| `flash -- <args>` | 其余参数原样转发给 `espflash`（例如 `-- --baud 115200`）。 |
| `help` / `--help` | 打印 usage，其中包含固件编译期环境变量说明。 |

### 2.2 实际执行的构建命令

`sekai-build build` 内部等价于（在 workspace 根目录）：

```text
cargo +esp build \
  --package sekai-build --bin sekai-fw \
  --no-default-features --features esp32 \
  --target xtensa-esp32-none-elf \
  -Zbuild-std=core,alloc \
  --config 'target."xtensa-esp32-none-elf".rustflags=["-C","link-arg=-Tlinkall.x"]'
```

- 只选中 `sekai-embedded` 的 `esp32` 驱动，不选任何其他驱动（`x86` 驱动与 `esp32` 驱动互斥，分属 `sekai-worker` 与 `sekai-embedded`）。
- 链接参数以 target 为作用域，不影响宿主 build script。
- 产物路径：

```text
target/xtensa-esp32-none-elf/debug/sekai-fw
target/xtensa-esp32-none-elf/release/sekai-fw
```

（设置 `CARGO_TARGET_DIR` 时按该目录计算。）

### 2.3 构建前检查与警告

`build` 会先做三项检查：

1. 工具链 `esp`（或 `SEKAI_TOOLCHAIN`）存在，且 `rustc --print target-list` 包含目标 triple；
   缺失时给出可操作提示（`espup install -t esp32`）。
2. 对必填的固件编译期变量打印 warning（**只警告不失败**，方便 CI 只编译）：

```text
warning: required firmware environment variable `SEKAI_WIFI_SSID` is not set
warning: the built firmware will report a configuration error over serial at startup; ...
```

3. `flash` 额外检查 `espflash` 是否在 PATH（缺失时提示 `cargo install espflash`）。

### 2.4 构建/烧录示例

Bash（Linux/macOS/WSL/Git Bash）：

```bash
SEKAI_WIFI_SSID=my-ssid \
SEKAI_WIFI_PASSWORD=my-pass \
SEKAI_MQTT_ADDR=192.168.1.10:1883 \
SEKAI_DEVICE_ID=esp32-lab \
cargo run -p sekai-build -- build --release

# 构建并烧录（--monitor 保持串口输出）
SEKAI_WIFI_SSID=my-ssid \
SEKAI_WIFI_PASSWORD=my-pass \
SEKAI_MQTT_ADDR=192.168.1.10:1883 \
SEKAI_DEVICE_ID=esp32-lab \
cargo run -p sekai-build -- flash --release --monitor -- --baud 115200
```

Windows PowerShell（环境变量语法不同）：

```powershell
$env:SEKAI_WIFI_SSID = "my-ssid"
$env:SEKAI_WIFI_PASSWORD = "my-pass"
$env:SEKAI_MQTT_ADDR = "192.168.1.10:1883"
$env:SEKAI_DEVICE_ID = "esp32-lab"
cargo run -p sekai-build -- build --release
```

- 编译期变量只在 `build` 时读取；`flash` 也会先执行 `build`，所以同一条命令里同样要带上这些变量。
- 如果只验证编译，可以不设置变量（会得到 warning + 一个启动时报配置错误的固件）。

---

## 3. 编译期环境变量

固件通过 `option_env!` 在**编译期**读取以下变量；修改后必须重新构建，重新烧录才会生效。

| 变量 | 必填 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `SEKAI_WIFI_SSID` | **是** | — | 2.4GHz SSID；空字符串视为配置错误。 |
| `SEKAI_WIFI_PASSWORD` | 否 | 空（Open） | 空 = Open 网络；非空 = WPA2 Personal。 |
| `SEKAI_MQTT_ADDR` | **是** | — | broker 的 **IPv4:port**，例如 `192.168.1.10:1883`；本轮不支持 DNS 主机名。 |
| `SEKAI_MQTT_USERNAME` | 否 | 空 | MQTT 用户名。 |
| `SEKAI_MQTT_PASSWORD` | 否 | 空 | MQTT 密码（UTF-8 字节）。 |
| `SEKAI_DEVICE_ID` | 否 | `sekai-esp32` | Worker id（Hello/`state` 主题使用）；必须符合协议名称规则。 |
| `SEKAI_TOPIC_ROOT` | 否 | `sekai/v1` | MQTT 主题根。 |
| `SEKAI_DEVICES` | 否 | 空 | Jump 目标列表，格式 `name=IPv4:port[,name=IPv4:port]`，最多 8 项。 |

校验与失败行为（启动时，见 `Settings::from_env`）：

- `SEKAI_WIFI_SSID` 为空 → 打印 `[sekai] 配置错误：SEKAI_WIFI_SSID 未设置（必填，2.4GHz SSID）`，随后空转（**不 panic**）。
- `SEKAI_MQTT_ADDR` 为空或不是合法 IPv4:port → 打印对应 `配置错误`，随后空转。
- `SEKAI_WIFI_PASSWORD` 非空但非法（`Password::try_from` 失败）→ 同样打印配置错误并空转。
- `SEKAI_DEVICES` 非法 → 打印 `[sekai] SEKAI_DEVICES 非法：...`，使用空列表继续启动（Jump 无目标）。
- `SEKAI_DEVICE_ID`/`SEKAI_TOPIC_ROOT` 为空白 → 使用默认值。
- 固件标识为 `sekai-esp32/<workspace version>`（当前 `sekai-esp32/0.1.0`）。

> 编译期变量缺失不会让构建失败（方便 CI 只编译），但固件启动后只会打印配置错误并空转，
> 不会连接 WiFi/MQTT。排查“串口只有配置错误”时先检查这些变量是否在 `build` 前设置。

---

## 4. 启动顺序

`crate/sekai-embedded/src/driver/esp32/mod.rs` 的启动顺序（对应 `start_`）：

1. **堆分配器**：`esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 96 * 1024)` +
   `esp_alloc::heap_allocator!(size: 48 * 1024)`。
2. **硬件初始化**：`esp_hal::init(Config::default().with_cpu_clock(CpuClock::max()))`。
3. **调度器**：`esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0)`；必须在 WiFi 初始化之前完成。
4. **配置校验**：读取 `option_env!` 常量并校验（见 §3）；失败时打印原因并空转。
5. **boot_id**：用 `esp_hal::rng::Rng` 生成非零 `u64`（每次启动不同）。
6. **WiFi**：`WifiController::new` → `Interface::station()` → 设置 SSID/认证 → `connect_async()`
   （20s 超时）；失败只打印日志，后续主循环会重试。
7. **网络栈**：`embassy_net::new` + `StackResources::<4>`（DHCPv4），spawn `net_task` runner；
   等待 `wait_config_up()`（15s 超时，超时后主循环继续重试）。
8. **GPIO + 运行时**：`sekai_gpio::register_tools(&mut tools, Esp32Gpio::new())`，构造 `RuntimeConfig`
   （Worker id、固件版本、主题根、MQTT 凭据、LWT、Jump 目标）与 `Runtime`。
9. **MQTT 主循环**：`net::run_mqtt(runtime, stack, wifi, endpoint)`，见 §6。

启动时串口会打印（前缀 `[sekai]`）：

```text
[sekai] 正在连接 WiFi：SSID=my-ssid
[sekai] WiFi 已连接
[sekai] DHCP 已就绪：192.168.1.42/24
[sekai] MQTT TCP 已连接 192.168.1.10:1883
[sekai] Worker esp32-lab 启动
```

### 4.1 最小链路的限制

| 限制 | 说明 |
| --- | --- |
| 仅 Station + DHCPv4 | 不做静态 IP、不做 AP 模式。 |
| 仅 2.4GHz WiFi | 不支持 5GHz；不支持企业级 WiFi（EAP）。 |
| 仅 IPv4 字面量 broker | 无 DNS；`SEKAI_MQTT_ADDR` 必须是 `IPv4:port`。 |
| 明文 MQTT 5 over TCP | 无 TLS；broker 必须在可信 LAN 内。 |
| 单 TCP 连接 | 同时只维护一条 MQTT 链路；Jump 时先关旧链路再连新端点。 |
| 无 NVS/配网 | WiFi/MQTT 参数编译期固定，改参数必须重新构建固件。 |
| 无 BLE/AP | 本轮只做 WiFi Station。 |

### 4.2 堆与 profile

- 堆共 96 KiB（reclaimed）+ 48 KiB（普通）；TCP 收发缓冲各 4 KiB，读块 512 字节。
- 根 `Cargo.toml` 对 `esp-radio` 单独设置 `[profile.dev.package.esp-radio] opt-level = 3`：
  WiFi blob 在 debug 下也需要足够优化级别；debug 构建仍可用，但真正上板建议 release。
- 如果硬件上 WiFi 初始化失败/堆不足，优先增大 reclaimed 堆并打印错误（需要改代码后重新构建）。

---

## 5. UART0 与 esp-println

ESP32 classic 的 UART0 固定在 **IO1（TX）/ IO3（RX）**。固件用 `esp-println` 0.17 通过 UART0 打印诊断日志，
`espflash` 的串口烧录/监视也走 UART0。

- `gpio.info` 对 IO1/IO3 输出 `flags=uart0`，提示这两个引脚被 UART0 占用。
- **不要把 IO1/IO3 配置为输出**：会破坏日志输出，甚至影响烧录/启动时的串口握手。
- AI 选择引脚时应避开 `uart0` 标志；宿主 `sekai-worker`（PC Worker，原 `sekai-host`）的虚拟 ESP32 引脚表同样带该标志，便于提前验证。
- 如果板级必须把 IO1/IO3 作普通 IO，需要自行移除日志输出（修改固件代码）并接受失去串口诊断能力；
  本轮不提供“日志改到其他 UART”的配置。
- panic handler 会先打印 `[sekai] panic: ...`，然后进入忙等循环（不会自动重启）。

---

## 6. MQTT 主循环与重连退避

### 6.1 主循环结构

`net::run_mqtt` 的结构（简化）：

```text
loop {
    if !wifi.is_connected() { connect_async()（20s 超时，失败只打印） }
    等待 DHCP 地址（15s 超时，失败打印后继续）
    创建 4KB rx/tx 缓冲的 TcpSocket，连接 broker（5s 超时）
    连接成功：runtime.link_up(endpoint) → mqtt_session() → socket.close()
    连接失败：runtime.link_failed(endpoint)
    根据会话结果：link_lost() 或 link_closed()
    处理 take_link_action()（Jump/迁移/回退）→ 更新 endpoint
    Timer::after(backoff.next())
}
```

`mqtt_session` 的单轮步骤：

1. 把 `runtime.next_outgoing()` 的报文全部写出（每包 5s 写超时）；失败 → 会话 Lost。
2. 读 socket（20ms 超时）；读到字节则 `StreamDecoder` 拼包并逐包 `runtime.feed()`；
   EOF/解码错误 → Lost；读超时是正常情况，继续推进。
3. `runtime.poll()`（MQTT 定时器、收件箱、Task、发件箱、Jump）；失败 → Lost。
4. 若出现 `LinkAction`（Jump/Router 迁移）：先 flush 剩余报文，再关闭 socket → `Switch(action)`。
5. 按 `runtime.next_deadline()` 睡眠，上限 20ms、下限 1ms，保证 20ms 级 poll。

链路语义与宿主一致：正常关闭（Jump）→ `link_closed()`；EOF/读写错误/超时/运行时错误 → `link_lost()`；
连接失败 → `link_failed(endpoint)`。后两者可能产生 `LinkAction::Switch { endpoint, reason }`
（`reason` 为 `JumpCommand`/`ServerMoved`/`Fallback`），下一轮切到该 endpoint。

### 6.2 超时与退避

| 项 | 值 |
| --- | --- |
| WiFi 连接超时 | 20s |
| DHCP 等待超时 | 15s |
| TCP 连接超时 | 5s |
| 单次 socket 写入超时 | 5s |
| MQTT 读轮询间隔 | 20ms |
| 会话空闲睡眠 | 1ms..=20ms |
| MQTT Keep Alive / 会话过期 | 30s / 300s（协议 §2.4） |

**指数退避**：首次 500ms，之后每次翻倍，封顶 30s；连接成功或切换到新端点后重置为 500ms。
序列为 `500 → 1000 → 2000 → 4000 → 8000 → 16000 → 30000 → 30000 → ...` 毫秒。

- WiFi 断开、DHCP 未就绪、TCP 连接失败、MQTT 会话断开都会走到同一个退避等待。
- 退避只影响重连节奏，不清空 `Runtime` 状态；发件箱/最终事件缓存按运行时规则保留。
- 运行时的 `link_lost`/`link_failed` 后的回退动作会优先于退避执行，避免连接风暴。

### 6.3 Jump 与 Router 迁移

- Jump 只改 **TCP 目标端点**：WiFi 连接保持，不重新配网。
- 收到 `LinkAction::Switch` 时：先 flush 剩余报文（尽量发出 DISCONNECT）→ 关闭 socket → `link_closed()` →
  用新端点下一轮 `link_up()` → 在新 broker 上发布 Hello。
- 目标不可达/CONNACK 被拒时按运行时回退策略回到原端点，并上报 `Failed(JumpUnreachable)` 等最终事件
  （协议 §7.3）。
- 新一轮会话的 `boot_id`/`seq` 语义不变；Router 按 `(device_id, boot_id, seq)` 去重。

### 6.4 串口诊断行

固件所有诊断都以 `[sekai]` 为前缀（`esp_println::println!`）：

| 阶段 | 典型输出 |
| --- | --- |
| 配置 | `[sekai] 配置错误：...`、`[sekai] SEKAI_DEVICES 非法：...` |
| WiFi | `[sekai] 正在连接 WiFi：SSID=...`、`[sekai] WiFi 已连接`、`[sekai] WiFi 连接失败：...`、`[sekai] WiFi 连接超时`、`[sekai] WiFi 未连接，尝试重连` |
| DHCP | `[sekai] DHCP 已就绪：192.168.1.42/24`、`[sekai] DHCP 超时，稍后由 MQTT 主循环重试`、`[sekai] DHCP 地址仍未就绪` |
| TCP | `[sekai] MQTT TCP 已连接 ...`、`[sekai] TCP 连接 ... 失败：...`、`[sekai] TCP 连接 ... 超时` |
| 运行时/链路 | `[sekai] Worker ... 启动`、`[sekai] link_up 失败：...`、`[sekai] 切换链路到 ...`、`[sekai] 运行时推进失败：...` |
| 初始化失败 | `[sekai] WiFi 控制器初始化失败：...`、`[sekai] WiFi 配置失败：...`、`[sekai] 注册 GPIO 工具失败：...`、`[sekai] 运行时初始化失败：...`、`[sekai] 网络任务创建失败：...` |

---

## 7. 故障排查

| 现象 | 可能原因 | 处理 |
| --- | --- | --- |
| `sekai-build` 报工具链未安装 / 不认识 target | 没有安装 `esp` 工具链，或 `SEKAI_TOOLCHAIN` 指向错误工具链 | 按 §1 安装；`rustup toolchain list` 确认存在 `esp`；`rustup run esp rustc --print target-list` 确认 target。 |
| 构建报 `linkall.x` 找不到 | 没用 `sekai-build` 的链接参数，或 esp-hal 版本/构建缓存异常 | 用 `cargo run -p sekai-build -- build`；`cargo clean` 后重试。 |
| `espflash: command not found` | 未安装 espflash | `cargo install espflash`。 |
| 串口只有 `配置错误：SEKAI_WIFI_SSID 未设置` | 编译期变量没设置就构建了 | 设置 `SEKAI_WIFI_SSID` 后重新 `build`（不是只重跑 `flash`）。 |
| 串口只有 `配置错误：SEKAI_MQTT_ADDR ...` | 地址为空或不是 IPv4:port | 使用 `192.168.x.y:1883` 形式；本轮不支持 DNS 主机名。 |
| WiFi 一直连不上 | SSID/密码错误、AP 是 5GHz、信号弱、供电不足 | 确认 2.4GHz 与密码；靠近 AP；换稳定 5V 电源；看 `WiFi 连接失败/超时` 日志。 |
| WiFi 连上但 DHCP 超时 | 路由器 DHCP 池耗尽/隔离 | 检查 DHCP；`DHCP 超时` 后主循环会重试，不会退出。 |
| TCP 连接失败/超时 | broker 地址/端口错、防火墙、broker 未启动、WiFi 实际未联网 | 从同网段主机 `nc -vz <ip> 1883`；确认 broker 监听 0.0.0.0；看 `[sekai] TCP 连接 ...` 日志。 |
| TCP 连上但收不到 Hello | broker ACL/主题根不匹配、retain 不可用、MQTT 5 不兼容、CONNACK 被拒 | 订阅 `{root}/dev/{device_id}/state`；检查 `SEKAI_TOPIC_ROOT` 与 ACL；确认 broker 支持 MQTT 5。 |
| `link_up 失败` | CONNACK 拒绝（用户名/密码/client id/ACL） | 检查 `SEKAI_MQTT_USERNAME`/`SEKAI_MQTT_PASSWORD` 与 broker 配置。 |
| Worker 反复重连 | WiFi 信号差、broker 不稳定、认证失败、供电掉电 | 看退避日志与重启次数；检查 `boot_id` 是否变化（变化说明芯片重启）。 |
| 上电后反复重启/乱码 | 供电不足导致 brownout、波特率不对 | 换电源/加电容；串口 115200；必要时 `espflash --baud` 降速。 |
| `gpio.*` 返回“已保留/仅支持输入” | 选到了 SPI flash/调试口/仅输入引脚 | 这是安全默认值；用 `gpio.info` 重新选引脚，参考 `docs/sekai-gpio.md` §3。 |
| `运行时初始化失败：...工具...` | 工具数超过 16（用户工具 + 6 内置） | 减少用户工具；参考实现 ESP32 只注册 4 个 GPIO 工具（总数 10）。 |
| WiFi 初始化失败/堆不足 | `esp_alloc` 堆太小 | 增大 `#[ram(reclaimed)]` 堆大小并重新构建（见 §4.2）。 |
| 时间戳为 0 | Worker 未校准（无 RTC） | 正常；Router 连接后用 `sekai.set_time` 校准，或容忍 `timestamp=0`。 |
| Jump 后 Worker 回到原 broker | 目标不可达/被拒，运行时回退 | 检查目标 broker 与 `SEKAI_DEVICES`；这是预期回退，避免重试风暴（协议 §7.3）。 |

更多协议层故障（去重、Expired、Busy、Hello/Offline）见 `docs/sekai-protocol.md` §11。

---

## 8. 硬件验收清单

前置条件：一台 2.4GHz AP、一个 MQTT 5 broker（支持 retained）、一台 ESP32 classic 开发板、串口连接正常。

- [ ] `cargo run -p sekai-build -- build` 与 `--release` 均通过。
- [ ] 用 release 固件烧录后，串口依次看到 WiFi 连接、DHCP 地址、MQTT TCP 连接、`Worker ... 启动`。
- [ ] broker 收到 retained Hello（订阅 `state` 主题），`tools` 包含 `gpio.config/gpio.info/gpio.read/gpio.write`。
- [ ] 通过 MQTT 5 客户端发 `ToolCall gpio.info help`，返回的文本与 `docs/sekai-gpio.md` §5.2.4 逐字一致。
- [ ] 发 `gpio.config IO2 out level=0`，再 `gpio.write IO2 1`，用板载 LED 或逻辑分析仪确认电平变化；
      再 `gpio.write IO2 0` 确认可以翻转（IO2 只是示例，换成你板子的 LED 引脚）。
- [ ] 发 `gpio.config IO6 out` 应返回 `InvalidArgument`（保留引脚），且硬件不受影响。
- [ ] 断开 AP 或关闭 broker，串口出现重连/退避日志；恢复后 Worker 重新连接并发布新 Hello。
- [ ] 重启 Worker：`boot_id` 变化、retained Hello 更新、`seq` 从 1 重新开始。
- [ ] （可选）配置第二个 broker 与 `SEKAI_DEVICES`，发送 Jump，确认新 broker 收到 Hello 且旧链路关闭。
- [ ] （可选）断电模拟异常掉线，确认 broker 侧收到 retained Offline（LWT）。

> 注意：`gpio.config IO2 out level=0` 的 IO2 是 ESP32 的 strapping 引脚（启动模式相关）。
> 用真实板子验收时优先选板级 LED 所在引脚；若必须用 IO2，确认上电瞬间外部电路不会把它拉错。

---

## 附录 A：命令速查

```bash
# 工具链验证
rustup toolchain list
rustc +esp --version
rustup run esp rustc --print target-list | grep xtensa-esp32-none-elf

# 只编译（debug / release）
cargo run -p sekai-build -- build
cargo run -p sekai-build -- build --release

# 构建 + 烧录 + 串口监视
cargo run -p sekai-build -- flash --release --monitor -- --baud 115200

# 查看 usage（含编译期环境变量说明）
cargo run -p sekai-build -- help
```

环境变量配置后需要重新构建；`SEKAI_TOOLCHAIN` 可覆盖默认工具链名 `esp`。

## 附录 B：相关文档

- `docs/sekai-gpio.md`：GPIO 抽象、`gpio.*` 工具语法、AI 调用流程、host 虚拟后端与移植指南。
- `docs/sekai-protocol.md`：MQTT 5 主题、Stdin/Stdout 帧、错误码、Hello/Offline、Jump 与安全边界。
- `docs/sekai-gpio-plan.md`：本轮的原始实施计划（含设计背景）。

---
