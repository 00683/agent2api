import * as React from 'react'
import { createRoot } from 'react-dom/client'
import {
  Badge,
  Button,
  Dialog,
  DialogBody,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  Progress,
  Switch,
} from '@ui'
import { t } from '../i18n'

/**
 * 「提供商插件」页 —— React 岛（一级菜单，排在「设置」上面）。
 *
 * ── 这一页为什么必须存在 ────────────────────────────────────
 * 各家 provider 的转发与账号实现都在独立的插件包里（`ProvidersPlugin/Agent2Providers`，
 * 不随主仓库发布）。没装插件时所有内置家都只是占位适配器：转发与添加账号
 * 一律 503「需要安装提供商插件」。这一页就是**装插件**的唯一入口。
 *
 * ── 版式：两栏卡片网格（商店形态）───────────────────────────
 * 一个包一张卡，两列排布；窗口窄于 900px 时退成单列。这样以后多一个插件
 * 只是多一张卡，不必重排版面。每张卡自带动作区（安装 / 更新 / 卸载）与
 * 自己的进度条 —— 多个包同时操作时状态互不干扰。
 *
 * ── 安装有两条路 ────────────────────────────────────────────
 *   · **从仓库安装**（主路径）：服务端查 Release、按平台挑包、下载、验签、
 *     落盘。用户只需要选平台（默认当前平台），不碰文件系统。
 *   · **从文件安装**（兜底）：自建包、离线环境、内网分发走这条。
 *     两条路最终都进 `plugin_host::install_zip`，信任链完全一致。
 *
 * ── 数据来源（全部走 `api_request` 桥）─────────────────────
 * 桌面端页面来自 `tauri://` 协议，裸 fetch('/api/…') 打不到后端；headless
 * 形态下 `/api/plugins` 挂在 protected 上要凭据。两条都由桥补齐（壳侧带
 * X-API-Key，web_shim 带 Key 与面板令牌），所以这里统一 `invoke('api_request')`。
 *
 * ── 端点（与后端 api::plugins 一一对应）──────────────────────
 *   GET  /api/plugins                    列表 + 宿主状态
 *   GET  /api/plugins/release            仓库里有什么可装（不要求已装）
 *   POST /api/plugins/install-from-release {target} —— 从仓库下载并安装
 *   POST /api/plugins/install            {packB64} —— 从文件安装
 *   POST /api/plugins/uninstall          {id}
 *   POST /api/plugins/enable             {id, enabled}
 *   POST /api/plugins/provider           {id, provider, enabled}
 *   GET  /api/plugins/update             查已装包有没有新版
 *   POST /api/plugins/update             下载并安装新版
 *
 * ── 更新为什么不是自动的 ────────────────────────────────────
 * 插件包是能执行任意代码的二进制。静默替换它 = 一次无人确认的代码执行；
 * 用户装的若是自建包，更新的还是别人维护的那一份。所以只有点了「检查更新」
 * 才动网络，装之前也照常重新验签（旧版本验过不代表新版本可信）。
 */

/* ─── 契约类型（与后端 serde 形状逐字对应）─────────────── */

type PackProvider = {
  id: string
  label: string
  enabled: boolean
  /** 这一家此刻是否真的装进了运行时注册表（宿主在跑且握手成功） */
  loaded: boolean
}

type PackSummary = {
  id: string
  name: string
  version: string
  enabled: boolean
  signed: boolean
  installedAt: number
  entryPresent: boolean
  protocol: number
  homepage?: string | null
  /** 包内图标（后端内联成 data URI；没有则为 null，界面回落字母徽章） */
  icon?: string | null
  providers: PackProvider[]
}

type HostState = {
  running: boolean
  providers: string[]
  crashes: number
  disabled: boolean
  lastError?: string | null
} | null

type PluginsPayload = {
  packs: PackSummary[]
  host: HostState
  /** 已加载插件申报的管理面板（`["upstream_headers", …]`）；没装插件时是空数组 */
  adminPanels?: string[]
  target: string
  protocol: number
  pluginsDir: string
}

/** 商店页的仓库清单（与后端 `api::plugins::release` 逐字段对应） */
type ReleasePlatform = {
  target: string
  size: number
  url: string
  current: boolean
}

/** 商店里的一个插件（与后端 `api::plugins::release` 的 plugins[] 逐字段对应） */
type StorePlugin = {
  id: string
  name: string
  description: string
  official: boolean
  version: string | null
  page: string | null
  platforms: ReleasePlatform[]
  /** 图标（后端从索引仓库内联成 data URI）；null = 没有，回落字母徽章 */
  icon?: string | null
}

type ReleaseState = {
  available: boolean
  current: string
  plugins: StorePlugin[]
  reason: string | null
  /** true = 查询本身失败（网络 / 限额），界面报红；false = 「仓库没发版」这类中性结论 */
  error: boolean
}

/** 更新检查的响应（与后端 `plugin_host::update::check` 逐字段对应） */
type UpdateState = {
  available: boolean
  current: string | null
  latest: string | null
  asset: { name: string; size: number; url: string; page: string } | null
  reason: string | null
  error: boolean
}

/**
 * GitHub 令牌状态（`/api/update/token` 的返回）：**没有本体**，只有有没有、来自哪。
 *
 * ── 与「设置 → 更新 → 更新设置」是同一个值 ──────────────────
 * 两边都打同一个端点、读同一个配置键（`githubToken`），所以任一处填过，
 * 另一处立刻就是「已填写」。这里不新建任何存储 —— 建第二份就会分叉成
 * 「到底哪份在生效」。
 */
type GithubTokenStatus = {
  filled?: boolean
  /** `stored`（界面保存）/ `env`（环境变量）/ null（没配） */
  origin?: string | null
  /** 已存令牌解不开时的原因（界面据此提示重新保存一次） */
  error?: string | null
  /** 仅 POST 有：是否落盘成功（false = 本次运行已生效，重启会丢） */
  saved?: boolean
}

