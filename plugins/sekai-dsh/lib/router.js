/**
 * router.js — Sekai Router 插件：spawn `sekai-router --serve`（JSONL over
 * stdio）并把 4 个 sekai_* 工具注册进 DSH 工具注册表。
 *
 * 类插件范式对齐 @aiwayds/dsh-dcp：static inject、static Config
 * （schemastery，peer 依赖）、constructor(ctx, config)、ctx.effect 生成器
 * 生命周期。schemastery 在仓库冒烟环境里不可用时 static Config 回退为
 * undefined，默认值由 resolveRouterConfig 内部兜底（两处默认保持一致）。
 *
 * 行为：
 * - ready 后 autoLink 发 `link` 请求与 `policy.set`（每次重启都重放）；
 * - 进程意外退出按 500ms→30s 指数退避重启；ready 后退避复位；
 * - dispose 顺序交给 JsonlProcess.dispose（shutdown→宽限→kill）。
 *
 * 工具错误面：spawn_error / not_ready / timeout / aborted / 协议错误原样
 * code，一律以 {error:{code,message,hint?}} 信封返回，不向 DSH 抛异常。
 *
 * @module @sekai/dsh-plugin/router
 */
import { createRequire } from 'node:module'
import {
  JsonlError,
  JsonlProcess,
  RESTART_BACKOFF_INITIAL_MS,
  describeError,
  describeExitInfo,
  nextBackoffMs,
  renderJson,
  resolveBin,
} from './shared.js'

const require = createRequire(import.meta.url)

/**
 * schemastery 仅 peer 依赖：宿主内可用则构建 static Config；仓库冒烟
 * （无 node_modules）下 require 失败回退 null，插件仍以内部默认值工作。
 */
function loadSchemastery() {
  try {
    return require('@deepseek-ai/schemastery')
  } catch {
    return null
  }
}
const z = loadSchemastery()

/** 内部默认值（static Config 的镜像，冒烟环境的权威来源）。 */
export const ROUTER_DEFAULTS = Object.freeze({
  enabled: true,
  link: 'tcp',
  listenAddr: '127.0.0.1:7421',
  routerBin: 'sekai-router',
  extraArgs: Object.freeze([]),
  autoLink: true,
  policy: Object.freeze({ allowWrite: false, allowTasks: false, allowAdmin: false, workerFilter: null }),
  requestTimeoutMs: 120000,
})

/** router 重启退避上限（毫秒）；config 未暴露此键，属守护参数。 */
const RESTART_BACKOFF_MAX_MS = 30000

/** spawn 失败的可操作提示（工具信封与日志共用）。 */
export const ROUTER_SPAWN_HINT = 'Run `cargo build --release` in the sekai repo, then either add target/release to PATH, set the routerBin config to the absolute path of target/release/sekai-router(.exe), or set SEKAI_ROUTER_BIN.'

/** 与 crate/sekai/src/sekai_wire.rs 的 MAX_DDL_MS 冻结一致。 */
export const MAX_DDL_MS = 300000

function makeRouterConfig() {
  if (!z) return undefined
  return z.object({
    enabled: z.boolean().default(true),
    link: z.union(['tcp', 'stdio', 'mqtt']).default('tcp'),
    listenAddr: z.string().default(ROUTER_DEFAULTS.listenAddr),
    routerBin: z.string().default(ROUTER_DEFAULTS.routerBin),
    extraArgs: z.array(z.string()).default([]),
    autoLink: z.boolean().default(true),
    policy: z.object({
      allowWrite: z.boolean().default(false),
      allowTasks: z.boolean().default(false),
      allowAdmin: z.boolean().default(false),
      // '' 或省略 = 不限制 Worker。
      workerFilter: z.string(),
    }),
    requestTimeoutMs: z.number().step(1).min(1000).default(ROUTER_DEFAULTS.requestTimeoutMs),
  })
}

/**
 * 内部配置兜底：任何来源（含无 schemastery 环境）的 config 都归一成
 * 带默认值的形状。策略键转成插件内部的 camelCase 快照。
 * @param {object} [raw]
 */
