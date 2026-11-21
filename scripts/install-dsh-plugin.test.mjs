/**
 * install-dsh-plugin.test.mjs — scripts/install-dsh-plugin.mjs 的离线测试。
 *
 * 全离线：mkdtemp 临时 DSH_HOME + 假 dsh shim（Windows 用 .cmd 包 node，
 * unix 用带 shebang 的可执行 node 脚本）+ --bin-dir 假二进制，不跑 cargo、
 * 不碰真实 ~/.dsh、不写仓库。fixture 插件是小型 stub package，避免依赖
 * plugins/sekai-dsh 的并发改动；真实插件闭环由 verifier/Lead 演练。
 *
 * 运行：node --test scripts/install-dsh-plugin.test.mjs
 */

import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { after, before, test } from 'node:test'
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

import { main, PLUGIN_PACKAGE, parseArgs, resolveContext } from './install-dsh-plugin.mjs'

const IS_WIN = process.platform === 'win32'

/** 假 dsh shim（CommonJS，win 经 .cmd 包 node，unix 直接 shebang）。 */
const FAKE_DSH_SOURCE = String.raw`
'use strict'
const fs = require('node:fs')
const path = require('node:path')

const argv = process.argv.slice(2)
const home = process.env.DSH_HOME
if (!home) { console.error('fake-dsh: DSH_HOME 未注入'); process.exit(3) }
const profilesDir = path.join(home, 'profiles')

function die(message) { console.error('fake-dsh: ' + message); process.exit(2) }
function readJson(file) { return JSON.parse(fs.readFileSync(file, 'utf8')) }
function writeJson(file, value) { fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, JSON.stringify(value, null, 2) + '\n') }
function profilePackage(profile) { return path.join(profilesDir, profile, 'package.json') }
function getProfile() {
  const index = argv.findIndex((arg) => arg === '--profile' || arg === '-p')
  if (index === -1) die('缺少 --profile')
  const profile = argv[index + 1]
  if (!profile) die('--profile 缺少值')
  if (profile !== process.env.DSH_PROFILE) die('DSH_PROFILE 与 --profile 不一致')
  return profile
}
function copyTree(source, dest) {
  fs.mkdirSync(dest, { recursive: true })
  for (const entry of fs.readdirSync(source, { withFileTypes: true })) {
    const from = path.join(source, entry.name)
    const to = path.join(dest, entry.name)
    if (entry.isDirectory()) copyTree(from, to)
    else fs.copyFileSync(from, to)
  }
}
function register(pkg, profile, spec) {
  if (!pkg.dsh) pkg.dsh = {}
  if (!pkg.dsh.profile) pkg.dsh.profile = {}
  if (!Array.isArray(pkg.dsh.profile.bundles)) pkg.dsh.profile.bundles = []
  if (!pkg.dependencies) pkg.dependencies = {}
  pkg.dependencies['@sekai/dsh-plugin'] = spec
  if (!pkg.dsh.profile.bundles.includes('@sekai/dsh-plugin')) pkg.dsh.profile.bundles.push('@sekai/dsh-plugin')
  writeJson(profilePackage(profile), pkg)
}
function addPlugin(profile, spec, force) {
  if (!spec.startsWith('file:')) die('add 只支持 file: spec，收到：' + spec)
  const source = spec.slice('file:'.length)
  if (!fs.existsSync(path.join(source, 'package.json'))) die('插件目录不存在：' + source)
  const pkg = readJson(profilePackage(profile))
  if (process.env.FAKE_DSH_SKIP_REGISTER === '1') return
  const dest = path.join(profilesDir, profile, 'node_modules', '@sekai', 'dsh-plugin')
  fs.appendFileSync(path.join(profilesDir, profile, 'cli.log'), force ? 'force\n' : 'add\n')
  if (force && process.env.FAKE_DSH_FORCE_FAIL === '1') die('forced refresh failed')
  if ((!force && fs.existsSync(dest)) || (force && process.env.FAKE_DSH_STAY_STALE === '1')) return
  fs.rmSync(dest, { recursive: true, force: true })
  copyTree(source, dest)
  register(pkg, profile, spec)
}
function removePlugin(profile, name) {
  if (name !== '@sekai/dsh-plugin') die('remove 只支持 @sekai/dsh-plugin，收到：' + name)
  const pkg = readJson(profilePackage(profile))
  const dest = path.join(profilesDir, profile, 'node_modules', '@sekai', 'dsh-plugin')
  fs.rmSync(dest, { recursive: true, force: true })
  if (pkg.dependencies) delete pkg.dependencies['@sekai/dsh-plugin']
  if (pkg.dsh && pkg.dsh.profile && Array.isArray(pkg.dsh.profile.bundles)) {
    pkg.dsh.profile.bundles = pkg.dsh.profile.bundles.filter((item) => item !== '@sekai/dsh-plugin')
  }
  writeJson(profilePackage(profile), pkg)
}

if (argv.includes('--dump-config')) {
  const profile = getProfile()
  if (!fs.existsSync(profilePackage(profile))) die('profile 不存在：' + profile)
  if (process.env.FAKE_DSH_NO_SEKAI === '1') {
    console.log('plugins: []')
  } else {
    console.log('plugins:')
    console.log('  - id: sekai-router')
    console.log('    config: { link: tcp }')
    console.log('  - id: sekai-worker')
    console.log('    config: { workerId: pc-worker }')
  }
  process.exit(0)
}

if (argv[0] === 'plugin') {
  const profile = getProfile()
  if (!fs.existsSync(profilePackage(profile))) die('profile 不存在：' + profile)
  const index = argv.findIndex((arg) => arg === 'add' || arg === 'remove')
  if (index === -1) die('未知 plugin 子命令：' + argv.join(' '))
  const subcommand = argv[index]
  const target = argv[index + 1]
  if (!target) die(subcommand + ' 缺少目标')
  if (subcommand === 'add') addPlugin(profile, target, argv.includes('--force'))
  else removePlugin(profile, target)
  process.exit(0)
}

die('不支持的 argv：' + argv.join(' '))
`

