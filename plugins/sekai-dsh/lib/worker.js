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
 * 意外退出按 500ms 起步指数退避重启（封顶 restartBackoffMaxMs）；稳定
 * 运行（≥5s）后的退出把退避复位。dispose 直接 kill（无优雅协议），并
 * 清理挂起的 prompt 中转（回 error aborted）。
 *
 * 不声明 static inject：宿主没有 tools 服务也能加载本插件，状态工具仅在
 * 注册表可用时发布（缺 registry 只 warn 不崩）。
 *
 * @module @sekai/dsh-plugin/worker
 */
import { spawn } from 'node:child_process'
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
  restartBackoffMaxMs: 30000,
  publishStatusTool: true,
})

/** LLM prompt 通道取值（与 worker `--llm` 旗标一致）。 */
export const LLM_CHANNELS = Object.freeze(['dsh', 'router', 'off'])

/** 稳定运行阈值：存活超过该时长的进程退出后，退避复位。 */
const STABLE_UPTIME_MS = 5000

/** stderr 环形缓冲容量（任务卡约定 ~40 行）。 */
const STDERR_RING_SIZE = 40

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

/** spawn 失败的可操作提示。 */
export const WORKER_SPAWN_HINT = 'Run `cargo build --release` in the sekai repo, then set workerBin to the absolute path of target/release/sekai-worker(.exe), put it on PATH, or set SEKAI_WORKER_BIN.'

function makeWorkerConfig() {
  if (!z) return undefined
  return z.object({
    enabled: z.boolean().default(true),
    workerBin: z.string().default(WORKER_DEFAULTS.workerBin),
    routerAddr: z.string().default(WORKER_DEFAULTS.routerAddr),
    workerId: z.string().default(WORKER_DEFAULTS.workerId),
    llm: z.union(LLM_CHANNELS).default(WORKER_DEFAULTS.llm),
    promptAddr: z.string().default(WORKER_DEFAULTS.promptAddr),
    promptTimeoutMs: z.number().step(1).min(1000).default(WORKER_DEFAULTS.promptTimeoutMs),
    // 虚拟 GPIO 家族（esp32|stm32f1|stm32f4）；省略/空 = 关闭（PC agent 默认）。
    gpio: z.string(),
    // 旧键兼容：等价 gpio（已废弃，仅映射）。
    family: z.string(),
    restartBackoffMaxMs: z.number().step(1).min(RESTART_BACKOFF_INITIAL_MS).default(WORKER_DEFAULTS.restartBackoffMaxMs),
    publishStatusTool: z.boolean().default(true),
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
    restartBackoffMaxMs: cap,
    publishStatusTool: raw?.publishStatusTool !== false,
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
    this.lastStderr = new RingBuffer(STDERR_RING_SIZE)
    // dsh 中转的挂起 prompt：id → {socket, timer, done}。
    this.pendingPrompts = new Map()

    const plugin = this
    ctx?.effect?.(function* () {
      if (!plugin.config.enabled) {
        plugin.log('info', 'sekai-worker plugin is disabled (enabled:false); nothing spawned')
        return
      }
      plugin.startWorker()
      let stopTools = () => {}
      if (plugin.config.publishStatusTool) stopTools = plugin.registerStatusTool()
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

  /** spawn worker 并接线；重复调用安全。 */
  startWorker() {
    if (this.stopped || this.child) return
    const bin = resolveBin(this.config.workerBin, 'SEKAI_WORKER_BIN', WORKER_DEFAULTS.workerBin)
    const argv = ['connect', this.config.routerAddr, '--worker-id', this.config.workerId, '--llm', this.config.llm]
    if (this.config.llm === 'router') argv.push('--prompt-addr', this.config.promptAddr)
    if (this.config.gpio) argv.push('--gpio', this.config.gpio)
    const target = this.workerSpawnTarget(bin, argv)
    let child
    try {
      // stdin 是 dsh 中转的 prompt 响应回投通道（其他通道闲置不用）。
      child = spawn(target.command, target.args, {
        stdio: ['pipe', 'pipe', 'pipe'],
        windowsHide: true,
        detached: false,
        env: target.env ?? process.env,
      })
    } catch (error) {
      this.handleChildExit(null, { spawnError: error })
      return
    }
    this.child = child
    this.startedAt = Date.now()
    this.log('info', `sekai-worker spawn: ${target.command} ${target.args.join(' ')} (pid ${child.pid ?? '?'})`)

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

    // stderr：阶段化中文进度行 → 环形缓冲 + 宿主日志。
    if (child.stderr) {
      const stderrLines = readline.createInterface({ input: child.stderr })
      stderrLines.on('line', (line) => {
        this.lastStderr.push(line)
        this.log('info', `[worker] ${line}`)
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
  }

  /**
   * worker stdout 单行入口：prompt 请求行进入中转（仅 llm==='dsh'），
   * 其他行（含非 prompt 的 JSON）维持现状记日志。
   * @param {string} line
   */
  handleWorkerStdoutLine(line) {
    if (this.config.llm === 'dsh' && !this.stopped) {
      const request = parsePromptRequest(line)
      if (request) {
        this.relayPrompt(request)
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
    if (this.stopped) return
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
    const child = this.child
    this.child = null
    this.startedAt = null
    if (child && !child.killed) {
      try { child.kill() } catch { /* 已退出 */ }
    }
  }

  /** 状态工具的值。 */
  statusValue() {
    const value = {
      running: this.child !== null,
      restarts: this.restarts,
      uptimeMs: this.startedAt !== null ? Date.now() - this.startedAt : 0,
      lastStderr: this.lastStderr.toArray(),
      llm: this.config.llm,
    }
    const pid = this.child?.pid
    if (typeof pid === 'number') value.pid = pid
    return value
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
}

// Cordis 类插件约定：默认导出即插件本体（参照 dsh-dcp 的 `export default DcpEngine`）。
export default SekaiWorkerPlugin
