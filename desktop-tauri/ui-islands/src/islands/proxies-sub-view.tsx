/**
 * Agent2API · 「订阅」区块（机场/节点订阅 → mihomo 内核本地监听器）。
 *
 * 与 Clash Verge 的订阅导入同一形态：粘贴订阅链接（或从图片解二维码）导入，
 * 内核拉取订阅、把每个节点起成本地 mixed 监听器，节点镜像进下面的代理池
 * （`source: "subscription"` 的只读条目，账号按 id 引用）。存储与内核管理在
 * Rust 侧：`core::subscriptions`（订阅）与 `core::sub_kernel`（内核）是唯一
 * 事实来源，HTTP 契约见 `api::subscriptions` 的模块头。
 *
 * ── 数据层在隔壁 ─────────────────────────────────────────────
 * 本文件只放视图层（内核状态行 + 订阅表 + 新增/编辑弹窗）；状态、取数、
 * 动作都在 `proxies-sub-state.ts`（普通模块，与 proxies-state.ts 同一拆分）。
 *
 * ── 内核命令是异步的（界面轮询）────────────────────────────
 * 写操作后端投一条内核命令就立即返回（内核重启 + 拉订阅要几秒到几十秒），
 * 这里在每次写操作后轮询内核状态几轮（见 proxies-sub-state 的
 * pollKernelAfterChange），节点数 / 运行状态随之刷新；下载内核的进度条
 * 随轮询的 received/total 变。
 *
 * ── 二维码导入 ──────────────────────────────────────────────
 * 桌面端没有摄像头：入口是「从图片识别」按钮（文件选择器）与弹窗内的
 * 粘贴事件（Ctrl+V 剪贴板里的截图）。解码在服务端（rqrr），这里只做
 * base64 上传与回填链接；识别到单节点分享链接时后端明确报「暂不支持」。
 *
 * ── 样式 ────────────────────────────────────────────────────
 * 面板骨架（.panel / .panel-head / .models-table）与代理池面板同一套全局
 * 样式；内核状态行用既有令牌（border-border / bg-surface-2 / text-subtle），
 * 不新增 CSS 文件。
 */

import * as React from 'react'
import {
  Badge,
  Button,
  Dialog,
  DialogBody,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  Input,
  Label,
  Progress,
  Switch,
} from '@ui'
import {
  addSubscription, decodeQrImage, downloadKernel, errorMessage, kernel, refreshSubscription,
  removeSubscription, restartKernel, saveSubscription, subs, subscribe, getSnapshot, toast,
  toggleSubscription, wb, type KernelStatus, type SubscriptionItem,
} from './proxies-sub-state'
import { formatTime } from './proxies-state'
import { t } from '../i18n'

/* ─── 订阅区块 ───────────────────────────────── */