/** stub 插件入口与真实插件共享候选 generation 解析契约。 */
const STUB_PLUGIN_INDEX = String.raw`
export { resolveBin } from './shared.js'

export class SekaiRouterPlugin {}
export class SekaiWorkerPlugin {}
`
const STUB_PLUGIN_SHARED = String.raw`
import { existsSync } from 'node:fs'
import path from 'node:path'

const SUFFIX = process.platform === 'win32' ? '.exe' : ''

export function resolveBin(configured, envKey, defaultBin, { env, generation } = {}) {
  if (!env?.DSH_HOME || !env?.DSH_PROFILE || !/^[a-f0-9]{64}$/.test(generation)) return defaultBin
  const candidate = path.join(env.DSH_HOME, 'sekai', 'bin', 'profiles', env.DSH_PROFILE, 'generations', generation, defaultBin + SUFFIX)
  return existsSync(candidate) ? candidate : defaultBin
}
`

let tempRoot
let dshHome
let profilesDir
let binDir
let emptyBinDir
let fixtureRepo
let fakeDshPath
let baseEnv

/** 创建 profile；bundles 初始含 @deepseek-ai/dsh-base（与真实新 profile 一致）。 */
function makeProfile(name) {
  const dir = path.join(profilesDir, name)
  mkdirSync(dir, { recursive: true })
  const pkg = {
    name: `dsh-profile-${name}`,
    private: true,
    dsh: { profile: { bundles: ['@deepseek-ai/dsh-base'] } },
  }
  writeFileSync(path.join(dir, 'package.json'), `${JSON.stringify(pkg, null, 2)}\n`)
  return dir
}

function readProfilePackage(name) {
  return JSON.parse(readFileSync(path.join(profilesDir, name, 'package.json'), 'utf8'))
}

function profilePluginsDir(name) {
  return path.join(profilesDir, name, 'node_modules', '@sekai', 'dsh-plugin')
}

function writeFakeBinary(dir, base) {
  const file = path.join(dir, IS_WIN ? `${base}.exe` : base)
  writeFileSync(file, `fake-${base}-binary\n`)
  if (!IS_WIN) chmodSync(file, 0o755)
  return file
}

function managedDir(profile) {
  return path.join(dshHome, 'sekai', 'bin', 'profiles', profile)
}

function markerPath(profile) {
  return path.join(managedDir(profile), 'current.json')
}

