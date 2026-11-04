#!/usr/bin/env node
/**
 * scripts/smoke.mjs — plugins/sekai-dsh 的无 Rust 冒烟。
 *
 * 不需要真实二进制：
 * - 用 node 写一个假 serve（按冻结的 JSONL 协议 v1 回放 ready/link_state/
 *   worker_online 通知与 link/workers/tool_call/task/policy.get/policy.set/
 *   status/shutdown 响应），验证 router 侧桥接（JsonlProcess 分帧、请求-
 *   响应、通知、丢弃告警、超时、本地中止、dispose、崩溃重启、autoLink
 *   重放、四个 sekai_* 工具、错误信封、schema 卫生）；
 * - 用一个假 worker 脚本（打印阶段化 stderr 后退出/长驻）验证 worker 侧
 *   的生命周期与状态工具（重启退避、环形 stderr、spawn 失败告警）。
 *
 * 全部断言通过 exit 0；任何失败 exit 1。
 */
import { strict as assert } from 'node:assert'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  JsonlError,
  JsonlProcess,
  RESTART_BACKOFF_INITIAL_MS,
  RingBuffer,
  nextBackoffMs,
  resolveBin,
} from '../lib/shared.js'
import { SekaiRouterPlugin } from '../lib/router.js'
import { SekaiWorkerPlugin } from '../lib/worker.js'

// ---------------------------------------------------------------------------
// 假进程脚本（写入临时目录后以 node 运行）
// ---------------------------------------------------------------------------

/** 假 serve：按 JSONL 协议 v1 回放。注意保持无反引号/无 ${ }。 */
const FAKE_SERVE = `
import readline from 'node:readline'
const write = (obj) => process.stdout.write(JSON.stringify(obj) + '\\n')
let policy = { allow_write: false, allow_tasks: false, allow_admin: false, worker_filter: null }
let linked = false
let linkDesc = ''
write({ event: 'ready' })
process.stdout.write('this-line-is-not-json' + '\\n') // 客户端必须丢弃并告警
write({ event: 'bogus_event' })    // 未知名通知：router 侧应告警不崩
const rl = readline.createInterface({ input: process.stdin })
rl.on('line', (line) => {
  let msg = null
  try { msg = JSON.parse(line) } catch { return }
  if (!msg || typeof msg !== 'object' || typeof msg.method !== 'string') return
  const id = msg.id
  const params = msg.params ?? {}
  if (msg.method === 'link') {
    linked = true
    linkDesc = (params.mode ?? 'tcp') + ' ' + (params.listen_addr ?? 'dynamic')
    write({ id, ok: true, result: { mode: params.mode ?? 'tcp', listen_addr: params.listen_addr ?? null, linked: true } })
    setTimeout(() => write({ event: 'link_state', desc: linkDesc, ready: true }), 10)
    setTimeout(() => write({ event: 'worker_online', worker_id: 'pc-worker', worker: { worker_id: 'pc-worker', online: true } }), 20)
    return
  }
  if (msg.method === 'link_off') {
    linked = false
    write({ id, ok: true, result: { linked: false } })
    write({ event: 'link_state', desc: '', ready: false })
    return
  }
  if (msg.method === 'workers') {
    if (!linked) { write({ id, ok: false, error: { code: 'link_error', message: 'not linked' } }); return }
    write({ id, ok: true, result: { count: 1, workers: [
      { worker_id: 'pc-worker', online: true, boot_id: 1, firmware: 'fake-1.0.0', uptime_ms: 42, unix_ms: 0,
        tools: ['sekai.info', 'gpio.info', 'gpio.read', 'gpio.write'], tasks: ['blink'], workers: [], active_tasks: [] },
    ] } })
    return
  }
  if (msg.method === 'tool_call' || msg.method === 'task') {
    if (!linked) { write({ id, ok: false, error: { code: 'link_error', message: 'not linked' } }); return }
    const name = msg.method === 'tool_call' ? params.tool : params.task
    const wantsWrite = name === 'gpio.write' || name === 'gpio.config'
    if (wantsWrite && !policy.allow_write) {
      write({ id, ok: true, result: { ok: false, worker_id: params.worker_id,
        kind: msg.method === 'tool_call' ? 'tool' : 'task', name, cmd_id: 1, elapsed_ms: 3,
        payload: '', encoding: 'utf8',
        error: { code: 'policy_denied', message: name + ' not allowed by policy', retryable: true } } })
      return
    }
    write({ id, ok: true, result: { ok: true, worker_id: params.worker_id,
      kind: msg.method === 'tool_call' ? 'tool' : 'task', name, cmd_id: 7, elapsed_ms: 12,
      payload: 'echo:' + (params.args ?? ''), encoding: 'utf8' } })
    return
  }
  if (msg.method === 'policy.get') { write({ id, ok: true, result: { ...policy } }); return }
  if (msg.method === 'policy.set') {
    if (typeof params.allow_write === 'boolean') policy.allow_write = params.allow_write
    if (typeof params.allow_tasks === 'boolean') policy.allow_tasks = params.allow_tasks
    if (typeof params.allow_admin === 'boolean') policy.allow_admin = params.allow_admin
    if ('worker_filter' in params) policy.worker_filter = params.worker_filter ?? null
    write({ id, ok: true, result: { ...policy } })
    return
  }
  if (msg.method === 'status') {
    write({ id, ok: true, result: { ready: linked, mode: 'tcp', link: linked ? linkDesc : '',
      workers: linked ? 1 : 0, policy: 'write=' + (policy.allow_write ? 'allow' : 'deny'),
      version: 'fake-serve-0.1.0', pid: process.pid } })
    return
  }
  if (msg.method === 'slow') return // 永不回应：验证超时/本地中止
  if (msg.method === 'shutdown') {
    write({ id, ok: true, result: { bye: true } })
    setTimeout(() => process.exit(0), 15)
    return
  }
  write({ id, ok: false, error: { code: 'unknown_method', message: 'unknown method: ' + msg.method } })
})
`

