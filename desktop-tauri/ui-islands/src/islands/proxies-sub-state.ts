/**
 * 「订阅」区块的数据层（非岛：`.ts` 不被 import.meta.glob 当岛加载）。
 *
 * 视图层在 `proxies-sub-view.tsx`（岛内组件），本文件只放状态、取数与动作；
 * 依赖方向是视图 → 本文件 → 桥（与 proxies-state.ts 的拆分同一套约定）。
 *
 * ── 与代理池的关系 ─────────────────────────────────────────
 * 订阅节点由内核镜像进代理池（`source: "subscription"` 的只读条目），本区块
 * 只管「订阅与内核」这一层：添加 / 编辑 / 删除订阅、下载 / 重启内核。节点
 * 本身出现在下面的代理池表格里（proxies-page 的表格）—— 但**那份表格要在
 * 订阅变动后跟着刷**（见 `refreshPool`）：节点是从内核同步进池的，只刷本区块
 * 的话下面那张表会停在旧快照上，用户得手点一次「刷新」才看得见刚加/刚删的
 * 节点（实测就是这个体验）。
 *
 * ── 内核命令是异步的（轮询而不是推送）────────────────────────
 * 写操作（增删改订阅）后端投一条内核命令就立即返回：内核重启 + provider
 * 拉取要几秒到几十秒，同步等会把本地请求挂死。这里在每次写操作后轮询订阅与
 * 内核状态几轮（`pollSectionAfterChange`，顺带刷代理池），节点数 / 运行状态
 * 随之刷新；下载内核的进度也靠轮询（`downloadKernel` 自带轮询循环，直到状态
 * 离开 downloading）。不引入第二条推送通道 —— 面板页面的其它区块（余额快照 /
 * 连接数）同样轮询。
 */

import { t } from '../i18n'
import { loadPanel } from './proxies-state'

/* ─── 后端形态 ───────────────────────────────── */

/** 一条订阅（`api::subscriptions` 的 items 项；写操作返回同形全量列表） */
export type SubscriptionItem = {
  id: string
  name: string
  url: string
  enabled: boolean
  createdAt: number
  updatedAt: number
  /** 内核上次同步到的节点数（内核快照注入） */
  nodeCount: number
}

/** 内核状态（`sub_kernel::status_json`） */
export type KernelStatus = {
  installed: boolean
  version: string
  asset?: string
  running: boolean
  controllerPort?: number
  listenerCount: number
  kernelDir: string
  lastError: string | null
  /** 各订阅最后一次成功同步：`subId → {name, count, at}` */
  providerSync?: Record<string, { name?: string; count?: number; at?: number }>
  download: {
    state: 'idle' | 'downloading' | 'done' | 'error'
    received: number
    total: number
    error: string | null
  }
}

export type SubscriptionsPayload = {
  items?: SubscriptionItem[]
  kernel?: KernelStatus
  /** 删除响应里带被删条目的名称（toast 文案用） */
  removed?: string
}

/* ─── 桥（只声明本区块用到的成员）──────────────── */

export type SubscriptionBridge = {
  getSubscriptions?: () => Promise<SubscriptionsPayload | null | undefined>
  addSubscription?: (payload: Record<string, unknown>) => Promise<SubscriptionsPayload | null | undefined>
  saveSubscription?: (payload: Record<string, unknown>) => Promise<SubscriptionsPayload | null | undefined>
  refreshSubscription?: (id: string) => Promise<SubscriptionsPayload | null | undefined>
  removeSubscription?: (id: string) => Promise<SubscriptionsPayload | null | undefined>
  decodeQrImage?: (payload: { imageBase64: string }) => Promise<{ url?: string } | null | undefined>
  getKernelStatus?: () => Promise<KernelStatus | null | undefined>
  downloadKernel?: () => Promise<KernelStatus | null | undefined>
  restartKernel?: () => Promise<KernelStatus | null | undefined>
}