function readMarker(profile) {
  return JSON.parse(readFileSync(markerPath(profile), 'utf8'))
}

function generationDir(profile, generation = readMarker(profile).generation) {
  return path.join(managedDir(profile), 'generations', generation)
}

function deployedBinary(profile, base, generation) {
  return path.join(generationDir(profile, generation), IS_WIN ? `${base}.exe` : base)
}

function cliCalls(profile) {
  return readFileSync(path.join(profilesDir, profile, 'cli.log'), 'utf8').trim().split('\n')
}

/** 可注入的字符串输出流（替代 process.stdout/stderr）。 */
function sink() {
  let text = ''
  return {
    write(chunk) {
      text += String(chunk)
      return true
    },
    get text() {
      return text
    },
  }
}

function installArgv({ profile = 'testprof', binDir: binDirectory = binDir, extra = [] } = {}) {
  const argv = ['--dsh-home', dshHome, '--dsh', fakeDshPath, '--profile', profile]
  if (binDirectory !== undefined) argv.push('--bin-dir', binDirectory)
  return [...argv, ...extra]
}

async function runInstaller({ profile, binDir: binDirectory, extra = [], env = baseEnv } = {}) {
  const stdout = sink()
  const stderr = sink()
  const code = await main(installArgv({ profile, binDir: binDirectory, extra }), {
    env,
    platform: process.platform,
    home: tempRoot,
    cwd: fixtureRepo,
    repoRoot: fixtureRepo,
    stdout,
    stderr,
  })
  return { code, out: stdout.text, err: stderr.text }
}

before(() => {
  tempRoot = mkdtempSync(path.join(tmpdir(), 'sekai-install-test-'))
  dshHome = path.join(tempRoot, 'dsh-home')
  profilesDir = path.join(dshHome, 'profiles')
  mkdirSync(profilesDir, { recursive: true })
  for (const name of ['testprof', 'failprof', 'strictprof']) makeProfile(name)

  binDir = path.join(tempRoot, 'fake-bin')
  mkdirSync(binDir, { recursive: true })
  writeFakeBinary(binDir, 'sekai-router')
  writeFakeBinary(binDir, 'sekai-worker')
  emptyBinDir = path.join(tempRoot, 'empty-bin')
  mkdirSync(emptyBinDir, { recursive: true })

  // fixture 仓库：只有 plugins/sekai-dsh（stub），足以驱动安装/校验。
  fixtureRepo = path.join(tempRoot, 'fixture-repo')
  const pluginDir = path.join(fixtureRepo, 'plugins', 'sekai-dsh')
  mkdirSync(path.join(pluginDir, 'lib'), { recursive: true })
  writeFileSync(
    path.join(pluginDir, 'package.json'),
    `${JSON.stringify(
      {
        name: PLUGIN_PACKAGE,
        version: '0.0.0-test',
        type: 'module',
        main: './lib/index.js',
        exports: { '.': './lib/index.js' },
      },
      null,
      2,
    )}\n`,
  )
  writeFileSync(path.join(pluginDir, 'lib', 'index.js'), STUB_PLUGIN_INDEX)
  writeFileSync(path.join(pluginDir, 'lib', 'shared.js'), STUB_PLUGIN_SHARED)
  writeFileSync(path.join(pluginDir, 'cordis.patch.yml'), '[]\n')

  // 假 dsh：win 生成 .cmd 包装 node 实现；unix 生成带 shebang 的可执行脚本。
  const fakeDir = path.join(tempRoot, 'fake-dsh')
  mkdirSync(fakeDir, { recursive: true })
  writeFileSync(path.join(fakeDir, 'fake-dsh.cjs'), FAKE_DSH_SOURCE)
  if (IS_WIN) {
    fakeDshPath = path.join(fakeDir, 'fake-dsh.cmd')
    writeFileSync(fakeDshPath, '@echo off\r\nnode "%~dp0fake-dsh.cjs" %*\r\n')
  } else {
    fakeDshPath = path.join(fakeDir, 'fake-dsh')
    writeFileSync(fakeDshPath, `#!/usr/bin/env node\n${FAKE_DSH_SOURCE}`)
    chmodSync(fakeDshPath, 0o755)
  }

  // 基础环境：去掉真实 DSH_HOME/DSH_PROFILE，确保不会意外读写真实 ~/.dsh。
  baseEnv = { ...process.env }
  delete baseEnv.DSH_HOME
  delete baseEnv.DSH_PROFILE
})

