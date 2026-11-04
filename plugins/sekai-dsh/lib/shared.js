/**
 * shared.js — JSONL over stdio 子进程封装与两个插件共用的小工具。
 *
 * 这里唯一的运行时依赖是 Node 内置模块（child_process / readline /
 * module）。@deepseek-ai/schemastery 只在插件文件里以 peer 方式按需加载，
 * 本文件保持零 npm 依赖，仓库冒烟（scripts/smoke.mjs）可直接 import。
 *
 * JsonlProcess 约定（与 sekai-router --serve 的冻结 JSONL 协议 v1 一致）：
 * - 请求：stdin 每行 {"id":<整数>,"method":"<名>","params":{...}}
 * - 响应：{"id":..,"ok":true,"result":{..}} 或
 *         {"id":..,"ok":false,"error":{"code":"..","message":".."}}
 * - 通知（无 id）：{"event":"ready" | "link_state" | "worker_online" | ...}
 * - 未匹配（无 id 也无 event，或 JSON 解析失败）的行丢弃并 logger.warn
 *
 * @module @sekai/dsh-plugin/shared
 */
import { spawn } from 'node:child_process'
import readline from 'node:readline'

/** 退避重启的起始间隔（毫秒），router 与 worker 共用。 */
export const RESTART_BACKOFF_INITIAL_MS = 500

/**
 * 结构化错误：所有跨进程边界的失败都用它承载，code 是稳定的机器可读名。
 * JSONL 协议侧错误沿用 router 返回的 code（invalid_arguments /
 * policy_denied / unknown_method / link_error 等）；本地产生的 code 有
 * spawn_error / not_ready / timeout / aborted / not_running /
 * protocol_error / invalid_request。
 */
export class JsonlError extends Error {
  /**
   * @param {string} code 稳定错误码。
   * @param {string} message 人类可读错误信息。
   * @param {string | null} [hint] 可选的可操作提示。
   */
  constructor(code, message, hint = null) {
    super(message)
    this.name = 'JsonlError'
    this.code = code
    this.hint = hint ?? null
  }

  /** 序列化为工具结果里的 error 信封。 */
  toJSON() {
    const out = { code: this.code, message: this.message }
    if (this.hint) out.hint = this.hint
    return out
  }
}

/** 定长环形缓冲：worker stderr 进度行等诊断信息。 */
export class RingBuffer {
  /** @param {number} [max] 容量，最新在后。 */
  constructor(max = 40) {
    this.max = Math.max(1, Math.floor(max))
    this.items = []
  }

  /** @param {unknown} item 追加一项，超容量时丢最旧。 */
  push(item) {
    this.items.push(item)
    if (this.items.length > this.max) {
      this.items.splice(0, this.items.length - this.max)
    }
  }

  /** @returns {unknown[]} 当前内容快照（最新在后）。 */
  toArray() {
    return [...this.items]
  }

  /** 清空。 */
  clear() {
    this.items = []
  }
}

/**
 * 下一档退避间隔：翻倍并封顶。稳定运行后的重置由调用方处理。
 * @param {number} currentMs 当前间隔。
 * @param {number} capMs 上限。
 * @returns {number} 下一档间隔。
 */
export function nextBackoffMs(currentMs, capMs) {
  const base = Math.max(1, currentMs)
  return Math.min(base * 2, Math.max(base, capMs))
}

/**
 * 二进制解析：显式 config（≠默认值）> 环境变量 > 默认值（PATH 查找）。
 * config 的默认值与默认二进制同名时无法区分“显式配置”，但两者语义相同
 * （都等价于 PATH 查找），故不产生歧义。
 * @param {string | undefined} configured 插件 config 里的值。
 * @param {string} envKey 环境变量名（如 SEKAI_ROUTER_BIN）。
 * @param {string} defaultBin 默认二进制名。
 * @returns {string} 实际使用的命令。
 */
export function resolveBin(configured, envKey, defaultBin) {
  if (typeof configured === 'string' && configured.trim() !== '' && configured !== defaultBin) {
    return configured
  }
  const fromEnv = process.env[envKey]
  if (typeof fromEnv === 'string' && fromEnv.trim() !== '') return fromEnv
  return configured && configured.trim() !== '' ? configured : defaultBin
}

