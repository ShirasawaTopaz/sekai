/**
 * worker.js — Sekai Worker 插件：spawn `sekai-worker connect <addr>` 并
 * 管理其生命周期，发布 sekai_worker_status 状态工具。
 *
 * worker 的 connect 模式不参与 Router 的 JSONL 协议：stderr 是阶段化中文
 * 进度行——因此这里不走 JsonlProcess，只做 spawn + stderr 环形缓冲 +
 * 退避重启。
 *
 * PC agent 形态（config 键 `llm`）：
 * - `'dsh'`（默认）：worker 的 pc.ask prompt 经本插件中转——worker stdout
 *   上形如 {"id":n,"method":"prompt","params":{...}} 的行被转发到
 *   `promptAddr`（Router 的 prompt ingress，JSONL over TCP），响应按 id
 *   匹配后原样写回 worker stdin；其余 stdout 行仍只记日志。
 * - `'router'`：worker 自己连 ingress（spawn 时传 `--prompt-addr`），
 *   插件不中转。
 * - `'off'`：不传 prompt 目标，stdout 的 JSON 行只记日志。
 *
 * spawn 参数：`connect <routerAddr> --worker-id <id> --llm <channel>`
 * （router 模式加 `--prompt-addr <addr>`）；虚拟 GPIO 默认关闭，仅
 * `gpio`（兼容旧键 `family`）非空时加 `--gpio <family>`。
 *
 * worker→DSH 工具桥（config 键 `dshTools`，默认关闭）：
 * - `enabled: true` 时 spawn 追加 `--dsh-tools`，worker 的 `dsh.call` /
 *   `dsh.tools` Task 变成 stdout JSONL 请求 `dsh_tool` / `dsh_tools`；
 *   取消是 `dsh_cancel` 通知（`{"method":"dsh_cancel","params":{"id":N}}`）。
 * - 插件按 `allow` / `deny` 白名单过滤（默认只读 read/glob/grep），硬拒绝
 *   `sekai_worker_` 前缀（防递归回到本插件）与 `run_code`（任意代码）。
 * - 工具经懒创建的 bridge agent（不是主 agent——ops-ptc 模式会拒绝直接
 *   程序化调用）用 `ctx.tools.execute` 执行，并发由 maxConcurrent 串行化，
 *   每条调用独立超时/AbortSignal。
 * - 回包信封：`{"id":N,"ok":true,"result":{...}}` / `{"id":N,"ok":false,
 *   "error":{"code":...}}`；工具自身失败是 ok:true + `result.is_error:true`。
 *
 * 自动连接开关（config 键 `autoConnect`，默认 true）：
 * - `true`：插件加载（DSH 启动）且 `enabled` 时，立即 spawn worker 连接
 *   **上次成功连接的 Router**（持久化在 `$DSH_HOME/sekai/worker-state.json`，
 *   按 workerId 隔离；无记录时回退 config 的 `routerAddr`）；
 * - `false`：不自动连接；由 `sekai_worker_connect` 工具按需连接
 *   （省略地址时同样优先上次连接的 Router），`sekai_worker_disconnect` 断开。
 *
 * 「上次连接的 Router」来源：worker stderr 的稳定进度行——`TCP 已连接 <addr>`
 * 与 `切换到新端点 <addr>`（jump 迁移）；识别成功即更新状态文件（与当前值
 * 相同则跳过写盘）。写盘异步、原子（tmp + rename），失败只 warn 不影响链路。
 *
 * 意外退出按 500ms 起步指数退避重启（封顶 restartBackoffMaxMs）；稳定
 * 运行（≥5s）后的退出把退避复位。手动 `disconnect` 后不再自动重启，
 * 直到再次 `connect`。dispose 直接 kill（无优雅协议），并清理挂起的
 * prompt 中转（回 error aborted）。
 *
 * 不声明 static inject：宿主没有 tools 服务也能加载本插件，工具仅在
 * 注册表可用时发布（缺 registry 只 warn 不崩）；状态工具由
 * `publishStatusTool`、控制工具由 `publishControlTools` 分别控制。
 *
 * @module @sekai/dsh-plugin/worker
 */
import { spawn } from 'node:child_process'
import { randomUUID } from 'node:crypto'
import fs from 'node:fs'
import { mkdir, rename, writeFile } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import readline from 'node:readline'
import net from 'node:net'
import { createRequire } from 'node:module'
import {
  RESTART_BACKOFF_INITIAL_MS,
  RingBuffer,
  describeError,
  describeExitInfo,
  nextBackoffMs,
  renderJson,
  resolveBin,
  resolveDshHome,
} from './shared.js'

const require = createRequire(import.meta.url)

/** 与 router.js 相同的 peer 加载策略：宿主内可用，仓库冒烟回退默认。 */
function loadSchemastery() {
  try {
    return require('@deepseek-ai/schemastery')
  } catch {
    return null
  }
}
const z = loadSchemastery()

/**
 * worker→DSH 工具桥（config 键 `dshTools`）默认值：默认关闭，白名单默认
 * 只读（read/glob/grep）。`cwd: ''` 表示用主会话 cwd；`agentPreset: ''`
 * 表示创建 bridge agent 时不带 preset（默认 'standard'）。
 */
export const DSH_TOOLS_DEFAULTS = Object.freeze({
  enabled: false,
  allow: Object.freeze(['read', 'glob', 'grep']),
  deny: Object.freeze([]),
  cwd: '',
  agentPreset: 'standard',
  maxConcurrent: 1,
  timeoutMs: 300000,
  maxResponseBytes: 65536,
})

/** 内部默认值（static Config 的镜像，冒烟环境的权威来源）。 */
export const WORKER_DEFAULTS = Object.freeze({
  enabled: true,
  workerBin: 'sekai-worker',
  routerAddr: '127.0.0.1:7421',
  workerId: 'pc-worker',
  llm: 'dsh',
  promptAddr: '127.0.0.1:7422',
  promptTimeoutMs: 300000,
  gpio: null,
  // 开启 DSH 且 worker 启用时，是否自动连接上次连接的 Router。
  autoConnect: true,
  // 是否发布 sekai_worker_connect / sekai_worker_disconnect 手动控制工具。
  publishControlTools: true,
  // 上次连接 Router 的持久化状态文件；'' = $DSH_HOME/sekai/worker-state.json。
  stateFile: '',
  restartBackoffMaxMs: 30000,
  publishStatusTool: true,
  // worker→DSH 工具桥（默认关闭，白名单默认只读）。
  dshTools: DSH_TOOLS_DEFAULTS,
})

/** LLM prompt 通道取值（与 worker `--llm` 旗标一致）。 */
export const LLM_CHANNELS = Object.freeze(['dsh', 'router', 'off'])

/** 稳定运行阈值：存活超过该时长的进程退出后，退避复位。 */
const STABLE_UPTIME_MS = 5000

/** stderr 环形缓冲容量（任务卡约定 ~40 行）。 */
const STDERR_RING_SIZE = 40

/** 状态文件格式版本（未来做兼容迁移用）。 */
const STATE_FILE_VERSION = 1

/** dshTools 桥的数值范围（与 static Config 的 min/max 保持一致）。 */
export const DSH_TOOLS_LIMITS = Object.freeze({
  maxConcurrent: Object.freeze({ min: 1, max: 8 }),
  timeoutMs: Object.freeze({ min: 1000, max: 600000 }),
  maxResponseBytes: Object.freeze({ min: 1024, max: 262144 }),
})

/** 硬拒绝：`sekai_worker_` 前缀防递归回本插件，`run_code` 防任意代码。 */
const DSH_TOOL_DENY_PREFIX = 'sekai_worker_'
const DSH_TOOL_HARD_DENY = Object.freeze(['run_code'])

/**
 * worker stderr 的「TCP 已连接 <addr>」行（`connected_line`，Rust 侧单测
 * 冻结文案）。连接成功即视为「连上了这个 Router」。
 */
const CONNECTED_LINE_RE = /^sekai-worker: TCP 已连接 (?<addr>\S+)$/

/**
 * worker stderr 的「切换到新端点 <addr>」行（jump 迁移后的新 Router）。
 * @type {RegExp}
 */
const SWITCH_LINE_RE = /^sekai-worker: 切换到新端点 (?<addr>\S+)$/

/** 空入参工具 schema。 */
const NO_ARGS_PARAMETERS = Object.freeze({
  type: 'object',
  properties: {},
  additionalProperties: false,
})

/** 控制工具（connect/disconnect）的输出契约：成功值与 error 信封都透传。 */
const CONTROL_OUTPUT = Object.freeze({
  schema: {
    type: 'object',
    additionalProperties: true,
    properties: {
      started: { type: 'boolean', description: 'Whether a new worker process was spawned by this call.' },
      running: { type: 'boolean', description: 'Whether the worker process is alive after this call.' },
      disconnected: { type: 'boolean', description: 'Whether an existing worker process was asked to stop.' },
      routerAddr: { type: 'string', description: 'Router address HOST:PORT the worker is (re)connecting to.' },
      lastRouterAddr: { type: 'string', description: 'Last successfully connected router address known from the state file.' },
      error: {
        type: 'object',
        description: 'Structured failure envelope {code, message, hint?}.',
        properties: {
          code: { type: 'string' },
          message: { type: 'string' },
          hint: { type: 'string' },
        },
        additionalProperties: true,
      },
    },
  },
  render: renderJson,
})