after(() => {
  if (tempRoot) rmSync(tempRoot, { recursive: true, force: true })
})

test('parseArgs 解析短/长参数与布尔开关', () => {
  const args = parseArgs([
    '-p',
    'web',
    '--dsh-home',
    '/tmp/home',
    '--bin-dir=/tmp/bin',
    '--no-build',
    '--dry-run',
    '--strict',
  ])
  assert.equal(args.profile, 'web')
  assert.equal(args.dshHome, '/tmp/home')
  assert.equal(args.binDir, '/tmp/bin')
  assert.equal(args.noBuild, true)
  assert.equal(args.dryRun, true)
  assert.equal(args.strict, true)
  assert.equal(args.uninstall, false)
})

test('resolveContext：--dsh-home / --profile / --dsh 优先', () => {
  const args = parseArgs(['--dsh-home', dshHome, '--profile', 'testprof', '--dsh', fakeDshPath])
  const ctx = resolveContext(args, {
    platform: process.platform,
    env: { ...baseEnv, DSH_HOME: path.join(tempRoot, 'ignored-home'), DSH_PROFILE: 'ignored' },
    home: tempRoot,
    cwd: fixtureRepo,
    repoRoot: fixtureRepo,
  })
  assert.equal(ctx.dshHome, path.resolve(dshHome))
  assert.equal(ctx.profile, 'testprof')
  assert.equal(ctx.dsh, path.resolve(fakeDshPath))
  assert.equal(ctx.pluginDir, path.join(fixtureRepo, 'plugins', 'sekai-dsh'))
})

test('--help 退出 0 并列出关键参数', async () => {
  const stdout = sink()
  const stderr = sink()
  const code = await main(['--help'], {
    stdout,
    stderr,
    env: baseEnv,
    home: tempRoot,
    cwd: fixtureRepo,
  })
  assert.equal(code, 0)
  assert.match(stdout.text, /--profile/)
  assert.match(stdout.text, /--dry-run/)
  assert.match(stdout.text, /--uninstall/)
  assert.match(stdout.text, /--strict/)
  assert.equal(stderr.text, '')
})

test('--help 优先于未知参数：--help --bogus 仍退出 0', async () => {
  const stdout = sink()
  const stderr = sink()
  const code = await main(['--help', '--bogus'], {
    stdout,
    stderr,
    env: baseEnv,
    home: tempRoot,
    cwd: fixtureRepo,
  })
  assert.equal(code, 0)
  assert.match(stdout.text, /--profile/)
  assert.equal(stderr.text, '')
})

test('--dry-run 零写入：不建 sekai/bin、不改 profile', async () => {
  const profileFile = path.join(profilesDir, 'testprof', 'package.json')
  const before = readFileSync(profileFile, 'utf8')

  const { code, out } = await runInstaller({ extra: ['--dry-run'] })

  assert.equal(code, 0, out)
  assert.equal(readFileSync(profileFile, 'utf8'), before)
  assert.equal(existsSync(path.join(dshHome, 'sekai', 'bin')), false)
  assert.equal(existsSync(profilePluginsDir('testprof')), false)
  assert.match(out, /\[dry-run\]/)
  assert.match(out, /未执行构建、未写盘、未调用 dsh/)
})

test('profile 不存在：非零退出并列出可用 profile', async () => {
  const { code, err } = await runInstaller({ profile: 'ghost' })
  assert.notEqual(code, 0)
  assert.match(err, /profile "ghost" 不存在/)
  assert.match(err, /testprof/)
  assert.match(err, /hint:/)
})

test('缺二进制：非零退出并给可操作 hint', async () => {
  const { code, err } = await runInstaller({
    binDir: emptyBinDir,
    extra: ['--no-build'],
  })
  assert.notEqual(code, 0)
  assert.match(err, /缺少二进制/)
  assert.match(err, /hint:/)
  assert.match(err, /--bin-dir/)
})