/** 假 worker：阶段化中文 stderr 进度行；按环境变量决定寿命/长驻。 */
const FAKE_WORKER = `
const lifetime = Number(process.env.FAKE_WORKER_LIFETIME_MS ?? 250)
const exitCode = Number(process.env.FAKE_WORKER_EXIT ?? 0)
process.stderr.write('[worker] 阶段1：连接路由器 ' + (process.argv[2] ?? '') + '\\n')
setTimeout(() => process.stderr.write('[worker] 阶段2：注册工具 gpio.*\\n'), Math.floor(lifetime / 3))
setTimeout(() => process.stderr.write('[worker] 阶段3：进入命令循环\\n'), Math.floor((lifetime * 2) / 3))
setTimeout(() => process.exit(exitCode), lifetime)
if (process.env.FAKE_WORKER_HOLD === '1') setInterval(() => {}, 1000)
// stdout 保持静默（真实 worker connect 模式 stdout 无输出）
`

// ---------------------------------------------------------------------------
// 微型测试架
// ---------------------------------------------------------------------------

const tests = []
function test(name, fn) {
  tests.push({ name, fn })
}

async function waitFor(predicate, { timeoutMs = 10000, label = 'condition' } = {}) {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    const value = predicate()
    if (value) return value
    if (Date.now() > deadline) throw new Error(`timeout waiting for ${label}`)
    await new Promise((resolve) => setTimeout(resolve, 25))
  }
}

/** 模拟 Cordis 宿主：logger、tools 注册表、延迟执行的 effect 生成器。 */
function createFakeHost() {
  const tools = new Map()
  const disposers = []
  const entries = []
  const logger = {
    info: (m) => entries.push(['info', String(m)]),
    warn: (m) => entries.push(['warn', String(m)]),
    error: (m) => entries.push(['error', String(m)]),
  }
  const generators = []
  const toolsService = {
    register(def) {
      if (tools.has(def.name)) throw new Error('duplicate tool: ' + def.name)
      tools.set(def.name, def)
      return () => tools.delete(def.name)
    },
  }
  const ctx = {
    logger,
    get(name) {
      return name === 'tools' ? toolsService : undefined
    },
    effect(generator) {
      generators.push(generator)
    },
  }
  return {
    ctx,
    tools,
    entries,
    logger,
    /** 逐个执行收集到的 effect 生成器（收集 yield 的清理函数）。 */
    start() {
      for (const generator of generators) {
        const it = generator()
        for (;;) {
          const next = it.next()
          if (next.done) break
          if (typeof next.value === 'function') disposers.push(next.value)
        }
      }
    },
    teardown() {
      for (const dispose of disposers.splice(0).reverse()) dispose()
      generators.length = 0
    },
  }
}