/** 桌面桥的最小形态（与别的岛同一手法：只认本页用到的成员） */
type Bridge = {
  __TAURI_INTERNALS__?: {
    invoke?: (command: string, args?: unknown) => Promise<unknown>
  }
  wbApp?: { toast?: (message: string, kind?: 'err' | 'ok') => void }
  wbIcons?: { icon?: (name: string, size?: number) => string }
}

function bridge(): Bridge {
  return window as unknown as Bridge
}

function toast(message: string, kind?: 'err' | 'ok'): void {
  bridge().wbApp?.toast?.(message, kind)
}

/** 一次管理 API 调用（走壳桥，见文件头）。失败抛一句可读的中文。 */
async function api<T>(method: string, path: string, body?: unknown): Promise<T> {
  const internals = bridge().__TAURI_INTERNALS__
  if (!internals || typeof internals.invoke !== 'function') {
    throw new Error(t('桌面运行时不可用（Tauri 未初始化）'))
  }
  const result = await internals.invoke('api_request', {
    request: { method, path, body: body === undefined ? null : body },
  })
  // 壳的 api_request 已经剥过 `{success, data}` 外壳；这里再兜一次，
  // 兼容直接返回信封的形态（web_shim 的降级路径）
  if (result && typeof result === 'object' && 'success' in (result as Record<string, unknown>)) {
    const envelope = result as { success?: boolean; data?: unknown; error?: string }
    if (envelope.success === false) throw new Error(envelope.error || t('请求失败'))
    return envelope.data as T
  }
  return result as T
}

/* ─── 工具函数 ─────────────────────────────── */

/** 时间戳 → 本地时间串（后端给毫秒；0 = 未知） */
function formatTime(value: number): string {
  if (!value) return '—'
  try {
    return new Date(value).toLocaleString()
  } catch {
    return '—'
  }
}

