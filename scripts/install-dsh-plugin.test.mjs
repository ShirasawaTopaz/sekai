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
import { after, before, test } from 'node:test'
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
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
function addPlugin(profile, spec) {
  if (!spec.startsWith('file:')) die('add 只支持 file: spec，收到：' + spec)
  const source = spec.slice('file:'.length)
  if (!fs.existsSync(path.join(source, 'package.json'))) die('插件目录不存在：' + source)
  const pkg = readJson(profilePackage(profile))
  if (process.env.FAKE_DSH_SKIP_REGISTER === '1') return
  const dest = path.join(profilesDir, profile, 'node_modules', '@sekai', 'dsh-plugin')
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
  if (subcommand === 'add') addPlugin(profile, target)
  else removePlugin(profile, target)
  process.exit(0)
}

die('不支持的 argv：' + argv.join(' '))
`

/** stub 插件入口：与真实插件同一 4 参 resolveBin 契约，命中 <DSH_HOME>/sekai/bin。 */
const STUB_PLUGIN_INDEX = String.raw`
import { existsSync } from 'node:fs'
import { homedir } from 'node:os'
import path from 'node:path'

const SUFFIX = process.platform === 'win32' ? '.exe' : ''

export class SekaiRouterPlugin {
  constructor() {
    this.id = 'sekai-router'
  }
}

export class SekaiWorkerPlugin {
  constructor() {
    this.id = 'sekai-worker'
  }
}

export function resolveBin(configured, envKey, defaultBin, options = {}) {
  if (typeof configured === 'string' && configured.trim() !== '' && configured !== defaultBin) return configured
  const env = options.env ?? process.env
  const fromEnv = env[envKey]
  if (typeof fromEnv === 'string' && fromEnv.trim() !== '') return fromEnv
  const envHome = typeof env.DSH_HOME === 'string' && env.DSH_HOME.trim() !== '' ? env.DSH_HOME : ''
  const home = envHome !== '' ? envHome : path.join(homedir(), '.dsh')
  const candidate = path.join(home, 'sekai', 'bin', defaultBin + SUFFIX)
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

function deployedBinary(base) {
  return path.join(dshHome, 'sekai', 'bin', IS_WIN ? `${base}.exe` : base)
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

  for (const base of ['sekai-router', 'sekai-worker']) {
    const file = deployedBinary(base)
    assert.ok(existsSync(file), `缺少部署文件 ${file}`)
    assert.ok(statSync(file).size > 0, `部署文件为空 ${file}`)
  }

  assert.ok(existsSync(path.join(profilePluginsDir('testprof'), 'package.json')))

  assert.match(out, /\[ok\] 校验 a/)
  assert.match(out, /\[ok\] 校验 b/)
  assert.match(out, /\[ok\] 校验 c：sekai-router 零配置解析命中/)
  assert.match(out, /\[ok\] 校验 c：sekai-worker 零配置解析命中/)
  assert.match(out, /\[ok\] 校验 d/)
  assert.match(out, /下一步/)
  assert.match(out, /sekai_\*/)
})

test('重复安装幂等：bundles 不重复、依赖不变', async () => {
  const first = readProfilePackage('testprof')
  const { code, err } = await runInstaller()
  assert.equal(code, 0, `stderr=${err}`)

  const second = readProfilePackage('testprof')
  assert.deepEqual(second.dependencies, first.dependencies)
  assert.equal(second.dsh.profile.bundles.filter((entry) => entry === PLUGIN_PACKAGE).length, 1)
})

test('校验失败路径：dsh 未注册插件时非零退出且带 hint', async () => {
  const env = { ...baseEnv, FAKE_DSH_SKIP_REGISTER: '1' }
  const { code, err } = await runInstaller({ profile: 'failprof', env })
  assert.notEqual(code, 0)
  assert.match(err, /\[fail\]/)
  assert.match(err, /校验 b 失败/)
  assert.match(err, /hint:/)
  // 部署步骤已成功（失败发生在注册校验），说明脚本确实部署了二进制。
  assert.ok(existsSync(deployedBinary('sekai-router')))
})

test('--strict：--dump-config 缺组件行时失败；默认只告警', async () => {
  const env = { ...baseEnv, FAKE_DSH_NO_SEKAI: '1' }

  const strictRun = await runInstaller({ profile: 'strictprof', env, extra: ['--strict'] })
  assert.notEqual(strictRun.code, 0)
  assert.match(strictRun.err, /校验 d 失败/)

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
  assert.ok(existsSync(deployedBinary('sekai-router')))
  assert.ok(existsSync(deployedBinary('sekai-worker')))
  assert.ok(existsSync(stateFile))
  assert.match(out, /保留二进制目录/)
  assert.match(out, /保留 worker 状态文件/)
  assert.match(out, /清理示例/)
})