/**
 * 解析 `HOST:PORT`（兼容 `[IPv6]:PORT`）；非法返回 null。
 * @param {string} addr
 * @returns {{host: string, port: number} | null}
 */
export function splitHostPort(addr) {
  const value = typeof addr === 'string' ? addr.trim() : ''
  if (value === '') return null
  if (value.startsWith('[')) {
    const close = value.indexOf(']')
    if (close === -1) return null
    const host = value.slice(1, close)
    const rest = value.slice(close + 1)
    if (!rest.startsWith(':')) return null
    const port = Number(rest.slice(1))
    if (!Number.isInteger(port) || port <= 0 || port > 65535 || host === '') return null
    return { host, port }
  }
  const idx = value.lastIndexOf(':')
  if (idx === -1 || idx === 0 || idx === value.length - 1) return null
  const port = Number(value.slice(idx + 1))
  if (!Number.isInteger(port) || port <= 0 || port > 65535) return null
  return { host: value.slice(0, idx), port }
}

/**
 * 识别 worker stdout 上的 prompt 请求行：能 JSON.parse 且形如
 * {"id":<int>,"method":"prompt","params":{...}}；其余（含解析失败）返回
 * null（维持「只记日志」的现状）。返回原行文本（转发时原样发出）。
 * @param {string} line
 * @returns {{id: number, raw: string} | null}
 */
export function parsePromptRequest(line) {
  const text = line.trim()
  if (!text.startsWith('{')) return null
  let msg = null
  try {
    msg = JSON.parse(text)
  } catch {
    return null
  }
  if (msg === null || typeof msg !== 'object' || Array.isArray(msg)) return null
  if (!Number.isInteger(msg.id)) return null
  if (msg.method !== 'prompt') return null
  if (msg.params === null || typeof msg.params !== 'object' || Array.isArray(msg.params)) return null
  return { id: msg.id, raw: text }
}

/**
 * 识别 worker stdout 上的 DSH 工具桥请求行（与 Rust 侧冻结的 JSONL 一致）：
 * - `{"id":N,"method":"dsh_tool","params":{"tool":"read","arguments":{...},"timeout_ms":300000}}`
 * - `{"id":N,"method":"dsh_tools","params":{}}`
 * - `{"method":"dsh_cancel","params":{"id":<目标请求 id>}}`（通知，可无顶层 id）
 *
 * 解析失败 / 非桥方法 / dsh_tool·dsh_tools 缺整数顶层 id（无法回投）返回
 * null；`arguments` 缺省 {}，`timeout_ms` 归一化到 1000..=600000（非法为
 * null，交给 config 默认值）。tool 为空字符串时仍返回对象，由执行器回
 * invalid_arguments。
 * @param {string} line
 * @returns {{kind: 'tool', id: number, tool: string, arguments: unknown, timeoutMs: number | null}
 *   | {kind: 'tools', id: number}
 *   | {kind: 'cancel', id: number | null, targetId: number}
 *   | null}
 */
export function parseDshToolRequest(line) {
  const text = typeof line === 'string' ? line.trim() : ''
  if (!text.startsWith('{')) return null
  let message = null
  try {
    message = JSON.parse(text)
  } catch {
    return null
  }
  if (message === null || typeof message !== 'object' || Array.isArray(message)) return null
  const method = message.method
  if (method !== 'dsh_tool' && method !== 'dsh_tools' && method !== 'dsh_cancel') return null
  const params = isPlainObject(message.params) ? message.params : {}
  const id = Number.isInteger(message.id) ? message.id : null
  if (method === 'dsh_cancel') {
    const targetId = params.id
    if (!Number.isInteger(targetId) || targetId <= 0) return null
    return { kind: 'cancel', id, targetId }
  }
  if (id === null) return null
  if (method === 'dsh_tools') return { kind: 'tools', id }
  const timeoutMs = Number.isFinite(params.timeout_ms)
    ? Math.min(DSH_TOOLS_LIMITS.timeoutMs.max, Math.max(DSH_TOOLS_LIMITS.timeoutMs.min, Math.floor(params.timeout_ms)))
    : null
  return {
    kind: 'tool',
    id,
    tool: typeof params.tool === 'string' ? params.tool.trim() : '',
    arguments: 'arguments' in params ? params.arguments : {},
    timeoutMs,
  }
}

/** 是否普通对象（非 null / 非数组）。 */
function isPlainObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

/**
 * 单行 stderr 是否为「TCP 已连接 <addr>」的成功连接行；返回地址或 null。
 * @param {string} line
 * @returns {string | null}
 */
export function parseConnectedLine(line) {
  return CONNECTED_LINE_RE.exec(typeof line === 'string' ? line.trim() : '')?.groups?.addr ?? null
}

/**
 * 单行 stderr 是否为 jump 迁移后的「切换到新端点 <addr>」行；返回地址或 null。
 * @param {string} line
 * @returns {string | null}
 */
export function parseSwitchLine(line) {
  return SWITCH_LINE_RE.exec(typeof line === 'string' ? line.trim() : '')?.groups?.addr ?? null
}

/**
 * 从一行 worker stderr 提取「当前连上的 Router 地址」：优先成功连接行，
 * 其次 jump 切换行；无关行（含进度行/错误行）返回 null。地址都按
 * `HOST:PORT` 语法校验，非法值忽略（返回 null）。
 * @param {string} line
 * @returns {string | null}
 */
export function parseReachedRouterLine(line) {
  const addr = parseConnectedLine(line) ?? parseSwitchLine(line)
  return addr !== null && splitHostPort(addr) ? addr : null
}

/**
 * 默认状态文件路径：`$DSH_HOME/sekai/worker-state.json`（DSH_HOME 未设置时
 * 回退 `~/.dsh`，与 DSH 插件生态的约定一致）。home 解析复用 shared.js 的
 * `resolveDshHome`，与二进制自动发现保持同一套规则。只做路径拼接，不落盘。
 * @param {NodeJS.ProcessEnv} [env]
 * @param {string} [home]
 * @returns {string}
 */
export function defaultWorkerStateFile(env = process.env, home = os.homedir()) {
  return path.join(resolveDshHome(env, home), 'sekai', 'worker-state.json')
}

/**
 * 同步读取状态文件（构造函数里用：文件很小）。缺失/损坏/形状非法都返回
 * null（调用方回退默认值），不抛错。
 * @param {string} file
 * @returns {{version: number, workers: Record<string, object>} | null}
 */
export function readWorkerState(file) {
  try {
    const raw = JSON.parse(fs.readFileSync(file, 'utf8'))
    if (!isPlainObject(raw)) return null
    const workers = isPlainObject(raw.workers) ? raw.workers : {}
    return { version: STATE_FILE_VERSION, workers }
  } catch {
    return null
  }
}

/**
 * 从状态快照取某个 worker 上次成功连接的 Router 地址；缺失或非法返回 null。
 * @param {{version?: number, workers?: object} | null} state
 * @param {string} workerId
 * @returns {string | null}
 */
export function lastRouterFromState(state, workerId) {
  if (!isPlainObject(state) || !isPlainObject(state.workers)) return null
  const entry = state.workers[workerId]
  if (!isPlainObject(entry)) return null
  const addr = typeof entry.lastRouterAddr === 'string' ? entry.lastRouterAddr.trim() : ''
  return splitHostPort(addr) ? addr : null
}

/**
 * 纯函数：把「上次连接的 Router」写进状态快照（返回新对象，不改入参）。
 * @param {{version?: number, workers?: object} | null} state
 * @param {string} workerId
 * @param {string} addr 已校验的 HOST:PORT
 * @param {number} [now] 时间戳（测试可注入）
 * @returns {{version: number, workers: Record<string, object>}}
 */
export function withLastRouter(state, workerId, addr, now = Date.now()) {
  const workers = { ...(isPlainObject(state?.workers) ? state.workers : {}) }
  const previous = isPlainObject(workers[workerId]) ? workers[workerId] : {}
  workers[workerId] = { ...previous, lastRouterAddr: addr, updatedAt: now }
  return { version: STATE_FILE_VERSION, workers }
}

/** spawn 失败的可操作提示。 */
export const WORKER_SPAWN_HINT = 'Run `node scripts/install-dsh-plugin.mjs` in the sekai repo (it builds the binaries and deploys them to $DSH_HOME/sekai/bin). Fallbacks: `cargo build --release` for a local build; the worker bin resolves as workerBin config > SEKAI_WORKER_BIN > $DSH_HOME/sekai/bin/sekai-worker(.exe) > <repo>/target/release/sekai-worker(.exe) > PATH.'

