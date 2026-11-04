/**
 * worker.js — Sekai Worker 插件：spawn `sekai-worker connect <addr>` 并
 * 管理其生命周期，发布 sekai_worker_status 状态工具。
 *
 * worker 的 connect 模式不参与 JSONL 协议：正常情况下 stdout 无输出，
 * stderr 是阶段化中文进度行——因此这里不走 JsonlProcess，只做 spawn +
 * stderr 环形缓冲 + 退避重启。
 *
 * 意外退出按 500ms 起步指数退避重启（封顶 restartBackoffMaxMs）；稳定
 * 运行（≥5s）后的退出把退避复位。dispose 直接 kill（无优雅协议）。
 *
 * 不声明 static inject：宿主没有 tools 服务也能加载本插件，状态工具仅在
 * 注册表可用时发布（缺 registry 只 warn 不崩）。
 *
 * @module @sekai/dsh-plugin/worker
 */
import { spawn } from 'node:child_process'
import readline from 'node:readline'
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
  family: 'esp32',
  restartBackoffMaxMs: 30000,
  publishStatusTool: true,
})

/** 稳定运行阈值：存活超过该时长的进程退出后，退避复位。 */
const STABLE_UPTIME_MS = 5000

/** stderr 环形缓冲容量（任务卡约定 ~40 行）。 */
const STDERR_RING_SIZE = 40

/** spawn 失败的可操作提示。 */
export const WORKER_SPAWN_HINT = 'Run `cargo build --release` in the sekai repo, then set workerBin to the absolute path of target/release/sekai-worker(.exe), put it on PATH, or set SEKAI_WORKER_BIN.'

function makeWorkerConfig() {
  if (!z) return undefined
  return z.object({
    enabled: z.boolean().default(true),
    workerBin: z.string().default(WORKER_DEFAULTS.workerBin),
    routerAddr: z.string().default(WORKER_DEFAULTS.routerAddr),
    workerId: z.string().default(WORKER_DEFAULTS.workerId),
    family: z.string().default(WORKER_DEFAULTS.family),
    restartBackoffMaxMs: z.number().step(1).min(RESTART_BACKOFF_INITIAL_MS).default(WORKER_DEFAULTS.restartBackoffMaxMs),
    publishStatusTool: z.boolean().default(true),
  })
}

/** 内部配置兜底：归一成带默认值的形状。 */
export function resolveWorkerConfig(raw = {}) {
  const cap = Number.isFinite(raw?.restartBackoffMaxMs)
    ? Math.max(RESTART_BACKOFF_INITIAL_MS, Math.floor(raw.restartBackoffMaxMs))
    : WORKER_DEFAULTS.restartBackoffMaxMs
  const stringOrDefault = (value, fallback) =>
    typeof value === 'string' && value.trim() !== '' ? value : fallback
  return {
    enabled: raw?.enabled !== false,
    workerBin: stringOrDefault(raw?.workerBin, WORKER_DEFAULTS.workerBin),
    routerAddr: stringOrDefault(raw?.routerAddr, WORKER_DEFAULTS.routerAddr),
    workerId: stringOrDefault(raw?.workerId, WORKER_DEFAULTS.workerId),
    family: stringOrDefault(raw?.family, WORKER_DEFAULTS.family),
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
    const argv = ['connect', this.config.routerAddr, '--worker-id', this.config.workerId, '--family', this.config.family]
    const target = this.workerSpawnTarget(bin, argv)
    let child
    try {
      child = spawn(target.command, target.args, {
        stdio: ['ignore', 'pipe', 'pipe'],
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

    // stderr：阶段化中文进度行 → 环形缓冲 + 宿主日志。
    if (child.stderr) {
      const stderrLines = readline.createInterface({ input: child.stderr })
      stderrLines.on('line', (line) => {
        this.lastStderr.push(line)
        this.log('info', `[worker] ${line}`)
      })
    }
    // stdout 正常无输出；出现内容也只记日志，不当错误。
    if (child.stdout) {
      const stdoutLines = readline.createInterface({ input: child.stdout })
      stdoutLines.on('line', (line) => this.log('info', `[worker stdout] ${line}`))
    }

    if (child.exitCode !== null || child.killed) {
      // 极端时序：进程在接线前已退出。
      onGone({ code: child.exitCode, signal: null })
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

  /** 停止：清定时器 + kill（worker 无优雅协议）。幂等。 */
  stop() {
    if (this.stopped) return
    this.stopped = true
    if (this.restartTimer) {
      clearTimeout(this.restartTimer)
      this.restartTimer = null
    }
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
