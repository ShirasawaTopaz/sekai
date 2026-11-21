# 实板 GPIO 回环

## 接线

断电接线。用约 **1 kΩ** 电阻连接表中的输出脚和输入脚，验收时移除外部负载。

| 板卡 | Router 链路 | 回环输出 → 输入 | 串口接线 |
| --- | --- | --- | --- |
| ESP32-DevKitC V4 | MQTT | `IO25` → `IO26` | 使用板上原有 Wi-Fi/MQTT 固件 |
| STM32F103C8 Blue Pill | TCP → UART | `PB0` → `PB1` | USART1，`PA9` TX / `PA10` RX |
| STM32F407VG Discovery | TCP → UART | `PB0` → `PB1` | USART2，`PA2` TX / `PA3` RX |

STM32 使用 **3.3 V TTL USB 转串口，115200 baud、8N1、无流控**。板卡 TX 接转接器 RX，板卡 RX 接转接器 TX，两端 GND 共地。STM32 固件只向 GPIO 工具开放 `PB0` 和 `PB1`，启动及 UART 会话关闭时将两脚切为输入。首次上板时用万用表确认输出高、低电平。

## 构建与烧录

从仓库根目录执行以下命令。镜像使用 `stm32` release profile。使用 SWD 下载，下载后复位板卡。ESP32 的构建、Wi-Fi、MQTT 和烧录步骤见 [ESP32 文档](sekai-esp32.md)。

```sh
rustup target add thumbv7m-none-eabi thumbv7em-none-eabi
cargo build --locked -p sekai-stm32 --profile stm32 --target thumbv7m-none-eabi --features blue-pill
cargo build --locked -p sekai-stm32 --profile stm32 --target thumbv7em-none-eabi --features discovery

probe-rs download --chip STM32F103C8 --protocol swd target/thumbv7m-none-eabi/stm32/sekai-stm32
probe-rs download --chip STM32F407VG --protocol swd target/thumbv7em-none-eabi/stm32/sekai-stm32

cargo build --locked -p sekai-router
cargo build --locked -p sekai-worker --features uart-bridge --bin stm32-bridge
```

STM32 默认 Worker ID 为 `stm32-blue-pill` 或 `stm32-discovery`。多块同型号板可通过编译环境变量 `SEKAI_STM32_WORKER_ID` 分配唯一 ID。ESP32 使用固件配置 `SEKAI_DEVICE_ID`，其默认值为 `sekai-esp32`。

## 自动验收

使用 Node.js 22 或更新版本。下面给出 Windows 命令。Linux 将可执行文件改为 `./target/debug/sekai-router` 和 `./target/debug/stm32-bridge`，串口使用实际设备名，例如 `/dev/ttyUSB0`。

```powershell
node scripts/gpio-loopback.mjs --board esp32 --worker sekai-esp32 --broker 192.168.1.10:1883 --router .\target\debug\sekai-router.exe
node scripts/gpio-loopback.mjs --board blue-pill --worker stm32-blue-pill --serial COM5 --router .\target\debug\sekai-router.exe --bridge .\target\debug\stm32-bridge.exe
node scripts/gpio-loopback.mjs --board discovery --worker stm32-discovery --serial COM6 --router .\target\debug\sekai-router.exe --bridge .\target\debug\stm32-bridge.exe
```

脚本启动 Router `--serve`，只调用 `link`、`workers` 和 `tool_call`，无需 LLM 或 API key。ESP32 要求 Router 和固件使用同一个 broker 及主题根。STM32 模式自动启动本机临时 TCP 监听和串口桥。桥也可独立启动，第二个参数为 Router TCP 地址，省略时使用 `127.0.0.1:8765`。

```powershell
.\target\debug\stm32-bridge.exe COM5 127.0.0.1:8765
```

脚本核对指定 Worker 的 Hello、芯片、`backend=hardware` 及两脚能力后才配置 GPIO。每步输出 JSON，记录期望、回读及命令结果。成功时 `gpio.read` 依次读出 `0 → 1 → 0` 并确认清理。超时、结果错误、掉线或 Worker 实例改变均返回非零退出码。清理标记为 `unconfirmed` 时，在板上确认输出电平。每次运行重新配置两脚。

STM32 使用共享的 `no_std` 直连代码与 GPIO 工具注册表，固件以内存有界的分派器执行即时 GPIO 调用。UART 用 COBS 分帧，校验长度和 CRC-16。每次 TCP 连接产生新的随机 `boot_id`，一个 UART 工具帧处理完毕后再接收下一帧。桥每秒发一次心跳，三秒收不到响应就关闭 TCP 并重连。会话关停后释放 GPIO。旧会话的流缓冲随连接丢弃。

## 验收记录

### 交叉构建

| 板卡 | 结果 | Flash 装载量 | 静态 RAM | 栈空间 |
| --- | --- | ---: | ---: | ---: |
| STM32F103C8 Blue Pill | release ELF 链接通过 | 35,080 B / 64 KiB | 14,588 B / 20 KiB | 5,892 B，链接至少保留 4 KiB |
| STM32F407VG Discovery | release ELF 链接通过 | 36,788 B | 14,588 B | 链接至少保留 4 KiB |
| ESP32-DevKitC V4 | `sekai-build build --release` 通过 | | | |

STM32 静态 RAM 包含 10 KiB 堆。宿主内存测试输入满长的 V1 GPIO 请求并执行 2,000 次连续写调用。满长请求由现有 GPIO 参数校验拒绝，测试记录的峰值为 5,656 B。

### 模拟集成

- Router TCP、串口桥与 STM32 Worker 的完整请求往返通过，覆盖分片、心跳超时重连、旧会话丢弃和禁用引脚拒绝。
- ESP32 模式使用实际 Router `--serve` 和本机模拟 MQTT broker，回读 `0 → 1 → 0` 通过。
- 虚拟后端拒绝、错误电平、超时、实例切换和清理分支通过测试。既有 Router GPIO 环回测试全部通过。

### 实板

| 板卡 | 固件版本 | 0 → 1 → 0 | 重复运行 | 断线重连 | 万用表 | 新 Hello 与旧写命令检查 |
| --- | --- | --- | --- | --- | --- | --- |
| ESP32-DevKitC V4 | 待测 | 待测 | 待测 | 待测 | 待测 | — |
| STM32F103C8 Blue Pill | 待测 | 待测 | 待测 | 待测 | 待测 | 待测 |
| STM32F407VG Discovery | 待测 | 待测 | 待测 | 待测 | 待测 | 待测 |

STM32 的断线验收需分别拔插 USB 转串口和复位板卡。用独立桥接命令保持 Router 监听，在 `workers` 中确认新 Hello 的 `boot_id_hex` 已变化，并确认输出脚保持输入，直到重新配置。随后重新运行回环验证。