test('首次安装成功：依赖 file:、bundles 含、部署两文件、resolveBin 闭环', async () => {
  const { code, out, err } = await runInstaller()
  assert.equal(code, 0, `stderr=${err}`)

  const pkg = readProfilePackage('testprof')
  const pluginDir = path.join(fixtureRepo, 'plugins', 'sekai-dsh')
  assert.equal(pkg.dependencies[PLUGIN_PACKAGE], `file:${path.resolve(pluginDir)}`)
  assert.equal(pkg.dsh.profile.bundles.filter((entry) => entry === PLUGIN_PACKAGE).length, 1)
  assert.ok(pkg.dsh.profile.bundles.includes('@deepseek-ai/dsh-base'))

  const marker = readMarker('testprof')
  assert.deepEqual(Object.keys(marker).sort(), ['generation', 'version'])
  assert.equal(marker.version, 1)
  assert.match(marker.generation, /^[a-f0-9]{64}$/)
  const manifest = JSON.parse(readFileSync(path.join(generationDir('testprof'), 'manifest.json'), 'utf8'))
  assert.equal(manifest.version, 1)
  assert.match(manifest.bundle, /^[a-f0-9]{64}$/)
  for (const base of ['sekai-router', 'sekai-worker']) {
    const file = deployedBinary('testprof', base)
    const bytes = readFileSync(file)
    assert.equal(bytes.toString(), `fake-${base}-binary\n`)
    assert.equal(manifest.binaries[base], createHash('sha256').update(bytes).digest('hex'))
    assert.equal(existsSync(path.join(dshHome, 'sekai', 'bin', path.basename(file))), false)
  }
  assert.deepEqual(cliCalls('testprof'), ['add'])

  assert.ok(existsSync(path.join(profilePluginsDir('testprof'), 'package.json')))

  assert.match(out, /\[ok\] 校验 a/)
  assert.match(out, /\[ok\] 校验 b/)
  assert.match(out, /\[ok\] 校验 c：sekai-router 零配置解析命中/)
  assert.match(out, /\[ok\] 校验 c：sekai-worker 零配置解析命中/)
  assert.match(out, /\[ok\] 校验 d/)
  assert.match(out, /已发布 generation/)
  assert.match(out, /只验证了磁盘上的安装/)
})

test('重复安装幂等：依赖、CLI 调用和发布标记不变', async () => {
  const first = readProfilePackage('testprof')
  const marker = readMarker('testprof')
  const calls = cliCalls('testprof')
  const { code, err } = await runInstaller()
  assert.equal(code, 0, `stderr=${err}`)
  assert.deepEqual(readProfilePackage('testprof'), first)
  assert.deepEqual(readMarker('testprof'), marker)
  assert.deepEqual(cliCalls('testprof'), calls)
})

test('代码和二进制独立升级，失败保留标记和旧文件', async () => {
  const profile = 'updateprof'
  makeProfile(profile)
  assert.equal((await runInstaller({ profile })).code, 0)
  const initial = readMarker(profile).generation
  const oldBundle = readFileSync(path.join(generationDir(profile), 'manifest.json'), 'utf8')
  const sourceEntry = path.join(fixtureRepo, 'plugins', 'sekai-dsh', 'lib', 'index.js')
  const source = readFileSync(sourceEntry, 'utf8')
  const isolatedBinaryDir = path.join(tempRoot, 'changed-bin')
  mkdirSync(isolatedBinaryDir)
  const router = writeFakeBinary(isolatedBinaryDir, 'sekai-router')
  writeFakeBinary(isolatedBinaryDir, 'sekai-worker')
  try {
    writeFileSync(sourceEntry, `${source}\nexport const updateProbe = true\n`)
    for (const flag of ['FAKE_DSH_FORCE_FAIL', 'FAKE_DSH_STAY_STALE']) {
      const failed = await runInstaller({ profile, env: { ...baseEnv, [flag]: '1' } })
      assert.notEqual(failed.code, 0)
      assert.equal(readMarker(profile).generation, initial)
    }
    const jsOnly = await runInstaller({ profile })
    assert.equal(jsOnly.code, 0, jsOnly.err)
    const jsGeneration = readMarker(profile).generation
    assert.notEqual(jsGeneration, initial)
    assert.deepEqual(readFileSync(deployedBinary(profile, 'sekai-router')), readFileSync(deployedBinary(profile, 'sekai-router', initial)))
    assert.equal(readFileSync(path.join(generationDir(profile, initial), 'manifest.json'), 'utf8'), oldBundle)

    writeFileSync(router, 'router v2\n')
    const calls = cliCalls(profile)
    const binaryOnly = await runInstaller({ profile, binDir: isolatedBinaryDir })
    assert.equal(binaryOnly.code, 0, binaryOnly.err)
    assert.notEqual(readMarker(profile).generation, jsGeneration)
    assert.deepEqual(cliCalls(profile), calls)
    assert.equal(readFileSync(deployedBinary(profile, 'sekai-router'), 'utf8'), 'router v2\n')
    assert.equal(readMarker('testprof').generation, initial)

    const marker = readMarker(profile)
    writeFileSync(markerPath(profile), '{invalid marker')
    const repair = await runInstaller({ profile, binDir: isolatedBinaryDir })
    assert.equal(repair.code, 0, repair.err)
    assert.deepEqual(readMarker(profile), marker)
    assert.deepEqual(cliCalls(profile), calls)
  } finally {
    writeFileSync(sourceEntry, source)
  }
})