/** 宿主 JSON Schema 卫生检查：无 type 数组，required ⊆ properties。 */
function assertSchemaHygiene(schema, path) {
  assert.ok(schema !== null && typeof schema === 'object', `${path}: schema node must be an object`)
  if ('type' in schema) {
    assert.equal(typeof schema.type, 'string', `${path}.type must be a single string (type arrays are rejected by the host)`)
  }
  if (schema.properties !== undefined) {
    for (const [name, child] of Object.entries(schema.properties)) {
      assertSchemaHygiene(child, `${path}.properties.${name}`)
    }
  }
  if (schema.items !== undefined) assertSchemaHygiene(schema.items, `${path}.items`)
  if (schema.additionalProperties !== undefined && schema.additionalProperties !== null && typeof schema.additionalProperties === 'object') {
    assertSchemaHygiene(schema.additionalProperties, `${path}.additionalProperties`)
  }
  if (Array.isArray(schema.required)) {
    const names = Object.keys(schema.properties ?? {})
    for (const key of schema.required) {
      assert.ok(names.includes(key), `${path}: required key "${key}" must appear in properties`)
    }
  }
}

function assertToolDefinition(def) {
  assert.equal(typeof def.name, 'string', 'tool name')
  assert.equal(typeof def.description, 'string', 'tool description')
  assert.equal(def.parameters.additionalProperties, false, `${def.name}: parameters.additionalProperties must be false`)
  assertSchemaHygiene(def.parameters, `${def.name}.parameters`)
  assert.ok(def.output && typeof def.output.schema === 'object', `${def.name}: output.schema`)
  assert.equal(typeof def.output.render, 'function', `${def.name}: output.render`)
  assertSchemaHygiene(def.output.schema, `${def.name}.output.schema`)
  assert.equal(typeof def.execute, 'function', `${def.name}: execute`)
}

// ---------------------------------------------------------------------------
// 测试定义
// ---------------------------------------------------------------------------

test('RingBuffer：超容量丢最旧，顺序保持', () => {
  const ring = new RingBuffer(3)
  for (const item of ['a', 'b', 'c', 'd', 'e']) ring.push(item)
  assert.deepEqual(ring.toArray(), ['c', 'd', 'e'])
  ring.clear()
  assert.deepEqual(ring.toArray(), [])
})

test('nextBackoffMs：翻倍且封顶', () => {
  assert.equal(nextBackoffMs(500, 30000), 1000)
  assert.equal(nextBackoffMs(20000, 30000), 30000)
  assert.equal(nextBackoffMs(30000, 30000), 30000)
  assert.equal(RESTART_BACKOFF_INITIAL_MS, 500)
})

test('resolveBin：config 显式 > 环境变量 > 默认(PATH)', () => {
  const envKey = 'SEKAI_SMOKE_BIN'
  const saved = process.env[envKey]
  try {
    delete process.env[envKey]
    assert.equal(resolveBin('/abs/sekai-router.exe', envKey, 'sekai-router'), '/abs/sekai-router.exe')
    assert.equal(resolveBin('sekai-router', envKey, 'sekai-router'), 'sekai-router')
    process.env[envKey] = '/from-env/sekai-router.exe'
    assert.equal(resolveBin('sekai-router', envKey, 'sekai-router'), '/from-env/sekai-router.exe')
    assert.equal(resolveBin('/abs/sekai-router.exe', envKey, 'sekai-router'), '/abs/sekai-router.exe')
  } finally {
    if (saved === undefined) delete process.env[envKey]
    else process.env[envKey] = saved
  }
})