type SubSharedWindow = {
  workbuddyDesktop?: SubscriptionBridge
  wbApp?: {
    toast?: (message: string, kind?: 'err' | 'ok') => void
  }
  wbConfirm?: {
    ask?: (options: { title?: string; html?: string; okText?: string; okClass?: string }) => Promise<boolean>
  }
}

export function wb(): SubSharedWindow {
  return window as unknown as SubSharedWindow
}

export function toast(message: string, kind?: 'err' | 'ok'): void {
  wb().wbApp?.toast?.(message, kind)
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

/* ─── store（模块级快照 + useSyncExternalStore，照 proxies-state 的形态）─── */

type SubState = {
  data: SubscriptionsPayload | null
  loading: boolean
  error: string
  /** 写操作在途（按钮禁用，防连点） */
  pending: boolean
  /** 正在刷新的订阅 id（行内「立即更新」的文案） */
  refreshing: string | null
}

let state: SubState = {
  data: null,
  loading: false,
  error: '',
  pending: false,
  refreshing: null,
}

const listeners = new Set<() => void>()

export function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => { listeners.delete(listener) }
}

export function getSnapshot(): SubState {
  return state
}

function patch(partial: Partial<SubState>): void {
  state = { ...state, ...partial }
  listeners.forEach(listener => listener())
}

/** 当前列表（`[]` = 还没读到或一条都没有，用 `state.data` 区分） */
export function subs(): SubscriptionItem[] {
  return state.data?.items ?? []
}

/** 当前内核状态（null = 还没读到） */
export function kernel(): KernelStatus | null {
  return state.data?.kernel ?? null
}

/** 写操作返回的整份列表就地替换（与代理池同一约定） */
function accept(payload: SubscriptionsPayload | null | undefined): void {
  if (!payload || typeof payload !== 'object' || !Array.isArray(payload.items)) return
  patch({ data: payload, error: '' })
}

/**
 * 顺带把代理池表格拉一遍（静默：失败不弹提示）。
 *
 * 订阅的每个写操作都会改动池：新增 / 启用会带来新节点条目，删除 / 禁用会把
 * 条目摘掉，节点端口还要等内核重建后才写回条目 —— 所以不止写操作回来时刷
 * 一次，`pollSectionAfterChange` 的每一轮也刷（端口 / 节点数在那几秒里变）。
 */
function refreshPool(): void {
  void loadPanel(true)
}

/* ─── 取数 ───────────────────────────────────── */

export async function loadSection(silent = false): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.getSubscriptions !== 'function') {
    patch({ error: t('当前环境不支持订阅（桥接方法缺失）') })
    return
  }
  if (!silent) patch({ loading: true })
  try {
    accept(await bridge.getSubscriptions())
  } catch (error) {
    patch({ error: errorMessage(error) })
  } finally {
    if (!silent) patch({ loading: false })
  }
}

/** 写操作后的轮询上限（20 轮 × 1.5s：够覆盖「内核重启 + 首次拉订阅」的慢路径） */
const POLL_ROUNDS = 20

/**
 * 内核是否已经把**每个启用订阅**都同步过一轮。
 *
 * 「同步记录存在」= `runtime::run_restart` 的第 ⑥ 步已经跑过 —— 那一步先
 * 把节点（含端口）镜像进代理池、再记 providerSync，所以看到记录就说明**代理池
 * 已是最终状态**。这是轮询能不靠碰运气的收手判据；早先只看 `kernel.running`
 * 会误判：投命令后的头一两秒内核还是旧的那一个（还在跑），旧判据当场收手，
 * 新节点于是永远等不到刷新的那一刻。
 *
 * 空实现（内核没装 / 已报错且没在跑）也算收手：那两种情况不会再产生同步记录。
 */