function makeWorkerConfig() {
  if (!z) return undefined
  return z.object({
    enabled: z.boolean().default(true)
      .description('Spawn and manage the sekai-worker process. When off, no worker tools are published.'),
    workerBin: z.string().default(WORKER_DEFAULTS.workerBin)
      .description('Explicit path to the sekai-worker binary. Empty follows env, $DSH_HOME/sekai/bin, repo target, then PATH.'),
    routerAddr: z.string().default(WORKER_DEFAULTS.routerAddr)
      .description('Router HOST:PORT to connect to when no previous connection is remembered.'),
    workerId: z.string().default(WORKER_DEFAULTS.workerId)
      .description('Worker identity reported to the router; it also scopes the remembered router.'),
    llm: z.union(LLM_CHANNELS).default(WORKER_DEFAULTS.llm)
      .description('Prompt channel: dsh relays pc.ask prompts through this plugin, router lets the worker connect itself, off disables relaying.'),
    promptAddr: z.string().default(WORKER_DEFAULTS.promptAddr)
      .description('Router prompt ingress HOST:PORT used when llm is dsh.'),
    promptTimeoutMs: z.number().step(1).min(1000).default(WORKER_DEFAULTS.promptTimeoutMs)
      .description('How long a relayed prompt waits for the router answer, in milliseconds.'),
    // 虚拟 GPIO 家族（esp32|stm32f1|stm32f4）；省略/空 = 关闭（PC agent 默认）。
    gpio: z.string()
      .description('Virtual GPIO family (esp32, stm32f1, stm32f4). Empty runs the PC agent shape.'),
    // 旧键兼容：等价 gpio（已废弃，仅映射）。
    family: z.string()
      .description('Deprecated alias of gpio, kept only for old configuration.'),
    // 开启 DSH 且 worker 启用时，是否自动连接上次连接的 Router。
    autoConnect: z.boolean().default(WORKER_DEFAULTS.autoConnect)
      .description('Connect to the remembered router when DSH starts. Off keeps manual control through the sekai_worker_* tools.'),
    // 是否发布 sekai_worker_connect / sekai_worker_disconnect 工具。
    publishControlTools: z.boolean().default(WORKER_DEFAULTS.publishControlTools)
      .description('Publish the sekai_worker_connect and sekai_worker_disconnect tools.'),
    // 上次连接 Router 的持久化文件；空 = $DSH_HOME/sekai/worker-state.json。
    stateFile: z.string()
      .description('Remembered-router state file. Empty uses $DSH_HOME/sekai/worker-state.json.'),
    restartBackoffMaxMs: z.number().step(1).min(RESTART_BACKOFF_INITIAL_MS).default(WORKER_DEFAULTS.restartBackoffMaxMs)
      .description('Upper bound of the unexpected-exit restart backoff, in milliseconds.'),
    publishStatusTool: z.boolean().default(true)
      .description('Publish the sekai_worker_status tool.'),
    // worker→DSH 工具桥（默认关闭；allow 默认只读）。
    dshTools: z.object({
      enabled: z.boolean().default(DSH_TOOLS_DEFAULTS.enabled)
        .description('Execute the worker dsh.tools and dsh.call requests through this DSH host (opt-in).'),
      allow: z.array(z.string()).default([...DSH_TOOLS_DEFAULTS.allow])
        .description('Tool names the worker may call through the bridge.'),
      deny: z.array(z.string()).default([])
        .description('Tool names removed from the allow list.'),
      cwd: z.string().default('')
        .description('Working directory for bridge tool calls. Empty uses the main session directory.'),
      agentPreset: z.string().default(DSH_TOOLS_DEFAULTS.agentPreset)
        .description('Agent preset for the bridge agent. Empty creates it without a preset.'),
      maxConcurrent: z.number().step(1).min(DSH_TOOLS_LIMITS.maxConcurrent.min).max(DSH_TOOLS_LIMITS.maxConcurrent.max).default(DSH_TOOLS_DEFAULTS.maxConcurrent)
        .description('Maximum concurrent bridge tool calls.'),
      timeoutMs: z.number().step(1).min(DSH_TOOLS_LIMITS.timeoutMs.min).max(DSH_TOOLS_LIMITS.timeoutMs.max).default(DSH_TOOLS_DEFAULTS.timeoutMs)
        .description('Per-call bridge timeout, in milliseconds.'),
      maxResponseBytes: z.number().step(1).min(DSH_TOOLS_LIMITS.maxResponseBytes.min).max(DSH_TOOLS_LIMITS.maxResponseBytes.max).default(DSH_TOOLS_DEFAULTS.maxResponseBytes)
        .description('Truncate bridge results larger than this many bytes.'),
    }),
  })
}

/**
 * 内部配置兜底：归一成带默认值的形状。旧键 `family` 若存在则映射到
 * `gpio`（gpio 显式非空优先），保证老配置不崩。
 * @param {object} [raw]
 */
export function resolveWorkerConfig(raw = {}) {
  const cap = Number.isFinite(raw?.restartBackoffMaxMs)
    ? Math.max(RESTART_BACKOFF_INITIAL_MS, Math.floor(raw.restartBackoffMaxMs))
    : WORKER_DEFAULTS.restartBackoffMaxMs
  const stringOrDefault = (value, fallback) =>
    typeof value === 'string' && value.trim() !== '' ? value : fallback
  const nonEmpty = (value) => (typeof value === 'string' && value.trim() !== '' ? value.trim() : null)
  const gpio = nonEmpty(raw?.gpio) ?? nonEmpty(raw?.family)
  // dshTools 归一化：数值钳制到范围内（非法回默认），数组只保留非空字符串，
  // 显式空 allow 保持空（= 一个都不允许），非数组才回默认白名单。
  const clampInt = (value, min, max, fallback) => {
    if (!Number.isFinite(value)) return fallback
    return Math.min(max, Math.max(min, Math.floor(value)))
  }
  const toolNameList = (value, fallback) => {
    if (!Array.isArray(value)) return [...fallback]
    return value
      .filter((name) => typeof name === 'string')
      .map((name) => name.trim())
      .filter((name) => name !== '')
  }
  const dshToolsRaw = isPlainObject(raw?.dshTools) ? raw.dshTools : {}
  return {
    enabled: raw?.enabled !== false,
    workerBin: stringOrDefault(raw?.workerBin, WORKER_DEFAULTS.workerBin),
    routerAddr: stringOrDefault(raw?.routerAddr, WORKER_DEFAULTS.routerAddr),
    workerId: stringOrDefault(raw?.workerId, WORKER_DEFAULTS.workerId),
    llm: LLM_CHANNELS.includes(raw?.llm) ? raw.llm : WORKER_DEFAULTS.llm,
    promptAddr: stringOrDefault(raw?.promptAddr, WORKER_DEFAULTS.promptAddr),
    promptTimeoutMs: Number.isFinite(raw?.promptTimeoutMs) && raw.promptTimeoutMs >= 1000
      ? Math.floor(raw.promptTimeoutMs)
      : WORKER_DEFAULTS.promptTimeoutMs,
    gpio,
    autoConnect: raw?.autoConnect !== false,
    publishControlTools: raw?.publishControlTools !== false,
    stateFile: nonEmpty(raw?.stateFile) ?? WORKER_DEFAULTS.stateFile,
    restartBackoffMaxMs: cap,
    publishStatusTool: raw?.publishStatusTool !== false,
    dshTools: {
      enabled: dshToolsRaw.enabled === true,
      allow: toolNameList(dshToolsRaw.allow, DSH_TOOLS_DEFAULTS.allow),
      deny: toolNameList(dshToolsRaw.deny, []),
      cwd: nonEmpty(dshToolsRaw.cwd) ?? '',
      agentPreset: typeof dshToolsRaw.agentPreset === 'string'
        ? dshToolsRaw.agentPreset.trim()
        : DSH_TOOLS_DEFAULTS.agentPreset,
      maxConcurrent: clampInt(dshToolsRaw.maxConcurrent, DSH_TOOLS_LIMITS.maxConcurrent.min, DSH_TOOLS_LIMITS.maxConcurrent.max, DSH_TOOLS_DEFAULTS.maxConcurrent),
      timeoutMs: clampInt(dshToolsRaw.timeoutMs, DSH_TOOLS_LIMITS.timeoutMs.min, DSH_TOOLS_LIMITS.timeoutMs.max, DSH_TOOLS_DEFAULTS.timeoutMs),
      maxResponseBytes: clampInt(dshToolsRaw.maxResponseBytes, DSH_TOOLS_LIMITS.maxResponseBytes.min, DSH_TOOLS_LIMITS.maxResponseBytes.max, DSH_TOOLS_DEFAULTS.maxResponseBytes),
    },
  }
}

const STATUS_OUTPUT_SCHEMA = Object.freeze({
  type: 'object',
  additionalProperties: false,
  required: ['running', 'restarts', 'uptimeMs', 'lastStderr'],
  properties: {
    running: { type: 'boolean', description: 'Whether the worker process is currently alive.' },
    pid: { type: 'integer', description: 'Worker process id; omitted while not running.' },
    restarts: { type: 'integer', description: 'Unexpected-exit restarts since plugin start (disposal not counted).' },
    uptimeMs: { type: 'integer', description: 'Current process uptime in milliseconds; 0 while not running.' },
    lastStderr: {
      type: 'array',
      items: { type: 'string' },
      description: 'Most recent staged worker stderr progress lines (ring buffer, newest last, kept across restarts).',
    },
    llm: { type: 'string', description: "LLM prompt channel: 'dsh' (relayed by this plugin to the router prompt ingress), 'router' (worker connects to the ingress itself), or 'off'." },
    autoConnect: { type: 'boolean', description: 'Whether the plugin auto-starts the worker connection when the plugin loads (config autoConnect).' },
    routerAddr: { type: 'string', description: 'Router address HOST:PORT the worker is connecting to / will connect to next (last connected router, else config routerAddr).' },
    lastRouterAddr: { type: 'string', description: 'Last successfully connected router address remembered in the state file; omitted when none is known.' },
    dshTools: {
      type: 'object',
      additionalProperties: true,
      description: 'worker→DSH tool bridge state: {enabled, allow, inFlight}.',
      properties: {
        enabled: { type: 'boolean', description: 'Whether the worker→DSH tool bridge is enabled (config dshTools.enabled).' },
        allow: { type: 'array', items: { type: 'string' }, description: 'Tool-name allow-list exposed to the worker.' },
        inFlight: { type: 'integer', description: 'Tool-bridge requests currently queued or executing.' },
      },
    },
  },
})

