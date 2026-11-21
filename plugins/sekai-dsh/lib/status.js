const readers = { router: new Set(), worker: new Set() }

export function registerStatus(kind, read) {
  readers[kind].add(read)
  return () => readers[kind].delete(read)
}

export function statusSnapshot() {
  const routers = Array.from(readers.router, read => read())
  const workers = Array.from(readers.worker, read => read())
  return {
    routerReady: routers.some(router => router.ready),
    connectedWorkers: routers.reduce((total, router) => total + router.workerCount, 0),
    workerRunning: workers.some(worker => worker.running),
  }
}

export const inject = ['typert']

export function apply(ctx) {
  const service = { snapshot: statusSnapshot }
  service.typertRemote = { service, serviceKey: 'sekaiStatus', namespace: 'sekaiStatus' }
  ctx.provide('sekaiStatus', service)
  ctx.typert.register({
    package: '@sekai/dsh-plugin',
    face: 'host',
    schemas: [],
    model: { services: [], events: [], objects: [] },
    invocations: [{
      id: '@sekai/dsh-plugin#sekaiStatus/snapshot',
      service: 'sekaiStatus',
      namespace: 'sekaiStatus',
      method: 'snapshot',
      invocation: { kind: 'direct' },
      parameters: [],
      result: { mode: 'src-json' },
    }],
  })
}