function sectionSettled(payload: SubscriptionsPayload): boolean {
  const kernel = payload.kernel
  if (!kernel || kernel.download.state === 'downloading') return false
  if (!kernel.installed) return true
  if (kernel.lastError && !kernel.running) return true
  const synced = kernel.providerSync ?? {}
  return (payload.items ?? [])
    .filter(item => item.enabled !== false)
    .every(item => Boolean(synced[item.id]))
}

/**
 * 写操作后轮询整个区块几轮：内核命令是异步的（重启 + 拉订阅要几秒到几十秒），
 * **订阅列表、内核状态、代理池三者必须一起刷** —— 节点数由内核的同步结果注入
 * 列表，只刷 kernel 会让「N 个节点」徽标冻在操作瞬间的 0 上不更新（实测踩过这
 * 坑）；而节点条目本身在下面的代理池表格里，不刷它用户就看不到刚加 / 刚删的
 * 节点（实测的第二个坑：加完订阅要手点一次「刷新」）。
 *
 * 收手条件见 `sectionSettled`（每个启用订阅都有了同步记录），另有 20 轮的
 * 硬上限兜底 —— 订阅一直拉不到时界面也不能永远轮下去。
 */
async function pollSectionAfterChange(): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.getSubscriptions !== 'function') return
  for (let round = 0; round < POLL_ROUNDS; round += 1) {
    await new Promise(resolve => setTimeout(resolve, 1500))
    refreshPool()
    let payload: SubscriptionsPayload | null | undefined
    try {
      payload = await bridge.getSubscriptions()
    } catch {
      return
    }
    accept(payload)
    // 至少刷过一轮再判收手：让列表与池都至少拿到一次写操作后的快照
    if (round >= 1 && payload && Array.isArray(payload.items) && sectionSettled(payload)) return
  }
}

/* ─── 订阅动作 ───────────────────────────────── */

export async function addSubscription(name: string, url: string): Promise<SubscriptionsPayload | null> {
  const bridge = wb().workbuddyDesktop
  const data = await bridge?.addSubscription?.({ name: name.trim(), url: url.trim(), enabled: true })
  accept(data)
  toast(t('已新增订阅「{name}」，内核正在同步节点…', { name: name.trim() }))
  refreshPool()
  void pollSectionAfterChange()
  return data ?? null
}

export async function saveSubscription(
  item: SubscriptionItem,
  payload: { name: string; url: string; enabled: boolean },
): Promise<void> {
  const bridge = wb().workbuddyDesktop
  const data = await bridge?.saveSubscription?.({ id: item.id, ...payload })
  accept(data)
  toast(t('已保存「{name}」', { name: payload.name.trim() }))
  refreshPool()
  void pollSectionAfterChange()
}

