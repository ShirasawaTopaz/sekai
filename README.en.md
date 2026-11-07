# Sekai

**A minimal control plane that turns natural-language goals into real machine operations.**

[中文](README.md) | English

Sekai is a control-plane framework written in Rust: a resident **Router**
uses an LLM (DeepSeek function tools) to evaluate and decompose natural-language
goals into Tasks / ToolCalls, dispatches them to one or more **Workers** (PC
processes or ESP32 firmware), and summarizes the results back into natural
language. The first end-to-end loop that works today: *"list the files in the
current directory" → Router → PC Worker's `pc.ls` → the listing actually comes
back* — the PC Worker is a **PC agent**: it executes local commands and file
operations out of the box.

```text
   Natural-language goal       Task / ToolCall (V1 binary frames)
You ─────────────▶ sekai-router ─────────────────────────▶ sekai-worker (PC Worker)
                    · LLM eval/decompose    ◀───────────    · Runtime executes
                    · Worker registry           Stdout       · pc.* tools (shell/files)
                    · Policy (read-only default)              │
                                                              │ embedded phase (deferred)
                              Same V1 frames over MQTT topics ▼
                                                    ESP32 firmware (WiFi · real GPIO)
```

- **Minimal protocol**: the control plane has exactly two directions — the Router
  sends Stdin frames, Workers reply with Stdout frames. The wire format is fully
  specified in [docs/sekai-protocol.md](docs/sekai-protocol.md) (V1, frozen).
- **Transport-agnostic**: the PC Worker uses a **direct link** (stdio subprocess
  or TCP, length-prefixed V1 frames, no broker); embedded Workers use MQTT
  topics (`{root}/dev/{id}/in|out|state`). Same frames, different carriers.
- **Multiple Workers**: the TCP listener keeps accepting; several direct
  Workers can be online at once and are routed by worker id (a new connection
  with the same id replaces the old one).
- **Cancellable / async**: serve `tool_call`/`task` accept `async:true` (returns
  `cmd_id` immediately, then a completion notification) together with
  `cancel {cmd_id}`; REPL `/cancel` and TUI `Ctrl-C` abort in-flight long tasks.
- **Read-only by default**: `pc.exec`/`pc.write` and other writes, Task dispatch,
  and admin tools are gated behind explicit switches; policy is enforced *before*
  the Router publishes a Stdin frame.
- **Prompt channel (out-of-band)**: the Worker's `pc.ask` task can hand a
  question to the LLM via the Router prompt ingress (direct, or relayed by the
  DSH plugin); JSONL out-of-band, V1 frames unchanged.
- **Built-in HTTPS**: the Router talks to the DeepSeek API with `native-tls`
  (Windows schannel / system OpenSSL) by default — no node or curl needed
  (`SEKAI_HTTPS_MODE=helper` falls back to the old helper).
- **`no_std` core**: `crate/sekai` depends only on `core` + `alloc`; the same
  protocol code compiles into host tests and ESP32 firmware alike.

## Repository layout

