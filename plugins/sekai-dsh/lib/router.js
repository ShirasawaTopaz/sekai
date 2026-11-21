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
import { registerStatus } from './status.js'
import {
  JsonlError,
  JsonlProcess,
  RESTART_BACKOFF_INITIAL_MS,
  describeError,
  describeExitInfo,
  nextBackoffMs,
  registerHotUpdate,
  renderJson,
  resolveBinTarget,
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
  policy: Object.freeze({
    allowWrite: false,
    allowTasks: false,
    allowedTasks: Object.freeze([]),
    allowAdmin: false,
    workerFilter: null,
  }),
  requestTimeoutMs: 120000,
})

/** router 重启退避上限（毫秒）；config 未暴露此键，属守护参数。 */
const RESTART_BACKOFF_MAX_MS = 30000

/**
 * 链路状态兜底轮询间隔（毫秒）。
 *
 * serve 的 `link_state` 通知只在 link/link_off 时发；TCP 对端（Worker）断开后
 * serve 的 poll 线程只 diff Worker 集合，插件 `ready` 会一直停在 true。
 * 这里定期用 `status` 探测做防御性补偿：旧 serve 也能工作；新 serve 补发
 * `link_state` 时 watcher 幂等（同状态不重复更新/告警）。
 */
const LINK_WATCH_INTERVAL_MS = 3000
/** 单次 status 探测超时（毫秒）：serve 串行 dispatcher 被长命令占用时不能误判。 */
const LINK_WATCH_TIMEOUT_MS = 2000
/** 连续多少次“status 显示未就绪”才认定链路断开（过滤瞬时抖动）。 */
const LINK_WATCH_DOWN_SAMPLES = 2

/** 长任务（pc.ask / pc.job）建议的最小 ddl_ms：Router 默认预算只有 1s。 */
export const LONG_TASK_DDL_HINT_MS = 60000

/** spawn 失败的可操作提示（工具信封与日志共用）。 */
export const ROUTER_SPAWN_HINT = 'Run `node scripts/install-dsh-plugin.mjs` in the sekai repo (it builds the binaries and deploys them to $DSH_HOME/sekai/bin). Fallbacks: `cargo build --release` for a local build; the router bin resolves as routerBin config > SEKAI_ROUTER_BIN > $DSH_HOME/sekai/bin/sekai-router(.exe) > <repo>/target/release/sekai-router(.exe) > PATH.'

/** 与 crate/sekai/src/sekai_wire.rs 的 MAX_DDL_MS 冻结一致。 */
export const MAX_DDL_MS = 300000

// Mirror the Router's trimmed wire-name grammar and distinct-name limit.
const TASK_NAME_PATTERN = /^\p{White_Space}*([a-z0-9._-]{1,48})\p{White_Space}*$/u
const MAX_ALLOWED_TASKS = 64

function normalizeAllowedTasks(value) {
  if (!Array.isArray(value)) throw new TypeError('policy.allowedTasks must be an array of Task names')
  const names = new Set()
  for (const [index, item] of value.entries()) {
    const name = typeof item === 'string' ? TASK_NAME_PATTERN.exec(item)?.[1] : undefined
    if (!name) {
      throw new TypeError(`policy.allowedTasks[${index}] must contain 1–48 lowercase ASCII letters, digits, or ._- after trimming`)
    }
    names.add(name)
    if (names.size > MAX_ALLOWED_TASKS) {
      throw new RangeError(`policy.allowedTasks may contain at most ${MAX_ALLOWED_TASKS} distinct Task names`)
    }
  }
  return [...names].sort()
}