function defineJsonlProcessTests(servePath) {
  test('JsonlProcess：ready 通知、垃圾行丢弃告警、请求-响应往返', async () => {
    const events = []
    const warns = []
    const exits = []
    const logger = { info: () => {}, warn: (m) => warns.push(String(m)), error: () => {} }
    const proc = new JsonlProcess({
      command: process.execPath,
      args: [servePath],
      logger,
      onEvent: (event) => events.push(event),
      onExit: (info) => exits.push(info),
    })
    try {
      await waitFor(() => events.some((event) => event.event === 'ready'), { label: 'ready notification' })
      await waitFor(() => warns.some((w) => w.includes('non-JSON')), { label: 'non-JSON line warn' })
      assert.ok(events.some((event) => event.event === 'bogus_event'), 'unknown notifications are still delivered to onEvent')

      const linked = await proc.request('link', { mode: 'tcp', listen_addr: '127.0.0.1:7421' })
      assert.equal(linked.linked, true)
      const workers = await proc.request('workers', {})
      assert.equal(workers.count, 1)
      assert.equal(workers.workers[0].worker_id, 'pc-worker')
      const call = await proc.request('tool_call', { worker_id: 'pc-worker', tool: 'gpio.read', args: 'x' })
      assert.equal(call.ok, true)
      assert.equal(call.payload, 'echo:x')

      await assert.rejects(
        proc.request('bogus', {}),
        (error) => error instanceof JsonlError && error.code === 'unknown_method',
      )
      const status = await proc.request('status', {})
      assert.equal(status.version, 'fake-serve-0.1.0')
    } finally {
      await proc.dispose()
    }
    assert.equal(proc.exited, true, 'serve should exit after shutdown')
    await proc.dispose() // 幂等
    await waitFor(() => exits.length === 1, { label: 'onExit fired once' })
  })

  test('JsonlProcess：超时与本地中止（exec.signal）', async () => {
    const proc = new JsonlProcess({ command: process.execPath, args: [servePath], logger: null })
    try {
      await waitFor(() => proc.pid !== null, { label: 'spawn' })
      await assert.rejects(
        proc.request('slow', {}, { timeoutMs: 150 }),
        (error) => error instanceof JsonlError && error.code === 'timeout' && /timed out/.test(error.message),
      )
      const controller = new AbortController()
      const slow = proc.request('slow', {}, { timeoutMs: 5000, signal: controller.signal })
      setTimeout(() => controller.abort(), 60)
      await assert.rejects(
        slow,
        (error) => error instanceof JsonlError && error.code === 'aborted' && /keeps running/.test(error.message),
      )
    } finally {
      await proc.dispose()
    }
  })

  test('JsonlProcess：dispose 期间未决请求被拒绝；崩溃触发 onExit 且未决请求拒绝', async () => {
    const exits = []
    const proc = new JsonlProcess({
      command: process.execPath,
      args: [servePath],
      logger: null,
      onExit: (info) => exits.push(info),
    })
    await waitFor(() => proc.pid !== null, { label: 'spawn' })
    const pending = proc.request('slow', {}, { timeoutMs: 5000 })
    const disposing = proc.dispose()
    await assert.rejects(
      pending,
      (error) => error instanceof JsonlError && (error.code === 'not_running' || error.code === 'not_ready'),
    )
    await disposing
    assert.equal(proc.exited, true)

    // 崩溃路径：外部 kill
    const exits2 = []
    const proc2 = new JsonlProcess({
      command: process.execPath,
      args: [servePath],
      logger: null,
      onExit: (info) => exits2.push(info),
    })
    await waitFor(() => proc2.pid !== null, { label: 'spawn 2' })
    const pending2 = proc2.request('slow', {}, { timeoutMs: 5000 })
    proc2.kill()
    await assert.rejects(pending2, (error) => error instanceof JsonlError && error.code === 'not_running')
    await waitFor(() => exits2.length === 1, { label: 'crash onExit' })
    assert.equal(proc2.exited, true)
  })

  test('JsonlProcess：spawn 失败 → spawn_error + onExit', async () => {
    const exits = []
    const warns = []
    const proc = new JsonlProcess({
      command: 'definitely-missing-sekai-serve-xyz',
      args: ['--serve'],
      logger: { info: () => {}, warn: (m) => warns.push(String(m)), error: () => {} },
      spawnHint: 'build it first',
      onExit: (info) => exits.push(info),
    })
    await waitFor(() => proc.exited, { timeoutMs: 5000, label: 'spawn failure' })
    assert.ok(exits.length === 1 && exits[0].spawnError, 'spawn error recorded in onExit')
    await assert.rejects(
      proc.request('status', {}),
      (error) => error instanceof JsonlError && error.code === 'spawn_error' && error.hint === 'build it first',
    )
  })
}