/**
 * Sekai Worker 生命周期插件。状态字段不用 `#private`（Cordis 派生接收者，
 * 见 router.js 头注）。
 */
export class SekaiWorkerPlugin {
  static Config = makeWorkerConfig()

  /**
   * @param {object} ctx Cordis context。
   * @param {object} [config] 插件配置（见 WORKER_DEFAULTS）。
   */
  constructor(ctx, config = {}) {
    this.ctx = ctx
    this.config = resolveWorkerConfig(config)
    this.child = null
    this.startedAt = null
    this.restarts = 0
    this.backoffMs = RESTART_BACKOFF_INITIAL_MS
    this.restartTimer = null
    this.stopped = false
    // 手动断开后抑制自动重启，直到再次 connect（dispose 用 stopped）。
    this.manualStopped = false
    this.lastStderr = new RingBuffer(STDERR_RING_SIZE)
    // dsh 中转的挂起 prompt：id → {socket, timer, done}。
    this.pendingPrompts = new Map()
    // worker→DSH 工具桥：在途请求（id → entry）、等待队列、运行中计数、
    // 懒创建的 bridge agent（显式 agent，绝不复用主 agent）。
    this.pendingDshTools = new Map()
    this.dshToolQueue = []
    this.dshToolsRunning = 0
    this.bridgeAgentHandle = null
    this.bridgeAgentPromise = null
    // 上次连接的 Router：状态文件（按 workerId 隔离）→ 本次连接目标。
    this.stateFile = this.config.stateFile !== '' ? this.config.stateFile : defaultWorkerStateFile()
    this.lastRouterAddr = lastRouterFromState(readWorkerState(this.stateFile), this.config.workerId)
    this.routerAddr = this.lastRouterAddr ?? this.config.routerAddr

    const plugin = this
    ctx?.effect?.(function* () {
      if (!plugin.config.enabled) {
        plugin.log('info', 'sekai-worker plugin is disabled (enabled:false); nothing spawned')
        return
      }
      const stopTools = plugin.registerToolsWhenAvailable()
      if (plugin.config.autoConnect) {
        const remembered = plugin.lastRouterAddr !== null
        plugin.log(
          'info',
          `sekai-worker autoConnect: connecting to ${plugin.routerAddr}` +
            (remembered ? ' (last connected router)' : ' (config routerAddr)'),
        )
        plugin.startWorker()
      } else {
        plugin.log(
          'info',
          `sekai-worker autoConnect=false; not connecting at startup` +
            ` (call sekai_worker_connect to start; next target ${plugin.routerAddr})`,
        )
      }
      yield () => {
        plugin.stop()
        stopTools()
      }
    }, 'sekai-worker lifecycle')
  }

  /** @param {'info'|'warn'|'error'} level @param {string} message */
  log(level, message) {
    try {
      this.ctx?.logger?.[level]?.(message)
    } catch {
      // 日志失败不影响插件。
    }
  }

  /**
   * worker 启动目标 seam：默认原样返回。scripts/smoke.mjs 覆写它换成假
   * worker，可返回 {env} 覆盖子进程环境。
   * @param {string} bin @param {string[]} args
   * @returns {{command: string, args: string[], env?: object}}
   */
  workerSpawnTarget(bin, args) {
    return { command: bin, args }
  }

  /**
   * spawn worker 并接线；重复调用安全。
   * @param {string} [routerAddr] 连接目标 HOST:PORT；省略 = this.routerAddr。
   * @returns {boolean} 是否真的 spawn（已有子进程/已 dispose 时 false）。
   */
  startWorker(routerAddr = this.routerAddr) {
    if (this.stopped || this.child) return false
    const target = typeof routerAddr === 'string' && routerAddr.trim() !== '' ? routerAddr.trim() : this.routerAddr
    this.routerAddr = target
    const bin = resolveBin(this.config.workerBin, 'SEKAI_WORKER_BIN', WORKER_DEFAULTS.workerBin)
    const argv = ['connect', target, '--worker-id', this.config.workerId, '--llm', this.config.llm]
    if (this.config.llm === 'router') argv.push('--prompt-addr', this.config.promptAddr)
    if (this.config.gpio) argv.push('--gpio', this.config.gpio)
    if (this.config.dshTools.enabled) argv.push('--dsh-tools')
    const spawnTarget = this.workerSpawnTarget(bin, argv)
    let child
    try {
      // stdin 是 dsh 中转的 prompt 响应回投通道（其他通道闲置不用）。
      child = spawn(spawnTarget.command, spawnTarget.args, {
        stdio: ['pipe', 'pipe', 'pipe'],
        windowsHide: true,
        detached: false,
        env: spawnTarget.env ?? process.env,
      })
    } catch (error) {
      this.handleChildExit(null, { spawnError: error })
      return false
    }
    this.child = child
    this.startedAt = Date.now()
    this.log('info', `sekai-worker spawn: ${spawnTarget.command} ${spawnTarget.args.join(' ')} (pid ${child.pid ?? '?'})`)

    // 'error' 与 'exit' 可能都到达（kill 竞态）：闭包 flag 保证恰好一次。
    let gone = false
    const onGone = (info) => {
      if (gone) return
      gone = true
      this.handleChildExit(child, info)
    }
    child.on('error', (error) => onGone({ spawnError: error }))
    child.on('exit', (code, signal) => onGone({ code, signal }))

    // stdin：best-effort 回投 prompt 响应；EPIPE 等错误只记日志。
    if (child.stdin) {
      child.stdin.on('error', (error) => {
        if (!this.stopped) this.log('warn', `sekai-worker stdin write failed: ${describeError(error)}`)
      })
    }

    // stderr：阶段化中文进度行 → 环形缓冲 + 宿主日志 + 「上次连接的 Router」观测。
    if (child.stderr) {
      const stderrLines = readline.createInterface({ input: child.stderr })
      stderrLines.on('line', (line) => {
        this.lastStderr.push(line)
        this.log('info', `[worker] ${line}`)
        this.observeWorkerProgress(line)
      })
    }
    // stdout：llm==='dsh' 时识别 prompt 请求行并中转；其余行只记日志。
    if (child.stdout) {
      const stdoutLines = readline.createInterface({ input: child.stdout })
      stdoutLines.on('line', (line) => this.handleWorkerStdoutLine(line))
    }

    if (child.exitCode !== null || child.killed) {
      // 极端时序：进程在接线前已退出。
      onGone({ code: child.exitCode, signal: null })
    }
    return true
  }

  /**
   * worker stderr 进度行观察：识别「TCP 已连接 / 切换到新端点」即更新
   * 本次连接目标与「上次连接的 Router」（状态文件）。
   * @param {string} line
   */
  observeWorkerProgress(line) {
    const addr = parseReachedRouterLine(line)
    if (addr === null) return
    this.routerAddr = addr
    if (this.lastRouterAddr === addr) return
    this.log('info', `sekai-worker connected to router ${addr}; remembered as last connected router`)
    this.persistLastRouter(addr)
  }

  /**
   * 记住「上次连接的 Router」：先更新内存，再异步原子写状态文件
   * （tmp + rename，失败只 warn）。同 workerId 隔离，多 workerId 合并。
   * @param {string} addr 已校验的 HOST:PORT
   */
  persistLastRouter(addr) {
    if (!splitHostPort(addr)) return
    this.lastRouterAddr = addr
    const file = this.stateFile
    const workerId = this.config.workerId
    const payload = `${JSON.stringify(withLastRouter(readWorkerState(file), workerId, addr), null, 2)}\n`
    const plugin = this
    void (async () => {
      await mkdir(path.dirname(file), { recursive: true })
      const temp = `${file}.tmp-${process.pid}`
      await writeFile(temp, payload, 'utf8')
      await rename(temp, file)
    })().catch((error) => {
      plugin.log('warn', `sekai-worker: last connected router was not saved to ${file}: ${describeError(error)}`)
    })
  }