function makeRouterConfig() {
  if (!z) return undefined
  return z.object({
    enabled: z.boolean().default(true)
      .description('Spawn and manage the sekai-router serve process. When off, no sekai_* tools are published.'),
    link: z.union(['tcp', 'stdio', 'mqtt']).default('tcp')
      .description('Link mode passed to `sekai-router --serve --link`.'),
    listenAddr: z.string().default(ROUTER_DEFAULTS.listenAddr)
      .description('Address the router listens on for workers, as HOST:PORT.'),
    routerBin: z.string().default(ROUTER_DEFAULTS.routerBin)
      .description('Explicit path to the sekai-router binary. Empty follows env, $DSH_HOME/sekai/bin, repo target, then PATH.'),
    extraArgs: z.array(z.string()).default([])
      .description('Extra arguments appended to `sekai-router --serve`.'),
    autoLink: z.boolean().default(true)
      .description('Re-send link and policy after the router restarts.'),
    policy: z.object({
      allowWrite: z.boolean().default(false)
        .description('Allow write-class worker tools such as gpio.write.'),
      allowTasks: z.boolean().default(false)
        .description('Allow all worker Tasks. Cannot be combined with a nonempty allowedTasks list.'),
      // Keep null distinct from omission so resolveRouterConfig can reject it.
      allowedTasks: z.array(z.string().pattern(TASK_NAME_PATTERN).required()).default(undefined)
        .description('Allow only these exact Task names, e.g. blink. Defaults to []. At most 64 distinct lowercase ASCII names of 1–48 characters after trimming. Requires a Router with --allow-task support.'),
      allowAdmin: z.boolean().default(false)
        .description('Allow admin operations such as set_time, workers.set, and jump.'),
      // '' 或省略 = 不限制 Worker。
      workerFilter: z.string()
        .description('Only forward to this worker id. Empty means every connected worker.'),
    }),
    requestTimeoutMs: z.number().step(1).min(1000).default(ROUTER_DEFAULTS.requestTimeoutMs)
      .description('Per-request timeout for forwarded tool calls, in milliseconds.'),
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
    if (raw.policy.allowedTasks !== undefined) {
      policy.allowedTasks = Object.freeze(normalizeAllowedTasks(raw.policy.allowedTasks))
    }
    const filter = raw.policy.workerFilter
    policy.workerFilter = typeof filter === 'string' && filter.trim() !== '' ? filter : null
  }
  const extraArgs = Array.isArray(raw?.extraArgs)
    ? raw.extraArgs.filter((item) => typeof item === 'string')
    : []
  if (policy.allowedTasks.length > 0) {
    if (policy.allowTasks) throw new TypeError('policy.allowTasks=true conflicts with nonempty policy.allowedTasks')
    if (extraArgs.some((arg) => /^(--allow-task|--allow-tasks)(=|$)/.test(arg))) {
      throw new TypeError('extraArgs Task flags conflict with nonempty policy.allowedTasks')
    }
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
    extraArgs,
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
    policy: { type: 'object', additionalProperties: true, description: 'Effective policy snapshot when available {allowWrite, allowTasks, allowedTasks, allowAdmin, workerFilter}. allowTasks means all Tasks; allowedTasks lists exact names.' },
    routerPid: { type: 'integer', description: 'sekai-router serve process id; omitted while not running.' },
    version: { type: 'string', description: 'Router version string; omitted until known.' },
    binaryGeneration: { type: 'string', description: 'Managed binary generation of the active Router; omitted for unmanaged binaries.' },
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
        description: `Optional execution budget in milliseconds; omit only for short commands. Long tasks (pc.job / pc.ask) must pass ddl_ms >= ${LONG_TASK_DDL_HINT_MS}: the Router default budget is 1000ms and would time out first.`,
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
    this.binaryTarget = null
    this.retiringProc = null
    this.retiringPromise = null
    this.stopPromise = null
    this.hotUpdate = null
    this.disposeLifecycle = null
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
    this.exactPolicyProc = null
    // 链路状态兜底探测（B11）：serve 不在 TCP 对端断开时补发 link_state 时，
    // 由 status 轮询把插件状态修正回来。
    this.watchTimer = null
    this.watchBusy = false
    this.linkDownSamples = 0

    const plugin = this
    ctx?.effect?.(function* () {
      if (!plugin.config.enabled) {
        plugin.log('info', 'sekai-router plugin is disabled (enabled:false); nothing spawned, no tools registered')
        return
      }
      if (plugin.stopped) return
      const update = registerHotUpdate(ctx, {
        key: 'router',
        component: 'sekai-router',
        restart: (generation) => plugin.restartForUpdate(generation),
      })
      plugin.hotUpdate = update
      const stopTools = plugin.registerTools()
      const stopStatus = registerStatus('router', () => plugin.state)
      plugin.disposeLifecycle = () => {
        const previous = plugin.proc ?? plugin.retiringProc
        try {
          update?.dispose({ isAlive: () => previous != null && !previous.exited })
        } finally {
          stopStatus()
          stopTools()
        }
      }
      plugin.start()
      plugin.startLinkWatch()
      yield () => plugin.stop()
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

  resolveBinary(generation = this.hotUpdate?.generation?.()) {
    return resolveBinTarget(this.config.routerBin, 'SEKAI_ROUTER_BIN', ROUTER_DEFAULTS.routerBin, {
      profileContext: this.ctx?.root?.get?.('profileContext') ?? this.ctx?.get?.('profileContext'),
      generation,
    })
  }

  /** Spawn through the same policy and event path for startup, crashes and updates. */
  start(binary) {
    if (this.stopped || !this.config.enabled || this.proc || this.retiringPromise) return
    if ((this.retiringProc && !this.retiringProc.exited) || this.hotUpdate?.canStart?.() === false) {
      this.scheduleRestart()
      return
    }
    this.retiringProc = null
    this.cancelRestart()
    const target = binary ?? this.resolveBinary()
    this.binaryTarget = target
    this.lastExitInfo = null
    const taskArgs = this.config.policy.allowedTasks.flatMap((name) => ['--allow-task', name])
    const argv = ['--serve', '--link', this.config.link, ...taskArgs, ...this.config.extraArgs]
    const spawnTarget = this.serveSpawnTarget(target.command, argv)
    let proc
    proc = new JsonlProcess({
      command: spawnTarget.command,
      args: spawnTarget.args,
      env: spawnTarget.env,
      logger: this.ctx?.logger ?? null,
      spawnHint: ROUTER_SPAWN_HINT,
      onEvent: (event) => {
        if (this.proc === proc) this.handleEvent(event)
      },
      onExit: (info) => {
        if (this.proc === proc) this.handleExit(info)
      },
    })
    this.proc = proc
    this.state.routerPid = proc.pid
    this.log('info', `sekai-router serve spawn: ${spawnTarget.command} ${spawnTarget.args.join(' ')} (pid ${proc.pid ?? '?'})`)
    // A synchronous spawn failure runs onExit before proc receives its identity.
    if (proc.exited) this.handleExit(proc.exitInfo)
  }

  async restartForUpdate(generation) {
    if (this.stopped || !this.config.enabled) return
    const current = this.binaryTarget
    const candidate = this.resolveBinary(generation)
    if (!candidate.generation || candidate.generation !== generation) return
    if (current?.source === 'config' || current?.source === 'env') return
    const sameBinary = current?.hash && candidate.hash
      ? current.hash === candidate.hash
      : current?.command === candidate.command && current?.generation === candidate.generation
    if (sameBinary) return

    this.cancelRestart()
    this.backoffMs = RESTART_BACKOFF_INITIAL_MS
    this.log('info', `sekai-router switching to managed binary generation ${generation}`)
    const exited = await this.retireProcess()
    if (this.stopped) return
    if (!exited) {
      this.scheduleRestart()
      throw new Error('sekai-router previous process exit was not confirmed; replacement was deferred')
    }
    this.start(candidate)
  }

  /**
   * serve 通知：ready → 重放 autoLink；link_state → 就绪与描述；
   * worker_online/offline → 计数。未知事件告警不崩。
   * @param {object} event
   */
  handleEvent(event) {
    switch (event?.event) {
      case 'ready':
        // With an exact allowlist, reset backoff only after the Router confirms it.
        if (this.config.policy.allowedTasks.length === 0) this.backoffMs = RESTART_BACKOFF_INITIAL_MS
        void this.runAutoLink()
        break
      case 'link_state':
        this.applyLinkState({
          ready: event.ready === true,
          desc: typeof event.desc === 'string' && event.desc !== '' ? event.desc : null,
          source: 'event',
        })
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

  /** Verify the startup Task policy, then optionally link and replay the configured policy. */
  async runAutoLink() {
    const proc = this.proc
    const policy = this.config.policy
    const needsExactPolicy = policy.allowedTasks.length > 0
    if (!proc || this.autoLinkBusy || this.stopped || (!this.config.autoLink && !needsExactPolicy)) return
    this.autoLinkBusy = true
    const request = (method, params = {}) => proc.request(method, params, { timeoutMs: this.config.requestTimeoutMs })
    try {
      if (needsExactPolicy) {
        this.exactPolicyProc = null
        const startup = await request('policy.get')
        this.assertExactPolicy(startup)
        if (proc !== this.proc) return
        this.state.policy = { ...this.state.policy, ...normalizeRouterPolicy(startup) }
      }
      if (!this.config.autoLink) {
        this.exactPolicyProc = proc
        this.backoffMs = RESTART_BACKOFF_INITIAL_MS
        return
      }
      const linkParams = { mode: this.config.link }
      if (this.config.link === 'tcp') linkParams.listen_addr = this.config.listenAddr
      else if (this.config.link === 'mqtt') linkParams.mqtt_addr = this.config.listenAddr
      const linked = await request('link', linkParams)
      if (proc !== this.proc) return
      if (linked && typeof linked === 'object') {
        if (typeof linked.mode === 'string' && linked.mode !== '') this.state.mode = linked.mode
        // serve 的 link 响应实际地址字段是 `addr`；`listen_addr`/`mqtt_addr`
        // 是请求侧参数名（旧版本/兼容路径也可能回显），按优先级兼容读取（B11）。
        const addr = firstString(linked.addr, linked.listen_addr, linked.mqtt_addr)
        this.applyLinkState({
          ready: true,
          desc: addr ? `${this.state.mode} ${addr}` : null,
          source: 'event',
        })
      }
      const policyParams = {}
      if (typeof policy.allowWrite === 'boolean') policyParams.allow_write = policy.allowWrite
      if (typeof policy.allowTasks === 'boolean') policyParams.allow_tasks = policy.allowTasks
      // Legacy Routers reject unknown policy.set fields. The boolean-only request
      // already expresses deny-all or allow-all when the list is empty.
      if (policy.allowedTasks.length > 0) policyParams.allowed_tasks = policy.allowedTasks
      if (typeof policy.allowAdmin === 'boolean') policyParams.allow_admin = policy.allowAdmin
      if (policy.workerFilter) policyParams.worker_filter = policy.workerFilter
      const applied = await request('policy.set', policyParams)
      if (needsExactPolicy) this.assertExactPolicy(applied)
      if (proc !== this.proc) return
      const merged = normalizeRouterPolicy(applied)
      if (merged) this.state.policy = { ...this.state.policy, ...merged }
      if (needsExactPolicy) {
        this.exactPolicyProc = proc
        this.backoffMs = RESTART_BACKOFF_INITIAL_MS
      }
      this.log('info', `sekai-router autoLink complete (mode=${this.state.mode}, desc=${this.state.linkDesc})`)
    } catch (error) {
      if (proc !== this.proc) return
      this.log('warn', `sekai-router autoLink failed: ${describeError(error)}`)
      if (needsExactPolicy) proc.kill()
    } finally {
      if (proc === this.proc) this.autoLinkBusy = false
    }
  }

  assertExactPolicy(raw) {
    const expected = this.config.policy.allowedTasks
    const actual = raw?.allowed_tasks
    if (raw?.allow_tasks !== false || !Array.isArray(actual) || actual.length !== expected.length
      || [...actual].sort().some((name, index) => name !== expected[index])) {
      throw this.unverifiedPolicyError()
    }
  }

  unverifiedPolicyError() {
    return new JsonlError(
      'policy_unverified',
      'Router has not confirmed the configured exact Task allowlist.',
      'Check that routerBin supports --allow-task and the allowed_tasks JSONL field.',
    )
  }

  exactPolicyPending() {
    return this.config.policy.allowedTasks.length > 0 && (!this.proc || this.proc !== this.exactPolicyProc)
  }

  /**
   * 应用一次链路状态（serve `link_state` 通知或 status 探测）。
   *
   * 幂等：只有 `ready`/`linkDesc` 真的变化才更新状态并记日志；重复的同类
   * 状态（新旧 serve 都可能发）不会重复告警或重复事件。
   * `ready=false` 同时清空链路描述并清零 Worker 计数（在线表已失效）。
   *
   * @param {{ready: boolean, desc?: string|null, source?: 'event'|'watch'}} update
   * @returns {boolean} 状态是否发生变化
   */
  applyLinkState({ ready, desc = null, source = 'event' }) {
    const nextReady = ready === true
    const nextDesc = nextReady ? (desc ?? this.state.linkDesc) : ''
    if (this.state.ready === nextReady && this.state.linkDesc === nextDesc) return false
    this.state.ready = nextReady
    this.state.linkDesc = nextDesc
    if (!nextReady) this.state.workerCount = 0
    this.linkDownSamples = 0
    const origin = source === 'watch' ? '（status 探测补发）' : ''
    this.log(
      nextReady ? 'info' : 'warn',
      `sekai-router link_state: ${nextDesc || (nextReady ? '(link up)' : '(offline)')} (ready=${nextReady})${origin}`,
    )
    return true
  }

  /** 启动链路状态兜底轮询（生命周期内常驻；进程不在时自动空转）。 */
  startLinkWatch() {
    if (this.watchTimer) return
    const timer = setInterval(() => {
      void this.pollLinkState()
    }, LINK_WATCH_INTERVAL_MS)
    timer.unref?.()
    this.watchTimer = timer
  }

  /** 停止兜底轮询（幂等）。 */
  stopLinkWatch() {
    if (!this.watchTimer) return
    clearInterval(this.watchTimer)
    this.watchTimer = null
  }

  /**
   * 用 `status` 探测链路真实状态（B11：serve 不补发 link_state 时的兜底）。
   *
   * 只在明确拿到 `ready !== true` 且连续 [`LINK_WATCH_DOWN_SAMPLES`] 次采样
   * 一致时才把状态改为断开：单次超时/失败可能只是 serve 的串行 dispatcher
   * 被长命令占用，不能误判。状态更新统一走 [`applyLinkState`]（幂等）。
   */
  async pollLinkState() {
    if (this.stopped || this.watchBusy) return
    const proc = this.proc
    if (!proc || proc.exited) return
    this.watchBusy = true
    try {
      const raw = await proc.request('status', {}, { timeoutMs: LINK_WATCH_TIMEOUT_MS })
      if (proc !== this.proc || !isPlainObject(raw)) return
      if (raw.ready === true) {
        this.linkDownSamples = 0
        const desc = firstString(raw.link, raw.link_desc, raw.linkDesc, raw.desc)
        if (!this.state.ready) {
          this.applyLinkState({ ready: true, desc, source: 'watch' })
        } else if (desc && desc !== this.state.linkDesc) {
          // 描述变化不影响就绪性、不发事件，只同步展示文本。
          this.state.linkDesc = desc
        }
        return
      }
      this.linkDownSamples += 1
      if (this.linkDownSamples >= LINK_WATCH_DOWN_SAMPLES) {
        this.linkDownSamples = 0
        this.applyLinkState({ ready: false, desc: null, source: 'watch' })
      }
    } catch {
      // 请求失败（超时 / not_ready 等）保持现状：进程退出由 handleExit 统一处理。
    } finally {
      if (proc === this.proc) this.watchBusy = false
    }
  }

  /** 插件级请求：进程不在时以结构化 not_ready/spawn_error 拒绝。 */
  async request(method, params, exec) {
    const proc = this.proc
    if (!proc || proc.exited) throw this.downError()
    if (method !== 'status' && this.exactPolicyPending()) throw this.unverifiedPolicyError()
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
    this.resetProcessState()
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
      if (this.restartTimer !== timer) return
      this.restartTimer = null
      this.start()
    }, delay)
    timer.unref?.()
    this.restartTimer = timer
  }

  cancelRestart() {
    clearTimeout(this.restartTimer)
    this.restartTimer = null
  }

  resetProcessState() {
    this.state.ready = false
    this.state.routerPid = null
    this.state.version = null
    this.state.mode = this.config.link
    this.state.linkDesc = ''
    this.state.workerCount = 0
    this.state.policy = { ...this.config.policy }
    this.autoLinkBusy = false
    this.exactPolicyProc = null
    this.watchBusy = false
    this.linkDownSamples = 0
  }

  retireProcess() {
    if (this.retiringPromise) return this.retiringPromise
    const proc = this.proc ?? this.retiringProc
    this.proc = null
    this.resetProcessState()
    if (!proc || proc.exited) {
      this.retiringProc = null
      return Promise.resolve(true)
    }
    this.retiringProc = proc
    this.retiringPromise = (async () => {
      try {
        if (await proc.dispose({ requestTimeoutMs: 1500, graceMs: 1000 })) {
          this.retiringProc = null
          return true
        }
      } catch (error) {
        this.log('warn', `sekai-router shutdown failed: ${describeError(error)}`)
      }
      this.log('warn', `sekai-router process ${proc.pid ?? '?'} exit was not confirmed`)
      return false
    })().finally(() => { this.retiringPromise = null })
    return this.retiringPromise
  }

  /** Clear synchronous state and join the child shutdown. */
  stop() {
    if (this.stopped) return this.stopPromise
    const disposeLifecycle = this.disposeLifecycle
    this.disposeLifecycle = null
    try { disposeLifecycle?.() } catch (error) {
      this.log('warn', `sekai-router lifecycle cleanup failed: ${describeError(error)}`)
    }
    this.stopped = true
    this.stopLinkWatch()
    this.cancelRestart()
    this.stopPromise = this.retireProcess()
    return this.stopPromise
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
            const proc = plugin.proc
            const result = await plugin.request('workers', {}, exec)
            if (proc === plugin.proc && result && typeof result.count === 'number') plugin.state.workerCount = result.count
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
        description: `Start a Task on one connected Sekai worker. args is raw UTF-8 task argument text; empty string for no args. The Router must authorize the exact Task name via policy.allowedTasks or authorize all Tasks via policy.allowTasks. Call sekai_list_workers first; worker ids and task names must come from its result. Long tasks (pc.job / pc.ask) need an explicit ddl_ms >= ${LONG_TASK_DDL_HINT_MS}; the Router default budget is 1000ms. Calls are forwarded serially through one sekai-router --serve process; aborting this call only stops waiting locally — the command keeps running on the worker.`,
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
    const proc = this.proc
    if (proc && this.binaryTarget?.generation) value.binaryGeneration = this.binaryTarget.generation
    try {
      const raw = await this.request('status', {})
      if (proc !== this.proc) throw this.downError()
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
        const pid = firstInteger(raw.pid)
        if (pid !== null) value.routerPid = pid
        const version = firstString(raw.version, raw.ver)
        if (version) {
          value.version = version
          state.version = version
        }
        let effectivePolicy = normalizeRouterPolicy(raw.policy)
        if (!effectivePolicy && proc === this.proc && proc && !proc.exited) {
          // Serve status normally returns a text summary. policy.get supplies
          // structured state even on legacy Routers and with autoLink disabled.
          try {
            const effective = await proc.request('policy.get', {}, { timeoutMs: LINK_WATCH_TIMEOUT_MS })
            effectivePolicy = normalizeRouterPolicy(effective)
          } catch {
            // Best effort: preserve an otherwise successful status result.
          }
        }
        if (effectivePolicy && proc === this.proc) {
          state.policy = { ...state.policy, ...effectivePolicy }
          value.policy = { ...state.policy }
        }
      }
      if (this.exactPolicyPending()) {
        value.ready = false
        value.error = this.unverifiedPolicyError().toJSON()
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

/** Convert structured Router policy to the plugin's camelCase snapshot. */
function normalizeRouterPolicy(raw) {
  if (!isPlainObject(raw)) return null
  const out = {}
  const allowWrite = raw.allow_write ?? raw.allowWrite
  const allowTasks = raw.allow_tasks ?? raw.allowTasks
  const allowAdmin = raw.allow_admin ?? raw.allowAdmin
  const allowedTasks = raw.allowed_tasks ?? raw.allowedTasks
  const workerFilter = Object.hasOwn(raw, 'worker_filter') ? raw.worker_filter : raw.workerFilter
  if (typeof allowWrite === 'boolean') out.allowWrite = allowWrite
  if (typeof allowTasks === 'boolean') out.allowTasks = allowTasks
  if (typeof allowAdmin === 'boolean') out.allowAdmin = allowAdmin
  if (workerFilter === null || typeof workerFilter === 'string') out.workerFilter = workerFilter || null
  if (Array.isArray(allowedTasks) && allowedTasks.every((name) => typeof name === 'string')) {
    out.allowedTasks = Object.freeze([...allowedTasks])
  } else if (typeof allowTasks === 'boolean') {
    // Legacy booleans have no exact-list mode.
    out.allowedTasks = ROUTER_DEFAULTS.policy.allowedTasks
  }
  return Object.keys(out).length > 0 ? out : null
}

// Cordis 类插件约定：默认导出即插件本体（参照 dsh-dcp 的 `export default DcpEngine`）。
export default SekaiRouterPlugin