/**
 * 把任意抛出的值变成单行可读描述（'ENOENT: ...'）。
 * @param {unknown} error
 * @returns {string}
 */
export function describeError(error) {
  if (error === null || error === undefined) return 'unknown error'
  if (typeof error === 'string') return error
  if (typeof error === 'object') {
    const code = error.code
    const message = error.message ?? String(error)
    return code ? `${code}: ${message}` : String(message)
  }
  return String(error)
}

/**
 * 进程退出信息的单行描述。
 * @param {{code?: number|null, signal?: string|null, spawnError?: unknown} | null} info
 * @returns {string}
 */
export function describeExitInfo(info) {
  if (!info) return 'unknown reason'
  if (info.spawnError) return `spawn error (${describeError(info.spawnError)})`
  const parts = []
  if (typeof info.code === 'number') parts.push(`code ${info.code}`)
  if (typeof info.signal === 'string' && info.signal) parts.push(`signal ${info.signal}`)
  return parts.length > 0 ? parts.join(', ') : 'no exit details'
}

/**
 * 工具结果的统一渲染：pretty JSON 文本。
 * @returns {{type: 'text', text: string}[]}
 */
export function renderJson(_args, value) {
  return [{ type: 'text', text: JSON.stringify(value ?? {}, null, 2) }]
}

/** 不保持事件循环存活的 sleep（dispose 宽限等待用）。 */
function sleep(ms) {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms)
    timer.unref?.()
  })
}

/**
 * JSONL 子进程：spawn + readline 分帧 + id→Promise 关联 + 通知回调。
 *
 * 生命周期约定：
 * - 进程退出（含 spawn 失败）时，所有未决请求以 not_running/spawn_error
 *   拒绝，onExit(info) 恰好被调用一次，上层据此退避重启；
 * - dispose 顺序：发 shutdown 请求（短超时，忽略失败）→ 宽限等待自然退出
 *   → kill → 再等 → 强杀；幂等，dispose 引发的退出同样回调 onExit，上层
 *   用自己的 stopped 标志忽略即可。
 */
export class JsonlProcess {
  /**
   * @param {object} [options]
   * @param {string} options.command 可执行文件。
   * @param {string[]} options.args 参数。
   * @param {{info?: (m: string) => void, warn?: (m: string) => void} | null} [options.logger]
   *   宿主 logger；null 时静默。
   * @param {((event: object) => void) | null} [options.onEvent] 通知回调。
   * @param {((info: {code?: number|null, signal?: string|null, spawnError?: unknown}) => void) | null} [options.onExit]
   * @param {string | null} [options.spawnHint] spawn 失败时附在错误里的可操作提示。
   * @param {NodeJS.ProcessEnv} [options.env] 子进程环境，默认继承。
   */
  constructor({ command, args, logger = null, onEvent = null, onExit = null, spawnHint = null, env } = {}) {
    this.command = command
    this.args = args ?? []
    this.logger = logger
    this.onEvent = onEvent
    this.onExit = onExit
    this.spawnHint = spawnHint
    this.pending = new Map()
    this.nextId = 1
    this.disposed = false
    this.exited = false
    this.exitInfo = null
    this.exitWaiters = []
    this.child = null

    let child = null
    try {
      child = spawn(command, this.args, {
        stdio: ['pipe', 'pipe', 'pipe'],
        windowsHide: true,
        detached: false,
        env: env ?? process.env,
      })
    } catch (error) {
      // 极少数同步抛出（参数非法等）：按 spawn 失败统一处理。
      this.markExited({ spawnError: error })
      return
    }
    this.child = child
    // stdin 写入失败（进程恰好退出）不能演变成未捕获异常。
    child.stdin?.on?.('error', (error) => {
      this.warnLog(`stdin write failed: ${describeError(error)}`)
    })

    const stdoutLines = readline.createInterface({ input: child.stdout })
    stdoutLines.on('line', (line) => this.handleLine(line))
    // serve 的人类可读日志全走 stderr：转发给宿主 logger，不参与协议。
    if (child.stderr) {
      const stderrLines = readline.createInterface({ input: child.stderr })
      stderrLines.on('line', (line) => this.logger?.info?.(`[serve] ${line}`))
    }

    child.on('error', (error) => {
      // spawn 失败（ENOENT 等）在这里到达。
      this.markExited({ spawnError: error })
    })
    child.on('exit', (code, signal) => {
      this.markExited({ code, signal })
    })
  }