export function SubscriptionSection() {
  const snapshot = React.useSyncExternalStore(subscribe, getSnapshot)
  const items = subs()
  const kernelStatus = kernel()
  const ready = snapshot.data !== null
  const [modal, setModal] = React.useState<{ item: SubscriptionItem | null } | null>(null)

  const downloading = kernelStatus?.download.state === 'downloading'
  const downloadPercent =
    downloading && kernelStatus!.download.total > 0
      ? Math.round((kernelStatus!.download.received / kernelStatus!.download.total) * 100)
      : null

  return (
    <section className='panel'>
      <div className='panel-head'>
        <h2>{t('订阅')}</h2>
        <span className='panel-sub'>
          {ready ? t('{n} 条订阅', { n: items.length }) : t('加载中…')}
        </span>
        <div className='head-actions'>
          <Button variant='default' disabled={snapshot.pending}
            onClick={() => setModal({ item: null })}>{t('＋ 新增订阅')}</Button>
        </div>
      </div>

      <KernelRow kernel={kernelStatus} downloading={downloading || false}
        percent={downloadPercent} pending={snapshot.pending} />

      <div className='models-table-wrap'>
        <table className='models-table'>
          <colgroup>
            <col className='w-[22%]' />
            <col className='w-[36%]' />
            <col className='w-[9%]' />
            <col className='w-[13%]' />
            <col className='w-[8%]' />
            <col className='w-[12%]' />
          </colgroup>
          <thead>
            <tr>
              <th>{t('名称')}</th>
              <th>{t('订阅链接')}</th>
              <th className='ta-center'>{t('节点')}</th>
              <th>{t('上次同步')}</th>
              <th className='ta-center'>{t('启用')}</th>
              <th className='ta-right'>{t('操作')}</th>
            </tr>
          </thead>
          <tbody>
            {items.length ? items.map(item => {
              const sync = kernelStatus?.providerSync?.[item.id]
              return (
                <tr key={item.id} className={item.enabled ? '' : 'off'}>
                  <td><b>{item.name || t('未命名')}</b></td>
                  <td><span className='font-mono text-[12px] text-text-2'
                    title={item.url}>{shortUrl(item.url)}</span></td>
                  <td className='ta-center'>
                    {item.nodeCount > 0 ? (
                      <Badge variant='secondary'>{t('{n} 个', { n: item.nodeCount })}</Badge>
                    ) : (
                      <span className='text-subtle'>{t('—')}</span>
                    )}
                    {sync?.at ? (
                      <div className='text-[11px] text-subtle'>{formatTime(sync.at)}</div>
                    ) : null}
                  </td>
                  <td>
                    <span className='text-[12px] text-text-2'>
                      {item.enabled ? t('正常') : t('已禁用')}
                    </span>
                  </td>
                  <td className='ta-center'>
                    <Switch checked={item.enabled} disabled={snapshot.pending}
                      aria-label={t('启用订阅「{name}」', { name: item.name || t('未命名') })}
                      onCheckedChange={next => void toggleSubscription(item, next)} />
                  </td>
                  <td>
                    <div className='row-actions'>
                      <Button size='sm' variant='ghost' disabled={snapshot.pending || snapshot.refreshing !== null}
                        onClick={() => void refreshSubscription(item)}>
                        {snapshot.refreshing === item.id ? t('更新中…') : t('立即更新')}
                      </Button>
                      <Button size='sm' variant='ghost' disabled={snapshot.pending}
                        onClick={() => setModal({ item })}>{t('编辑')}</Button>
                      <Button size='sm' variant='destructive' disabled={snapshot.pending}
                        onClick={() => void removeSubscription(item)}>{t('删除')}</Button>
                    </div>
                  </td>
                </tr>
              )
            }) : (
              <tr>
                <td colSpan={6} className='empty'>
                  {ready
                    ? t('还没有订阅。点右上角「＋ 新增订阅」粘贴订阅链接，或从图片识别二维码导入。')
                    : t('加载中…')}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      <div className='panel-foot'>
        <span>
          {t('订阅里的节点会被内核起成本地监听器，镜像到下方代理池（账号按 id 引用，与本页其它出口同一用法）。')}
        </span>
        {snapshot.error ? (
          <span className='text-destructive'>{t('读取失败：{reason}', { reason: snapshot.error })}</span>
        ) : null}
      </div>

      {modal ? (
        <SubModal item={modal.item} onClose={() => setModal(null)} />
      ) : null}
    </section>
  )
}

/* ─── 内核状态行 ─────────────────────────────── */

function KernelRow({
  kernel, downloading, percent, pending,
}: {
  kernel: KernelStatus | null
  downloading: boolean
  percent: number | null
  pending: boolean
}) {
  return (
    <div className='mx-3 mb-3 flex flex-col gap-2 rounded-md border border-border bg-surface-2 p-3'>
      <div className='flex flex-wrap items-center gap-2'>
        <Badge variant={kernel?.running ? 'default' : 'secondary'}>
          {kernel?.running
            ? t('内核运行中')
            : kernel?.installed
              ? t('内核已停止')
              : t('内核未安装')}
        </Badge>
        <span className='text-[12.5px] text-text-2'>
          {t('内核 {version}')}　·　{kernel?.running
            ? t('控制端口 {port} · {n} 个节点监听器', { port: kernel?.controllerPort ?? '—', n: kernel?.listenerCount ?? 0 })
            : kernel?.installed
              ? t('下载完成后可启动')
              : t('按需下载（约 20-40MB），只在启用订阅功能时需要')}
        </span>
        <div className='ml-auto'>
          {!kernel?.installed ? (
            <Button size='sm' variant='outline' disabled={pending || downloading}
              onClick={() => void downloadKernel()}>
              {downloading ? t('下载中…') : t('下载内核')}
            </Button>
          ) : (
            <Button size='sm' variant='outline' disabled={pending || downloading}
              onClick={() => void restartKernel()}>
              {kernel?.running ? t('重启内核') : t('启动内核')}
            </Button>
          )}
        </div>
      </div>
      {downloading ? (
        <div className='flex items-center gap-3'>
          <Progress className='min-w-[200px] flex-1' value={percent ?? null} />
          <span className='text-[11px] text-subtle'>
            {percent !== null
              ? t('{percent}%（{received} / {total} MB）', {
                  percent,
                  received: mb(kernel!.download.received),
                  total: mb(kernel!.download.total),
                })
              : t('下载中…')}
          </span>
        </div>
      ) : null}
      {kernel?.lastError ? (
        <span className='text-[12px] text-destructive'>{kernel.lastError}</span>
      ) : null}
    </div>
  )
}

/** 字节 → MB 文案（下载进度用；total 未知时是 —） */
function mb(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return '—'
  return (value / 1000 / 1000).toFixed(1)
}

/** 链接截断（表格里只显示主机段；title 里有完整链接） */
function shortUrl(url: string): string {
  try {
    const parsed = new URL(url)
    return parsed.host + (parsed.pathname !== '/' ? '/…' : '')
  } catch {
    return url.slice(0, 60)
  }
}

/* ─── 新增 / 编辑弹窗 ───────────────────────── */

function SubModal({
  item,
  onClose,
}: {
  item: SubscriptionItem | null
  onClose: () => void
}) {
  const editing = item !== null
  const [name, setName] = React.useState(item?.name || '')
  const [url, setUrl] = React.useState(item?.url || '')
  const [enabled, setEnabled] = React.useState(item ? item.enabled : true)
  const [saving, setSaving] = React.useState(false)
  const [decoding, setDecoding] = React.useState(false)
  const [status, setStatus] = React.useState<React.ReactNode>(null)
  const fileInput = React.useRef<HTMLInputElement | null>(null)

  async function save(): Promise<void> {
    if (saving) return
    const nameText = name.trim()
    const urlText = url.trim()
    if (!nameText) {
      setStatus(<span className='text-destructive'>❌ {t('请填写订阅名称')}</span>)
      return
    }
    if (!(urlText.startsWith('http://') || urlText.startsWith('https://'))) {
      setStatus(<span className='text-destructive'>❌ {t('订阅链接必须是 http:// 或 https:// 开头')}</span>)
      return
    }
    setSaving(true)
    try {
      if (editing) {
        await saveSubscription(item!, { name: nameText, url: urlText, enabled })
      } else {
        await addSubscription(nameText, urlText)
      }
      onClose()
    } catch (error) {
      setStatus(<span className='text-destructive'>❌ {errorMessage(error)}</span>)
      setSaving(false)
    }
  }

  /** 从图片文件解二维码 → 回填链接（服务端解码，见 proxies-sub-state） */
  async function decodeFrom(file: File): Promise<void> {
    if (decoding) return
    setDecoding(true)
    setStatus(t('正在识别二维码…'))
    try {
      const decoded = await decodeQrImage(file)
      setUrl(decoded)
      setStatus(<span className='text-success'>✅ {t('已识别订阅链接')}</span>)
    } catch (error) {
      setStatus(<span className='text-destructive'>❌ {errorMessage(error)}</span>)
    } finally {
      setDecoding(false)
    }
  }

  /** 剪贴板里是图片（截图的二维码）时直接识别：粘贴事件里取文件 */
  function onPaste(event: React.ClipboardEvent): void {
    const files: File[] = Array.from(event.clipboardData?.files || [])
    const image = files.find(file => file.type.startsWith('image/'))
    if (image) {
      event.preventDefault()
      void decodeFrom(image)
    }
  }

  return (
    <Dialog open onOpenChange={open => { if (!open && !saving) onClose() }}>
      <DialogContent className='max-w-[560px]' onPaste={onPaste}>
        <DialogHeader>
          <DialogTitle>{editing ? t('编辑订阅') : t('新增订阅')}</DialogTitle>
        </DialogHeader>
        <DialogBody className='flex flex-col gap-3.5'>
          <div className='flex flex-col gap-1.5'>
            <Label htmlFor='sub-name'>{t('名称')}</Label>
            <Input id='sub-name' maxLength={60} autoComplete='off' spellCheck={false}
              placeholder={t('例如：我的机场')} value={name}
              onChange={event => setName(event.currentTarget.value)} />
          </div>

          <div className='flex flex-col gap-1.5'>
            <Label htmlFor='sub-url'>{t('订阅链接')}</Label>
            <Input id='sub-url' autoComplete='off' spellCheck={false}
              placeholder='https://…' value={url}
              onChange={event => setUrl(event.currentTarget.value)} />
            <div className='flex items-center gap-2'>
              <Button size='sm' variant='outline' disabled={decoding}
                onClick={() => fileInput.current?.click()}>
                {decoding ? t('识别中…') : t('从图片识别二维码')}
              </Button>
              <span className='text-[11px] text-subtle'>{t('也可以把二维码截图直接粘贴到弹窗里（Ctrl+V）')}</span>
              <input ref={fileInput} type='file' accept='image/*' className='hidden'
                onChange={event => {
                  const file = event.currentTarget.files?.[0]
                  if (file) void decodeFrom(file)
                  event.currentTarget.value = ''
                }} />
            </div>
          </div>

          <div className='flex flex-row items-center gap-2.5'>
            <Switch checked={enabled} onCheckedChange={setEnabled} aria-label={t('启用这条订阅')} />
            <span className='text-[12.5px]'>{t('启用（禁用的订阅不参与内核同步，它的节点会从代理池移除）')}</span>
          </div>

          <p className='text-xs leading-[1.65] text-subtle'>
            {t('订阅内容由内核定时刷新（每 24 小时）；节点数与同步状态在保存后由轮询刷新，稍等几秒。')}
          </p>

          <div className='min-h-[18px] text-xs text-subtle'>{status}</div>
        </DialogBody>
        <DialogFooter>
          <div className='mr-auto' />
          <Button variant='outline' disabled={saving} onClick={onClose}>{t('取消')}</Button>
          <Button variant='default' disabled={saving || decoding} onClick={() => void save()}>
            {saving ? t('保存中…') : t('保存')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
