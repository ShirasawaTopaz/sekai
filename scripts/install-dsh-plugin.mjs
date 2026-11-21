#!/usr/bin/env node
/**
 * install-dsh-plugin.mjs — sekai DSH 插件一键安装/卸载脚本。
 *
 * 纯 ESM、零运行时依赖、Node >= 20。做四件事：
 *   1. 构建（可选）并为所选 profile 暂存一代 sekai-router / sekai-worker；
 *   2. 通过 `dsh plugin --profile <p> add file:<repo>/plugins/sekai-dsh` 安装或刷新插件；
 *   3. 校验二进制、已安装的 bundle 内容、候选路径解析与 profile 组合；
 *   4. 原子发布 <DSH_HOME>/sekai/bin/profiles/<profile>/current.json。
 *
 * dsh 子进程会收到所选 DSH_HOME 和 DSH_PROFILE。Windows 上 .cmd 通过 ComSpec
 * `/d /s /c` 包装。--dry-run 不写盘。
 *
 * 用法：node scripts/install-dsh-plugin.mjs [选项]（--help 查看全部）
 */

import { spawn } from 'node:child_process'
import { createHash, randomUUID } from 'node:crypto'
import { createRequire } from 'node:module'
import {
  chmodSync,
  closeSync,
  copyFileSync,
  existsSync,
  fsyncSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  readSync,
  readdirSync,
  realpathSync,
  renameSync,
  rmSync,
  rmdirSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs'
import { homedir, tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

/** 插件包名（profile package.json 的 dependencies / dsh.profile.bundles 条目）。 */
export const PLUGIN_PACKAGE = '@sekai/dsh-plugin'
/** profile `dsh.profile.bundles` 的路径（profile package.json 内）。 */
export const BUNDLES_PATH = ['dsh', 'profile', 'bundles']
/** 两个二进制组件：base 名字、默认文件名后缀、环境变量名。 */
export const BINARIES = [
  { key: 'router', base: 'sekai-router', envKey: 'SEKAI_ROUTER_BIN', id: 'sekai-router' },
  { key: 'worker', base: 'sekai-worker', envKey: 'SEKAI_WORKER_BIN', id: 'sekai-worker' },
]
/** 插件默认监听地址（仅用于结果提示）。 */
export const DEFAULT_ROUTER_ADDR = '127.0.0.1:7421'

const LOCALAPPDATA_DSH_CMD = [
  'Programs',
  'DeepSeek Harness',
  'resources',
  'runtime',
  'cli',
  'bin',
  'dsh.cmd',
]

/** 可操作错误：message = 发生了什么，hint = 怎么修。 */
export class InstallError extends Error {
  /**
   * @param {string} message 发生了什么（中文短句）。
   * @param {string} [hint] 可操作修复提示。
   */
  constructor(message, hint) {
    super(message)
    this.name = 'InstallError'
    this.hint = hint
  }
}

/**
 * 解析命令行参数。未知参数/缺值抛 InstallError（usage 错误，退出码 2）。
 * @param {string[]} argv 不含 node 与脚本名的参数。
 * @returns {{profile?: string, dshHome?: string, dsh?: string, binDir?: string,
 *   noBuild: boolean, dryRun: boolean, uninstall: boolean, strict: boolean, help: boolean}}
 */
export function parseArgs(argv = []) {
  const args = {
    profile: undefined,
    dshHome: undefined,
    dsh: undefined,
    binDir: undefined,
    noBuild: false,
    dryRun: false,
    uninstall: false,
    strict: false,
    help: false,
  }

  for (let i = 0; i < argv.length; i += 1) {
    const raw = argv[i]
    let key = raw
    let inline
    if (raw.startsWith('--')) {
      const eq = raw.indexOf('=')
      if (eq !== -1) {
        key = raw.slice(0, eq)
        inline = raw.slice(eq + 1)
      }
    }

    const takeValue = (name) => {
      if (inline !== undefined && inline !== '') return inline
      if (inline === '') throw new InstallError(`参数 ${name} 缺少值`, `示例：${name} <值>；--help 查看全部参数。`)
      const next = argv[i + 1]
      if (next === undefined || next.startsWith('-')) {
        throw new InstallError(`参数 ${name} 缺少值`, `示例：${name} <值>；--help 查看全部参数。`)
      }
      i += 1
      return next
    }

    switch (key) {
      case '--profile':
      case '-p':
        args.profile = takeValue(key)
        break
      case '--dsh-home':
        args.dshHome = takeValue(key)
        break
      case '--dsh':
        args.dsh = takeValue(key)
        break
      case '--bin-dir':
        args.binDir = takeValue(key)
        break
      case '--no-build':
        args.noBuild = true
        break
      case '--dry-run':
        args.dryRun = true
        break
      case '--uninstall':
        args.uninstall = true
        break
      case '--strict':
        args.strict = true
        break
      case '--help':
      case '-h':
        args.help = true
        break
      default:
        throw new InstallError(`未知参数：${raw}`, '运行 node scripts/install-dsh-plugin.mjs --help 查看支持的参数。')
    }
  }

  return args
}

/**
 * 仓库根 = 本脚本所在 scripts/ 的上一级。
 * @param {string} [moduleUrl]
 * @returns {string}
 */
export function deriveRepoRoot(moduleUrl = import.meta.url) {
  return path.dirname(path.dirname(fileURLToPath(moduleUrl)))
}

/** 展开 ~ 前缀。 */
function expandTilde(value, home) {
  if (typeof value !== 'string' || value === '') return value
  if (value === '~') return home
  if (value.startsWith('~/') || value.startsWith('~\\')) return path.join(home, value.slice(2))
  return value
}

/** 读取目录下的 profile 名（要求 profiles/<name>/package.json 存在）。 */
function listProfiles(profilesDir) {
  try {
    return readdirSync(profilesDir, { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .filter((entry) => existsSync(path.join(profilesDir, entry.name, 'package.json')))
      .map((entry) => entry.name)
      .sort()
  } catch {
    return []
  }
}

/** PATH 目录列表（Windows 环境变量可能写作 Path，但 process.env 大小写不敏感）。 */
function pathDirs(env) {
  const raw = env.PATH ?? env.Path ?? ''
  return String(raw)
    .split(path.delimiter)
    .filter(Boolean)
    .map((dir) => dir.replace(/^"(.*)"$/, '$1'))
}

/** 在 PATH 上找 dsh；Windows 依次尝试 .cmd/.exe/.bat。 */
function findOnPath(name, { env, platform }) {
  const candidates =
    platform === 'win32'
      ? path.extname(name) !== ''
        ? [name]
        : [`${name}.cmd`, `${name}.exe`, `${name}.bat`, name]
      : [name]
  for (const dir of pathDirs(env)) {
    for (const candidate of candidates) {
      const full = path.join(dir, candidate)
      if (existsSync(full)) return path.resolve(full)
    }
  }
  return undefined
}

/**
 * 解析 dsh CLI 绝对路径：--dsh > PATH >（Windows）LOCALAPPDATA 桌面端内置 CLI。
 * @param {string | undefined} explicit --dsh 的值。
 * @param {{env: NodeJS.ProcessEnv, platform: string, cwd: string, home: string}} options
 * @returns {string}
 */
export function resolveDshCli(explicit, { env, platform, cwd, home }) {
  if (explicit !== undefined && explicit !== '') {
    const expanded = expandTilde(explicit, home)
    const asPath = path.resolve(cwd, expanded)
    if (existsSync(asPath)) return asPath
    if (!/[\\/]/.test(expanded)) {
      const found = findOnPath(expanded, { env, platform })
      if (found) return found
    }
    throw new InstallError(
      `指定的 dsh CLI 不存在：${explicit}`,
      '传 --dsh <dsh 路径>（Windows 通常是 ...\\bin\\dsh.cmd）；不加 --dsh 时脚本会查 PATH。',
    )
  }

  const fromPath = findOnPath('dsh', { env, platform })
  if (fromPath) return fromPath

  if (platform === 'win32') {
    const localAppData = env.LOCALAPPDATA ?? env.LocalAppData
    if (localAppData) {
      const candidate = path.join(localAppData, ...LOCALAPPDATA_DSH_CMD)
      if (existsSync(candidate)) return candidate
    }
  }

  throw new InstallError(
    '找不到 dsh CLI',
    platform === 'win32'
      ? '把 dsh 加入 PATH，或传 --dsh "%LOCALAPPDATA%\\Programs\\DeepSeek Harness\\resources\\runtime\\cli\\bin\\dsh.cmd"；先确认桌面端/CLI 已安装。'
      : '把 dsh 加入 PATH，或传 --dsh /path/to/dsh。',
  )
}

/**
 * 解析安装上下文（只读，不写盘）：仓库根、插件目录、DSH_HOME、profile、dsh CLI。
 * @param {ReturnType<typeof parseArgs>} args
 * @param {{platform?: string, env?: NodeJS.ProcessEnv, home?: string, cwd?: string, repoRoot?: string}} [options]
 */
export function resolveContext(args, options = {}) {
  const platform = options.platform ?? process.platform
  const env = options.env ?? process.env
  const home = options.home ?? homedir()
  const cwd = options.cwd ?? process.cwd()
  const repoRoot = options.repoRoot ?? deriveRepoRoot()

  // DSH_HOME = --dsh-home > $DSH_HOME > ~/.dsh
  const fromEnvHome = typeof env.DSH_HOME === 'string' ? env.DSH_HOME.trim() : ''
  const dshHomeRaw =
    args.dshHome !== undefined ? args.dshHome : fromEnvHome !== '' ? fromEnvHome : path.join(home, '.dsh')
  const dshHome = path.resolve(cwd, expandTilde(dshHomeRaw, home))
  const profilesDir = path.join(dshHome, 'profiles')

  // profile = --profile > $DSH_PROFILE > 唯一 profile > desktop > 报错
  const availableProfiles = listProfiles(profilesDir)
  const envProfile = typeof env.DSH_PROFILE === 'string' ? env.DSH_PROFILE.trim() : ''
  let profile = args.profile !== undefined ? args.profile : envProfile !== '' ? envProfile : undefined
  if (profile === undefined) {
    if (availableProfiles.length === 1) {
      profile = availableProfiles[0]
    } else if (availableProfiles.includes('desktop')) {
      profile = 'desktop'
    } else {
      throw new InstallError(
        `无法自动选择 profile（${profilesDir} 下可用：${availableProfiles.join('、') || '无'}）`,
        '用 --profile <name> 指定；先启动一次 DSH 桌面端或 dsh web 以创建 profile。',
      )
    }
  }
  if (!availableProfiles.includes(profile)) {
    throw new InstallError(
      `profile "${profile}" 不存在（${path.join(profilesDir, profile)}）`,
      `可用 profile：${availableProfiles.join('、') || '（无）'}。用 --profile <name> 指定已存在的 profile；先启动一次 DSH 桌面端/CLI 创建 profile。`,
    )
  }
  if (profile === 'node_modules' || profile.trim() !== profile || /[\\/:\u0000-\u001f]/.test(profile)) {
    throw new InstallError(`profile 名称不适用于托管二进制：${JSON.stringify(profile)}`)
  }
  const profileDir = path.join(profilesDir, profile)
  const managedDir = path.join(dshHome, 'sekai', 'bin', 'profiles', profile)

  const dsh = resolveDshCli(args.dsh, { env, platform, cwd, home })

  return {
    args,
    platform,
    env,
    home,
    cwd,
    repoRoot,
    pluginDir: path.join(repoRoot, 'plugins', 'sekai-dsh'),
    dshHome,
    profilesDir,
    profile,
    profileDir,
    managedDir,
    dsh,
    availableProfiles,
  }
}

/** 二进制文件名（Windows 带 .exe）。 */
export function binaryFileName(base, platform) {
  return platform === 'win32' ? `${base}.exe` : base
}

/** 文件存在且非空。 */
export function isUsableFile(filePath) {
  try {
    const stat = statSync(filePath)
    return stat.isFile() && stat.size > 0
  } catch {
    return false
  }
}

/** 人类可读字节数（只用于输出）。 */
function formatBytes(size) {
  if (size < 1024) return `${size} B`
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KiB`
  return `${(size / (1024 * 1024)).toFixed(1)} MiB`
}

/** 单行错误描述。 */
function describeError(error) {
  if (error === null || error === undefined) return 'unknown error'
  if (typeof error === 'string') return error
  if (typeof error === 'object') {
    const code = error.code
    const message = error.message ?? String(error)
    return code ? `${code}: ${message}` : String(message)
  }
  return String(error)
}

/** Windows 单参数引号：含空格/特殊字符时用双引号包起来。 */
export function quoteWindowsArg(value) {
  const text = String(value)
  if (text === '') return '""'
  if (!/[\s"&<>|^()%,;=]/.test(text)) return text
  return `"${text.replace(/"/g, '""')}"`
}

/**
 * 生成子进程调用计划。Windows 上把 dsh.cmd（可能带空格）包进 ComSpec：
 * `cmd.exe /d /s /c "<command>"`，并用 windowsVerbatimArguments 防止 Node 二次转义。
 * @param {string} file 可执行文件。
 * @param {string[]} args 参数。
 * @param {{platform: string, env: NodeJS.ProcessEnv}} options
 */
export function buildSpawnPlan(file, args, { platform, env }) {
  if (platform === 'win32') {
    const commandLine = [file, ...args].map(quoteWindowsArg).join(' ')
    const comspec = env.ComSpec ?? env.COMSPEC ?? 'cmd.exe'
    return {
      command: comspec,
      args: ['/d', '/s', '/c', `"${commandLine}"`],
      options: { windowsVerbatimArguments: true },
    }
  }
  return { command: file, args, options: {} }
}

/** 继承 stdio 跑子进程（cargo / dsh plugin add|remove：输出透传）。 */
function spawnInherit(file, args, { cwd, env, platform }) {
  const plan = buildSpawnPlan(file, args, { platform, env })
  return new Promise((resolve) => {
    let child
    try {
      child = spawn(plan.command, plan.args, { cwd, env, stdio: 'inherit', ...plan.options })
    } catch (error) {
      resolve({ error })
      return
    }
    child.once('error', (error) => resolve({ error }))
    child.once('close', (code, signal) => resolve({ code, signal }))
  })
}

/**
 * 捕获输出的子进程调用（--dump-config：需要读输出）。
 * 用临时文件重定向而不是管道：某些受限沙箱禁止创建命名管道（spawn 会 EPERM），
 * 文件重定向在所有环境都可用。
 */
function spawnCapture(file, args, { cwd, env, platform }) {
  const plan = buildSpawnPlan(file, args, { platform, env })
  const logFile = path.join(
    tmpdir(),
    `sekai-dsh-capture-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}.log`,
  )
  let fd
  try {
    fd = openSync(logFile, 'w')
  } catch (error) {
    return Promise.resolve({ error, stdout: '', stderr: '' })
  }

  return new Promise((resolve) => {
    let settled = false
    const finish = (result) => {
      if (settled) return
      settled = true
      try {
        closeSync(fd)
      } catch {
        /* 已关闭则忽略 */
      }
      let text = ''
      try {
        text = readFileSync(logFile, 'utf8')
      } catch {
        /* 读取失败则按空输出处理 */
      }
      try {
        unlinkSync(logFile)
      } catch {
        /* 临时文件清理失败不阻断 */
      }
      resolve({ stderr: '', ...result, stdout: result.stdout ?? text })
    }

    let child
    try {
      child = spawn(plan.command, plan.args, {
        cwd,
        env,
        stdio: ['ignore', fd, fd],
        ...plan.options,
      })
    } catch (error) {
      finish({ error })
      return
    }
    child.once('error', (error) => finish({ error }))
    child.once('close', (code, signal) => finish({ code, signal }))
  })
}

/**
 * 解析二进制来源（必要时构建）。--bin-dir 隐含不构建；--no-build 跳过 cargo。
 * --dry-run 只做路径解析并打印计划，不构建、不因缺文件失败。
 * @returns {Promise<{sourceDir: string, binaries: Array<{key: string, base: string,
 *   envKey: string, id: string, fileName: string, src: string}>, missing: string[]}>}
 */
export async function resolveBinaries(ctx) {
  const { args, platform, env, io, repoRoot, cwd, home } = ctx
  const names = BINARIES.map((component) => ({
    ...component,
    fileName: binaryFileName(component.base, platform),
  }))
  const sourceDir = args.binDir
    ? path.resolve(cwd, expandTilde(args.binDir, home))
    : path.join(repoRoot, 'target', 'release')
  const skipBuild = args.noBuild || args.binDir !== undefined

  if (args.dryRun) {
    if (skipBuild) {
      io.out(`[dry-run] 跳过 cargo 构建（${args.binDir !== undefined ? '--bin-dir' : '--no-build'}）`)
    } else {
      io.out('[dry-run] 将执行：cargo build --release -p sekai-router -p sekai-worker')
      io.out(`[dry-run]   cwd: ${repoRoot}`)
    }
  } else if (!skipBuild) {
    io.out('[..] 构建二进制：cargo build --release -p sekai-router -p sekai-worker')
    const result = await spawnInherit(
      'cargo',
      ['build', '--release', '-p', 'sekai-router', '-p', 'sekai-worker'],
      { cwd: repoRoot, env, platform },
    )
    if (result.error) {
      const missingNow = names
        .map((component) => path.join(sourceDir, component.fileName))
        .filter((filePath) => !isUsableFile(filePath))
      if (missingNow.length > 0) {
        throw new InstallError(
          `无法执行 cargo（${describeError(result.error)}），且 ${sourceDir} 缺少已构建二进制`,
          '安装 Rust 工具链（cargo 在 PATH）后重试，或用 --bin-dir 指向已有构建目录。',
        )
      }
      io.out('[warn] 无法执行 cargo，改用 target/release 中已有二进制（版本可能较旧）')
    } else if (result.code !== 0) {
      throw new InstallError(
        `cargo build 失败（退出码 ${result.code}）`,
        '先手动运行 cargo build --release -p sekai-router -p sekai-worker 查看编译错误，修好后再安装。',
      )
    } else {
      io.out('[ok] cargo build 完成')
    }
  } else if (!args.dryRun) {
    io.out('[ok] 跳过 cargo 构建（使用已有二进制）')
  }

  const binaries = names.map((component) => ({
    ...component,
    src: path.join(sourceDir, component.fileName),
  }))
  const missing = binaries.filter((binary) => !isUsableFile(binary.src)).map((binary) => binary.src)

  if (missing.length > 0 && !args.dryRun) {
    throw new InstallError(
      `缺少二进制：${missing.join('、')}`,
      args.binDir !== undefined
        ? '--bin-dir 目录里需要 sekai-router(.exe) 与 sekai-worker(.exe)；先构建或改用默认 target/release。'
        : '先运行 cargo build --release -p sekai-router -p sekai-worker，或用 --bin-dir 指定已构建目录。',
    )
  }

  return { sourceDir, binaries, missing }
}

function sha256(value) {
  return createHash('sha256').update(value).digest('hex')
}

function hashFile(file) {
  const fd = openSync(file, 'r')
  try {
    const hash = createHash('sha256')
    const buffer = Buffer.allocUnsafe(256 * 1024)
    let size = 0
    for (let length; (length = readSync(fd, buffer, 0, buffer.length, null)) > 0;) {
      hash.update(buffer.subarray(0, length))
      size += length
    }
    return { hash: hash.digest('hex'), size }
  } finally {
    closeSync(fd)
  }
}

function lstatIfPresent(file) {
  try {
    return lstatSync(file)
  } catch (error) {
    if (error.code === 'ENOENT') return undefined
    throw error
  }
}

// These roots contain the manifest, bundle composition, module entry points and runtime assets.
// README and smoke scripts are packaging collateral, so editing them does not restart services.
const BUNDLE_RUNTIME_PATHS = ['package.json', 'cordis.patch.yml', 'lib', 'locale', 'icon.svg', 'icons', 'docs']

function fingerprintBundle(directory) {
  const files = new Map()
  function visit(relative, optional = false) {
    const full = path.join(directory, relative)
    const stat = lstatIfPresent(full)
    if (!stat && optional) return
    if (stat?.isDirectory()) {
      for (const entry of readdirSync(full).sort()) visit(`${relative}/${entry}`)
    } else if (stat?.isFile()) {
      files.set(relative, hashFile(full).hash)
    } else {
      throw new InstallError(`bundle 运行文件缺失或类型不支持：${full}`)
    }
  }
  for (const relative of BUNDLE_RUNTIME_PATHS) visit(relative, relative !== 'package.json' && relative !== 'lib')
  return { hash: sha256(JSON.stringify([...files])), files }
}

function verifyGeneration(ctx, directory, manifest, binaries) {
  const hint = '检查该 generation 的完整性后重试；安装器不会覆盖或删除已有 generation。'
  try {
    if (!lstatSync(directory).isDirectory()) throw new Error('不是普通目录')
    const manifestPath = path.join(directory, 'manifest.json')
    if (!lstatSync(manifestPath).isFile()) throw new Error('manifest.json 不是普通文件')
    const actual = JSON.parse(readFileSync(manifestPath, 'utf8'))
    if (actual?.version !== manifest.version || actual.bundle !== manifest.bundle) {
      throw new Error('manifest.json 不匹配')
    }
    return binaries.map(({ base, fileName }) => {
      const dest = path.join(directory, fileName)
      const stat = lstatSync(dest)
      if (!stat.isFile() || (ctx.platform !== 'win32' && (stat.mode & 0o111) === 0)) {
        throw new Error(`${fileName} 不是可执行文件`)
      }
      const { hash, size } = hashFile(dest)
      if (size === 0 || hash !== manifest.binaries[base] || hash !== actual.binaries?.[base]) {
        throw new Error(`${fileName} 的 SHA-256 不匹配`)
      }
      return { base, fileName, dest, size }
    })
  } catch (error) {
    throw new InstallError(`部署校验失败：${directory}（${describeError(error)}）`, hint)
  }
}

/** Stage a complete generation using a directory rename. An existing generation is read-only. */
export function deployBinaries(ctx, resolved, bundleFingerprint) {
  const generationsDir = path.join(ctx.managedDir, 'generations')
  const { binaries } = resolved
  if (ctx.args.dryRun) {
    ctx.io.out(`[dry-run] 将暂存二进制到 ${path.join(generationsDir, '<generation>')}`)
    for (const { src, fileName } of binaries) {
      ctx.io.out(`[dry-run]   ${src} -> ${fileName}（${isUsableFile(src) ? 'ok' : '缺失'}）`)
    }
    return
  }

  const hashes = Object.fromEntries(binaries.map(({ base, src }) => {
    const { hash, size } = hashFile(src)
    if (size === 0) throw new InstallError(`来源二进制为空：${src}`)
    return [base, hash]
  }))
  const manifest = { version: 1, bundle: bundleFingerprint.hash, binaries: hashes }
  // Include filenames so the same bytes on different platforms do not collide.
  const generation = sha256(JSON.stringify([manifest, binaries.map(({ fileName }) => fileName)]))
  const destDir = path.join(generationsDir, generation)
  if (!lstatIfPresent(destDir)) {
    mkdirSync(generationsDir, { recursive: true })
    const stageDir = mkdtempSync(path.join(generationsDir, '.stage-'))
    let staged = true
    try {
      for (const { src, fileName } of binaries) {
        const stagedPath = path.join(stageDir, fileName)
        copyFileSync(src, stagedPath)
        if (ctx.platform !== 'win32') chmodSync(stagedPath, 0o755)
      }
      writeFileSync(path.join(stageDir, 'manifest.json'), `${JSON.stringify(manifest)}\n`, { flag: 'wx' })
      verifyGeneration(ctx, stageDir, manifest, binaries)
      renameSync(stageDir, destDir)
      staged = false
    } finally {
      if (staged) rmSync(stageDir, { recursive: true, force: true })
    }
  }
  const deployed = verifyGeneration(ctx, destDir, manifest, binaries)
  ctx.io.out(`[ok] 候选 generation：${generation}`)
  return { destDir, deployed, generation, manifest }
}

function publishGeneration(ctx, generation) {
  if (typeof generation !== 'string' || generation.length !== 64 || !/^[0-9a-f]+$/.test(generation)) {
    throw new InstallError('generation 格式无效')
  }
  const marker = path.join(ctx.managedDir, 'current.json')
  const previous = lstatIfPresent(marker)
  if (previous) {
    if (!previous.isFile()) throw new InstallError(`发布失败：${marker} 不是普通文件`)
    const text = readFileSync(marker, 'utf8')
    let current
    try { current = JSON.parse(text) } catch { /* A verified candidate can replace invalid JSON. */ }
    if (current?.version === 1 && current.generation === generation) return false
  }

  const temporary = `${marker}.tmp-${randomUUID()}`
  let created = false
  try {
    const fd = openSync(temporary, 'wx', 0o600)
    created = true
    try {
      writeFileSync(fd, `${JSON.stringify({ version: 1, generation })}\n`)
      fsyncSync(fd)
    } finally {
      closeSync(fd)
    }
    renameSync(temporary, marker)
    created = false
  } catch (error) {
    throw new InstallError(`无法原子发布 ${marker}（${describeError(error)}）`, '解决文件占用或权限问题后重试。')
  } finally {
    if (created) rmSync(temporary, { force: true })
  }
  return true
}

/** Serialize installers for the same profile without holding the CLI's own package lock. */
async function withInstallerLock(ctx, operation) {
  const lock = path.join(ctx.profileDir, '.sekai-install.lock')
  try {
    mkdirSync(lock)
  } catch (error) {
    if (error.code !== 'EEXIST') throw error
    throw new InstallError(
      `profile "${ctx.profile}" 已被另一个 Sekai 安装或卸载占用`,
      `等待它结束后重试。若脚本意外退出，确认没有正在运行的安装或卸载，再移除锁目录 ${lock}。`,
    )
  }
  try {
    return await operation()
  } finally {
    rmdirSync(lock)
  }
}

/** 安装 spec：`file:<仓库绝对路径>/plugins/sekai-dsh`（Windows 用反斜杠）。 */
export function pluginInstallSpec(ctx) {
  return `file:${path.resolve(ctx.pluginDir)}`
}

/** 子进程环境使用所选 profile。 */
function childEnv(ctx) {
  return { ...ctx.env, DSH_HOME: ctx.dshHome, DSH_PROFILE: ctx.profile }
}

/**
 * 安装插件：`dsh plugin --profile <p> add file:<repo>/plugins/sekai-dsh`。
 * dsh 输出透传（stdio inherit）。
 */
export async function installPlugin(ctx, { force = false } = {}) {
  const { args, io, dsh, profile, pluginDir } = ctx
  const spec = pluginInstallSpec(ctx)

  if (!existsSync(path.join(pluginDir, 'package.json'))) {
    throw new InstallError(
      `插件目录不存在或不完整：${pluginDir}`,
      '在仓库根运行脚本（scripts/ 的上一级必须有 plugins/sekai-dsh）；确认仓库未裁剪。',
    )
  }

  const command = ['plugin', '--profile', profile, 'add', spec, ...(force ? ['--force'] : [])]
  io.out(`[..] ${force ? '强制刷新' : '安装'}插件：dsh ${command.join(' ')}`)
  if (args.dryRun) {
    io.out(`[dry-run] 将按需执行：${dsh} ${command.join(' ')}`)
    io.out('[dry-run] 已安装内容过期时将通过 dsh plugin add --force 刷新并复查')
    io.out(`[dry-run] 注入环境变量：DSH_HOME=${ctx.dshHome}，DSH_PROFILE=${profile}`)
    return { spec }
  }

  const result = await spawnInherit(dsh, command, {
    cwd: ctx.repoRoot,
    env: childEnv(ctx),
    platform: ctx.platform,
  })
  if (result.error) {
    throw new InstallError(
      `无法执行 dsh：${describeError(result.error)}`,
      '确认 --dsh 指向可执行文件（Windows 用 dsh.cmd）；PATH 上的 dsh 是否可用。',
    )
  }
  if (result.code !== 0) {
    throw new InstallError(
      `dsh plugin add 失败（退出码 ${result.code}）`,
      `检查 pnpm 输出或手动运行 "${dsh}" ${command.join(' ')}（常见原因：网络、权限、profile 被占用）。`,
    )
  }
  io.out('[ok] 插件安装命令执行成功')
  return { spec }
}

/** 读取 profile package.json（失败给可操作 hint）。 */
function readProfilePackage(ctx) {
  const pkgPath = path.join(ctx.profileDir, 'package.json')
  try {
    return JSON.parse(readFileSync(pkgPath, 'utf8'))
  } catch (error) {
    throw new InstallError(
      `无法读取 profile package.json：${describeError(error)}`,
      `确认 ${pkgPath} 是合法 JSON；必要时在 DSH 中重建 profile。`,
    )
  }
}

/** 读取 dsh.profile.bundles（缺失返回 []）。 */
function bundleEntries(pkg) {
  const bundles = pkg?.dsh?.profile?.bundles
  return Array.isArray(bundles) ? bundles : []
}

/** 对比解析后的路径，Windows 不区分大小写。 */
function samePath(first, second, platform) {
  try {
    const a = path.resolve(String(first))
    const b = path.resolve(String(second))
    return platform === 'win32' ? a.toLowerCase() === b.toLowerCase() : a === b
  } catch {
    return false
  }
}

function verifyProfileRegistration(ctx, { quiet = false } = {}) {
  const { io, profile, profileDir } = ctx
  const pkg = readProfilePackage(ctx)
  const dependency = pkg?.dependencies?.[PLUGIN_PACKAGE]
  if (typeof dependency !== 'string' || dependency === '') {
    throw new InstallError(
      `校验 b 失败：profile "${profile}" 的 dependencies 里没有 ${PLUGIN_PACKAGE}`,
      `重新运行 dsh plugin --profile ${profile} add ${pluginInstallSpec(ctx)}；若 pnpm 报错先解决网络/权限问题。`,
    )
  }
  const bundles = bundleEntries(pkg)
  const bundleCount = bundles.filter((entry) => entry === PLUGIN_PACKAGE).length
  if (bundleCount === 0) {
    throw new InstallError(
      `校验 b 失败：${profile} 的 dsh.profile.bundles 里没有 ${PLUGIN_PACKAGE}`,
      `装了依赖但 bundle 未被选中（例如在「插件」页关闭过）时，pnpm 不会重复登记：在 DSH 的「插件」页重新启用，` +
        `或调用 plugin_manager set_bundle target="${PLUGIN_PACKAGE}" enabled=true；` +
        `也可以先 dsh plugin --profile ${profile} remove ${PLUGIN_PACKAGE} 再重跑安装。`,
    )
  }
  if (bundleCount > 1) {
    throw new InstallError(
      `校验 b 失败：${profile} 的 dsh.profile.bundles 里 ${PLUGIN_PACKAGE} 重复 ${bundleCount} 次`,
      `手动编辑 ${path.join(profileDir, 'package.json')} 保留一条；重跑安装。`,
    )
  }
  if (!isSourceDependency(ctx, dependency)) {
    throw new InstallError(
      `校验 b 失败：${PLUGIN_PACKAGE} 的依赖 ${JSON.stringify(dependency)} 未指向来源目录`,
      `检查 pnpm 输出；预期由 ${pluginInstallSpec(ctx)} 安装。`,
    )
  }
  if (!quiet) io.out(`[ok] 校验 b：profile 已登记（dependencies=${dependency}，bundles 含 ${PLUGIN_PACKAGE}）`)
}

function isSourceDependency(ctx, dependency) {
  if (typeof dependency !== 'string' || !dependency.startsWith('file:')) return false
  // pnpm may save an absolute path as a relative file: spec in the profile manifest.
  return samePath(path.resolve(ctx.profileDir, dependency.slice('file:'.length)), ctx.pluginDir, ctx.platform)
}

function inspectInstalledBundle(ctx, source) {
  try {
    const nodeModules = realpathSync(path.join(ctx.profileDir, 'node_modules'))
    const installedDir = realpathSync(path.join(nodeModules, PLUGIN_PACKAGE))
    const relative = path.relative(nodeModules, installedDir)
    if (relative === '' || relative === '..' || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
      throw new Error('安装包不在 profile node_modules 内')
    }
    const installed = fingerprintBundle(installedDir)
    if (installed.hash !== source.hash) {
      const files = new Set([...source.files.keys(), ...installed.files.keys()])
      const changed = [...files].filter((file) => source.files.get(file) !== installed.files.get(file))
      return { mismatch: changed.join('、') }
    }
    const requireFromProfile = createRequire(path.join(ctx.profileDir, 'package.json'))
    const entry = requireFromProfile.resolve(PLUGIN_PACKAGE)
    const relativeEntry = path.relative(installedDir, realpathSync(entry)).split(path.sep).join('/')
    if (!installed.files.has(relativeEntry)) throw new Error('入口模块不在已验证的已安装 bundle 内')
    return { entry }
  } catch (error) {
    return { mismatch: describeError(error) }
  }
}

async function ensureInstalledBundle(ctx, source) {
  const pkg = readProfilePackage(ctx)
  const dependency = pkg?.dependencies?.[PLUGIN_PACKAGE]
  if (!isSourceDependency(ctx, dependency)) await installPlugin(ctx)
  verifyProfileRegistration(ctx, { quiet: true })
  let installed = inspectInstalledBundle(ctx, source)
  if (installed.mismatch !== undefined) {
    ctx.io.out(`[warn] 已安装 bundle 内容需要刷新：${installed.mismatch}`)
    await installPlugin(ctx, { force: true })
    installed = inspectInstalledBundle(ctx, source)
  }
  if (installed.mismatch !== undefined) {
    throw new InstallError(
      `强制刷新后，已安装 bundle 运行文件仍与来源不一致：${installed.mismatch}`,
      '检查 pnpm 输出和 profile 的依赖，修复后重新运行安装脚本。',
    )
  }
  return installed.entry
}

/** Verify candidate resolution before changing the marker. The CLI dump checks disk composition. */
export async function verifyInstallation(ctx, deploy, entry) {
  const { args, io, platform, dshHome, profile, dsh } = ctx
  const warnings = []
  io.out(`[ok] 校验 a：候选二进制哈希已验证（${deploy.deployed
    .map(({ fileName, size }) => `${fileName} ${formatBytes(size)}`).join('、')}）`)
  verifyProfileRegistration(ctx)
  io.out('[ok] 校验 c：已安装 bundle 的运行文件与来源一致')

  let module
  try {
    module = await import(`${pathToFileURL(entry).href}?verify=${randomUUID()}`)
  } catch (error) {
    throw new InstallError(
      `校验 c 失败：导入已安装插件出错（${describeError(error)}）`,
      '插件包可能损坏或不完整；重跑安装脚本以重新拷贝最新插件。',
    )
  }
  for (const exportName of ['SekaiRouterPlugin', 'SekaiWorkerPlugin']) {
    if (typeof module[exportName] !== 'function') {
      throw new InstallError(
        `校验 c 失败：插件缺少导出 ${exportName}`,
        '确认 plugins/sekai-dsh/lib/index.js 已导出两个插件类；重跑安装以拷贝最新插件。',
      )
    }
  }
  if (typeof module.resolveBin !== 'function') {
    throw new InstallError(
      '校验 c 失败：插件缺少 resolveBin 导出（无法做零配置二进制发现）',
      '确认 plugins/sekai-dsh/lib/shared.js 与 index.js 已包含 resolveBin 导出；重跑安装以拷贝最新插件。',
    )
  }

  for (const component of BINARIES) {
    const expected = deploy.deployed.find((entryItem) => entryItem.base === component.base)
    if (!expected) {
      throw new InstallError(
        `校验 c 失败：没有部署 ${component.base}`,
        '重新运行安装脚本；检查 --bin-dir 目录内容。',
      )
    }
    let resolved
    try {
      resolved = module.resolveBin(component.base, component.envKey, component.base, {
        env: { DSH_HOME: dshHome, DSH_PROFILE: profile },
        generation: deploy.generation,
      })
    } catch (error) {
      throw new InstallError(
        `校验 c 失败：调用 resolveBin(${component.base}) 抛错（${describeError(error)}）`,
        '确认已安装插件支持按 profile 解析候选 generation，再重新运行安装脚本。',
      )
    }
    if (typeof resolved !== 'string' || !path.isAbsolute(resolved) || !samePath(resolved, expected.dest, platform)) {
      throw new InstallError(
        `校验 c 失败：零配置闭环未命中——${component.base} 解析为 "${resolved}"，期望 "${expected.dest}"`,
        '确认已安装插件支持按 profile 解析候选 generation，再重新运行安装脚本。',
      )
    }
    io.out(`[ok] 校验 c：${component.base} 零配置解析命中 ${expected.dest}`)
  }

  // d. --dump-config（告警级）
  const dump = await spawnCapture(dsh, ['--profile', profile, '--dump-config'], {
    cwd: ctx.repoRoot,
    env: childEnv(ctx),
    platform,
  })
  const strictHint = '先手动运行 "dsh --profile <p> --dump-config" 排查；不加 --strict 时该项只告警。'
  if (dump.error || dump.code !== 0) {
    const reason = dump.error
      ? describeError(dump.error)
      : `退出码 ${dump.code}${dump.stderr ? `：${dump.stderr.trim().slice(0, 300)}` : ''}`
    // desktop profile 由桌面端独占管理：CLI 的 --dump-config 会被拒绝，这不是插件问题。
    if (profile === 'desktop' && /managed exclusively by the Electron application/i.test(`${dump.stderr ?? ''}${dump.stdout ?? ''}`)) {
      io.out('[ok] 校验 d：desktop profile 由桌面端独占管理，跳过 --dump-config（组合校验由桌面端负责）')
    } else {
      const message = `--dump-config 执行失败（${reason}）`
      if (args.strict) throw new InstallError(`校验 d 失败：${message}`, strictHint)
      io.out(`[warn] 校验 d：${message}（已忽略；--strict 时会失败）`)
      warnings.push(message)
    }
  } else {
    const dumpText = `${dump.stdout}\n${dump.stderr}`
    const missingRows = BINARIES.filter((component) => !dumpText.includes(component.id)).map(
      (component) => component.id,
    )
    if (missingRows.length > 0) {
      const message = `--dump-config 输出缺少组件行：${missingRows.join('、')}`
      if (args.strict) {
        throw new InstallError(
          `校验 d 失败：${message}`,
          '确认插件 cordis.patch.yml 已随 profile bundle 生效；重启 DSH 后重试；--strict 时该项才算失败。',
        )
      }
      io.out(`[warn] 校验 d：${message}（已忽略；--strict 时会失败）`)
      warnings.push(message)
    } else {
      io.out(`[ok] 校验 d：--dump-config 输出含 ${BINARIES.map((component) => component.id).join(' 与 ')}`)
    }
  }

  return { warnings }
}

/**
 * 卸载：`dsh plugin --profile <p> remove @sekai/dsh-plugin`，
 * 校验依赖与 bundles 条目消失；保留 <DSH_HOME>/sekai/bin 与 worker 状态文件。
 */
export async function uninstallPlugin(ctx) {
  const { args, io, dsh, profile, dshHome } = ctx
  const binDir = path.join(dshHome, 'sekai', 'bin')
  const stateFile = path.join(dshHome, 'sekai', 'worker-state.json')

  io.out(`[..] 卸载插件：dsh plugin --profile ${profile} remove ${PLUGIN_PACKAGE}`)
  if (args.dryRun) {
    io.out(`[dry-run] 将执行：${dsh} plugin --profile ${profile} remove ${PLUGIN_PACKAGE}`)
    io.out(`[dry-run] 保留（不删除）：${binDir}、${stateFile}`)
    return { removed: false }
  }

  const result = await spawnInherit(
    dsh,
    ['plugin', '--profile', profile, 'remove', PLUGIN_PACKAGE],
    { cwd: ctx.repoRoot, env: childEnv(ctx), platform: ctx.platform },
  )
  if (result.error) {
    throw new InstallError(
      `无法执行 dsh：${describeError(result.error)}`,
      '确认 --dsh 指向可执行文件（Windows 用 dsh.cmd）；PATH 上的 dsh 是否可用。',
    )
  }
  if (result.code !== 0) {
    throw new InstallError(
      `dsh plugin remove 失败（退出码 ${result.code}）`,
      `手动运行 "${dsh}" plugin --profile ${profile} remove ${PLUGIN_PACKAGE} 查看错误。`,
    )
  }

  // 校验依赖与 bundles 条目消失
  const pkg = readProfilePackage(ctx)
  const leftovers = []
  if (pkg?.dependencies?.[PLUGIN_PACKAGE] !== undefined) leftovers.push('dependencies')
  if (bundleEntries(pkg).includes(PLUGIN_PACKAGE)) leftovers.push('dsh.profile.bundles')
  if (leftovers.length > 0) {
    throw new InstallError(
      `卸载不完整：${leftovers.join(' / ')} 里仍有 ${PLUGIN_PACKAGE}`,
      `手动编辑 ${path.join(ctx.profileDir, 'package.json')} 删除该条目，或重新运行 dsh plugin --profile ${profile} remove ${PLUGIN_PACKAGE}。`,
    )
  }
  io.out(`[ok] 校验：profile "${profile}" 的 dependencies 与 bundles 条目已移除`)

  if (existsSync(binDir)) io.out(`[ok] 已保留二进制及 generation 标记：${binDir}`)
  if (existsSync(stateFile)) io.out(`[ok] 已保留 worker 状态文件：${stateFile}`)
  io.out('[ok] 已从所选 profile 卸载插件。此命令只验证了磁盘上的 profile。')
  return { removed: true }
}

/** --help 文本。 */
export function helpText() {
  return [
    'sekai DSH 插件一键安装/卸载（构建 → 暂存 → 安装/刷新 bundle → 校验 → 发布）',
    '',
    '用法：',
    '  node scripts/install-dsh-plugin.mjs [选项]',
    '',
    '选项：',
    '  -p, --profile <name>  指定 DSH profile（默认：$DSH_PROFILE > 唯一 profile > desktop）',
    '      --dsh-home <dir>  指定 DSH_HOME（默认：$DSH_HOME > ~/.dsh）',
    '      --dsh <path>      指定 dsh CLI（默认：PATH；Windows 回落桌面端内置 dsh.cmd）',
    '      --bin-dir <dir>   用该目录里已构建的两个二进制（隐含 --no-build）',
    '      --no-build        跳过 cargo build，使用 target/release 现有二进制',
    '      --dry-run         只打印将执行的命令/写入位置，不构建、不写盘、不调用 dsh',
    '      --uninstall       卸载插件（删除 profile 依赖与 bundles 条目；保留二进制与状态文件）',
    '      --strict          --dump-config 组合校验失败也视为失败（默认只告警）',
    '  -h, --help            显示本帮助',
    '',
    '说明：',
    '  - 默认构建 `cargo build --release -p sekai-router -p sekai-worker`，并在',
    '    <DSH_HOME>/sekai/bin/profiles/<profile>/generations/<generation>/ 暂存两个二进制。',
    '  - 通过 dsh CLI 安装或刷新 bundle。校验成功后原子发布所选 profile 的 current.json。',
    '  - 此命令校验磁盘上的安装。若运行中的 DSH 尚未载入新 bundle，请重新启动该 profile。',
    `  - 默认 router 监听 ${DEFAULT_ROUTER_ADDR}，worker autoConnect 自动连接。`,
    '  - 详细文档：docs/sekai-dsh-plugin.md §1（安装/升级/卸载与二进制解析顺序）。',
    '',
    '示例：',
    '  node scripts/install-dsh-plugin.mjs',
    '  node scripts/install-dsh-plugin.mjs --profile web --no-build',
    '  node scripts/install-dsh-plugin.mjs --dry-run',
    '  node scripts/install-dsh-plugin.mjs --uninstall',
  ].join('\n')
}

/** 创建输出器（测试可注入 stdout/stderr 流）。 */
function createIo(options) {
  const stdout = options.stdout ?? process.stdout
  const stderr = options.stderr ?? process.stderr
  return {
    out: (message) => stdout.write(`${message}\n`),
    err: (message) => stderr.write(`${message}\n`),
  }
}

/** 打印错误（InstallError 带 hint；其他错误带堆栈，便于排查）。 */
function printError(io, error) {
  if (error instanceof InstallError) {
    io.err(`[fail] ${error.message}`)
    if (error.hint) io.err(`       hint: ${error.hint}`)
    return
  }
  io.err(`[fail] 未预期错误：${describeError(error)}`)
  if (error instanceof Error && error.stack) io.err(error.stack)
}

/** 安装成功后的结果与下一步。 */
function printInstallSummary(ctx, deploy, warnings, published) {
  const { io, profile, dshHome } = ctx
  io.out('')
  io.out(`[ok] 安装完成：profile=${profile}，DSH_HOME=${dshHome}`)
  io.out(`[ok] 插件：${pluginInstallSpec(ctx)}`)
  io.out(`[ok] 二进制：${deploy.deployed.map((entry) => entry.dest).join('、')}`)
  io.out(`[ok] ${published ? '已发布 generation' : 'generation 未变化，保留原标记'}：${deploy.generation}`)
  if (warnings.length > 0) {
    io.out(`[warn] 有 ${warnings.length} 项告警（不影响安装，见上）；可用 --strict 复查。`)
  }
  io.out('')
  io.out('此命令只验证了磁盘上的安装。已加载热更新机制的 DSH 会自动应用发布；首次从旧版迁移需加载新版一次。')
  io.out(`默认 router 监听 ${DEFAULT_ROUTER_ADDR}，worker autoConnect 自动连接。`)
  io.out('卸载：node scripts/install-dsh-plugin.mjs --uninstall（详情见 docs/sekai-dsh-plugin.md）。')
}

/**
 * CLI 主入口。
 * @param {string[]} [argv]
 * @param {{stdout?: {write: Function}, stderr?: {write: Function}, env?: NodeJS.ProcessEnv,
 *   platform?: string, home?: string, cwd?: string, repoRoot?: string}} [options]
 * @returns {Promise<number>} 退出码：0 成功；1 安装/校验失败；2 参数错误。
 */
export async function main(argv = process.argv.slice(2), options = {}) {
  const io = createIo(options)

  // --help 优先于参数校验：`--help --bogus` 也显示帮助并退出 0。
  if (argv.includes('--help') || argv.includes('-h')) {
    io.out(helpText())
    return 0
  }

  let args
  try {
    args = parseArgs(argv)
  } catch (error) {
    printError(io, error)
    return 2
  }

  const platform = options.platform ?? process.platform
  const env = options.env ?? process.env
  const home = options.home ?? homedir()
  const cwd = options.cwd ?? process.cwd()
  const repoRoot = options.repoRoot ?? deriveRepoRoot()

  let ctx
  try {
    ctx = resolveContext(args, { platform, env, home, cwd, repoRoot })
  } catch (error) {
    printError(io, error)
    return 1
  }
  ctx.io = io

  try {
    io.out(`==> ${args.uninstall ? '卸载' : '安装'} ${PLUGIN_PACKAGE}`)
    io.out(`[ok] DSH_HOME：${ctx.dshHome}`)
    io.out(`[ok] profile：${ctx.profile}（${ctx.profileDir}）`)
    io.out(`[ok] dsh CLI：${ctx.dsh}`)

    if (args.dryRun) {
      if (args.uninstall) {
        await uninstallPlugin(ctx)
        io.out('[dry-run] 未执行任何写操作。')
        return 0
      }
      const resolved = await resolveBinaries(ctx)
      deployBinaries(ctx, resolved)
      await installPlugin(ctx)
      io.out(`[dry-run] 校验成功后将按需发布：${path.join(ctx.managedDir, 'current.json')}`)
      if (resolved.missing.length > 0) {
        io.out(`[warn] 当前缺少二进制（真正安装时会失败）：${resolved.missing.join('、')}`)
      }
      io.out('[dry-run] 未执行构建、未写盘、未调用 dsh。')
      return 0
    }

    await withInstallerLock(ctx, async () => {
      if (args.uninstall) {
        await uninstallPlugin(ctx)
        return
      }
      const resolved = await resolveBinaries(ctx)
      const source = fingerprintBundle(ctx.pluginDir)
      const deploy = deployBinaries(ctx, resolved, source)
      const entry = await ensureInstalledBundle(ctx, source)
      const { warnings } = await verifyInstallation(ctx, deploy, entry)
      // Catch changes while the CLI or verifier ran, before the runtime can observe the candidate.
      if (fingerprintBundle(ctx.pluginDir).hash !== source.hash) {
        throw new InstallError('来源 bundle 在安装期间发生变化', '待来源文件稳定后重新运行安装脚本。')
      }
      const installed = inspectInstalledBundle(ctx, source)
      if (installed.mismatch !== undefined || !samePath(installed.entry, entry, platform)) {
        throw new InstallError(`已安装 bundle 在校验期间发生变化：${installed.mismatch ?? '入口模块已改变'}`)
      }
      verifyGeneration(ctx, deploy.destDir, deploy.manifest, deploy.deployed)
      verifyProfileRegistration(ctx, { quiet: true })
      const published = publishGeneration(ctx, deploy.generation)
      printInstallSummary(ctx, deploy, warnings, published)
    })
    return 0
  } catch (error) {
    printError(io, error)
    return 1
  }
}

// CLI 入口守卫：仅当直接执行本文件时运行（被测试 import 时不触发）。
const invokedPath = process.argv[1]
if (invokedPath && import.meta.url === pathToFileURL(path.resolve(invokedPath)).href) {
  main().then((code) => {
    process.exitCode = code
  })
}