  /** @returns {number | null} 子进程 pid，未启动/已退出为 null。 */
  get pid() {
    return this.child?.pid ?? null
  }

  warnLog(message) {
    try {
      this.logger?.warn?.(message)
    } catch {
      // 日志失败不影响协议处理。
    }
  }

  /**
   * 发一个请求并等待对应 id 的响应。
   * @param {string} method
   * @param {object} [params]
   * @param {{timeoutMs?: number, signal?: AbortSignal, internal?: boolean}} [options]
   *   internal=true 供 dispose 内部发送 shutdown，绕过 disposed 拒绝。
   * @returns {Promise<object>} ok:true 的 result；ok:false 或本地失败时以
   *   JsonlError 拒绝（协议错误沿用 router 的 code）。
   */
  request(method, params = {}, { timeoutMs = 120000, signal, internal = false } = {}) {
    if (typeof method !== 'string' || method === '') {
      return Promise.reject(new JsonlError('invalid_request', 'method must be a non-empty string'))
    }
    if (this.exited || (this.disposed && !internal)) {
      return Promise.reject(this.downError())
    }
    const child = this.child
    if (!child || !child.stdin || child.stdin.destroyed) {
      return Promise.reject(this.downError())
    }
    const id = this.nextId++
    return new Promise((resolve, reject) => {
      let settled = false
      let timer = null
      let onAbort = null
      const cleanup = () => {
        this.pending.delete(id)
        if (timer !== null) {
          clearTimeout(timer)
          timer = null
        }
        if (onAbort !== null && signal) {
          signal.removeEventListener('abort', onAbort)
          onAbort = null
        }
      }
      const finish = (settle, value) => {
        if (settled) return
        settled = true
        cleanup()
        settle(value)
      }
      const entry = {
        /** @param {{ok?: boolean, result?: object, error?: {code?: string, message?: string}}} msg */
        settle: (msg) => {
          if (msg.ok === true) {
            finish(resolve, msg.result ?? {})
          } else if (msg.ok === false) {
            const err = msg.error ?? {}
            const code = typeof err.code === 'string' && err.code !== '' ? err.code : 'protocol_error'
            const message = typeof err.message === 'string' && err.message !== ''
              ? err.message
              : 'serve returned an error without a message'
            finish(reject, new JsonlError(code, message))
          } else {
            finish(reject, new JsonlError(
              'protocol_error',
              'serve response carried no ok field',
              JSON.stringify(msg).slice(0, 160),
            ))
          }
        },
        /** @param {JsonlError} error */
        fail: (error) => finish(reject, error),
      }
      this.pending.set(id, entry)
      if (Number.isFinite(timeoutMs) && timeoutMs > 0) {
        timer = setTimeout(() => {
          entry.fail(new JsonlError(
            'timeout',
            `request ${method} timed out after ${timeoutMs}ms (the command may still be running on the router; this call only stopped waiting)`,
            'Increase requestTimeoutMs in the sekai-router plugin config if this command legitimately takes longer.',
          ))
        }, timeoutMs)
        timer.unref?.()
      }
      if (signal) {
        if (signal.aborted) {
          entry.fail(JsonlProcess.abortedError(method))
          return
        }
        onAbort = () => entry.fail(JsonlProcess.abortedError(method))
        signal.addEventListener('abort', onAbort, { once: true })
      }
      let writeError = null
      try {
        child.stdin.write(`${JSON.stringify({ id, method, params })}\n`, (error) => {
          if (error) entry.fail(new JsonlError('not_running', `failed to write request to serve: ${describeError(error)}`))
        })
      } catch (error) {
        writeError = error
      }
      if (writeError) {
        entry.fail(new JsonlError('not_running', `failed to write request to serve: ${describeError(writeError)}`))
      }
    })
  }