  /**
   * 手动连接（sekai_worker_connect）：启动/切换 worker 到指定 Router。
   * 省略地址时优先上次连接的 Router，其次 config 的 routerAddr。
   * @param {unknown} [requestedAddr]
   * @returns {{started: boolean, running: boolean, routerAddr?: string, lastRouterAddr?: string, error?: object}}
   */
  connectWorker(requestedAddr) {
    const requested = typeof requestedAddr === 'string' ? requestedAddr.trim() : ''
    const target = requested !== '' ? requested : (this.lastRouterAddr ?? this.config.routerAddr)
    if (!splitHostPort(target)) {
      return {
        error: {
          code: 'invalid_arguments',
          message: `invalid router address "${target}" (expected HOST:PORT)`,
          hint: 'Pass router_addr as HOST:PORT, e.g. 127.0.0.1:7421.',
        },
      }
    }
    if (this.stopped) {
      return { error: { code: 'not_running', message: 'sekai-worker plugin is disposed; restart DSH or the plugin to reconnect' } }
    }
    if (this.child !== null && this.routerAddr === target) {
      // 已在连接同一目标：保持现状（幂等）。
      return this.connectionValue({ started: false, running: true })
    }
    if (this.child !== null) {
      // 切换 Router：先断旧连接（抑制自动重启），再连新目标。
      this.disconnectWorker()
    }
    this.manualStopped = false
    this.backoffMs = RESTART_BACKOFF_INITIAL_MS
    const started = this.startWorker(target)
    this.log('info', `sekai-worker manual connect: ${started ? 'spawned' : 'already running'} → ${target}`)
    return this.connectionValue({ started, running: this.child !== null })
  }

  /**
   * 手动断开（sekai_worker_disconnect）：停掉进程并抑制自动重启，直到再次
   * connect。幂等；插件自身生命周期不受影响。
   * @returns {{disconnected: boolean, running: boolean, routerAddr?: string, lastRouterAddr?: string}}
   */
  disconnectWorker() {
    if (this.restartTimer) {
      clearTimeout(this.restartTimer)
      this.restartTimer = null
    }
    this.manualStopped = true
    const child = this.child
    if (child === null) return this.connectionValue({ disconnected: false, running: false })
    // 先摘引用：exit 事件到达时 handleChildExit 视作迟到事件，不再计重启。
    this.child = null
    this.startedAt = null
    this.clearPendingPrompts('aborted', 'worker disconnected manually; pending prompt aborted', { write: false })
    this.clearPendingDshTools('cancelled', { write: false })
    if (!child.killed) {
      try { child.kill() } catch { /* 已退出 */ }
    }
    this.log('info', 'sekai-worker disconnected manually; auto-restart suppressed until reconnected')
    return this.connectionValue({ disconnected: true, running: false })
  }

  /**
   * 控制工具的统一返回值：running 取当前真实进程状态，附加 Router 地址；
   * `disconnected` 固定出现（connect 工具恒为 false，便于模型判断）。
   * @param {{started?: boolean, running: boolean, disconnected?: boolean}} base
   */
  connectionValue({ started = false, running, disconnected = false }) {
    const value = { started, running, disconnected, routerAddr: this.routerAddr }
    if (this.lastRouterAddr !== null) value.lastRouterAddr = this.lastRouterAddr
    return value
  }

  /**
   * worker stdout 单行入口：先按 prompt 请求中转（仅 llm==='dsh'），再按
   * worker→DSH 工具桥请求处理（不看 llm），其余行记日志。
   * @param {string} line
   */
  handleWorkerStdoutLine(line) {
    if (!this.stopped) {
      if (this.config.llm === 'dsh') {
        const request = parsePromptRequest(line)
        if (request) {
          this.relayPrompt(request)
          return
        }
      }
      const dshRequest = parseDshToolRequest(line)
      if (dshRequest) {
        this.handleDshToolRequest(dshRequest)
        return
      }
    }
    this.log('info', `[worker stdout] ${line}`)
  }

  /**
   * 把一条 prompt 请求中转到 promptAddr（独立连接），响应行按 id 匹配后
   * 原样写回 worker stdin；连接失败/超时回 unreachable/timeout 错误信封。
   * @param {{id: number, raw: string}} request
   */
  relayPrompt({ id, raw }) {
    const addrText = this.config.promptAddr
    const fail = (code, message) => {
      this.finishPrompt(id, () => this.writeWorkerStdin(`${JSON.stringify({ id, ok: false, error: { code, message } })}\n`))
      this.log('warn', `[prompt relay] #${id} failed: ${message}`)
    }
    const finish = (responseLine) => {
      const text = responseLine.endsWith('\n') ? responseLine : `${responseLine}\n`
      this.finishPrompt(id, () => this.writeWorkerStdin(text))
    }
    if (this.pendingPrompts.has(id)) {
      this.writeWorkerStdin(`${JSON.stringify({ id, ok: false, error: { code: 'invalid_arguments', message: `prompt id ${id} is already pending` } })}\n`)
      return
    }
    const target = splitHostPort(addrText)
    if (!target) {
      this.writeWorkerStdin(`${JSON.stringify({ id, ok: false, error: { code: 'unreachable', message: `invalid promptAddr "${addrText}" (expected HOST:PORT)` } })}\n`)
      return
    }
    const entry = { socket: null, timer: null, done: false }
    this.pendingPrompts.set(id, entry)
    const timeoutMs = this.config.promptTimeoutMs
    entry.timer = setTimeout(() => {
      fail('timeout', `prompt #${id} to ${addrText} timed out after ${timeoutMs}ms`)
    }, timeoutMs)
    entry.timer.unref?.()
    const socket = net.connect({ host: target.host, port: target.port })
    entry.socket = socket
    socket.on('error', (error) => {
      if (!entry.done) fail('unreachable', describeError(error))
    })
    socket.on('connect', () => {
      if (entry.done) return
      try {
        socket.write(`${raw}\n`)
      } catch (error) {
        fail('unreachable', describeError(error))
      }
    })
    const lines = readline.createInterface({ input: socket })
    // Node 的 readline 会把输入流的错误转发为 Interface 自身的 'error' 事件；
    // socket 侧 'error' 已统一处理，这里挂监听避免未处理错误事件把进程抛崩。
    lines.on('error', () => {})
    lines.on('line', (line) => {
      if (entry.done) return
      const text = line.trim()
      if (!text) return
      let msg = null
      try {
        msg = JSON.parse(text)
      } catch {
        this.log('warn', `[prompt relay] non-JSON line from ${addrText}: ${text}`)
        return
      }
      if (msg !== null && typeof msg === 'object' && msg.id === id) finish(text)
    })
    socket.on('close', () => {
      if (!entry.done) fail('unreachable', `prompt ingress ${addrText} closed the connection before answering prompt #${id}`)
    })
  }

  /**
   * 结束一条挂起 prompt：清定时器、断 socket，再执行 writeback。
   * @param {number} id
   * @param {() => void} writeback
   */
  finishPrompt(id, writeback) {
    const entry = this.pendingPrompts.get(id)
    if (!entry || entry.done) return
    entry.done = true
    this.pendingPrompts.delete(id)
    if (entry.timer) {
      clearTimeout(entry.timer)
      entry.timer = null
    }
    if (entry.socket) {
      try {
        entry.socket.destroy()
      } catch {
        // 已关闭。
      }
      entry.socket = null
    }
    try {
      writeback()
    } catch (error) {
      this.log('warn', `[prompt relay] #${id} writeback failed: ${describeError(error)}`)
    }
  }

  /**
   * 清理全部挂起 prompt（dispose / worker 退出）。
   * @param {string} code 错误码（dispose 用 aborted）。
   * @param {string} message
   * @param {{write?: boolean}} [options] write=false 时不回投（进程已亡）。
   */
  clearPendingPrompts(code, message, { write = true } = {}) {
    for (const id of [...this.pendingPrompts.keys()]) {
      this.finishPrompt(id, () => {
        if (write) this.writeWorkerStdin(`${JSON.stringify({ id, ok: false, error: { code, message } })}\n`)
      })
    }
  }

  // -------------------------------------------------------------------------
  // worker→DSH 工具桥（config.dshTools）
  // -------------------------------------------------------------------------

  /** dshTools 桥是否启用（config 在构造时归一化）。 */
  get dshToolsEnabled() {
    return this.config.dshTools.enabled === true
  }

  /**
   * 处理一条 worker→DSH 工具桥请求。dsh_tool / dsh_tools 必须有顶层 id；
   * dsh_cancel 是通知（无 id 只 abort、不回包）。所有路径恰好回投一次。
   * @param {{kind: string, id: number|null}} request
   */
  handleDshToolRequest(request) {
    if (request.kind === 'cancel') {
      this.cancelDshTool(request)
      return
    }
    if (!this.dshToolsEnabled) {
      this.writeDshFailure(request.id, 'disabled', 'worker→DSH tool bridge is disabled (set dshTools.enabled:true to enable it)')
      return
    }
    if (request.kind === 'tools') {
      void this.listDshTools(request)
      return
    }
    this.enqueueDshTool(request)
  }