export function resolveRouterConfig(raw = {}) {
  const policy = { ...ROUTER_DEFAULTS.policy }
  if (raw && typeof raw.policy === 'object' && raw.policy !== null) {
    for (const key of ['allowWrite', 'allowTasks', 'allowAdmin']) {
      if (typeof raw.policy[key] === 'boolean') policy[key] = raw.policy[key]
    }
    const filter = raw.policy.workerFilter
    policy.workerFilter = typeof filter === 'string' && filter.trim() !== '' ? filter : null
  }
  return {
    enabled: raw?.enabled !== false,
    link: ['tcp', 'stdio', 'mqtt'].includes(raw?.link) ? raw.link : 'tcp',
    listenAddr: typeof raw?.listenAddr === 'string' && raw.listenAddr.trim() !== ''
      ? raw.listenAddr
      : ROUTER_DEFAULTS.listenAddr,
    routerBin: typeof raw?.routerBin === 'string' && raw.routerBin.trim() !== ''
      ? raw.routerBin
      : ROUTER_DEFAULTS.routerBin,
    extraArgs: Array.isArray(raw?.extraArgs)
      ? raw.extraArgs.filter((item) => typeof item === 'string')
      : [],
    autoLink: raw?.autoLink !== false,
    policy,
    requestTimeoutMs: Number.isFinite(raw?.requestTimeoutMs) && raw.requestTimeoutMs >= 1000
      ? Math.floor(raw.requestTimeoutMs)
      : ROUTER_DEFAULTS.requestTimeoutMs,
  }
}

// ---------------------------------------------------------------------------
// 工具 schema
// ---------------------------------------------------------------------------

/** 结构化失败信封的 JSON Schema 片段（工具 output 共用）。 */
const ERROR_PROPERTY = Object.freeze({
  type: 'object',
  description: 'Structured failure envelope {code, message, hint?}; present only when forwarding failed before or during the call. Protocol errors keep the router-side code (e.g. policy_denied, invalid_arguments).',
  properties: {
    code: { type: 'string' },
    message: { type: 'string' },
    hint: { type: 'string' },
  },
  additionalProperties: true,
})

const NO_ARGS_PARAMETERS = Object.freeze({
  type: 'object',
  properties: {},
  additionalProperties: false,
})

/**
 * 输出契约刻意放宽（additionalProperties:true、无 required）：这些工具的
 * 值是冻结 JSONL 协议响应的原样透传，Rust 侧在 v1 内新增字段不应被宿主
 * 判为 invalid output；输入 parameters 才是严格契约（additionalProperties:
 * false）。已知字段在 properties 里为模型侧提供文档。
 */
function passthroughOutput(documented) {
  return {
    schema: {
      type: 'object',
      additionalProperties: true,
      properties: documented,
    },
    render: renderJson,
  }
}

const STATUS_OUTPUT_SCHEMA = Object.freeze({
  type: 'object',
  additionalProperties: false,
  required: ['ready', 'mode', 'linkDesc', 'workerCount', 'policy'],
  properties: {
    ready: { type: 'boolean', description: 'Whether the router link is up and commands can be forwarded.' },
    mode: { type: 'string', description: 'Link mode: tcp | stdio | mqtt.' },
    linkDesc: { type: 'string', description: 'Human-readable link description (empty when not linked).' },
    workerCount: { type: 'integer', description: 'Connected worker count.' },
    policy: { type: 'object', additionalProperties: true, description: 'Effective policy snapshot {allowWrite, allowTasks, allowAdmin, workerFilter}.' },
    routerPid: { type: 'integer', description: 'sekai-router serve process id; omitted while not running.' },
    version: { type: 'string', description: 'Router version string; omitted until known.' },
    error: ERROR_PROPERTY,
  },
})

/** worker_tool / worker_task 的入参 schema（严格 JSON Schema）。 */
function workerCallParameters(kind) {
  return {
    type: 'object',
    additionalProperties: false,
    required: ['worker_id', kind, 'args'],
    properties: {
      worker_id: { type: 'string', description: 'Exact worker id from sekai_list_workers.' },
      [kind]: kind === 'tool'
        ? { type: 'string', description: 'Tool name advertised by the worker, e.g. gpio.write; must come from sekai_list_workers.' }
        : { type: 'string', description: 'Task name advertised by the worker; must come from sekai_list_workers.' },
      args: { type: 'string', description: "Raw UTF-8 argument text, e.g. 'gpio.info help'; empty string for no args." },
      ddl_ms: {
        type: 'integer',
        minimum: 1,
        maximum: MAX_DDL_MS,
        description: 'Optional execution budget in milliseconds; omit for the worker default.',
      },
    },
  }
}