function defineRouterPluginTests(servePath) {
  test('router 插件：工具注册、schema 卫生、autoLink、四工具转发、策略解锁', async () => {
    const host = createFakeHost()
    const plugin = new SekaiRouterPlugin(host.ctx, {
      routerBin: 'unused-sekai-router',
      link: 'tcp',
      listenAddr: '127.0.0.1:7421',
      requestTimeoutMs: 4000,
    })
    // seam：把真实 spawn 换成假 serve（node fake-serve.mjs --serve --link tcp ...）
    plugin.serveSpawnTarget = (bin, args) => ({ command: process.execPath, args: [servePath, ...args] })
    host.start()
    try {
      assert.deepEqual(
        [...host.tools.keys()].sort(),
        ['sekai_list_workers', 'sekai_status', 'sekai_worker_task', 'sekai_worker_tool'],
      )
      for (const def of host.tools.values()) assertToolDefinition(def)

      // autoLink：ready → link → link_state → worker_online
      await waitFor(() => plugin.state.ready === true, { label: 'autoLink ready' })
      assert.ok(host.entries.some(([level, message]) => level === 'warn' && message.includes('unknown event')), 'unknown event warned')
      assert.match(plugin.state.linkDesc, /tcp 127\.0\.0\.1:7421/)
      await waitFor(() => plugin.state.workerCount === 1, { label: 'worker_online counted' })
      assert.deepEqual(plugin.state.policy, { allowWrite: false, allowTasks: false, allowAdmin: false, workerFilter: null })

      // sekai_status：合并 status 响应
      const status = await host.tools.get('sekai_status').execute({}, {})
      assert.equal(status.ready, true)
      assert.equal(status.mode, 'tcp')
      assert.equal(status.version, 'fake-serve-0.1.0')
      assert.equal(status.workerCount, 1)
      assert.equal(status.routerPid, plugin.state.routerPid)
      assert.ok(typeof status.routerPid === 'number')

      // sekai_list_workers：原样透传
      const workersResult = await host.tools.get('sekai_list_workers').execute({}, {})
      assert.equal(workersResult.count, 1)
      assert.equal(workersResult.workers[0].worker_id, 'pc-worker')
      assert.deepEqual(workersResult.workers[0].tools.slice(0, 2), ['sekai.info', 'gpio.info'])

      // sekai_worker_tool：成功路径原样透传
      const read = await host.tools.get('sekai_worker_tool').execute(
        { worker_id: 'pc-worker', tool: 'gpio.read', args: 'port=2' },
        {},
      )
      assert.equal(read.ok, true)
      assert.equal(read.name, 'gpio.read')
      assert.equal(read.payload, 'echo:port=2')

      // 默认策略：gpio.write → policy_denied 原样透传（router 侧拒绝）
      const denied = await host.tools.get('sekai_worker_tool').execute(
        { worker_id: 'pc-worker', tool: 'gpio.write', args: 'port=2 level=1' },
        {},
      )
      assert.equal(denied.ok, false)
      assert.equal(denied.error.code, 'policy_denied')

      // policy.set 解锁后成功
      await plugin.proc.request('policy.set', { allow_write: true })
      const granted = await host.tools.get('sekai_worker_tool').execute(
        { worker_id: 'pc-worker', tool: 'gpio.write', args: 'port=2 level=1' },
        {},
      )
      assert.equal(granted.ok, true)
      assert.equal(granted.name, 'gpio.write')
      await plugin.proc.request('policy.set', { allow_write: false })

      // sekai_worker_task：任务路径
      const task = await host.tools.get('sekai_worker_task').execute(
        { worker_id: 'pc-worker', task: 'blink', args: '', ddl_ms: 1000 },
        {},
      )
      assert.equal(task.ok, true)
      assert.equal(task.name, 'blink')
      assert.equal(task.kind, 'task')

      // exec.signal 预中止 → 本地 aborted 信封（router 侧照跑的提示）
      const controller = new AbortController()
      controller.abort()
      const aborted = await host.tools.get('sekai_worker_tool').execute(
        { worker_id: 'pc-worker', tool: 'gpio.read', args: '' },
        { signal: controller.signal },
      )
      assert.equal(aborted.error.code, 'aborted')
      assert.match(aborted.error.message, /keeps running/)

      // 崩溃重启：kill serve → 退避 500ms → 重生 → autoLink 重放
      const pidBefore = plugin.state.routerPid
      const crashedProc = plugin.proc
      crashedProc.child.kill()
      // 等待重启完成（新 pid + 重新 ready + worker 计数重建）
      await waitFor(
        () => plugin.state.ready === true
          && plugin.state.routerPid !== null
          && plugin.state.routerPid !== pidBefore
          && plugin.state.workerCount === 1,
        { timeoutMs: 15000, label: 'router respawn + autoLink replay' },
      )
      const status2 = await host.tools.get('sekai_status').execute({}, {})
      assert.equal(status2.ready, true)
      assert.equal(status2.workerCount, 1)
    } finally {
      host.teardown()
    }
    assert.equal(plugin.stopped, true)
    assert.equal(plugin.restartTimer, null)
    assert.equal(host.tools.size, 0, 'tools unregistered on teardown')
  })

  test('router 插件：二进制缺失 → spawn_error 信封（含可操作提示）', async () => {
    const host = createFakeHost()
    const plugin = new SekaiRouterPlugin(host.ctx, {
      routerBin: 'definitely-missing-sekai-router-xyz',
      requestTimeoutMs: 1000,
    })
    host.start()
    try {
      await waitFor(() => plugin.lastExitInfo?.spawnError, { timeoutMs: 5000, label: 'spawn error recorded' })
      const result = await host.tools.get('sekai_list_workers').execute({}, {})
      assert.equal(result.error.code, 'spawn_error')
      assert.match(result.error.hint ?? '', /cargo build --release/)
      assert.match(result.error.hint ?? '', /routerBin/)
      const status = await host.tools.get('sekai_status').execute({}, {})
      assert.equal(status.ready, false)
      assert.equal(status.error.code, 'spawn_error')
    } finally {
      host.teardown()
    }
  })

  test('router 插件：enabled=false 时不 spawn 不注册', () => {
    const host = createFakeHost()
    new SekaiRouterPlugin(host.ctx, { enabled: false }) // eslint-disable-line no-new
    host.start()
    assert.equal(host.tools.size, 0)
    host.teardown()
  })
}

