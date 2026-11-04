# sekai-embedded

Sekai 嵌入式 Worker 启动层（ESP32 固件）：MQTT 传输、硬件时间与真实 GPIO。

**优先级说明**：当前阶段 PC Worker 优先，嵌入式置后。宿主（默认 feature）构建只编译
共享的 no_std 兼容模块（`config` / `clock` / `framing` / `time`），不拉起任何 esp 依赖
（`cargo tree -p sekai-embedded` 默认只有 `sekai`）。固件镜像 `sekai-fw` 由
`crate/sekai-build` 的 `esp32` feature 门控，`cargo build --workspace` 会完全跳过它。

## 当前状态与恢复步骤（esp 工具链，2026-11-03 诊断）

### 结论

`~/.rustup/toolchains/esp` 是**真实可用的 esp-rs fork 工具链**，本仓库的固件构建路径
当前完全可用（离线）：

- rustc `1.97.0-nightly (8ea53bcd7 2026-07-08) (1.97.0.0)`（esp fork），
  `rustup run esp rustc --version` 正常；
- 组件齐备：rust-src（`lib/rustlib/src/rust/library/` 完整）、GCC `xtensa-esp-elf`、
  clang `xtensa-esp32-elf-clang`、`~/export-esp.ps1`；
- `espup 0.19` 的默认工具链名就是 `esp`（`espup install -a/--name` 缺省值），
  因此这个名字本身就是规范形态，不需要 `rustup toolchain link` 改名。

验证记录：`cargo run -p sekai-build -- build` 成功产出
`target/xtensa-esp32-none-elf/debug/sekai-fw`（2026-11-03，54.7s）；
`rustup run esp cargo build --target xtensa-esp32-none-elf -Zbuild-std=core` 冒烟通过。

### 已知缺口（不阻塞本仓库）

`lib/rustlib/` 下**没有**任何 xtensa 目标的预编译 rust-std（`xtensa-esp32-none-elf`
等目录不存在，只有宿主 `x86_64-pc-windows-msvc` 的）。这是否为 espup 安装残缺不确定；
但本仓库不受影响——`sekai-build/src/main.rs` 按设计用
`-Zbuild-std=core,alloc` 从 rust-src 现场构建 core/alloc（Xtensa 本就没有官方
预编译 std 分发渠道）。

### 为什么 `rustup target add --toolchain esp xtensa-esp32-none-elf` 报错（死路，勿再走）

1. rustup ≥1.28（本机 1.29.2）对 `--toolchain` 参数值做了 CLI 校验，拒绝 `esp`
   这类非 channel 名 → `error: invalid value 'esp' ... invalid toolchain name: 'esp'`。
   这是参数名被拒，不是工具链不存在；
2. 即便换个 rustup 认可的名字，xtensa rust-std 也只存在于 esp-rs/rust-build 的
   GitHub releases，不在 rustup dist 服务器上（本机 rsproxy.cn 镜像只镜像官方 dist），
   `rustup target add` 永远装不上它；
3. 不需要它：见上节，`-Zbuild-std` 是正解且已在用。

### 在线修复（当前被网络阻断，留作网络恢复后的步骤）

`espup update`（更新 Xtensa 工具链）需要访问
`api.github.com/repos/esp-rs/rust-build/releases/latest`，本机 GitHub 直连全部超时
（TCP connect 失败，os error 10060；github.com / raw.githubusercontent.com 均不可达），
故无法在线重装/补件。网络恢复（或配置代理）后：

```powershell
espup update                 # 更新到最新 esp fork（默认名仍是 esp）
# 若工具链彻底损坏，则重装：
espup uninstall
espup install -t esp32
```

`espup` 在 PATH 上（`~/.cargo/bin/espup`，v0.19.0）。esp32 feature 的全部依赖
（esp-hal 1.2.2、esp-radio 1.0.0-beta.1 等）已在本地 cargo 缓存，固件构建无需网络。

## 构建与烧录

```text
cargo run -p sekai-build -- build [--release]     # 编译固件（推荐 --release）
cargo run -p sekai-build -- flash [--release] [--monitor]
```

固件编译期环境变量（`SEKAI_WIFI_SSID`、`SEKAI_MQTT_ADDR` 必填）见
`cargo run -p sekai-build -- help`；详见 [docs/sekai-esp32.md](../../docs/sekai-esp32.md)。
