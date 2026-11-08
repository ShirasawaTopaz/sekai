/**
 * index.js — 聚合导出，不承载逻辑。
 *
 * bundle 的两个组件分别从 './router' 与 './worker' 子路径挂载；本入口只
 * 为 `import { SekaiRouterPlugin } from '@sekai/dsh-plugin'` 这类整包引用
 * 提供便利。
 *
 * @module @sekai/dsh-plugin
 */
export { SekaiRouterPlugin, ROUTER_DEFAULTS, ROUTER_SPAWN_HINT, MAX_DDL_MS } from './router.js'
export {
  SekaiWorkerPlugin,
  WORKER_DEFAULTS,
  WORKER_SPAWN_HINT,
  parseConnectedLine,
  parseSwitchLine,
  parseReachedRouterLine,
  defaultWorkerStateFile,
  readWorkerState,
  lastRouterFromState,
  withLastRouter,
} from './worker.js'
export {
  JsonlProcess,
  JsonlError,
  RingBuffer,
  RESTART_BACKOFF_INITIAL_MS,
  describeError,
  describeExitInfo,
  nextBackoffMs,
  renderJson,
  resolveBin,
  resolveDshHome,
  repoRootFromModule,
  dshBinDir,
  repoLocalBin,
} from './shared.js'