/** 字节 → 人类可读（与后端 `api::plugins::human_size` 同一口径） */
function formatSize(bytes: number): string {
  if (!bytes) return ''
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} KB`
  return `${bytes} B`
}

/**
 * 平台三元组 → 给人看的名字。
 *
 * 认不出的（第三方包里的自定义三元组）原样返回 —— 编一个「未知平台」不如
 * 让用户看到真实字符串，他至少能拿去搜索。
 */
function platformLabel(target: string): string {
  const known: Record<string, string> = {
    'x86_64-pc-windows-msvc': t('Windows (x64)'),
    'aarch64-pc-windows-msvc': t('Windows (ARM64)'),
    'x86_64-apple-darwin': t('macOS (Intel)'),
    'aarch64-apple-darwin': t('macOS (Apple Silicon)'),
    'x86_64-unknown-linux-gnu': t('Linux (amd64)'),
    'aarch64-unknown-linux-gnu': t('Linux (arm64)'),
  }
  return known[target] || target
}

/* ─── 小组件 ───────────────────────────────── */

/** 卡片骨架：圆角、描边、悬浮加深 */
function Card({ children, accent }: { children: React.ReactNode; accent?: 'installed' }): React.ReactElement {
  return (
    <article
      className={
        'flex flex-col overflow-hidden rounded-lg border bg-surface shadow-1 transition-shadow hover:shadow-2 ' +
        (accent === 'installed' ? 'border-success/40' : 'border-border')
      }
    >
      {children}
    </article>
  )
}

/** 卡片图标（有包内图标时显示图片，否则渐变方块 + 一个字） */
function CardIcon({
  text, tone, src,
}: {
  text: string
  tone?: 'installed' | 'muted'
  src?: string | null
}): React.ReactElement {
  const style: React.CSSProperties =
    tone === 'installed'
      ? { background: 'linear-gradient(150deg, var(--success), color-mix(in srgb, var(--success) 62%, oklch(0.7 0.2 200)))' }
      : tone === 'muted'
        ? { background: 'linear-gradient(150deg, var(--surface-3), var(--surface-2))', color: 'var(--text-3)' }
        : { background: 'linear-gradient(150deg, var(--primary), color-mix(in srgb, var(--primary) 62%, oklch(0.7 0.2 200)))' }
  // 图标是包作者给的方形图，自带底色；用 object-cover 裁成同一块 44px 方块，
  // 与字母徽章的尺寸/圆角保持一致，卡片头部不会因为换图而跳动
  if (src) {
    return (
      <div className='size-11 shrink-0 overflow-hidden rounded-xl bg-surface-3 shadow-glow'>
        <img src={src} alt='' className='size-full object-cover' />
      </div>
    )
  }
  return (
    <div
      className='grid size-11 shrink-0 place-items-center rounded-xl text-[19px] font-bold text-white shadow-glow'
      style={style}
    >
      {text}
    </div>
  )
}

/** 卡片头部：图标 + 标题 + 版本 + 徽标 */
function CardHead({
  icon, title, version, badges, tone, iconSrc,
}: {
  icon: string
  title: string
  version?: string | null
  badges?: React.ReactNode
  tone?: 'installed' | 'muted'
  iconSrc?: string | null
}): React.ReactElement {
  return (
    <div className='flex gap-3 px-4 pt-4'>
      <CardIcon text={icon} tone={tone} src={iconSrc} />
      <div className='min-w-0 flex-1'>
        <h3 className='mb-1 flex flex-wrap items-center gap-2 text-[14px] font-semibold leading-snug'>
          {title}
          {version ? (
            <span className='text-[11.5px] font-normal tabular-nums text-muted-foreground'>v{version}</span>
          ) : null}
        </h3>
        {badges ? <div className='flex flex-wrap gap-1.5'>{badges}</div> : null}
      </div>
    </div>
  )
}

/** 卡片元信息行（平台 / 大小 / 时间这类只读值） */
function MetaRow({ items }: { items: Array<{ k: string; v: React.ReactNode; mono?: boolean }> }): React.ReactElement {
  return (
    <div className='flex flex-wrap items-center gap-x-3 gap-y-1 px-4 pt-3 text-[11px] text-muted-foreground'>
      {items.map(item => (
        <span key={item.k} className='flex items-center gap-1'>
          <span>{item.k}</span>
          <span className={'font-medium text-foreground/80' + (item.mono ? ' font-mono text-[10.5px]' : '')}>
            {item.v}
          </span>
        </span>
      ))}
    </div>
  )
}

/** 卡片动作区（贴底） */
function CardFoot({ children }: { children: React.ReactNode }): React.ReactElement {
  return <div className='mt-auto flex items-center gap-2 px-4 pb-4 pt-3.5'>{children}</div>
}

/** 卡片内进度（安装/更新时） */
function CardProgress({ stage, percent }: { stage: string; percent: number }): React.ReactElement {
  return (
    <div className='px-4 pb-3.5'>
      <div className='mb-1.5 flex items-center justify-between text-[11.5px]'>
        <span className='text-foreground/80'>{stage}</span>
        <span className='tabular-nums text-muted-foreground'>{Math.round(percent)}%</span>
      </div>
      <Progress value={percent} />
    </div>
  )
}

/* ─── 页面 ─────────────────────────────────── */

function PluginsPage(): React.ReactElement {
  const [payload, setPayload] = React.useState<PluginsPayload | null>(null)
  const [release, setRelease] = React.useState<ReleaseState | null>(null)
  const [error, setError] = React.useState<string>('')
  const [busy, setBusy] = React.useState<string>('')
  const [confirmRemove, setConfirmRemove] = React.useState<PackSummary | null>(null)
  const [update, setUpdate] = React.useState<UpdateState | null>(null)
  /** 安装弹窗：`null` = 关闭；对象 = 打开并预选平台（`id` 是商店里的插件 id） */
  const [installFor, setInstallFor] = React.useState<{ target: string; id: string } | null>(null)
  /** 「插件设置」弹窗 */
  const [settingsOpen, setSettingsOpen] = React.useState(false)
  /** 插件设置里的 GitHub 令牌（与「更新设置」同一个后端键，不单独存） */
  const [token, setToken] = React.useState<GithubTokenStatus | null>(null)
  const [tokenDraft, setTokenDraft] = React.useState('')
  const [tokenBusy, setTokenBusy] = React.useState(false)
  /** 安装进度（卡片内显示；`null` = 不在装） */
  const [progress, setProgress] = React.useState<{ stage: string; percent: number } | null>(null)
  const fileRef = React.useRef<HTMLInputElement | null>(null)

  const load = React.useCallback(async (): Promise<void> => {
    try {
      const next = await api<PluginsPayload>('GET', '/api/plugins')
      setPayload(next)
      setError('')
      // 装 / 卸 / 启停之后侧边栏的插件页要跟着变（页面由插件包申报）。
      // 走 app.js 的同一个入口，避免两处各自解析 `packs[].ui[]`。
      const host = window as unknown as {
        wbApp?: { refreshPluginPages?: () => Promise<void> | void }
      }
      void host.wbApp?.refreshPluginPages?.()
      // 商店清单与已装列表一起取：两个请求都打 GitHub，但一个是本地状态、
      // 一个是远端清单，缺任何一个页面都不完整。失败只影响商店区。
      try {
        setRelease(await api<ReleaseState>('GET', '/api/plugins/release'))
      } catch {
        // 仓库查不到时商店区显示「暂时查不到」，已装列表照常可用
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    }
  }, [])

  React.useEffect(() => { void load() }, [load])

  // 每次打开「插件设置」都现拉令牌状态：用户可能刚在「更新设置」里填过，
  // 复用缓存会让两处显示不一致
  React.useEffect(() => {
    if (!settingsOpen) return
    setTokenDraft('')
    void loadToken()
  }, [settingsOpen])

  // 把本实例的刷新入口交给模块级的对外契约（见文件尾的 wbPluginsPage）
  React.useEffect(() => {
    currentLoad = load
    return () => { if (currentLoad === load) currentLoad = null }
  }, [load])

  async function run(key: string, action: () => Promise<void>): Promise<void> {
    if (busy) return
    setBusy(key)
    try {
      await action()
    } catch (cause) {
      toast(cause instanceof Error ? cause.message : String(cause), 'err')
    } finally {
      setBusy('')
      setProgress(null)
    }
  }

  /** 从文件安装（兜底路径：自建包、离线环境） */
  async function pickFile(): Promise<void> {
    const input = fileRef.current
    if (!input) return
    input.value = ''
    input.click()
  }

  async function onFile(event: React.ChangeEvent<HTMLInputElement>): Promise<void> {
    const file = event.currentTarget.files?.[0]
    if (!file) return
    await run('install-file', async () => {
      setProgress({ stage: t('正在读取文件…'), percent: 8 })
      const encoded = await readBase64(file)
      setProgress({ stage: t('正在校验包内容…'), percent: 45 })
      const result = await api<{ packs: PackSummary[]; host: HostState; hostError?: string | null }>(
        'POST', '/api/plugins/install', { packB64: encoded },
      )
      setPayload(prev => prev ? { ...prev, packs: result.packs, host: result.host } : prev)
      // 手工装的包可能正好就是「检查更新」提示的那一版，留着会让人以为还没装
      setUpdate(null)
      if (result.hostError) toast(result.hostError, 'err')
      else toast(t('插件包已安装'))
    })
  }

  /**
   * 从仓库安装（主路径）。
   *
   * 服务端自己查 Release 并按平台挑包再下载 —— 前端只传平台三元组，不传 URL
   * （见后端 `api::plugins::install_from_release` 的说明）。
   * 这个请求要跑十几秒，卡片内显示进度并禁用该卡的全部按钮。
   */
  async function installFromRelease(target: string, id: string): Promise<void> {
    setInstallFor(null)
    await run(`install:${id}`, async () => {
      setProgress({ stage: t('正在下载…'), percent: 12 })
      // 真实进度拿不到（后端一次性返回），用阶段文案表达「在做什么」，
      // 百分比停在下载阶段的上界，避免假装精确
      const tick = window.setInterval(() => {
        setProgress(prev => {
          if (!prev) return prev
          if (prev.percent >= 55) return prev
          return { ...prev, percent: Math.min(55, prev.percent + 3) }
        })
      }, 220)
      try {
        const result = await api<{ packs: PackSummary[]; host: HostState; hostError?: string | null }>(
          'POST', '/api/plugins/install-from-release', { target, id },
        )
        setPayload(prev => prev ? { ...prev, packs: result.packs, host: result.host } : prev)
        setUpdate(null)
        if (result.hostError) toast(result.hostError, 'err')
        else toast(t('插件包已安装'))
      } finally {
        window.clearInterval(tick)
      }
    })
  }

  async function setPackEnabled(pack: PackSummary, enabled: boolean): Promise<void> {
    await run(`pack:${pack.id}`, async () => {
      const result = await api<{ packs: PackSummary[]; host: HostState; hostError?: string | null }>(
        'POST', '/api/plugins/enable', { id: pack.id, enabled },
      )
      setPayload(prev => prev ? { ...prev, packs: result.packs, host: result.host } : prev)
      if (result.hostError) toast(result.hostError, 'err')
    })
  }

  async function setProviderEnabled(pack: PackSummary, provider: PackProvider, enabled: boolean): Promise<void> {
    await run(`provider:${provider.id}`, async () => {
      const result = await api<{ packs: PackSummary[]; host: HostState; hostError?: string | null }>(
        'POST', '/api/plugins/provider', { id: pack.id, provider: provider.id, enabled },
      )
      setPayload(prev => prev ? { ...prev, packs: result.packs, host: result.host } : prev)
      if (result.hostError) toast(result.hostError, 'err')
    })
  }

  async function remove(pack: PackSummary): Promise<void> {
    setConfirmRemove(null)
    await run(`remove:${pack.id}`, async () => {
      const result = await api<{ packs: PackSummary[]; host: HostState; hostError?: string | null }>(
        'POST', '/api/plugins/uninstall', { id: pack.id },
      )
      setPayload(prev => prev ? { ...prev, packs: result.packs, host: result.host } : prev)
      if (result.hostError) toast(result.hostError, 'err')
      else toast(t('插件包已卸载'))
    })
  }

  /**
   * 查已装包有没有新版。
   *
   * 刻意与「升级」分开：查询只读 GitHub，失败也只是「查不到」；把两步合成
   * 一个按钮会让「点一下没反应」变成常态（仓库没发版、断网、限额都长一个样）。
   */
  async function checkUpdate(): Promise<void> {
    await run('check-update', async () => {
      const result = await api<UpdateState>('GET', '/api/plugins/update')
      setUpdate(result)
      if (result.error) toast(result.reason || t('检查更新失败'), 'err')
    })
  }

  /**
   * 读 GitHub 令牌状态。
   *
   * 打的是 `/api/update/token` —— 与「设置 → 更新 → 更新设置」同一个端点。
   * 两边读写同一个配置键，所以任一处填过，另一处立刻显示「已填写」。
   */
  async function loadToken(): Promise<void> {
    try {
      setToken(await api<GithubTokenStatus>('GET', '/api/update/token'))
    } catch {
      // 读不到保持 null（显示「读取中」），保存动作会带出新状态
    }
  }

  async function saveToken(): Promise<void> {
    const value = tokenDraft.trim()
    if (!value) {
      toast(t('请先粘贴 GitHub 令牌'), 'err')
      return
    }
    setTokenBusy(true)
    try {
      const result = await api<GithubTokenStatus>('POST', '/api/update/token', { token: value })
      setToken(result ?? null)
      setTokenDraft('')
      if (result?.saved === false) toast(t('令牌已生效，但写入磁盘失败（重启后会丢失）'), 'err')
      else toast(t('✅ GitHub 令牌已保存（加密存储，界面不再显示）'))
    } catch (error) {
      toast(t('保存失败：{error}', { error: error instanceof Error ? error.message : String(error) }), 'err')
    } finally {
      setTokenBusy(false)
    }
  }

  async function clearToken(): Promise<void> {
    setTokenBusy(true)
    try {
      const result = await api<GithubTokenStatus>('POST', '/api/update/token', { token: null })
      setToken(result ?? null)
      toast(t('✅ 已清除界面保存的 GitHub 令牌（环境变量若配置过则继续生效）'))
    } catch (error) {
      toast(t('清除失败：{error}', { error: error instanceof Error ? error.message : String(error) }), 'err')
    } finally {
      setTokenBusy(false)
    }
  }

  /** 下载并安装最新版（已装包的升级路径） */
  async function runUpdate(): Promise<void> {
    await run('update', async () => {
      setProgress({ stage: t('正在下载…'), percent: 15 })
      const result = await api<{
        packs: PackSummary[]
        host: HostState
        hostError?: string | null
        pack?: PackSummary
        updatedFrom?: string
      }>('POST', '/api/plugins/update')
      setPayload(prev => prev ? { ...prev, packs: result.packs, host: result.host } : prev)
      setUpdate(null)
      if (result.hostError) toast(result.hostError, 'err')
      else toast(t('插件包已更新到 v{version}', { version: result.pack?.version || '' }))
    })
  }

  /** 用系统浏览器打开 Release 页（与设置页同一条出口：壳命令 open_release_page） */
  async function openPage(url: string): Promise<void> {
    if (!/^https?:\/\//i.test(url)) return
    try {
      const internals = bridge().__TAURI_INTERNALS__
      await internals?.invoke?.('open_release_page', { url })
    } catch {
      // 打不开浏览器不影响本页功能，静默
    }
  }

  const packs = payload?.packs ?? []
  const host = payload?.host ?? null
  const platformMismatch = payload
    ? packs.filter(pack => pack.protocol !== payload.protocol)
    : []
  const locked = busy !== ''
  const installedIds = new Set(packs.map(pack => pack.id))
  // 商店里可安装的插件：索引里有、且本地还没装同一个 id。
  // 索引读不到时后端回落成官方包一条，这里不用区分。
  const storePlugins = (release?.plugins ?? []).filter(item => !installedIds.has(item.id))
  const currentTarget = payload?.target || release?.current || ''

  return (
    <>
      <div className='panel'>
        <div className='panel-head'>
          <div>
            <h2>{t('插件')}</h2>
            <p className='muted'>
              {t('各家提供商的转发与账号实现都在插件包里，不随本软件发布。装好插件后，模型转发、添加账号、签到与余额查询才会生效。')}
            </p>
          </div>
          <div className='flex flex-wrap items-center justify-end gap-2'>
            {host?.running ? (
              <Badge variant='default'>{t('运行中')}</Badge>
            ) : packs.length ? (
              <Badge variant={host?.disabled ? 'destructive' : 'secondary'}>
                {host?.disabled ? t('已停用') : t('未运行')}
              </Badge>
            ) : (
              <Badge variant='outline'>{t('未安装')}</Badge>
            )}
            <Button variant='outline' size='sm' disabled={locked} onClick={() => void load()}>
              {busy === 'refresh' ? t('刷新中…') : t('刷新')}
            </Button>
            <Button variant='outline' size='sm' disabled={!packs.length || locked}
              onClick={() => void checkUpdate()}>
              {busy === 'check-update' ? t('检查中…') : t('检查更新')}
            </Button>
            <Button variant='outline' size='sm' onClick={() => setSettingsOpen(true)}>
              {t('插件设置')}
            </Button>
            <Button variant='ghost' size='sm' disabled={locked} onClick={() => void pickFile()}>
              {t('从文件安装…')}
            </Button>
          </div>
        </div>

        {/* 更新结果条：只在查过之后出现（未查 = null，什么都不显示） */}
        {update ? (
          <div className='px-4 pb-3'>
            <div
              className={
                update.available
                  ? 'flex flex-wrap items-center gap-3 rounded-md border border-primary-bd bg-primary-soft px-3 py-2'
                  : update.error
                    ? 'flex flex-wrap items-center gap-3 rounded-md border border-destructive-bd bg-destructive-soft px-3 py-2'
                    : 'flex flex-wrap items-center gap-3 rounded-md border border-border/60 bg-muted/30 px-3 py-2'
              }
            >
              <div className='min-w-0 flex-1'>
                {update.available ? (
                  <>
                    <p className='text-[13px] font-semibold'>
                      {t('有新版本：v{current} → v{latest}', {
                        current: update.current || '—',
                        latest: update.latest || '—',
                      })}
                    </p>
                    <p className='muted text-[12px]'>
                      {t('升级会重新下载并校验插件包（约 {size}），装完立即生效，账号数据不受影响。', {
                        size: formatSize(update.asset?.size || 0) || t('十几 MB'),
                      })}
                    </p>
                  </>
                ) : (
                  <p className={update.error ? 'text-[12.5px] text-destructive' : 'muted text-[12.5px]'}>
                    {update.reason || t('没有可更新的版本')}
                  </p>
                )}
              </div>
              {update.available ? (
                <div className='flex items-center gap-2'>
                  {update.asset?.page ? (
                    <Button variant='outline' size='sm' onClick={() => void openPage(update.asset?.page || '')}>
                      {t('查看发布说明')}
                    </Button>
                  ) : null}
                  <Button size='sm' disabled={busy === 'update'} onClick={() => void runUpdate()}>
                    {busy === 'update' ? t('更新中…') : t('立即升级')}
                  </Button>
                </div>
              ) : (
                <Button variant='ghost' size='sm' onClick={() => setUpdate(null)}>{t('关闭')}</Button>
              )}
            </div>
          </div>
        ) : null}

        <div className='px-4 pb-3'>
          <input ref={fileRef} type='file' accept='.zip,application/zip' className='hidden'
            onChange={event => void onFile(event)} />
          {error ? (
            <p className='mb-2 text-[12.5px] text-destructive'>{t('读取插件状态失败：{error}', { error })}</p>
          ) : null}

          {platformMismatch.length ? (
            <p className='mb-2 text-[12.5px] text-destructive'>
              {t('有插件包的协议版本与当前网关不一致（{versions}），请下载对应版本的插件包。', {
                versions: platformMismatch.map(pack => `${pack.id} v${pack.version} → ${pack.protocol}`).join('、'),
              })}
            </p>
          ) : null}

          {/* ── 商店网格：两栏 ── */}
          <div className='grid grid-cols-1 items-start gap-3.5 min-[900px]:grid-cols-2'>
            {storePlugins.map(item => {
              // 每个插件自己的当前平台包：没有可装平台时按钮禁用，
              // 但仍显示这张卡 —— 「这个插件存在但还没发版」是有用信息
              const platform = item.platforms.find(entry => entry.target === currentTarget)
                || item.platforms.find(entry => entry.current)
              const installable = Boolean(item.version && platform)
              return (
                <Card key={item.id}>
                  <CardHead
                    icon={item.name.slice(0, 1).toUpperCase() || 'A'}
                    iconSrc={item.icon}
                    title={item.name}
                    version={item.version}
                    badges={
                      <>
                        {item.official ? <Badge variant='brand'>{t('官方')}</Badge> : null}
                        {/* 商店里的包**还没有下载**，网关手上只有索引里的元数据 ——
                            签名要解包才看得见，所以这里不能显示「未签名」：
                            那会把每一个可装的包都标成未签名（包括已签好的官方包），
                            变成一句必然说错的话。签名状态在装完之后按实际内容显示
                            （见下方已装列表的 `pack.signed`）。 */}
                        {item.version ? null : <Badge variant='outline'>{t('暂未发布')}</Badge>}
                      </>
                    }
                  />
                  <p className='line-clamp-3 px-4 pt-2.5 text-[12px] leading-relaxed text-muted-foreground'>
                    {item.description || t('没有说明')}
                  </p>
                  <MetaRow
                    items={[
                      { k: t('当前平台'), v: currentTarget || '—', mono: true },
                      { k: t('大小'), v: formatSize(platform?.size || 0) || '—' },
                    ]}
                  />
                  {busy === `install:${item.id}` && progress ? (
                    <CardProgress stage={progress.stage} percent={progress.percent} />
                  ) : (
                    <CardFoot>
                      <Button size='sm' disabled={locked || !installable}
                        onClick={() => setInstallFor({ target: currentTarget, id: item.id })}>
                        {t('下载并安装')}
                      </Button>
                      <span className='flex-1' />
                      <span className='text-[10.5px] text-muted-foreground'>
                        {platform ? t('自动匹配平台') : t('当前平台暂无包')}
                      </span>
                    </CardFoot>
                  )}
                </Card>
              )
            })}

            {/* 仓库查不到时，给一张说明卡而不是整块空白 */}
            {!storePlugins.length && !packs.length ? (
              <Card>
                <CardHead icon='!' title={t('插件仓库')}
                  badges={<Badge variant='outline'>{t('暂不可用')}</Badge>} />
                <p className='px-4 pt-2.5 text-[12px] leading-relaxed text-muted-foreground'>
                  {release?.error
                    ? t('暂时查不到插件仓库（{reason}）。可以点「刷新」重试，或用「从文件安装…」选中本地 zip。', {
                      reason: release.reason || t('网络不可达'),
                    })
                    : t('插件仓库还没有发布任何版本。可以点「刷新」重试，或用「从文件安装…」选中本地 zip。')}
                </p>
                <MetaRow items={[{ k: t('当前平台'), v: currentTarget || '—', mono: true }]} />
                <CardFoot>
                  <Button size='sm' variant='outline' disabled={locked} onClick={() => void load()}>
                    {t('刷新')}
                  </Button>
                  <Button size='sm' variant='ghost' disabled={locked} onClick={() => void pickFile()}>
                    {t('从文件安装…')}
                  </Button>
                </CardFoot>
              </Card>
            ) : null}

            {/* ── 已安装的包：一个包一张卡 ── */}
            {packs.map(pack => (
              <Card key={pack.id} accent='installed'>
                <CardHead
                  icon='A'
                  tone='installed'
                  iconSrc={pack.icon}
                  title={pack.name || pack.id}
                  version={pack.version}
                  badges={
                    <>
                      {pack.enabled
                        ? <Badge variant='default'>{t('已启用')}</Badge>
                        : <Badge variant='secondary'>{t('已停用')}</Badge>}
                      {pack.signed ? (
                        <span title={t('官方签名：包内容与发布者签的完全一致，未被改过')}>
                          <Badge variant='success'>{t('已签名')}</Badge>
                        </span>
                      ) : (
                        <span title={t('未签名：可以正常使用，但无法证明这个包出自官方；升级时同样只做格式校验')}>
                          <Badge variant='warning'>{t('未签名')}</Badge>
                        </span>
                      )}
                      {!pack.entryPresent ? <Badge variant='destructive'>{t('文件缺失')}</Badge> : null}
                    </>
                  }
                />
                <p className='px-4 pt-2.5 text-[12px] leading-relaxed text-muted-foreground'>
                  {t('{count} 家提供商 · 安装于 {time}', {
                    count: String(pack.providers.length),
                    time: formatTime(pack.installedAt),
                  })}
                </p>
                <MetaRow
                  items={[
                    { k: t('宿主'), v: host?.running ? t('运行中') : t('未运行') },
                    {
                      k: t('已加载'),
                      v: t('{count} 家', { count: String(pack.providers.filter(p => p.loaded).length) }),
                    },
                  ]}
                />
                {!pack.entryPresent ? (
                  <p className='px-4 pt-2 text-[12px] text-destructive'>
                    {t('插件包的可执行文件不在磁盘上（可能被手工删除或移动），请重新安装。')}
                  </p>
                ) : null}
                {busy === `pack:${pack.id}` || busy === `remove:${pack.id}` ? (
                  <CardProgress stage={t('正在应用…')} percent={70} />
                ) : (
                  <CardFoot>
                    <label className='flex items-center gap-1.5 text-[12px] text-muted-foreground'>
                      {t('启用')}
                      <Switch checked={pack.enabled} disabled={busy === `pack:${pack.id}`}
                        onCheckedChange={next => void setPackEnabled(pack, next === true)} />
                    </label>
                    <Button variant='outline' size='sm' disabled={busy === `remove:${pack.id}`}
                      onClick={() => setConfirmRemove(pack)}>
                      {t('卸载')}
                    </Button>
                    <span className='flex-1' />
                    {update?.available ? (
                      <Button size='sm' disabled={locked} onClick={() => void runUpdate()}>
                        {t('更新到 v{version}', { version: update.latest || '' })}
                      </Button>
                    ) : (
                      <span className='text-[10.5px] text-muted-foreground'>{t('已是最新')}</span>
                    )}
                  </CardFoot>
                )}
                {/* 逐家启停：折叠在卡内，避免默认把 22 行都铺开 */}
                <details className='border-t border-border/60'>
                  <summary className='cursor-pointer select-none px-4 py-2 text-[12px] text-muted-foreground hover:text-foreground'>
                    {t('这个包提供的提供商（可单独停用某一家）')}
                  </summary>
                  <div className='grid gap-1 px-3 pb-3 sm:grid-cols-2'>
                    {pack.providers.map(provider => (
                      <label key={provider.id}
                        className='flex items-center gap-2 rounded px-1.5 py-1 text-[12.5px] hover:bg-muted/40'>
                        <Switch checked={provider.enabled}
                          disabled={!pack.enabled || busy === `provider:${provider.id}`}
                          onCheckedChange={next => void setProviderEnabled(pack, provider, next === true)} />
                        <span className='min-w-0 truncate'>{provider.label}</span>
                        <code className='text-[11px] text-muted-foreground'>{provider.id}</code>
                        {provider.enabled && !provider.loaded ? (
                          <Badge variant='secondary'>{t('未加载')}</Badge>
                        ) : null}
                      </label>
                    ))}
                  </div>
                </details>
              </Card>
            ))}
          </div>

          {/* 诊断信息（路径 / 平台 / 协议 / 宿主错误）收在折叠区，默认不占版面 */}
          {payload ? (
            <details className='mt-3 rounded-md border border-border/60 bg-muted/30 px-3 py-2'>
              <summary className='cursor-pointer select-none text-[12px] text-muted-foreground hover:text-foreground'>
                {t('诊断信息')}
              </summary>
              <div className='pt-1.5'>
                <div className='flex flex-wrap items-baseline gap-2 py-0.5 text-[12.5px]'>
                  <span className='text-muted-foreground'>{t('插件目录')}</span>
                  <code className='min-w-0 break-all'>{payload.pluginsDir}</code>
                </div>
                <div className='flex flex-wrap items-baseline gap-2 py-0.5 text-[12.5px]'>
                  <span className='text-muted-foreground'>{t('当前平台')}</span>
                  <code>{payload.target}</code>
                </div>
                <div className='flex flex-wrap items-baseline gap-2 py-0.5 text-[12.5px]'>
                  <span className='text-muted-foreground'>{t('协议版本')}</span>
                  <code>{String(payload.protocol)}</code>
                </div>
                {host?.lastError ? (
                  <div className='flex flex-wrap items-baseline gap-2 py-0.5 text-[12.5px]'>
                    <span className='text-muted-foreground'>{t('宿主错误')}</span>
                    <span className='text-destructive'>{host.lastError}</span>
                  </div>
                ) : null}
                {host && host.crashes > 0 ? (
                  <div className='flex flex-wrap items-baseline gap-2 py-0.5 text-[12.5px]'>
                    <span className='text-muted-foreground'>{t('连续崩溃')}</span>
                    <span className='text-destructive'>{String(host.crashes)}</span>
                  </div>
                ) : null}
              </div>
            </details>
          ) : null}
        </div>

        <div className='panel-foot'>
          <span>
            {t('插件包与主程序分开安装、分开升级。卸载插件后账号数据仍然保留，重新装回即可继续使用；但转发与添加账号需要插件在位。')}
          </span>
        </div>
      </div>

      {/* ── 安装弹窗：选平台 ── */}
      <Dialog open={installFor !== null} onOpenChange={next => { if (!next) setInstallFor(null) }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              {t('安装 {name}', {
                name: storePlugins.find(item => item.id === installFor?.id)?.name
                  || installFor?.id
                  || '',
              })}
            </DialogTitle>
          </DialogHeader>
          <DialogBody>
            <p className='mb-2 text-[12.5px] text-muted-foreground'>
              {t('选择要安装的平台。装好之后这些提供商才会出现在「添加提供商」里。')}
            </p>
            <div className='flex flex-col gap-1'>
              {(storePlugins.find(item => item.id === installFor?.id)?.platforms || []).map(item => {
                const picked = installFor?.target === item.target
                return (
                  <label key={item.target}
                    className={
                      'flex cursor-pointer items-center gap-2.5 rounded-md border px-2.5 py-2 ' +
                      (picked ? 'border-primary-bd bg-primary-soft' : 'border-transparent hover:bg-muted/50')
                    }>
                    <input type='radio' name='install-target' className='accent-primary'
                      checked={picked}
                      onChange={() => setInstallFor(prev => prev ? { ...prev, target: item.target } : prev)} />
                    <span className='min-w-0 flex-1'>
                      <span className='flex items-center gap-1.5 text-[12.5px] font-medium'>
                        {platformLabel(item.target)}
                        {item.current ? <Badge variant='brand'>{t('当前平台')}</Badge> : null}
                      </span>
                      <code className='text-[10.5px] text-muted-foreground'>{item.target}</code>
                    </span>
                    <span className='shrink-0 text-[11px] tabular-nums text-muted-foreground'>
                      {formatSize(item.size) || '—'}
                    </span>
                  </label>
                )
              })}
            </div>
          </DialogBody>
          <DialogFooter>
            <Button variant='outline' onClick={() => setInstallFor(null)}>{t('取消')}</Button>
            <Button disabled={locked || !installFor}
              onClick={() => {
                if (installFor) void installFromRelease(installFor.target, installFor.id)
              }}>
              {t('下载并安装')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* ── 插件设置：目前只有 GitHub 令牌 ── */}
      <Dialog open={settingsOpen} onOpenChange={next => {
        if (!next) setSettingsOpen(false)
      }}>
        <DialogContent className='w-[min(460px,calc(100vw-48px))]'>
          <DialogHeader>
            <DialogTitle>{t('插件设置')}</DialogTitle>
          </DialogHeader>
          <DialogBody className='gap-3.5'>
            <div>
              <div className='mb-1 flex items-center justify-between gap-2'>
                <div className='text-[13px] font-semibold text-foreground'>{t('GitHub 令牌')}</div>
                {token?.error ? (
                  <Badge shape='tag' variant='destructive' title={token.error}>{t('无法解密')}</Badge>
                ) : token?.filled && token.origin === 'stored' ? (
                  <Badge shape='tag' variant='success'>{t('已填写')}</Badge>
                ) : token?.filled ? (
                  <Badge shape='tag' variant='secondary' title={t('来自环境变量 GITHUB_TOKEN')}>
                    {t('来自环境变量')}
                  </Badge>
                ) : token === null ? (
                  <Badge shape='tag' variant='outline'>{t('读取中')}</Badge>
                ) : (
                  <Badge shape='tag' variant='outline'>{t('未填写')}</Badge>
                )}
              </div>
              {/* ── 与「更新设置」是同一个值 ─────────────────────
                  两边读写同一个配置键，任一处填过另一处立刻生效。
                  这里写清楚是怕用户以为要填两次。 */}
              <p className='mb-2 text-[11.5px] leading-relaxed text-muted-foreground'>
                {t('安装与更新插件时访问 GitHub 用。与「设置 → 更新 → 更新设置」里的同一项互通，填一次即可。不填也能用，只是匿名请求限额较低（60 次/小时）。')}
              </p>
              <div className='flex items-center gap-2'>
                <input
                  type='password'
                  value={tokenDraft}
                  onChange={event => setTokenDraft(event.target.value)}
                  onKeyDown={event => {
                    if (event.key === 'Enter') void saveToken()
                  }}
                  placeholder={token?.filled && token.origin !== 'env'
                    ? t('已填写（粘贴新令牌可覆盖）')
                    : t('粘贴 GitHub 令牌（ghp_… / github_pat_…）')}
                  className='min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 py-1.5 text-[12.5px] text-foreground outline-none placeholder:text-muted-foreground focus:border-primary-bd'
                />
                <Button size='sm' disabled={tokenBusy || !tokenDraft.trim()}
                  onClick={() => void saveToken()}>
                  {t('保存')}
                </Button>
                {token?.filled && token.origin === 'stored' ? (
                  <Button size='sm' variant='ghost' disabled={tokenBusy}
                    onClick={() => void clearToken()}>
                    {t('清除')}
                  </Button>
                ) : null}
              </div>
              <a
                href='https://github.com/settings/tokens/new?description=Agent2API'
                target='_blank'
                rel='noreferrer'
                className='mt-2 inline-block text-[11.5px] text-primary underline-offset-2 hover:underline'
              >
                {t('在 GitHub 上创建令牌（无需勾选任何权限）')}
              </a>
            </div>
          </DialogBody>
          <DialogFooter>
            <Button variant='outline' onClick={() => setSettingsOpen(false)}>{t('关闭')}</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* ── 卸载确认 ── */}
      <Dialog open={confirmRemove !== null} onOpenChange={next => { if (!next) setConfirmRemove(null) }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('卸载插件包')}</DialogTitle>
          </DialogHeader>
          <DialogBody>
            <p className='text-[13px]'>
              {t('确定卸载「{name}」吗？卸载后这个包里的提供商将无法转发，也无法添加账号。账号数据本身不会被删除，重新安装后可以继续使用。', {
                name: confirmRemove?.name || confirmRemove?.id || '',
              })}
            </p>
          </DialogBody>
          <DialogFooter>
            <Button variant='outline' onClick={() => setConfirmRemove(null)}>{t('取消')}</Button>
            <Button variant='destructive' disabled={busy.startsWith('remove:')}
              onClick={() => { if (confirmRemove) void remove(confirmRemove) }}>
              {t('卸载')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}

/** File → base64（去掉 data URL 前缀，后端只认纯 base64） */
function readBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onerror = () => reject(new Error(t('读取文件失败')))
    reader.onload = () => {
      const text = typeof reader.result === 'string' ? reader.result : ''
      const index = text.indexOf('base64,')
      resolve(index >= 0 ? text.slice(index + 'base64,'.length) : text)
    }
    reader.readAsDataURL(file)
  })
}

/* ─── 挂载：接管 index.html 里既有的页面区块 ─────── */

const PAGE_SELECTOR = '.page[data-page="plugins"]'

let mounted = false

function mount(): void {
  if (mounted) return
  const section = document.querySelector<HTMLElement>(PAGE_SELECTOR)
  if (!section) return
  mounted = true
  section.replaceChildren()
  createRoot(section).render(<PluginsPage />)
}

if (document.querySelector(PAGE_SELECTOR)) mount()
else document.addEventListener('DOMContentLoaded', mount, { once: true })

/* ─── 对外契约：切到本页时刷新一次 ─────────────── */

/**
 * 已挂载实例的刷新入口（未挂载时为 null，`load()` 变成无操作）。
 *
 * 为什么用模块级可空引用而不是让 app.js 去点 DOM：岛的挂载（脚本加载时）
 * 与 app.js 的 showPage 调用**先后不确定** —— 首屏就停在插件页时，showPage
 * 在 ui.js 之前跑，那时 React 还没挂。用引用把两边解耦，谁先谁后都对。
 */
let currentLoad: (() => Promise<void>) | null = null

declare global {
  interface Window {
    /** 提供商插件页（app.js 的 showPage 切到本页时调 `load()`） */
    wbPluginsPage?: { load(): Promise<void> }
  }
}

// 对外契约：切页时由 app.js 调一次 load()（与 wbSettingsPanel / wbTasksPanel
// 同一形态）。挂不上 load 的话，切页只会看到上次的旧数据 —— 而「装完包切回来
// 看不见」是最容易让人以为安装失败的现象。
window.wbPluginsPage = {
  async load(): Promise<void> {
    await currentLoad?.()
  },
}