  /**
   * dsh_tools：返回 bridge agent 可见且未被 allow/deny 过滤掉的工具清单。
   * 成功信封：{id, ok:true, result:{tools:[{name, description}], count}}。
   * @param {{id: number}} request
   */
  async listDshTools(request) {
    try {
      const registry = this.ctx?.get?.('tools')
      if (!registry || typeof registry.execute !== 'function') {
        this.writeDshFailure(request.id, 'unavailable', 'DSH tool registry is unavailable')
        return
      }
      const agent = await this.ensureBridgeAgent()
      const definitions = typeof registry.definitions === 'function' ? await registry.definitions(agent) : []
      const tools = (Array.isArray(definitions) ? definitions : [])
        .filter((definition) => this.dshToolPolicy(definition?.name) === null)
        .map((definition) => ({
          name: String(definition.name),
          description: this.firstLine(definition.description, 400),
        }))
      this.writeDshResponse({ id: request.id, ok: true, result: { tools, count: tools.length } })
      this.log('info', `[dsh-tools] #${request.id} listed ${tools.length} tool(s)`)
    } catch (error) {
      const code = error?.code === 'no_agent' ? 'no_agent' : 'bridge_error'
      this.writeDshFailure(request.id, code, describeError(error))
      this.log('warn', `[dsh-tools] #${request.id} list failed: ${code}`)
    }
  }

  /** 描述压成单行（Rust 侧展示只取首行/前 200 字符，这里先收敛体积）。 */
  firstLine(value, max) {
    const text = typeof value === 'string' ? value.trim().split('\n')[0].trim() : ''
    return text.length > max ? `${text.slice(0, max)}…` : text
  }

  /**
   * 工具名策略：硬拒绝（递归/任意代码）→ deny → allow（默认只读）。
   * 允许返回 null，拒绝返回 error 信封。
   */
  dshToolPolicy(tool) {
    if (typeof tool !== 'string' || tool.trim() === '') {
      return { code: 'invalid_arguments', message: 'params.tool must be a non-empty string' }
    }
    if (tool.startsWith(DSH_TOOL_DENY_PREFIX) || DSH_TOOL_HARD_DENY.includes(tool)) {
      return {
        code: 'policy_denied',
        message: `tool "${tool}" is always denied by the dshTools bridge (recursion / arbitrary-code guard)`,
        hint: 'The bridge never exposes run_code or sekai_worker_* tools.',
      }
    }
    if (this.config.dshTools.deny.includes(tool)) {
      return {
        code: 'policy_denied',
        message: `tool "${tool}" is listed in dshTools.deny`,
        hint: 'Remove it from dshTools.deny and add it to dshTools.allow to expose it.',
      }
    }
    if (!this.config.dshTools.allow.includes(tool)) {
      return {
        code: 'policy_denied',
        message: `tool "${tool}" is not in dshTools.allow`,
        hint: 'Add the exact tool name to dshTools.allow (defaults to read, glob, grep).',
      }
    }
    return null
  }

  /** 校验并排队一条 dsh_tool；超时从入队即开始计时。 */
  enqueueDshTool(request) {
    const { id, tool } = request
    const denied = this.dshToolPolicy(tool)
    if (denied !== null) {
      this.writeDshFailure(id, denied.code, denied.message, denied.hint)
      this.log('warn', `[dsh-tools] #${id} ${tool || '?'} denied: ${denied.code}`)
      return
    }
    if (this.pendingDshTools.has(id)) {
      this.writeDshFailure(id, 'invalid_arguments', `request id ${id} is already pending`)
      return
    }
    const timeoutMs = Number.isFinite(request.timeoutMs) ? request.timeoutMs : this.config.dshTools.timeoutMs
    const entry = {
      id,
      tool,
      arguments: request.arguments,
      timeoutMs,
      controller: new AbortController(),
      timer: null,
      settled: false,
      abortedReason: null,
    }
    this.pendingDshTools.set(id, entry)
    entry.timer = setTimeout(() => this.abortDshTool(entry, 'timeout'), timeoutMs)
    entry.timer.unref?.()
    this.dshToolQueue.push(entry)
    this.pumpDshToolQueue()
  }

  /** 简单 FIFO 队列：按 maxConcurrent 限流（默认 1 = 串行）。 */
  pumpDshToolQueue() {
    if (this.stopped) return
    while (this.dshToolsRunning < this.config.dshTools.maxConcurrent && this.dshToolQueue.length > 0) {
      const entry = this.dshToolQueue.shift()
      if (entry === undefined || entry.settled) continue
      this.dshToolsRunning += 1
      void this.executeDshTool(entry).finally(() => {
        this.dshToolsRunning = Math.max(0, this.dshToolsRunning - 1)
        this.pumpDshToolQueue()
      })
    }
  }

  /**
   * 执行一条 dsh_tool：懒创建 bridge agent → 可见性校验 → ctx.tools.execute
   * （maxParallelToolCalls:1、每条独立的 AbortSignal）。
   */
  async executeDshTool(entry) {
    const { id, tool } = entry
    try {
      const registry = this.ctx?.get?.('tools')
      if (!registry || typeof registry.execute !== 'function') {
        this.finishDshTool(entry, this.dshFailureEnvelope(id, 'unavailable', 'DSH tool registry is unavailable'))
        return
      }
      const agent = await this.ensureBridgeAgent()
      if (entry.settled) return
      if (typeof registry.definitions === 'function') {
        const definitions = await registry.definitions(agent)
        const visible = Array.isArray(definitions) && definitions.some((definition) => definition?.name === tool)
        if (!visible) {
          this.finishDshTool(entry, this.dshFailureEnvelope(id, 'unknown_tool', `tool "${tool}" is not visible to the bridge agent`))
          this.log('warn', `[dsh-tools] #${id} ${tool} failed: unknown_tool`)
          return
        }
      }
      if (entry.settled) return
      const batch = await registry.execute({
        agent,
        calls: [{ id: `dsh-${id}`, name: tool, arguments: JSON.stringify(entry.arguments ?? {}) }],
        maxParallelToolCalls: 1,
        signal: entry.controller.signal,
      })
      if (entry.settled) return
      const result = Array.isArray(batch?.results) ? batch.results[0]?.result : undefined
      if (result === undefined || result === null) {
        this.finishDshTool(entry, this.dshFailureEnvelope(id, 'bridge_error', 'tool executor returned no result'))
        return
      }
      this.finishDshTool(entry, this.dshToolResultEnvelope(id, result))
      this.log('info', `[dsh-tools] #${id} ${tool} ${result.isError === true ? 'failed' : 'ok'}`)
    } catch (error) {
      if (entry.settled) return
      const code = this.mapDshExecuteError(error)
      this.finishDshTool(entry, this.dshFailureEnvelope(id, code, describeError(error)))
      this.log('warn', `[dsh-tools] #${id} ${tool} failed: ${code}`)
    }
  }

  /**
   * 把 tools.execute 的结果转成回投信封：成功 {is_error:false, value, text,
   * truncated}；工具自身失败 {is_error:true, error:{name,code}, text}。
   */
  dshToolResultEnvelope(id, result) {
    const text = this.dshContentText(result?.content)
    if (result?.isError === true) {
      const error = isPlainObject(result.error) ? result.error : {}
      const info = isPlainObject(error.info) ? error.info : {}
      const name = typeof info.name === 'string' && info.name !== '' ? info.name : 'ToolError'
      const code = typeof info.code === 'string' && info.code !== '' ? info.code : 'tool_error'
      const body = text !== '' ? text : (typeof error.message === 'string' && error.message !== '' ? error.message : 'tool call failed')
      return { id, ok: true, result: { is_error: true, error: { name, code }, text: body, truncated: false } }
    }
    const value = result?.value === undefined ? null : result.value
    return { id, ok: true, result: { is_error: false, value, text: text !== '' ? text : JSON.stringify(value), truncated: false } }
  }

  /** content 里的 text 块拼接（native/PTC 结果都带 content）。 */
  dshContentText(content) {
    if (!Array.isArray(content)) return ''
    return content
      .filter((part) => isPlainObject(part) && part.type === 'text' && typeof part.text === 'string')
      .map((part) => part.text)
      .join('\n')
  }

  /** tools.execute reject → 稳定错误码。 */
  mapDshExecuteError(error) {
    const code = typeof error?.code === 'string' ? error.code : ''
    if (code === 'no_agent' || code === 'cancelled' || code === 'timeout') return code
    if (code === 'TOOL_EXECUTOR_UNAVAILABLE' || code === 'TOOL_ACTIVATION_FAILED') return 'unavailable'
    if (code === 'TOOL_NOT_FOUND' || error?.name === 'ToolNotFoundError') return 'unknown_tool'
    if (error?.name === 'AbortError' || code === 'ABORT_ERR') return 'cancelled'
    return 'bridge_error'
  }

  /**
   * 懒创建 bridge agent（显式 agent，绝不复用主 agent——ops-ptc 模式会拒绝
   * 程序化直接调用）。meta.cwd 取 config → 主会话 cwd → 进程 cwd；
   * agentPreset 创建失败时去掉 preset 重试一次，再失败回 no_agent。
   * @returns {Promise<object>} Agent
   */
  async ensureBridgeAgent() {
    if (this.bridgeAgentHandle !== null && this.bridgeAgentHandle.agent) return this.bridgeAgentHandle.agent
    if (this.bridgeAgentPromise === null) {
      this.bridgeAgentPromise = this.createBridgeAgent().catch((error) => {
        this.bridgeAgentPromise = null
        throw error
      })
    }
    return this.bridgeAgentPromise
  }

