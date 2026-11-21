window.__ModuleLoader__.load({
  id: '@sekai/dsh-plugin',
  factory(require) {
    const { createElement: h, useEffect, useState } = require('react')
    const slot = 'sidebar.footer.action'
    const locale = 'sekaiStatusPanel'
    const dictionaries = {
      en: {
        loading: 'Loading Sekai status…', unavailable: 'Sekai status unavailable',
        ready: 'Router ready', idle: 'Router offline',
        running: 'Local Worker running', stopped: 'Local Worker stopped',
        count: 'Connected Workers',
      },
      zh: {
        loading: '正在读取 Sekai 状态…', unavailable: '无法读取 Sekai 状态',
        ready: 'Router 就绪', idle: 'Router 离线',
        running: '本地 Worker 运行中', stopped: '本地 Worker 已停止',
        count: '已连接 Worker',
      },
    }

    function StatusPanel({ read, t, wide }) {
      const [status, setStatus] = useState(undefined)
      useEffect(() => {
        let active = true
        let timer
        async function refresh() {
          try {
            const value = await read()
            if (active) setStatus(value)
          } catch {
            if (active) setStatus(null)
          }
          if (active) timer = setTimeout(refresh, 3000)
        }
        void refresh()
        return () => {
          active = false
          clearTimeout(timer)
        }
      }, [read])
      const label = status
        ? `${t(status.routerReady ? 'ready' : 'idle')} · ${t('count')} ${status.connectedWorkers} · ${t(status.workerRunning ? 'running' : 'stopped')}`
        : t(status === undefined ? 'loading' : 'unavailable')
      return h('div', {
        role: 'status',
        title: label,
        'aria-label': label,
        style: { padding: '4px 8px', fontSize: '0.8em' },
      }, wide ? label : 'Sekai')
    }

    return {
      inject: ['slots', 'locale', 'remote'],
      apply(ctx) {
        ctx.effect(() => ctx.locale.register(locale, dictionaries))
        const remote = ctx.remote
        const mounted = remote.$mount({
          package: '@sekai/dsh-plugin',
          descriptors: [{
            id: '@sekai/dsh-plugin#sekaiStatus/snapshot',
            service: 'sekaiStatus',
            namespace: 'sekaiStatus',
            method: 'snapshot',
            invocation: { kind: 'direct' },
            parameters: [],
            result: {
              mode: 'strict',
              typeSymbol: '@sekai/dsh-plugin#StatusSnapshot',
              create: () => ({ parse: value => value }),
            },
          }],
        })
        ctx.effect(async () => {
          const dispose = await mounted
          return dispose
        })
        const read = async () => {
          await mounted
          const result = await ctx.get('remote.sekaiStatus').snapshot()
          if (!result.ok) throw new Error('Sekai status request failed')
          return result.value
        }
        ctx.slots.inject(slot, () => ctx.slots.register({
          name: slot, id: 'sekai-status-panel', locale,
          inject: () => ({ read }),
        }, StatusPanel))
      },
    }
  },
})