function defineWorkerPluginTests(workerPath) {
  test('worker 插件：状态工具、stderr 环形缓冲、崩溃重启、稳定计数', async () => {
    const host = createFakeHost()
    const plugin = new SekaiWorkerPlugin(host.ctx, { workerBin: 'unused-sekai-worker', restartBackoffMaxMs: 5000 })
    plugin.workerSpawnTarget = (bin, args) => ({
      command: process.execPath,
      args: [workerPath, ...args],
      env: { ...process.env, FAKE_WORKER_HOLD: '1', FAKE_WORKER_LIFETIME_MS: '600000' },
    })
    host.start()
    try {
      assert.deepEqual([...host.tools.keys()], ['sekai_worker_status'])
      assertToolDefinition(host.tools.get('sekai_worker_status'))
      await waitFor(() => plugin.child !== null, { label: 'worker spawn' })
      await waitFor(() => plugin.lastStderr.toArray().length >= 1, { label: 'stderr line buffered' })

      const status = await host.tools.get('sekai_worker_status').execute({}, {})
      assert.equal(status.running, true)
      assert.equal(typeof status.pid, 'number')
      assert.equal(status.restarts, 0)
      assert.ok(status.uptimeMs >= 0)
      assert.ok(status.lastStderr.some((line) => line.includes('阶段1')))

      // 崩溃重启：kill → 500ms 退避 → 新 pid
      const pidBefore = status.pid
      plugin.child.kill()
      await waitFor(
        () => plugin.child !== null && plugin.child.pid !== pidBefore,
        { timeoutMs: 15000, label: 'worker respawn' },
      )
      assert.equal(plugin.restarts, 1)
      const status2 = await host.tools.get('sekai_worker_status').execute({}, {})
      assert.equal(status2.running, true)
      assert.equal(status2.restarts, 1)
      assert.notEqual(status2.pid, pidBefore)
      assert.ok(status2.lastStderr.length >= 1, 'ring buffer kept across restarts')
    } finally {
      host.teardown()
    }
    assert.equal(plugin.stopped, true)
    assert.equal(plugin.child, null)
  })

  test('worker 插件：短命 worker 反复退出 → 退避重启循环', async () => {
    const host = createFakeHost()
    const plugin = new SekaiWorkerPlugin(host.ctx, { workerBin: 'unused-sekai-worker', restartBackoffMaxMs: 2000 })
    plugin.workerSpawnTarget = (bin, args) => ({
      command: process.execPath,
      args: [workerPath, ...args],
      env: { ...process.env, FAKE_WORKER_LIFETIME_MS: '80' },
    })
    host.start()
    try {
      await waitFor(() => plugin.restarts >= 2, { timeoutMs: 20000, label: 'two backoff restarts' })
      const status = await host.tools.get('sekai_worker_status').execute({}, {})
      assert.equal(typeof status.restarts, 'number')
      assert.ok(status.restarts >= 2)
      assert.ok(Array.isArray(status.lastStderr))
    } finally {
      host.teardown()
    }
  })

  test('worker 插件：二进制缺失 → 告警 + 状态工具可用不崩', async () => {
    const host = createFakeHost()
    const plugin = new SekaiWorkerPlugin(host.ctx, { workerBin: 'definitely-missing-sekai-worker-xyz' })
    host.start()
    try {
      await waitFor(
        () => host.entries.some(([level, message]) => level === 'warn' && message.includes('spawn failed')),
        { timeoutMs: 5000, label: 'spawn failure warn' },
      )
      const status = await host.tools.get('sekai_worker_status').execute({}, {})
      assert.equal(status.running, false)
      assert.equal(status.restarts >= 1, true)
      assert.deepEqual(status.lastStderr, [])
    } finally {
      host.teardown()
    }
  })

  test('worker 插件：publishStatusTool=false 时不注册工具', () => {
    const host = createFakeHost()
    const plugin = new SekaiWorkerPlugin(host.ctx, { workerBin: 'unused-sekai-worker', publishStatusTool: false })
    plugin.workerSpawnTarget = (bin, args) => ({
      command: process.execPath,
      args: [workerPath, ...args],
      env: { ...process.env, FAKE_WORKER_HOLD: '1', FAKE_WORKER_LIFETIME_MS: '600000' },
    })
    host.start()
    assert.equal(host.tools.size, 0)
    host.teardown()
    assert.equal(plugin.stopped, true)
  })
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

async function main() {
  const dir = await mkdtemp(join(tmpdir(), 'sekai-dsh-smoke-'))
  const servePath = join(dir, 'fake-serve.mjs')
  const workerPath = join(dir, 'fake-worker.mjs')
  await writeFile(servePath, FAKE_SERVE, 'utf8')
  await writeFile(workerPath, FAKE_WORKER, 'utf8')

  defineJsonlProcessTests(servePath)
  defineRouterPluginTests(servePath)
  defineWorkerPluginTests(workerPath)

  const failures = []
  for (const { name, fn } of tests) {
    try {
      await fn()
      console.log(`ok - ${name}`)
    } catch (error) {
      failures.push({ name, error })
      console.error(`FAIL - ${name}`)
      console.error(`  ${error?.stack ?? error}`)
    }
  }

  await rm(dir, { recursive: true, force: true }).catch(() => {})

  if (failures.length > 0) {
    console.error(`\n${failures.length}/${tests.length} smoke tests failed`)
    process.exit(1)
  }
  console.log(`\nall ${tests.length} smoke tests passed`)
  process.exit(0)
}

main().catch((error) => {
  console.error(`smoke harness error: ${error?.stack ?? error}`)
  process.exit(1)
})