/** 行内启停（整份覆盖语义：名称与链接原样回传） */
export async function toggleSubscription(item: SubscriptionItem, enabled: boolean): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (state.pending) return
  patch({ pending: true })
  try {
    const data = await bridge?.saveSubscription?.({ id: item.id, name: item.name, url: item.url, enabled })
    accept(data)
    toast(enabled ? t('已启用「{name}」', { name: item.name }) : t('已禁用「{name}」', { name: item.name }))
    refreshPool()
    void pollSectionAfterChange()
  } catch (error) {
    toast(t('保存失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ pending: false })
  }
}

/** 行内「立即更新」：强制拉取订阅内容（后端节点变了会自动重启内核） */
export async function refreshSubscription(item: SubscriptionItem): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (state.refreshing || state.pending) return
  patch({ refreshing: item.id })
  try {
    const data = await bridge?.refreshSubscription?.(item.id)
    accept(data)
    toast(t('已刷新「{name}」，正在同步节点…', { name: item.name }))
    refreshPool()
    void pollSectionAfterChange()
  } catch (error) {
    toast(t('刷新失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ refreshing: null })
  }
}

export async function removeSubscription(item: SubscriptionItem): Promise<void> {
  const bridge = wb().workbuddyDesktop
  const ok = await window.wbConfirm?.ask?.({
    title: t('删除订阅'),
    html: t('确定删除「<strong>{name}</strong>」？它在代理池里的节点会一并移除，引用这些节点的账号会回退直连。', {
      name: item.name || item.id,
    }),
    okText: t('删除'),
    okClass: 'danger',
  })
  if (!ok) return
  if (state.pending) return
  patch({ pending: true })
  try {
    const data = await bridge?.removeSubscription?.(item.id)
    accept(data)
    toast(t('已删除「{name}」', { name: item.name || item.id }))
    refreshPool()
    void pollSectionAfterChange()
  } catch (error) {
    toast(t('删除失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ pending: false })
  }
}

/* ─── 内核动作 ───────────────────────────────── */

/**
 * 下载内核（带轮询循环：每 1s 读一次状态直到离开 downloading，上限 300s
 * —— 20-40MB 在慢线路上可能要几分钟；期间进度条随 received/total 变）。
 */
export async function downloadKernel(): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.downloadKernel !== 'function') {
    toast(t('当前环境不支持订阅（桥接方法缺失）'), 'err')
    return
  }
  if (state.pending) return
  patch({ pending: true })
  try {
    const kernel = await bridge.downloadKernel()
    if (kernel && state.data) patch({ data: { ...state.data, kernel } })
    // 轮询直到下载结束（状态离开 downloading）或超时（300 轮）
    for (let round = 0; round < 300; round += 1) {
      await new Promise(resolve => setTimeout(resolve, 1000))
      const next = await bridge.getKernelStatus?.()
      if (next && state.data) patch({ data: { ...state.data, kernel: next } })
      if (!next || next.download.state !== 'downloading') break
    }
    const finalKernel = state.data?.kernel
    if (finalKernel?.download.state === 'error') {
      toast(t('内核下载失败：{reason}', { reason: finalKernel.download.error || t('未知错误') }), 'err')
    } else if (finalKernel?.download.state === 'done') {
      // 下载完成：后端已自动拉起内核（有启用的订阅时），再轮几轮看运行状态
      toast(t('✅ 内核下载完成，正在启动…'))
      void pollSectionAfterChange()
    }
  } catch (error) {
    toast(t('内核下载失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ pending: false })
  }
}

/** 手动启动 / 重启内核（重建配置；节点数随轮询刷新） */
export async function restartKernel(): Promise<void> {
  const bridge = wb().workbuddyDesktop
  if (state.pending) return
  patch({ pending: true })
  try {
    const kernel = await bridge?.restartKernel?.()
    if (kernel && state.data) patch({ data: { ...state.data, kernel } })
    toast(t('内核正在启动…'))
    // 重启会重建全部监听器（端口可能换号）→ 池里的地址要跟着刷
    refreshPool()
    void pollSectionAfterChange()
  } catch (error) {
    toast(t('内核启动失败：{reason}', { reason: errorMessage(error) }), 'err')
  } finally {
    patch({ pending: false })
  }
}

/**
 * 从图片解二维码（服务端解码）：图片转 base64 上传，返回订阅链接。
 * @param file 剪贴板 / 文件选择器拿到的图片文件
 * @returns 订阅链接；识别失败时抛错（调用方 toast）
 */
export async function decodeQrImage(file: File): Promise<string> {
  const bridge = wb().workbuddyDesktop
  if (typeof bridge?.decodeQrImage !== 'function') {
    throw new Error(t('当前环境不支持订阅（桥接方法缺失）'))
  }
  const imageBase64 = await new Promise<string>((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve(String(reader.result || ''))
    reader.onerror = () => reject(new Error(t('图片读取失败')))
    reader.readAsDataURL(file)
  })
  const data = await bridge.decodeQrImage({ imageBase64 })
  const url = data?.url
  if (!url) throw new Error(t('二维码里没有订阅链接'))
  return url
}