test('同一 profile 的安装和卸载由同一个锁串行化', async () => {
  const profile = 'lockedprof'
  const dir = makeProfile(profile)
  mkdirSync(path.join(dir, '.sekai-install.lock'))
  for (const extra of [[], ['--uninstall']]) {
    const result = await runInstaller({ profile, extra })
    assert.notEqual(result.code, 0)
    assert.match(result.err, /另一个 Sekai 安装或卸载占用/)
    assert.equal(existsSync(markerPath(profile)), false)
  }
})

test('校验失败路径：dsh 未注册插件时非零退出且带 hint', async () => {
  const env = { ...baseEnv, FAKE_DSH_SKIP_REGISTER: '1' }
  const { code, err } = await runInstaller({ profile: 'failprof', env })
  assert.notEqual(code, 0)
  assert.match(err, /\[fail\]/)
  assert.match(err, /校验 b 失败/)
  assert.match(err, /hint:/)
  assert.equal(existsSync(markerPath('failprof')), false)
  assert.ok(existsSync(path.join(managedDir('failprof'), 'generations')))
})

test('--strict：--dump-config 缺组件行时失败；默认只告警', async () => {
  const env = { ...baseEnv, FAKE_DSH_NO_SEKAI: '1' }

  const strictRun = await runInstaller({ profile: 'strictprof', env, extra: ['--strict'] })
  assert.notEqual(strictRun.code, 0)
  assert.match(strictRun.err, /校验 d 失败/)
  assert.equal(existsSync(markerPath('strictprof')), false)

  const lenientRun = await runInstaller({ profile: 'strictprof', env })
  assert.equal(lenientRun.code, 0, `stderr=${lenientRun.err}`)
  assert.match(lenientRun.out, /\[warn\] 校验 d/)
})

test('--uninstall：清理依赖与 bundles 条目，保留二进制与状态文件', async () => {
  const stateDir = path.join(dshHome, 'sekai')
  mkdirSync(stateDir, { recursive: true })
  const stateFile = path.join(stateDir, 'worker-state.json')
  writeFileSync(stateFile, '{"workerId":"pc-worker"}\n')

  const { code, out, err } = await runInstaller({ extra: ['--uninstall'] })
  assert.equal(code, 0, `stderr=${err}`)

  const pkg = readProfilePackage('testprof')
  assert.equal(pkg.dependencies?.[PLUGIN_PACKAGE], undefined)
  assert.equal(pkg.dsh.profile.bundles.includes(PLUGIN_PACKAGE), false)
  assert.equal(existsSync(profilePluginsDir('testprof')), false)
  // 二进制与状态文件保留，并打印清理提示。
  assert.ok(existsSync(deployedBinary('testprof', 'sekai-router')))
  assert.ok(existsSync(deployedBinary('testprof', 'sekai-worker')))
  assert.ok(existsSync(stateFile))
  assert.match(out, /保留二进制及 generation 标记/)
  assert.match(out, /保留 worker 状态文件/)
})