const WORKERS_OUTPUT_DOCUMENTED = Object.freeze({
  count: { type: 'integer' },
  workers: {
    type: 'array',
    items: {
      type: 'object',
      additionalProperties: true,
      properties: {
        worker_id: { type: 'string' },
        online: { type: 'boolean' },
        boot_id: { type: 'integer' },
        firmware: { type: 'string' },
        uptime_ms: { type: 'integer' },
        tools: { type: 'array', items: { type: 'string' } },
        tasks: { type: 'array', items: { type: 'string' } },
        active_tasks: { type: 'array', items: { type: 'object', additionalProperties: true } },
      },
    },
  },
  error: ERROR_PROPERTY,
})

const COMMAND_RESULT_DOCUMENTED = Object.freeze({
  ok: { type: 'boolean' },
  worker_id: { type: 'string' },
  kind: { type: 'string' },
  name: { type: 'string' },
  cmd_id: { type: 'integer' },
  elapsed_ms: { type: 'integer' },
  payload: { type: 'string' },
  encoding: { type: 'string' },
  truncated: { type: 'boolean' },
  // phases 是阶段名数组（serve 侧 command_result_json 的 phase_names → Vec<String>），
  // items 必须是 string；声明成对象数组会被宿主输出校验拒绝（invalid output）。
  phases: { type: 'array', items: { type: 'string' } },
  error: ERROR_PROPERTY,
})

// ---------------------------------------------------------------------------
// 插件
// ---------------------------------------------------------------------------

/**
 * Sekai Router 桥接插件。
 *
 * 状态字段全用普通属性、回调全用闭包/箭头函数：Cordis 会把插件方法派发到
 * 派生接收者上执行（dsh-dcp 的符号键注释），`#private` 品牌检查在这种
 * 接收者上会误抛。
 */
export class SekaiRouterPlugin {
  /** 宿主内保证 tools 服务存在；注册仍走防御式 ctx.get('tools')。 */
  static inject = ['tools']

  static Config = makeRouterConfig()