  /**
   * 优雅停止：shutdown 请求（短超时）→ 宽限 → kill → 再等 → 强杀。幂等。
   * @param {{method?: string, params?: object, requestTimeoutMs?: number, graceMs?: number}} [options]
   */
  async dispose({ method = 'shutdown', params = {}, requestTimeoutMs = 1500, graceMs = 1000 } = {}) {
    if (this.disposed) return
    this.disposed = true
    if (!this.exited && this.child) {
      try {
        await this.request(method, params, { timeoutMs: requestTimeoutMs, internal: true })
      } catch {
        // shutdown 无响应/失败：走宽限→kill 兜底。
      }
      if (!this.exited) await Promise.race([this.exitedPromise(), sleep(graceMs)])
      if (!this.exited) {
        try { this.child.kill() } catch { /* 已退出 */ }
        await Promise.race([this.exitedPromise(), sleep(graceMs)])
      }
      if (!this.exited) {
        try { this.child.kill('SIGKILL') } catch { /* 已退出 */ }
        await Promise.race([this.exitedPromise(), sleep(500)])
      }
    }
    // 即便没有子进程也等一小撮 exit 事件，保证调用方观察一致。
    if (!this.exited) await Promise.race([this.exitedPromise(), sleep(200)])
  }

  /** 直接杀进程（不发包）；一般调用方用 dispose 即可。 */
  kill() {
    if (this.exited || !this.child) return
    try { this.child.kill() } catch { /* 已退出 */ }
  }

  exitedPromise() {
    if (this.exited) return Promise.resolve()
    return new Promise((resolve) => this.exitWaiters.push(resolve))
  }

  /** 进程不在时的统一错误（区分 spawn 失败与普通退出）。 */
  downError() {
    if (this.exitInfo?.spawnError) {
      return new JsonlError(
        'spawn_error',
        `failed to spawn ${this.command}: ${describeError(this.exitInfo.spawnError)}`,
        this.spawnHint,
      )
    }
    if (this.disposed) {
      return new JsonlError('not_ready', 'serve process was disposed')
    }
    return new JsonlError(
      'not_ready',
      'serve process is not running (it exited; automatic restart may be pending)',
    )
  }

  /** 处理一行 stdout：JSON 解析 → id 关联 / 通知 / 丢弃告警。 */
  handleLine(line) {
    const trimmed = line.trim()
    if (trimmed === '') return
    let message = null
    try {
      message = JSON.parse(trimmed)
    } catch {
      this.warnLog(`dropping non-JSON line from serve: ${trimmed.slice(0, 160)}`)
      return
    }
    if (message === null || typeof message !== 'object' || Array.isArray(message)) {
      this.warnLog(`dropping non-object line from serve: ${trimmed.slice(0, 160)}`)
      return
    }
    if (typeof message.event === 'string') {
      if (this.disposed) return
      try {
        this.onEvent?.(message)
      } catch (error) {
        this.warnLog(`onEvent handler failed: ${describeError(error)}`)
      }
      return
    }
    const id = message.id
    if (typeof id !== 'number' || !Number.isInteger(id) || !this.pending.has(id)) {
      this.warnLog(`dropping unmatched line from serve: ${trimmed.slice(0, 160)}`)
      return
    }
    const entry = this.pending.get(id)
    this.pending.delete(id)
    entry.settle(message)
  }

  /** 进程退出（或 spawn 失败）的统一入口，恰好生效一次。 */
  markExited(info) {
    if (this.exited) return
    this.exited = true
    this.exitInfo = info
    for (const entry of this.pending.values()) {
      entry.fail(new JsonlError(
        'not_running',
        `serve process exited before responding (${describeExitInfo(info)})`,
      ))
    }
    this.pending.clear()
    const waiters = this.exitWaiters
    this.exitWaiters = []
    for (const resolve of waiters) resolve()
    // 关闭残留流，尽快释放句柄。
    for (const stream of [this.child?.stdout, this.child?.stderr]) {
      try { stream?.destroy?.() } catch { /* 已销毁 */ }
    }
    try { this.child?.stdin?.end?.() } catch { /* 已销毁 */ }
    try {
      this.onExit?.(info)
    } catch (error) {
      this.warnLog(`onExit handler failed: ${describeError(error)}`)
    }
  }

  /**
   * 本地中止错误：中止只停止本地等待，router 侧命令照跑。
   * @param {string} method
   * @returns {JsonlError}
   */
  static abortedError(method) {
    return new JsonlError(
      'aborted',
      `request ${method} aborted locally; the forwarded command keeps running on the router`,
      'Only this call stopped waiting. The router executes commands serially — poll with a later call if you need the outcome.',
    )
  }
}