  async createBridgeAgent() {
    if (this.stopped) throw this.dshBridgeError('cancelled', 'plugin disposed while creating the bridge agent')
    const agents = this.ctx?.get?.('agents')
    if (!agents || typeof agents.create !== 'function') {
      throw this.dshBridgeError('no_agent', 'DSH agent service (ctx.agents) is unavailable; cannot create the bridge agent')
    }
    const meta = {}
    const cwd = this.resolveDshToolsCwd(agents)
    if (cwd !== '') meta.cwd = cwd
    const preset = this.config.dshTools.agentPreset
    if (preset !== '') meta.agentPreset = preset
    let handle = null
    try {
      handle = await agents.create({ sessionId: `sekai-dsh-tools-${randomUUID()}`, meta: { ...meta } })
    } catch (error) {
      if (!('agentPreset' in meta)) {
        throw this.dshBridgeError('no_agent', `failed to create the bridge agent: ${describeError(error)}`)
      }
      this.log('warn', `[dsh-tools] bridge agent creation failed (${describeError(error)}); retrying without agentPreset`)
      const retryMeta = { ...meta }
      delete retryMeta.agentPreset
      try {
        handle = await agents.create({ sessionId: `sekai-dsh-tools-${randomUUID()}`, meta: retryMeta })
      } catch (retryError) {
        throw this.dshBridgeError(
          'no_agent',
          `failed to create the bridge agent: ${describeError(error)}; retry without agentPreset failed: ${describeError(retryError)}`,
        )
      }
    }
    if (!handle || typeof handle !== 'object' || !handle.agent) {
      throw this.dshBridgeError('no_agent', 'DSH agent service returned no agent handle')
    }
    if (this.stopped) {
      this.disposeBridgeAgentHandle(handle)
      throw this.dshBridgeError('cancelled', 'plugin disposed while creating the bridge agent')
    }
    this.bridgeAgentHandle = handle
    this.log('info', `[dsh-tools] bridge agent ready (cwd ${cwd === '' ? 'process cwd' : cwd}, preset ${preset === '' ? 'none' : preset})`)
    return handle.agent
  }

  /** bridge agent 的 cwd：config.dshTools.cwd → 主会话（roots/list）cwd → 进程 cwd。 */
  resolveDshToolsCwd(agents) {
    const configured = typeof this.config.dshTools.cwd === 'string' ? this.config.dshTools.cwd.trim() : ''
    if (configured !== '') return path.isAbsolute(configured) ? configured : path.resolve(configured)
    const cwdOf = (list) => {
      if (!Array.isArray(list)) return ''
      for (const agent of list) {
        const cwd = agent?.session?.header?.cwd
        if (typeof cwd === 'string' && cwd.trim() !== '') return cwd
      }
      return ''
    }
    try {
      if (typeof agents.roots === 'function') {
        const cwd = cwdOf(agents.roots())
        if (cwd !== '') return cwd
      }
      if (typeof agents.list === 'function') {
        const cwd = cwdOf(agents.list())
        if (cwd !== '') return cwd
      }
    } catch {
      // agent 服务异常：回退进程 cwd，探测失败不拖垮桥。
    }
    return process.cwd()
  }

  /** 桥层稳定错误（带 code，供 executeDshTool 映射）。 */
  dshBridgeError(code, message) {
    const error = new Error(message)
    error.code = code
    return error
  }

  /** dsh_cancel：abort 目标请求；有顶层 id 时回 cancelled 布尔值（防御 Rust 不发 id 之外的情况）。 */
  cancelDshTool(request) {
    const entry = this.pendingDshTools.get(request.targetId)
    const cancelled = entry !== undefined && !entry.settled
    if (cancelled) this.abortDshTool(entry, 'cancelled')
    this.log('info', `[dsh-tools] cancel #${request.targetId}: ${cancelled ? 'aborted' : 'not found'}`)
    if (request.id !== null) {
      this.writeDshResponse({ id: request.id, ok: true, result: { cancelled } })
    }
  }

  /** abort 一条在途/排队请求并回投一次（timeout / cancelled）。 */
  abortDshTool(entry, reason, { write = true } = {}) {
    if (!entry || entry.settled) return
    const index = this.dshToolQueue.indexOf(entry)
    if (index !== -1) this.dshToolQueue.splice(index, 1)
    entry.abortedReason = reason
    try {
      entry.controller.abort(new Error(reason))
    } catch {
      // AbortController.abort 不抛；防御宿主实现差异。
    }
    const message = reason === 'timeout'
      ? `tool "${entry.tool}" timed out after ${entry.timeoutMs}ms`
      : `tool "${entry.tool}" was cancelled`
    this.finishDshTool(entry, this.dshFailureEnvelope(entry.id, reason, message), write)
  }

  /** 结束一条请求：清定时器、摘 pending、恰好回投一次。 */
  finishDshTool(entry, envelope, write = true) {
    if (!entry || entry.settled) return
    entry.settled = true
    if (entry.timer !== null) {
      clearTimeout(entry.timer)
      entry.timer = null
    }
    this.pendingDshTools.delete(entry.id)
    const index = this.dshToolQueue.indexOf(entry)
    if (index !== -1) this.dshToolQueue.splice(index, 1)
    if (write) this.writeDshResponse(envelope)
  }

  /** 清理全部在途请求（dispose / 手动断开 / 进程退出）。 */
  clearPendingDshTools(reason = 'cancelled', { write = true } = {}) {
    const entries = [...this.pendingDshTools.values()]
    this.dshToolQueue.length = 0
    for (const entry of entries) this.abortDshTool(entry, reason, { write })
  }

  /** dispose bridge agent（插件停止时调用；幂等）。 */
  disposeBridgeAgent() {
    const handle = this.bridgeAgentHandle
    this.bridgeAgentHandle = null
    this.bridgeAgentPromise = null
    if (handle) this.disposeBridgeAgentHandle(handle)
  }

  disposeBridgeAgentHandle(handle) {
    if (!handle || typeof handle.dispose !== 'function') return
    try {
      void Promise.resolve(handle.dispose()).catch((error) => {
        this.log('warn', `[dsh-tools] bridge agent dispose failed: ${describeError(error)}`)
      })
    } catch (error) {
      this.log('warn', `[dsh-tools] bridge agent dispose failed: ${describeError(error)}`)
    }
  }

  /** 桥层失败信封 {id, ok:false, error:{code, message, hint?}}。 */
  dshFailureEnvelope(id, code, message, hint = null) {
    const error = { code, message }
    if (typeof hint === 'string' && hint !== '') error.hint = hint
    return { id, ok: false, error }
  }

  writeDshFailure(id, code, message, hint = null) {
    this.writeDshResponse(this.dshFailureEnvelope(id, code, message, hint))
  }

  /** 写回一条响应（先按 maxResponseBytes 收敛，保证单行合法 JSON）。 */
  writeDshResponse(envelope) {
    this.writeWorkerStdin(`${this.fitDshEnvelope(envelope, this.config.dshTools.maxResponseBytes)}\n`)
  }

  /**
   * 信封体积收敛：正常时原样序列化；超 maxResponseBytes 时保留骨架、
   * 截断 text/message 并置 `truncated:true`（必要时丢弃 value）。
   */
  fitDshEnvelope(envelope, maxBytes) {
    const max = Number.isFinite(maxBytes) && maxBytes > 0 ? Math.floor(maxBytes) : 65536
    const serialize = (value) => JSON.stringify(value)
    const text = serialize(envelope)
    if (Buffer.byteLength(text, 'utf8') <= max) return text
    const shrunk = JSON.parse(text)
    const isSuccess = shrunk.ok === true && isPlainObject(shrunk.result)
    const isFailure = shrunk.ok === false && isPlainObject(shrunk.error)
    if (isSuccess) {
      delete shrunk.result.value
      shrunk.result.truncated = true
    } else if (isFailure) {
      shrunk.error.truncated = true
    }
    const field = isSuccess ? shrunk.result : (isFailure ? shrunk.error : null)
    const key = isSuccess ? 'text' : 'message'
    const rawOriginal = isSuccess
      ? (typeof envelope?.result?.text === 'string' ? envelope.result.text : '')
      : (typeof envelope?.error?.message === 'string' ? envelope.error.message : '')
    // 最终行 ≤ max 字节，text 保留超过 max 个字符没有意义，先粗切。
    const original = rawOriginal.length > max ? rawOriginal.slice(0, max) : rawOriginal
    if (field !== null) {
      let low = 0
      let high = original.length
      let best = ''
      while (low <= high) {
        const mid = Math.floor((low + high) / 2)
        field[key] = original.slice(0, mid)
        if (Buffer.byteLength(serialize(shrunk), 'utf8') <= max) {
          best = field[key]
          low = mid + 1
        } else {
          high = mid - 1
        }
      }
      field[key] = best
      const candidate = serialize(shrunk)
      if (Buffer.byteLength(candidate, 'utf8') <= max) return candidate
    }
    // 极端兜底：只留最小合法信封（maxResponseBytes 下限 1024，必然放得下）。
    const minimal = isSuccess
      ? { id: shrunk.id, ok: true, result: { is_error: false, truncated: true } }
      : { id: shrunk.id, ok: false, error: { code: 'bridge_error', message: 'response exceeded dshTools.maxResponseBytes' } }
    return serialize(minimal)
  }