  /**
   * @param {object} ctx Cordis context。
   * @param {object} [config] 插件配置（见 ROUTER_DEFAULTS）。
   */
  constructor(ctx, config = {}) {
    this.ctx = ctx
    this.config = resolveRouterConfig(config)
    this.proc = null
    this.state = {
      ready: false,
      mode: this.config.link,
      linkDesc: '',
      workerCount: 0,
      policy: { ...this.config.policy },
      routerPid: null,
      version: null,
    }
    this.lastExitInfo = null
    this.backoffMs = RESTART_BACKOFF_INITIAL_MS
    this.restartTimer = null
    this.stopped = false
    this.autoLinkBusy = false

    const plugin = this
    ctx?.effect?.(function* () {
      if (!plugin.config.enabled) {
        plugin.log('info', 'sekai-router plugin is disabled (enabled:false); nothing spawned, no tools registered')
        return
      }
      const stopTools = plugin.registerTools()
      plugin.start()
      yield () => {
        plugin.stop()
        stopTools()
      }
    }, 'sekai-router lifecycle')
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
   * serve 启动目标 seam：默认原样返回 (bin, args)。scripts/smoke.mjs 覆写
   * 它把真实 spawn 换成假 serve（node fake-serve.mjs --serve ... 的 argv
   * 形状问题由此绕开），可返回 {env} 覆盖子进程环境。
   * @param {string} bin @param {string[]} args
   * @returns {{command: string, args: string[], env?: object}}
   */
  serveSpawnTarget(bin, args) {
    return { command: bin, args }
  }

  /** spawn serve 并接线事件；重复调用安全（已有进程时忽略）。 */
  start() {
    if (this.stopped || this.proc) return
    const bin = resolveBin(this.config.routerBin, 'SEKAI_ROUTER_BIN', ROUTER_DEFAULTS.routerBin)
    const argv = ['--serve', '--link', this.config.link, ...this.config.extraArgs]
    const target = this.serveSpawnTarget(bin, argv)
    const proc = new JsonlProcess({
      command: target.command,
      args: target.args,
      env: target.env,
      logger: this.ctx?.logger ?? null,
      spawnHint: ROUTER_SPAWN_HINT,
      onEvent: (event) => this.handleEvent(event),
      onExit: (info) => this.handleExit(info),
    })
    this.proc = proc
    this.state.routerPid = proc.pid
    this.log('info', `sekai-router serve spawn: ${target.command} ${target.args.join(' ')} (pid ${proc.pid ?? '?'})`)
    if (proc.exited) {
      // 同步 spawn 失败：onExit 已在构造里触发 handleExit；这里只清理引用。
      if (this.proc === proc) this.proc = null
    }
  }

  /**
   * serve 通知：ready → 重放 autoLink；link_state → 就绪与描述；
   * worker_online/offline → 计数。未知事件告警不崩。
   * @param {object} event
   */
  handleEvent(event) {
    switch (event?.event) {
      case 'ready':
        // ready 即视为本次启动成功：退避复位，然后重放 link/policy。
        this.backoffMs = RESTART_BACKOFF_INITIAL_MS
        void this.runAutoLink()
        break
      case 'link_state':
        this.state.ready = event.ready === true
        if (typeof event.desc === 'string' && event.desc !== '') this.state.linkDesc = event.desc
        this.log('info', `sekai-router link_state: ${event.desc ?? ''} (ready=${this.state.ready})`)
        break
      case 'worker_online':
        this.state.workerCount += 1
        this.log('info', `sekai worker online: ${event.worker_id}`)
        break
      case 'worker_offline':
        this.state.workerCount = Math.max(0, this.state.workerCount - 1)
        this.log('info', `sekai worker offline: ${event.worker_id}`)
        break
      default:
        this.log('warn', `unknown event from serve: ${JSON.stringify(event).slice(0, 160)}`)
    }
  }

  /** autoLink：link 请求（按模式附地址）→ policy.set（只发已配置键）。 */
  async runAutoLink() {
    if (!this.config.autoLink || this.autoLinkBusy || this.stopped) return
    this.autoLinkBusy = true
    try {
      const linkParams = { mode: this.config.link }
      if (this.config.link === 'tcp') linkParams.listen_addr = this.config.listenAddr
      else if (this.config.link === 'mqtt') linkParams.mqtt_addr = this.config.listenAddr
      const linked = await this.request('link', linkParams)
      if (linked && typeof linked === 'object') {
        if (typeof linked.mode === 'string' && linked.mode !== '') this.state.mode = linked.mode
        const addr = linked.listen_addr ?? linked.mqtt_addr
        if (typeof addr === 'string' && addr !== '') this.state.linkDesc = `${this.state.mode} ${addr}`
        this.state.ready = true
      }
      const policyParams = {}
      const policy = this.config.policy
      if (typeof policy.allowWrite === 'boolean') policyParams.allow_write = policy.allowWrite
      if (typeof policy.allowTasks === 'boolean') policyParams.allow_tasks = policy.allowTasks
      if (typeof policy.allowAdmin === 'boolean') policyParams.allow_admin = policy.allowAdmin
      if (policy.workerFilter) policyParams.worker_filter = policy.workerFilter
      const applied = await this.request('policy.set', policyParams)
      const merged = normalizeSnakePolicy(applied)
      if (merged) this.state.policy = { ...this.state.policy, ...merged }
      this.log('info', `sekai-router autoLink complete (mode=${this.state.mode}, desc=${this.state.linkDesc})`)
    } catch (error) {
      this.log('warn', `sekai-router autoLink failed: ${describeError(error)}`)
    } finally {
      this.autoLinkBusy = false
    }
  }

  /** 插件级请求：进程不在时以结构化 not_ready/spawn_error 拒绝。 */
  async request(method, params, exec) {
    const proc = this.proc
    if (!proc || proc.exited) throw this.downError()
    return proc.request(method, params, {
      timeoutMs: this.config.requestTimeoutMs,
      signal: exec?.signal,
    })
  }

  /** 工具信封错误工厂（优先保留 JsonlError 的 code/hint）。 */
  errorEnvelope(error) {
    if (error instanceof JsonlError) return error.toJSON()
    return { code: 'internal_error', message: describeError(error) }
  }

  downError() {
    const info = this.lastExitInfo
    if (info?.spawnError) {
      return new JsonlError(
        'spawn_error',
        `sekai-router failed to spawn: ${describeError(info.spawnError)}`,
        ROUTER_SPAWN_HINT,
      )
    }
    return new JsonlError(
      'not_ready',
      'sekai-router serve is not running (crashed or starting up; automatic restart is scheduled with backoff)',
      'Wait a moment and retry. If it keeps failing, check the sekai-router plugin logs and the routerBin config.',
    )
  }

  /**
   * 进程退出处理：清理引用与就绪态，未主动停止时按退避重启。
   * @param {{code?: number|null, signal?: string|null, spawnError?: unknown}} info
   */
  handleExit(info) {
    if (this.stopped) return
    if (this.proc && !this.proc.exited) return // 迟到的旧进程事件：忽略
    this.proc = null
    this.lastExitInfo = info ?? null
    this.state.ready = false
    this.state.routerPid = null
    this.state.linkDesc = ''
    // router 进程死亡意味着旧链路与在线表全部失效：计数清零，重启后由
    // worker_online 通知与 workers 响应重建。
    this.state.workerCount = 0
    if (info?.spawnError) {
      this.log('warn', `sekai-router spawn failed: ${describeError(info.spawnError)} — ${ROUTER_SPAWN_HINT}`)
    } else {
      this.log('warn', `sekai-router serve exited unexpectedly (${describeExitInfo(info)}); restarting with backoff`)
    }
    this.scheduleRestart()
  }

  scheduleRestart() {
    if (this.stopped || this.restartTimer) return
    const delay = this.backoffMs
    this.backoffMs = nextBackoffMs(this.backoffMs, RESTART_BACKOFF_MAX_MS)
    this.log('info', `sekai-router restart scheduled in ${delay}ms`)
    const timer = setTimeout(() => {
      this.restartTimer = null
      if (!this.stopped) this.start()
    }, delay)
    timer.unref?.()
    this.restartTimer = timer
  }

  /** 停止：清定时器 + dispose（shutdown→宽限→kill）。幂等。 */
  stop() {
    if (this.stopped) return
    this.stopped = true
    if (this.restartTimer) {
      clearTimeout(this.restartTimer)
      this.restartTimer = null
    }
    const proc = this.proc
    this.proc = null
    this.state.ready = false
    this.state.routerPid = null
    if (proc && !proc.exited) {
      void proc.dispose({ requestTimeoutMs: 1500, graceMs: 1000 }).catch(() => {})
    }
  }

  // -------------------------------------------------------------------------
  // 工具注册（防御式：registry 缺失或注册失败只 warn，不抛）
  // -------------------------------------------------------------------------

  registerTools() {
    const registry = this.ctx?.get?.('tools') ?? this.ctx?.tools
    if (!registry || typeof registry.register !== 'function') {
      this.log('warn', 'sekai-router: no tool registry (ctx.tools) mounted; sekai_* tools were not published')
      return () => {}
    }
    const plugin = this
    const defs = [
      {
        name: 'sekai_status',
        description: 'Inspect the local Sekai router bridge: link readiness, link mode and description, connected worker count, effective policy, router pid and version. Calls are forwarded serially through one sekai-router --serve process.',
        parameters: NO_ARGS_PARAMETERS,
        output: { schema: STATUS_OUTPUT_SCHEMA, render: renderJson },
        async execute() {
          return plugin.statusValue()
        },
      },
      {
        name: 'sekai_list_workers',
        description: 'List connected Sekai workers with online state, boot_id, firmware, advertised tools/tasks and active tasks. Call this first; worker ids, tool names and task names must come from this result. Calls are forwarded serially through one sekai-router --serve process; aborting a call only stops waiting locally — the command keeps running.',
        parameters: NO_ARGS_PARAMETERS,
        output: passthroughOutput(WORKERS_OUTPUT_DOCUMENTED),
        async execute(_args, exec) {
          try {
            const result = await plugin.request('workers', {}, exec)
            if (result && typeof result.count === 'number') plugin.state.workerCount = result.count
            if (isPlainObject(result)) return result
            return { error: { code: 'protocol_error', message: 'unexpected workers response shape' } }
          } catch (error) {
            return { error: plugin.errorEnvelope(error) }
          }
        },
      },
      {
        name: 'sekai_worker_tool',
        description: "Invoke a ToolCall on one connected Sekai worker. args is raw UTF-8 tool argument text, e.g. 'gpio.info help'. Call sekai_list_workers first; worker ids and tool names must come from its result. Calls are forwarded serially through one sekai-router --serve process; aborting this call only stops waiting locally — the command keeps running on the worker.",
        parameters: workerCallParameters('tool'),
        output: passthroughOutput(COMMAND_RESULT_DOCUMENTED),
        async execute(args, exec) {
          return plugin.forwardCall('tool_call', 'tool', args, exec)
        },
      },
      {
        name: 'sekai_worker_task',
        description: 'Start a Task on one connected Sekai worker. args is raw UTF-8 task argument text; empty string for no args. Only allowed when tasks are explicitly enabled (policy.allowTasks). Call sekai_list_workers first; worker ids and task names must come from its result. Calls are forwarded serially through one sekai-router --serve process; aborting this call only stops waiting locally — the command keeps running on the worker.',
        parameters: workerCallParameters('task'),
        output: passthroughOutput(COMMAND_RESULT_DOCUMENTED),
        async execute(args, exec) {
          return plugin.forwardCall('task', 'task', args, exec)
        },
      },
    ]
    const disposers = []
    for (const def of defs) {
      try {
        const dispose = registry.register(def)
        if (typeof dispose === 'function') disposers.push(dispose)
      } catch (error) {
        this.log('warn', `sekai-router: tool ${def.name} could not be registered (${describeError(error)})`)
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

  /** sekai_status 的值：本地状态视图 + status 请求尽力合并。 */
  async statusValue() {
    const state = this.state
    const value = {
      ready: state.ready,
      mode: state.mode,
      linkDesc: state.linkDesc,
      workerCount: state.workerCount,
      policy: { ...state.policy },
    }
    if (typeof state.routerPid === 'number') value.routerPid = state.routerPid
    if (state.version) value.version = state.version
    try {
      const raw = await this.request('status', {})
      if (isPlainObject(raw)) {
        if (raw.ready === true || state.ready) value.ready = true
        if (typeof raw.mode === 'string' && raw.mode !== '') value.mode = raw.mode
        const desc = firstString(raw.link, raw.link_desc, raw.linkDesc, raw.desc)
        if (desc) value.linkDesc = desc
        const count = firstInteger(raw.worker_count, raw.workers, raw.workerCount, raw.count)
        if (count !== null) {
          value.workerCount = count
          state.workerCount = count
        }
        // Rust 侧 policy 摘要可能是对象或字符串：只接受对象；字符串摘要忽略
        //（本地快照已提供结构化策略）。
        if (isPlainObject(raw.policy)) value.policy = raw.policy
        const pid = firstInteger(raw.pid)
        if (pid !== null) value.routerPid = pid
        const version = firstString(raw.version, raw.ver)
        if (version) {
          value.version = version
          state.version = version
        }
      }
      return value
    } catch (error) {
      return { ...value, error: this.errorEnvelope(error) }
    }
  }

  /**
   * tool_call / task 的转发：参数镜像冻结协议，响应原样透传。
   * @param {'tool_call'|'task'} method
   * @param {'tool'|'task'} kind
   */
  async forwardCall(method, kind, args, exec) {
    const params = {
      worker_id: typeof args?.worker_id === 'string' ? args.worker_id : '',
      [kind]: typeof args?.[kind] === 'string' ? args[kind] : '',
      args: typeof args?.args === 'string' ? args.args : '',
    }
    if (Number.isFinite(args?.ddl_ms)) {
      params.ddl_ms = Math.max(1, Math.min(MAX_DDL_MS, Math.floor(args.ddl_ms)))
    }
    try {
      const result = await this.request(method, params, exec)
      if (isPlainObject(result)) return result
      return { error: { code: 'protocol_error', message: `unexpected ${method} response shape` } }
    } catch (error) {
      return { error: this.errorEnvelope(error) }
    }
  }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

function isPlainObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
}

function firstString(...values) {
  for (const value of values) {
    if (typeof value === 'string' && value !== '') return value
  }
  return null
}

function firstInteger(...values) {
  for (const value of values) {
    if (typeof value === 'number' && Number.isInteger(value)) return value
  }
  return null
}

/** policy.set 响应（snake_case）→ 插件策略快照（camelCase），无效返回 null。 */
function normalizeSnakePolicy(raw) {
  if (!isPlainObject(raw)) return null
  const out = {}
  let any = false
  if (typeof raw.allow_write === 'boolean') { out.allowWrite = raw.allow_write; any = true }
  if (typeof raw.allow_tasks === 'boolean') { out.allowTasks = raw.allow_tasks; any = true }
  if (typeof raw.allow_admin === 'boolean') { out.allowAdmin = raw.allow_admin; any = true }
  if (typeof raw.worker_filter === 'string') { out.workerFilter = raw.worker_filter || null; any = true }
  else if (raw.worker_filter === null) { out.workerFilter = null; any = true }
  return any ? out : null
}

// Cordis 类插件约定：默认导出即插件本体（参照 dsh-dcp 的 `export default DcpEngine`）。
export default SekaiRouterPlugin