| Path | Form | Responsibility |
| --- | --- | --- |
| [crate/sekai](crate/sekai) | lib (no_std) | Core protocol library: V1 frame codec, Worker Runtime, DeviceList, clock abstraction, MQTT 5 client state machine. Zero IO dependencies, shared by every role. |
| [crate/sekai-router](crate/sekai-router) | resident process | Connects to / listens for Workers, maintains Worker lifecycle (Hello/Offline, boot_id/seq dedup), LLM evaluation & dispatch, aggregates answers; also serves the prompt ingress (`--prompt-addr`). |
| [crate/sekai-worker](crate/sekai-worker) | host process | PC Worker (**PC agent**): drives the Runtime for Tasks/ToolCalls, talks to the Router over a direct link (stdio/TCP), provides the `pc.*` shell/file capabilities and the `pc.ask` prompt channel; virtual GPIO is `--gpio` opt-in. |
| [crate/sekai-gpio](crate/sekai-gpio) | lib (no_std) | Family-agnostic GPIO abstraction + `gpio.info/config/read/write` user tools; ESP32/STM32F1/F4 pin tables, real backends, and a host virtual backend (embedded demo behind `--gpio`). |
| [crate/sekai-embedded](crate/sekai-embedded) | firmware (gated by `esp32` feature) | Embedded Worker bootstrap (ESP32): MQTT transport, hardware time, real GPIO. **Deferred phase.** |
| [crate/sekai-build](crate/sekai-build) | bin | Firmware build/flash orchestration (`build` / `flash`), serving the embedded Worker. |
| [plugins/sekai-dsh](plugins/sekai-dsh) | DSH plugin bundle | Bridges `sekai-router --serve` and `sekai-worker connect` into DeepSeek Harness so a DSH session drives Workers directly. |
| [docs/](docs/) | docs | Full documentation for protocol, architecture, GPIO, ESP32, and the DSH plugin — see [doc navigation](#documentation). |

## Quick start

### Prerequisites

- Rust **1.88+** (the workspace uses edition 2024; the code relies on let-chains, stable since 1.88); `cargo` on PATH.
- `DEEPSEEK_API_KEY`: required whenever the LLM is actually invoked (validated
  at startup in batch mode; the interactive REPL only needs it once you type a
  goal; `pc.ask` and the prompt ingress need it too; `--dry-run` and link-only
  `--serve` calls do not).
- No hardware required: the PC Worker is a **PC agent** — it executes local
  commands and file operations out of the box; virtual GPIO is optional via
  `--gpio esp32|stm32f1|stm32f4` (embedded demo).

### Build

```bash
cargo build --release -p sekai-router -p sekai-worker
# Artifacts: target/release/sekai-router(.exe) and target/release/sekai-worker(.exe)
```

### Run the loop in two terminals

```bash
# Terminal 1: start the Router (interactive REPL)
export DEEPSEEK_API_KEY=sk-...
./target/release/sekai-router

sekai> /start        # guided link wizard: pick 2 (tcp), Enter for default 127.0.0.1:7421
                    # the wizard prints a copy-paste command for the other side

# Terminal 2: a Worker connects in
./target/release/sekai-worker connect 127.0.0.1:7421

# Terminal 1 notifies asynchronously: Worker online: pc-worker (tools=… tasks=…)
sekai> show me this machine's system info   # → pc.info (read-only, allowed by default)
sekai> list the files in the current dir    # → pc.ls (read-only, allowed by default)
```

Going further (unlock as needed via `--allow-write`/`--allow-tasks` startup flags
or REPL `/policy`):

```bash
sekai> run cargo --version          # → pc.exec (needs allow-write; shell execution)
sekai> ask the LLM: what is Rust's Deref?   # → pc.ask (needs allow-tasks; answered
                                    #   via the Router prompt ingress, recursion-guarded)
```

The full `pc.*` tool/task table and env knobs (`SEKAI_PC_EXEC_MS` /
`SEKAI_PC_ASK_MS`) live in [docs/sekai-pc-agent.md](docs/sekai-pc-agent.md).

REPL commands: `/help`, `/link <mqtt|stdio|tcp>` (build/switch the link),
`/workers` (inspect the registry), `/wait` (wait for Workers), `/policy`
(view/adjust policy), `/exit`. Any input not starting with `/` is treated as a
natural-language goal; you can issue multiple goals in one session (no
cross-goal context is retained).

### Other run modes

| Mode | Command | Notes |
| --- | --- | --- |
| Batch | `sekai-router --link tcp --goal "list files in the current directory"` | One goal then exit, scriptable; `--json` prints a single JSON result |
| Spawn subprocess | `sekai-router --link stdio` (or `/link stdio` in REPL) | Router spawns `sekai-worker`; stdin/stdout are the link — single-process demo |
| Full-screen TUI | `sekai-router --tui` | ratatui UI: status bar + scrolling output + input line; falls back to line REPL when not a TTY |
| Headless service | `sekai-router --serve` | JSONL over stdio (link operations need no LLM/API key; the `prompt` method invokes the built-in LLM loop) for the [DSH plugin](plugins/sekai-dsh) |
| Feishu chat entry | `sekai-router --feishu` | Receives IM events over a WebSocket long connection and replies via REST; injects per-chat session metadata and the last N turns (in-memory, `SEKAI_FEISHU_CONTEXT_TURNS`, default 8). Needs `SEKAI_FEISHU_APP_ID`/`_APP_SECRET`; link defaults to tcp; see [docs/sekai-feishu.md](docs/sekai-feishu.md) and the [setup skill](skills/feishu-setup/SKILL.md) |
| MQTT link | `SEKAI_MQTT_ADDR=127.0.0.1:1883 sekai-router --goal "…"` | Embedded-style link (default `--link mqtt`); needs an MQTT 5 broker |
| Link check only | `sekai-router --dry-run` | Connect and list Workers without calling the LLM |

## Environment variables (common)

| Variable | Meaning | Default |
| --- | --- | --- |
| `DEEPSEEK_API_KEY` (or `API_KEY`) | DeepSeek API key | none (required for LLM) |
| `DEEPSEEK_BASE_URL` (or `BASE_URL`) | OpenAI-compatible base URL | `https://api.deepseek.com` |
| `SEKAI_ROUTER_MODEL` | Model name | `deepseek-flash` |
| `SEKAI_ROUTER_REASONING_EFFORT` | Thinking effort: enables thinking and sends `reasoning_effort` (`minimal`/`low`/`medium`/`high`/`xhigh`/`max`/`ultra`; thinking stays off when unset) | unset (off) |
| `SEKAI_LINK` | Link type `mqtt`/`stdio`/`tcp` | `mqtt` |
| `SEKAI_LISTEN_ADDR` | TCP listen address | `127.0.0.1:7421` |
| `SEKAI_PROMPT_ADDR` | Router prompt ingress address (`HOST:PORT`/`off`; interactive/serve default `127.0.0.1:7422`, batch default `off`) | see left |
| `SEKAI_INGRESS_TOKEN` | prompt ingress auth token (required for remote peers, optional on loopback) | none |
| `SEKAI_INGRESS_ALLOW_REMOTE` | Allow ingress to bind a non-loopback address (`1` to opt in) | off |
| `SEKAI_HTTPS_MODE` | HTTPS backend: `native` (in-process TLS, default) / `helper` (node/curl fallback) | `native` |
| `SEKAI_FEISHU_APP_ID` / `SEKAI_FEISHU_APP_SECRET` / `SEKAI_FEISHU_ALLOW_USERS` / `SEKAI_FEISHU_ALLOW_CHATS` / `SEKAI_FEISHU_CONTEXT_TURNS` | Feishu entry credentials, allowlist and context turns (default deny, no `*`; credentials required by `--feishu`; `CONTEXT_TURNS` defaults to 8, 0 disables multi-turn memory; other `SEKAI_FEISHU_*` in [docs/sekai-feishu.md](docs/sekai-feishu.md) §8) | none / empty / 8 |
| `SEKAI_PC_EXEC_MS` | Worker `pc.exec` internal timeout (100..=300000) | `10000` |
| `SEKAI_PC_ASK_MS` | Worker `pc.ask` internal hard timeout (1000..=300000) | `300000` |
| `SEKAI_MQTT_ADDR` | MQTT broker address (mqtt link) | none |
| `SEKAI_WORKER_BIN` | Worker binary for the stdio link | `sekai-worker` |
| `SEKAI_TOPIC_ROOT` | MQTT topic root | `sekai/v1` |
| `SEKAI_MAX_STEPS` / `SEKAI_DDL_MS` / `SEKAI_TIMEOUT_MS` / `SEKAI_WAIT_WORKER_MS` | agent steps / DDL / command timeout / wait-for-worker (steps default: unlimited on a direct PC Worker link, `12` over MQTT/embedded) | see left / 10000ms / — / 5000ms |
| `SEKAI_ROUTER_TRACE` | Redacted JSONL trace path | off |

Full list in `sekai-router --help`.

## Security model

- **Read-only by default**: `pc.exec` / `pc.write` and GPIO writes
  (`gpio.config`/`gpio.write`), Task dispatch (`pc.job` / `pc.ask` / `echo` /
  `sleep`), and admin/other tools require `--allow-write` / `--allow-tasks` /
  `--allow-admin` (or REPL `/policy`, or serve-side `policy.set`) to unlock;
  `pc.read` / `pc.ls` / `pc.info` are read-only whitelisted and allowed by
  default.
- **Denied means not published**: policy is checked before the Router frames and
  publishes a Stdin; rejected commands never reach the Worker. `pc.exec` /
  `pc.job` run arbitrary shell commands on the Worker host — the largest
  permission surface; unlock only in controlled environments.
- **Prompt recursion guard**: any LLM run arriving via the prompt ingress or the
  serve `prompt` method refuses to dispatch `pc.ask` again
  (`policy_denied`, "nested prompt (pc.ask)"), regardless of switches.
- **`--worker <ID>`** (or `policy.worker_filter`): locks operations to a single
  Worker.
- Credentials are read only from environment variables or explicit CLI
  arguments, redacted in Debug output, never written to disk.

## Documentation

All topic docs are written in Chinese; the code, CLI output, and examples are
self-explanatory.

| Doc | Contents | Audience |
| --- | --- | --- |
| [docs/sekai-architecture.md](docs/sekai-architecture.md) | Router/Worker architecture, transport matrix, PC agent & prompt channel, phase plan | Every contributor — **read this first** |
| [docs/sekai-gap-audit.md](docs/sekai-gap-audit.md) | Gap audit: what is unimplemented / not closed-loop, with evidence, priorities and suggested work packages | Anyone scoping, sequencing or accepting work |
| [docs/sekai-protocol.md](docs/sekai-protocol.md) | V1 control-plane protocol: frames, topics, QoS/retain, Hello, error codes | Protocol implementers, link debuggers |
| [docs/sekai-pc-agent.md](docs/sekai-pc-agent.md) | PC agent: full `pc.*` tool/task table, prompt JSONL protocol, three-party config, security | PC Worker users, plugin/client implementers |
| [docs/sekai-gpio.md](docs/sekai-gpio.md) | GPIO abstraction, pin tables, `gpio.*` tool syntax & AI call flow (embedded demo behind `--gpio`) | Board-level devs, GPIO integrators |
| [docs/sekai-esp32.md](docs/sekai-esp32.md) | ESP32 firmware: toolchain, build & flash, WiFi/MQTT link, troubleshooting | Firmware devs |
| [docs/sekai-dsh-plugin.md](docs/sekai-dsh-plugin.md) | Sekai × DSH plugin topology and the serve protocol at a glance | DSH users, plugin implementers |
| [docs/sekai-feishu.md](docs/sekai-feishu.md) | Feishu (Lark) chat entry `--feishu`: long-connection protocol, message loop, security model, setup skill | Feishu integrators, Router implementers |
| [plugins/sekai-dsh/README.md](plugins/sekai-dsh/README.md) | Full DSH plugin configuration (install, config keys, JSONL protocol) | Plugin users |
| [docs/sekai-ai.md](docs/sekai-ai.md) | Historical: AI control-plane design record (now realized by the Router) | Evolution history |
| [docs/sekai-ai-plan.md](docs/sekai-ai-plan.md) / [docs/sekai-gpio-plan.md](docs/sekai-gpio-plan.md) | Historical plan documents | As needed |

Per-crate intros live in each crate directory
([sekai](crate/sekai), [sekai-router](crate/sekai-router),
[sekai-worker](crate/sekai-worker), [sekai-gpio](crate/sekai-gpio),
[sekai-embedded](crate/sekai-embedded), [sekai-build](crate/sekai-build)).

## Project status

The **PC Worker** is the current mainline; the **embedded Worker (ESP32)** is
deferred (protocol and code paths are kept, but it does not block the
mainline). Phase progress (details in
[docs/sekai-architecture.md](docs/sekai-architecture.md) §3):

| Phase | Scope | Status |
| --- | --- | --- |
| P0–P1.7 | Renaming, direct links (stdio/TCP), REPL/TUI, connection wizard | Done |
| P2 | LLM eval/dispatch loop over direct Workers; PC-agent shape (`pc.*` capabilities, `--gpio` opt-in, prompt channel) end-to-end demos | **Single-Worker loop done; remaining items in the [gap audit](docs/sekai-gap-audit.md)** |
| P3 | Embedded Worker: esp32 firmware build, MQTT link regression | Deferred |

P2's exit criteria and remaining checklist live in
[docs/sekai-gap-audit.md](docs/sekai-gap-audit.md) §1/§9; P2 is not declared
complete until B1–B11 there are closed.

## Development

```bash
cargo test --workspace        # full host test suite (no hardware, no network, no broker)
cargo build --workspace       # builds everything (skips the esp32-gated firmware image)
```

- Embedded firmware builds (Xtensa toolchain required): see
  [crate/sekai-embedded/README.md](crate/sekai-embedded/README.md) and
  [docs/sekai-esp32.md](docs/sekai-esp32.md).
- DSH plugin local smoke (no Rust required): see
  [plugins/sekai-dsh/README.md](plugins/sekai-dsh/README.md).

## License

Apache License 2.0 — see [LICENSE](LICENSE). Each crate inherits
`license = "Apache-2.0"` from the workspace manifest; the DSH plugin declares
it in its `package.json`.