  /**
   * best-effort 写 worker stdin（prompt 响应回投）。进程不在或管道已坏时
   * 静默返回 false。
   * @param {string} chunk
   * @returns {boolean}
   */
  writeWorkerStdin(chunk) {
    const stdin = this.child?.stdin
    if (!stdin || stdin.destroyed || !stdin.writable) return false
    try {
      stdin.write(chunk)
      return true
    } catch {
      return false
    }
  }

  /**
   * 进程退出/ spawn 失败的统一入口。
   * @param {import('node:child_process').ChildProcess | null} child
   * @param {{code?: number|null, signal?: string|null, spawnError?: unknown}} info
   */
  handleChildExit(child, info) {
    if (child !== null && this.child !== child) return // 迟到的旧进程事件
    this.child = null
    const uptimeMs = this.startedAt !== null ? Date.now() - this.startedAt : 0
    this.startedAt = null
    // 进程已亡：挂起 prompt 只断开，不再回投。
    this.clearPendingPrompts('aborted', 'worker process exited; pending prompt aborted', { write: false })
    // 工具桥同理：进程已亡，abort 在途请求但不再回投。
    this.clearPendingDshTools('cancelled', { write: false })
    if (this.stopped) return
    if (this.manualStopped) {
      // 手动 disconnect 期间的退出：不计重启、不排定时器。
      this.log('info', `sekai-worker exited after ${uptimeMs}ms while manually disconnected (${describeExitInfo(info)}); no restart`)
      return
    }
    this.restarts += 1
    if (info?.spawnError) {
      this.log('warn', `sekai-worker spawn failed: ${describeError(info.spawnError)} — ${WORKER_SPAWN_HINT}`)
    } else {
      this.log('warn', `sekai-worker exited unexpectedly after ${uptimeMs}ms (${describeExitInfo(info)}); restarting with backoff`)
    }
    // 稳定运行过的退出把退避复位，短命循环则指数上升。
    if (uptimeMs >= STABLE_UPTIME_MS) this.backoffMs = RESTART_BACKOFF_INITIAL_MS
    this.scheduleRestart()
  }

  scheduleRestart() {
    if (this.stopped || this.restartTimer) return
    const delay = this.backoffMs
    this.backoffMs = nextBackoffMs(this.backoffMs, this.config.restartBackoffMaxMs)
    this.log('info', `sekai-worker restart scheduled in ${delay}ms`)
    const timer = setTimeout(() => {
      this.restartTimer = null
      if (!this.stopped) this.startWorker()
    }, delay)
    timer.unref?.()
    this.restartTimer = timer
  }

  /** 停止：清定时器 + 清挂起 prompt（回 aborted）+ kill。幂等。 */
  stop() {
    if (this.stopped) return
    this.stopped = true
    if (this.restartTimer) {
      clearTimeout(this.restartTimer)
      this.restartTimer = null
    }
    this.clearPendingPrompts('aborted', 'plugin disposed; pending prompt aborted')
    this.clearPendingDshTools('cancelled')
    const child = this.child
    this.child = null
    this.startedAt = null
    if (child && !child.killed) {
      try { child.kill() } catch { /* 已退出 */ }
    }
    this.disposeBridgeAgent()
  }

  /** 状态工具的值。 */
  statusValue() {
    const value = {
      running: this.child !== null,
      restarts: this.restarts,
      uptimeMs: this.startedAt !== null ? Date.now() - this.startedAt : 0,
      lastStderr: this.lastStderr.toArray(),
      llm: this.config.llm,
      autoConnect: this.config.autoConnect,
      routerAddr: this.routerAddr,
    }
    if (this.lastRouterAddr !== null) value.lastRouterAddr = this.lastRouterAddr
    value.dshTools = {
      enabled: this.config.dshTools.enabled === true,
      allow: [...this.config.dshTools.allow],
      inFlight: this.pendingDshTools.size,
    }
    const pid = this.child?.pid
    if (typeof pid === 'number') value.pid = pid
    return value
  }

  /**
   * 发布本插件的全部本地工具：状态工具（publishStatusTool）与控制工具
   * （publishControlTools，独立开关）。返回统一 dispose。
   */
  registerTools() {
    const disposers = []
    if (this.config.publishStatusTool) disposers.push(this.registerStatusTool())
    if (this.config.publishControlTools) disposers.push(this.registerControlTools())
    let done = false
    return () => {
      if (done) return
      done = true
      for (const dispose of disposers) {
        try { dispose() } catch { /* 注销失败忽略 */ }
      }
    }
  }

  /**
   * 等 `tools` 服务可用后再注册工具，返回统一 dispose。
   *
   * DSH 0.2.2 起 profile 条目不再保证按 bundle 顺序「挂载完成」，本插件激活时
   * 宿主可能还没建好工具注册表：直接 `ctx.get('tools')` 拿到 `undefined`，
   * `sekai_worker_*` 就被静默跳过（router 插件靠 `static inject` 躲过这一拍）。
   * 这里用 `ctx.inject(['tools'])` 在服务出现后注册——宿主没有工具注册表时
   * 回调不会运行，插件的 spawn/prompt 中转照常工作；没有 `inject` 的宿主
   * （冒烟用的假 ctx）退回一次性注册。
   * @returns {() => void} 清理函数；inject 路径随 fiber 卸载自动注销。
   */
  registerToolsWhenAvailable() {
    if (typeof this.ctx?.inject === 'function') {
      this.ctx.inject(['tools'], () => this.registerTools())
      return () => {}
    }
    return this.registerTools()
  }

  /** 状态工具注册（防御式：registry 缺失只 warn）。 */
  registerStatusTool() {
    // 只经 ctx.get 取服务：Cordis 的 ctx 代理对未 inject 的服务属性访问会直接
    // 抛错（"cannot get property ... without inject"），不能写 ctx.tools 兜底。
    const registry = this.ctx?.get?.('tools')
    if (!registry || typeof registry.register !== 'function') {
      this.log('warn', 'sekai-worker: no tool registry (ctx.tools) mounted; sekai_worker_status was not published')
      return () => {}
    }
    const plugin = this
    const def = {
      name: 'sekai_worker_status',
      description: 'Inspect the local sekai-worker process managed by this plugin: running state, pid, restart count, uptime, and the most recent staged stderr progress lines (Chinese progress text from the worker).',
      parameters: {
        type: 'object',
        properties: {},
        additionalProperties: false,
      },
      output: { schema: STATUS_OUTPUT_SCHEMA, render: renderJson },
      async execute() {
        return plugin.statusValue()
      },
    }
    let dispose = null
    try {
      dispose = registry.register(def)
    } catch (error) {
      this.log('warn', `sekai-worker: sekai_worker_status could not be registered (${describeError(error)})`)
      return () => {}
    }
    let done = false
    return () => {
      if (done) return
      done = true
      if (typeof dispose === 'function') {
        try { dispose() } catch { /* 注销失败忽略 */ }
      }
    }
  }

  /**
   * 手动控制工具：`sekai_worker_connect` / `sekai_worker_disconnect`。
   * 供 autoConnect=false 或运行期切换 Router 时使用（防御式注册，同状态工具）。
   */
  registerControlTools() {
    const registry = this.ctx?.get?.('tools')
    if (!registry || typeof registry.register !== 'function') {
      this.log('warn', 'sekai-worker: no tool registry (ctx.tools) mounted; sekai_worker_connect/disconnect were not published')
      return () => {}
    }
    const plugin = this
    const defs = [
      {
        name: 'sekai_worker_connect',
        description: "Start (or re-point) the local sekai-worker PC agent and connect it to a Sekai router. When the plugin config autoConnect is true the worker was already connected at DSH startup; use this tool when autoConnect is false, after sekai_worker_disconnect, or to switch routers. Omit router_addr to use the last successfully connected router (falling back to the configured routerAddr).",
        parameters: {
          type: 'object',
          properties: {
            router_addr: { type: 'string', description: 'Router address HOST:PORT to connect to; omit for the last connected / configured router.' },
          },
          additionalProperties: false,
        },
        output: CONTROL_OUTPUT,
        async execute(args) {
          return plugin.connectWorker(args?.router_addr)
        },
      },
      {
        name: 'sekai_worker_disconnect',
        description: 'Disconnect and stop the local sekai-worker process managed by this plugin. Auto-restart after an unexpected exit stays suppressed until sekai_worker_connect is called again (or the plugin restarts).',
        parameters: NO_ARGS_PARAMETERS,
        output: CONTROL_OUTPUT,
        async execute() {
          return plugin.disconnectWorker()
        },
      },
    ]
    const disposers = []
    for (const def of defs) {
      try {
        const dispose = registry.register(def)
        if (typeof dispose === 'function') disposers.push(dispose)
      } catch (error) {
        this.log('warn', `sekai-worker: tool ${def.name} could not be registered (${describeError(error)})`)
      }
    }
    let done = false
    return () => {
      if (done) return
      done = true
      for (const dispose of disposers) {
        try { dispose() } catch { /* 注销失败忽略 */ }
      }
    }
  }
}

// Cordis 类插件约定：默认导出即插件本体（参照 dsh-dcp 的 `export default DcpEngine`）。
export default SekaiWorkerPlugin
