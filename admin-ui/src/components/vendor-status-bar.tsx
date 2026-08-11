import { useState } from 'react'
import { toast } from 'sonner'
import {
  Wallet, PackageOpen, Send, Ticket, Upload, ShoppingCart,
  Boxes, Zap, Hand, History, CalendarClock,
} from 'lucide-react'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from '@/components/ui/select'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { Switch } from '@/components/ui/switch'
import {
  useVendorStatus, useRedeemVendorCode, useTestVendorWebhook,
  useSetVendorWebhookUrl, usePurchaseAdHoc, useSetVendorMode, useSetVendorPerChannel,
  useSetStockPollRespectGate, useSetVendorAutoReserve,
} from '@/hooks/use-vendor'
import { isRateLimited, vendorErrorMessage } from '@/api/vendor'
// 本地新增：Region 展示文案，单独成行避免与上游 import 块相撞。
import { formatVendorRegion } from '@/lib/vendor-region'
import type { VendorStatus, VendorZoneStock } from '@/types/api'

/** 四格状态卡片中的一格 */
function StatCard({
  icon, label, value, hint, sub, tone = 'normal',
}: {
  icon: React.ReactNode
  label: string
  value: React.ReactNode
  hint?: React.ReactNode
  /** hint 下方的补充行，用于放运行时长这类次要信息 */
  sub?: React.ReactNode
  tone?: 'normal' | 'warn' | 'good'
}) {
  const toneClass =
    tone === 'warn'
      ? 'text-amber-600 dark:text-amber-500'
      : tone === 'good'
        ? 'text-emerald-600 dark:text-emerald-500'
        : ''
  return (
    <Card>
      <CardContent className="p-4">
        <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
          {icon}
          <span>{label}</span>
        </div>
        <div className={`mt-1.5 text-xl font-semibold tabular-nums ${toneClass}`}>{value}</div>
        {hint && <div className="mt-1 text-xs text-muted-foreground">{hint}</div>}
        {sub && <div className="mt-0.5 text-xs text-muted-foreground/80">{sub}</div>}
      </CardContent>
    </Card>
  )
}

/**
 * 解析卖家返回的时间串。形如 `YYYY-MM-DD HH:mm:ss`（无时区标记），Safari 对该
 * 形式直接 new Date 会得到 Invalid Date，故先补 'T' 再解析。解析失败返回 null。
 */
function parseVendorTime(raw?: string | null): Date | null {
  const s = raw?.trim()
  if (!s) return null
  const d = new Date(s.includes('T') ? s : s.replace(' ', 'T'))
  return Number.isNaN(d.getTime()) ? null : d
}

/** 秒数转「1小时57分钟」。不足 1 分钟显示秒；有天数时省略分钟避免噪声。 */
function formatDuration(totalSeconds: number): string {
  const s = Math.max(0, Math.floor(totalSeconds))
  if (s < 60) return `${s}秒`
  const days = Math.floor(s / 86400)
  const hours = Math.floor((s % 86400) / 3600)
  const minutes = Math.floor((s % 3600) / 60)
  const parts: string[] = []
  if (days) parts.push(`${days}天`)
  if (hours) parts.push(`${hours}小时`)
  if (minutes && !days) parts.push(`${minutes}分钟`)
  return parts.join('') || '0分钟'
}

/**
 * 卖家运行时长。优先用 uptime_seconds —— 它是卖家自己算的时长，不受两侧时钟
 * 偏差和时区影响；缺失时才退回按 started_at 与本地时间相减。
 *
 * started_at 无时区标记，原样展示卖家给的字符串，不做本地化转换，免得在时区
 * 不同的机器上显示成另一个时刻。
 */
function describeUptime(system?: VendorStatus['system']): string | null {
  if (!system) return null
  const startedRaw = system.started_at?.trim()
  const seconds =
    typeof system.uptime_seconds === 'number' && system.uptime_seconds >= 0
      ? system.uptime_seconds
      : (() => {
          const d = parseVendorTime(startedRaw)
          return d ? (Date.now() - d.getTime()) / 1000 : null
        })()
  if (seconds == null) return startedRaw ? `启动于 ${startedRaw}` : null
  const ran = `已运行 ${formatDuration(seconds)}`
  return startedRaw ? `${ran} — 启动于 ${startedRaw}` : ran
}

/**
 * 车次的发车时间与存活时长，如「56 分钟 · 3小时前发车」。
 *
 * 存活时长优先用卖家给的 aliveText —— 与站点显示口径一致，免得我们自己换算出
 * 一个跟卖家不一样的数字。发车时间是 Unix 秒，转成「多久前」比绝对时刻更好读。
 *
 * 注意 aliveSecs 在活车上是「已存活多久」、会随时间增长，不是「还能活多久」，
 * 所以这里只叙述事实（存活 N），不写成「剩余 N」。
 */
function describeZoneBatch(zone: VendorZoneStock): string | null {
  const parts: string[] = []
  const alive = zone.aliveText?.trim() || (zone.aliveSecs != null ? formatDuration(zone.aliveSecs) : null)
  if (alive) parts.push(`存活 ${alive}`)
  if (zone.departedAt != null && zone.departedAt > 0) {
    parts.push(`${formatDuration(Date.now() / 1000 - zone.departedAt)}前发车`)
  }
  return parts.length ? parts.join(' · ') : null
}

/** 距今多久。用于开号记录里标出「刚开的」还是「很久没开了」。 */
function describeAgo(raw?: string | null): string | null {
  const d = parseVendorTime(raw)
  if (!d) return null
  return `${formatDuration((Date.now() - d.getTime()) / 1000)}前`
}

/**
 * 开号记录弹层。卖家近期开号批次 + 平均间隔，用来判断下一批新 Key 大概什么时候到。
 * 数据随 status 一起拉回来，弹层只负责展示，不额外打接口。
 */
function GenLogsDialog({
  status,
  open,
  onOpenChange,
}: {
  status?: VendorStatus
  open: boolean
  onOpenChange: (v: boolean) => void
}) {
  const logs = status?.genLogs
  const items = logs?.items ?? []
  const avg = logs?.avg_interval_min

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>卖家开号记录</DialogTitle>
          <DialogDescription>
            卖家侧最近几批开号情况
            {avg != null ? `，平均间隔 ${formatDuration(avg * 60)}` : ''}
            。时间为卖家本地时刻，原样展示。
          </DialogDescription>
        </DialogHeader>

        {status?.genLogsError ? (
          <div className="rounded-md border border-amber-500/40 bg-amber-500/5 p-3 text-xs text-amber-600 dark:text-amber-500">
            {status.genLogsError}
          </div>
        ) : items.length === 0 ? (
          <div className="p-4 text-center text-sm text-muted-foreground">暂无开号记录</div>
        ) : (
          <div className="max-h-[60vh] overflow-y-auto">
            <table className="w-full text-sm">
              <thead className="text-xs text-muted-foreground">
                <tr className="border-b">
                  <th className="py-2 pr-3 text-left font-medium">开号时间</th>
                  <th className="px-3 py-2 text-right font-medium">数量</th>
                  <th className="py-2 pl-3 text-left font-medium">状态</th>
                </tr>
              </thead>
              <tbody>
                {items.map((it, i) => {
                  const ago = describeAgo(it.created_at)
                  return (
                    <tr key={`${it.created_at ?? i}`} className="border-b last:border-0">
                      <td className="py-2 pr-3 text-xs">
                        <div className="tabular-nums">{it.created_at ?? '—'}</div>
                        {ago && <div className="text-muted-foreground">{ago}</div>}
                      </td>
                      <td className="px-3 py-2 text-right tabular-nums">{it.count ?? '—'}</td>
                      <td className="py-2 pl-3 text-xs">
                        {it.status === 'done' ? (
                          <span className="text-emerald-600 dark:text-emerald-500">已完成</span>
                        ) : (
                          <span className="text-muted-foreground">{it.status ?? '—'}</span>
                        )}
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            关闭
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

export function VendorStatusBar({ vendorId }: { vendorId?: string }) {
  const { data: status, isLoading } = useVendorStatus(vendorId)
  const redeem = useRedeemVendorCode(vendorId)
  const testWebhook = useTestVendorWebhook(vendorId)
  const setWebhookUrl = useSetVendorWebhookUrl(vendorId)
  const purchaseAdHoc = usePurchaseAdHoc(vendorId)
  const setMode = useSetVendorMode(vendorId)
  const setAutoReserve = useSetVendorAutoReserve(vendorId)
  const setPerChannel = useSetVendorPerChannel(vendorId)
  const setStockPollRespectGate = useSetStockPollRespectGate(vendorId)
  const confirm = useConfirm()

  const [redeemOpen, setRedeemOpen] = useState(false)
  const [code, setCode] = useState('')
  const [webhookOpen, setWebhookOpen] = useState(false)
  const [webhookUrl, setWebhookInput] = useState('')
  const [directOpen, setDirectOpen] = useState(false)
  const [directCount, setDirectCount] = useState('1')
  const [directZone, setDirectZone] = useState('')
  const [genLogsOpen, setGenLogsOpen] = useState(false)

  const profile = status?.profile
  const system = status?.system
  /**
   * 能力集缺失时按「全不支持」处理：状态还在加载时先不渲染这些按钮，
   * 免得先亮出来再消失，也免得点了对不支持的接口发请求拿 404。
   */
  const caps = status?.capabilities

  const handleRedeem = async () => {
    const trimmed = code.trim()
    if (!trimmed) {
      toast.error('请填写兑换码')
      return
    }
    try {
      const r = await redeem.mutateAsync(trimmed)
      if (r.replayed) {
        toast.info('这张码此前已兑换过，余额未变动', {
          description: `首次兑换于 ${r.redeemed_at ?? '未知时间'}，额度 ${r.quota ?? '-'}`,
        })
      } else {
        toast.success(`充值成功，+${r.quota ?? 0}`, {
          description: `余额 ${r.previous_quota ?? '-'} → ${r.balance ?? '-'}`,
        })
      }
      setRedeemOpen(false)
      setCode('')
    } catch (e) {
      toast.error(vendorErrorMessage(e))
    }
  }

  const handleTest = async () => {
    try {
      await testWebhook.mutateAsync()
      toast.success('已请求卖家推送测试消息', {
        description: '稍等几秒刷新事件列表，应能看到一条新记录',
      })
    } catch (e) {
      // 卖家对测试推送有频率限制，429 时给出可操作的提示而不是原始报错
      const rateLimited = isRateLimited(e)
      toast.error(vendorErrorMessage(e, '测试推送失败'), {
        description: rateLimited ? '卖家侧对测试推送限流，稍后再试即可' : undefined,
      })
    }
  }

  const handleSetWebhook = async () => {
    const url = webhookUrl.trim()
    if (!/^https?:\/\/.+/.test(url)) {
      toast.error('地址需以 http:// 或 https:// 开头')
      return
    }
    try {
      await setWebhookUrl.mutateAsync(url)
      toast.success('已写入卖家侧 webhook 地址')
      setWebhookOpen(false)
    } catch (e) {
      toast.error(vendorErrorMessage(e))
    }
  }

  /**
   * 切换提取模式。开自动要二次确认 —— 之后收到 `new_keys_available` 就会
   * 直接扣费提取，且提取数量与订单号永久绑定，没有人工复核的机会。
   * 关自动不确认：从花钱变不花钱，没有风险。
   */
  const handleToggleMode = async (next: boolean) => {
    if (next) {
      const ok = await confirm({
        title: '开启自动提取？',
        description:
          '开启后，收到「全部失效」事件时会核对本地凭据，确认名下卖家 Key 已全部失效；' +
          `之后收到「新 Key 就绪」才自动下单，当前时段每次最多 ${
            status?.autoPurchaseMaxCount ?? 1
          } 个（还会受事件声明数量与卖家上限限制）。` +
          (status?.autoPurchaseWindow
            ? `上限随时段变化，当前命中 ${status.autoPurchaseWindow}，其余时段 ${
                status.autoPurchaseBaseMaxCount ?? 1
              } 个。`
            : '') +
          '数量一旦提交就与该订单号永久绑定，无法改数量重试。',
        confirmText: '开启自动提取',
        destructive: true,
      })
      if (!ok) return
    }
    try {
      const r = await setMode.mutateAsync(next)
      if (!r.persisted) {
        toast.warning(next ? '已切到自动提取（仅本次运行）' : '已切回手动提取（仅本次运行）', {
          description: `配置未能写回文件，重启后会回退。${r.warning ?? ''}`,
        })
        return
      }
      toast.success(next ? '已切到自动提取' : '已切回手动提取')
    } catch (e) {
      toast.error(vendorErrorMessage(e, '切换提取模式失败'))
    }
  }

  const handleToggleAutoReserve = async (next: boolean) => {
    if (next) {
      const bypassesGate = status?.stockPollRespectGlobalGate === false
      const pollInterval = status?.stockPollIntervalSecs ?? 0
      const ok = await confirm({
        title: '开启自动预定？',
        description:
          '开启后，kiro.red 没有待发货预定单时，会选择名称以「Kiro拼车」开头、' +
          '允许预定且价格最低的商品预定 1 件，预定成功会立即扣积分。' +
          '它不会等待当前凭证失效，也不受全局池量限制；始终最多维持一张待发货单。' +
          (bypassesGate
            ? '当前轮询已越过全局总闸，因此总闸关闭时仍会创建新预定。'
            : '当前轮询遵循全局总闸，总闸关闭时不会创建新预定。') +
          (pollInterval > 0
            ? `订单每 ${pollInterval} 秒检查一次。`
            : '当前轮询间隔为 0，开启后也不会执行，需先在配置中启用轮询。') +
          '关闭开关只停止创建新预定，已经付款的订单发货后仍会自动取凭证并入库。',
        confirmText: '开启自动预定',
        destructive: true,
      })
      if (!ok) return
    }
    try {
      const r = await setAutoReserve.mutateAsync(next)
      const what = next ? '自动预定已开启' : '自动预定已关闭'
      if (!r.persisted) {
        toast.warning(`${what}（仅本次运行）`, {
          description: `配置未能写回文件，重启后会回退。${r.warning ?? ''}`,
        })
        return
      }
      toast.success(what, {
        description: next
          ? '没有待发货订单时会自动预定最便宜的 Kiro 拼车商品'
          : '已付款订单仍会继续轮询取货',
      })
    } catch (e) {
      toast.error(vendorErrorMessage(e, '切换自动预定失败'))
    }
  }

  /**
   * 切逐渠道补货。开启要确认 —— 它会让本家独立维持库存，账号消耗上升。
   *
   * 关闭不确认：从"各自保底"退回"按总量控"，只会少买不会多买。
   */
  const handleTogglePerChannel = async (next: boolean) => {
    if (next) {
      const ok = await confirm({
        title: '开启逐渠道补货？',
        description:
          '开启后本家只看自己有没有存活 Key，没有就补货，不再受全局提取限制约束。' +
          '本家会独立维持库存，账号消耗会上升；买来的号若很快被封，本家又回到无存活，' +
          '会再补一张。' +
          '注意：本家常驻的 Key 仍会计入全局池量，可能把其他未开本项的家挤到无法补货。',
        confirmText: '开启',
        destructive: true,
      })
      if (!ok) return
    }
    try {
      const r = await setPerChannel.mutateAsync(next)
      const what = r.perChannel ? '本家已改为独立补货' : '本家已改回按全局总量'
      if (!r.persisted) {
        toast.warning(`${what}（仅本次运行）`, {
          description: `配置未能写回文件，重启后会回退。${r.warning ?? ''}`,
        })
        return
      }
      toast.success(what)
    } catch (e) {
      toast.error(vendorErrorMessage(e, '切换逐渠道补货失败'))
    }
  }

  /**
   * 切换轮询是否遵循全局总闸。
   *
   * **关掉要二次确认**：它让总闸对本家这条轮询链路整体失效，包括下单 ——
   * 而总闸是能一键停掉全部自动扣费的急停，且会被健康联动自动翻转。
   * 开回来不确认：从「越过急停」退回「受急停管」，只会少买不会多买。
   */
  const handleToggleStockPollRespectGate = async (next: boolean) => {
    if (!next) {
      const ok = await confirm({
        title: '让轮询越过总闸？',
        description:
          '关闭后，即便全局总闸处于关闭状态，本家仍会继续轮询、发现新车并自动下单。' +
          '也就是说总闸对本家不再是急停 —— 而总闸会被健康联动自动翻转。' +
          '要停掉本家的自动扣费，得同时关本家的「自动提取」与「自动预定」开关，' +
          '或把轮询间隔改为 0。现货提取仍受池闸与失效授权限制；自动预定不看池量，' +
          '但最多维持一张待发货单。',
        confirmText: '确认越过',
        destructive: true,
      })
      if (!ok) return
    }
    try {
      const r = await setStockPollRespectGate.mutateAsync(next)
      const what = next
        ? '轮询已改为遵循总闸（总闸关闭时停止轮询）'
        : '轮询已越过总闸（总闸关闭时本家仍会自动下单）'
      if (!r.persisted) {
        toast.warning(`${what}（仅本次运行）`, {
          description: `配置未能写回文件，重启后会回退。${r.warning ?? ''}`,
        })
        return
      }
      toast.success(what)
    } catch (e) {
      toast.error(vendorErrorMessage(e, '切换轮询总闸遵循失败'))
    }
  }

  /**
   * 直接提取：不依赖 webhook 事件，服务端自行生成订单号。
   * 会真实扣费，故强制二次确认并把数量、预计扣费、余额变化列清楚。
   */
  const handleDirectPurchase = async () => {
    const n = Number(directCount)
    if (!Number.isInteger(n) || n <= 0) {
      toast.error('提取数量需为正整数')
      return
    }
    const balance = profile?.balance ?? profile?.quota
    const ok = await confirm({
      title: `确认直接提取 ${n} 个 Key？`,
      description:
        `该操作会立刻向卖家下单并按实际出 Key 数扣费${
          balance != null ? `，当前余额 ${balance}` : ''
        }。订单号由服务端生成，不与任何 webhook 事件关联，提交后无法撤销。`,
      confirmText: `确认提取 ${n} 个`,
      destructive: true,
    })
    if (!ok) return
    try {
      const r = await purchaseAdHoc.mutateAsync({
        count: n,
        zone: directZone || undefined,
      })
      toast.success(`提取完成：出 ${r.purchased} 个，入库 ${r.imported} 个`, {
        description: [
          r.zone ? `区域 ${r.zone}` : null,
          r.duplicated ? `重复 ${r.duplicated} 个` : null,
          r.failed ? `失败 ${r.failed} 个` : null,
          r.remaining != null ? `剩余余额 ${r.remaining}` : null,
        ]
          .filter(Boolean)
          .join('，'),
      })
      setDirectOpen(false)
    } catch (e) {
      toast.error(vendorErrorMessage(e))
    }
  }

  if (!isLoading && status && !status.configured) {
    return (
      <Card className="border-amber-500/40 bg-amber-500/5">
        <CardContent className="p-4 text-sm">
          <div className="font-medium text-amber-600 dark:text-amber-500">未配置卖家对接</div>
          <div className="mt-1 text-muted-foreground">
            在 config.json 补上 <code className="text-xs">vendor.baseUrl</code> 与{' '}
            <code className="text-xs">vendor.apiKey</code> 后重启即可启用；再加{' '}
            <code className="text-xs">vendor.webhookPathToken</code> 才会接收入站推送。
          </div>
        </CardContent>
      </Card>
    )
  }

  return (
    <>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div
          className={`flex items-center gap-2.5 rounded-md border px-3 py-1.5 ${
            status?.autoPurchase
              ? 'border-amber-500/40 bg-amber-500/5'
              : 'border-border bg-muted/30'
          }`}
        >
          {status?.autoPurchase ? (
            <Zap className="h-3.5 w-3.5 text-amber-600 dark:text-amber-500" />
          ) : (
            <Hand className="h-3.5 w-3.5 text-muted-foreground" />
          )}
          <div className="text-xs">
            <span className="font-medium">
              {status?.autoPurchase ? '自动提取' : '手动提取'}
            </span>
            <span className="ml-1.5 text-muted-foreground">
              {status?.autoPurchase
                ? `确认旧 Key 全部失效后，新 Key 就绪时最多自动提 ${
                    status.autoPurchaseMaxCount ?? 1
                  } 个${
                    // 命中时段时标出来源，否则面板上的数看不出为何与配置不同
                    status.autoPurchaseWindow
                      ? `（当前时段 ${status.autoPurchaseWindow}，其余时段 ${
                          status.autoPurchaseBaseMaxCount ?? 1
                        } 个）`
                      : ''
                  }`
                : '所有提取需在下方事件列表手动触发'}
            </span>
          </div>
          <Switch
            checked={status?.autoPurchase ?? false}
            onCheckedChange={handleToggleMode}
            disabled={setMode.isPending || !status}
            aria-label="切换自动 / 手动提取模式"
          />
        </div>

        {status?.flavor === 'kirored' && (
          <div
            className={`flex items-center gap-2.5 rounded-md border px-3 py-1.5 ${
              status.autoReserve
                ? 'border-amber-500/40 bg-amber-500/5'
                : 'border-border bg-muted/30'
            }`}
          >
            <CalendarClock
              className={`h-3.5 w-3.5 ${
                status.autoReserve
                  ? 'text-amber-600 dark:text-amber-500'
                  : 'text-muted-foreground'
              }`}
            />
            <div className="text-xs">
              <span className="font-medium">
                {status.autoReserve ? '自动预定' : '预定已关闭'}
              </span>
              <span className="ml-1.5 text-muted-foreground">
                {status.autoReserve
                  ? '无待发货单时预定最便宜的 Kiro 拼车商品'
                  : '不会创建新预定；已付款订单仍会取货'}
              </span>
            </div>
            <Switch
              checked={status.autoReserve ?? false}
              onCheckedChange={handleToggleAutoReserve}
              disabled={setAutoReserve.isPending}
              aria-label="切换 kiro.red 自动预定"
            />
          </div>
        )}

        {/* 逐渠道补货：只在自动模式下有意义，手动模式下整块隐藏 ——
            手动提取不过任何闸门，摆在那里只会让人以为它有作用。 */}
        {status?.autoPurchase && (
          <div className="flex items-center gap-2.5 rounded-md border border-border bg-muted/30 px-3 py-1.5">
            <div className="text-xs">
              <span className="font-medium">
                {status.autoPurchasePerChannel ? '本家独立补货' : '按全局总量'}
              </span>
              <span className="ml-1.5 text-muted-foreground">
                {status.autoPurchasePerChannel
                  ? '只看本家有没有存活 Key，不受别家影响'
                  : '按「全局提取限制」判池子总量，含别家的 Key'}
              </span>
            </div>
            <Switch
              checked={status.autoPurchasePerChannel ?? false}
              onCheckedChange={handleTogglePerChannel}
              disabled={setPerChannel.isPending}
              aria-label="切换逐渠道补货"
            />
          </div>
        )}

        {/* 库存轮询：只在开了轮询（stockPollIntervalSecs > 0）的家显示。
            关掉「遵循总闸」后总闸对本家整条轮询链路失效（含下单），故用告警色标出。 */}
        {status?.stockPollIntervalSecs && status.stockPollIntervalSecs > 0 && (
          <div
            className={`flex items-center gap-2.5 rounded-md border px-3 py-1.5 ${
              status.stockPollRespectGlobalGate
                ? 'border-border bg-muted/30'
                : 'border-amber-500/40 bg-amber-500/5'
            }`}
          >
            <div className="text-xs">
              <span className="font-medium">
                {status.stockPollRespectGlobalGate
                  ? '轮询遵循总闸'
                  : '轮询已越过总闸'}
              </span>
              <span className="ml-1.5 text-muted-foreground">
                {status.stockPollRespectGlobalGate
                  ? '总闸关闭时停止轮询（连库存都不查）'
                  : '总闸关闭时本家仍会发现新车并自动下单，总闸对本家不再是急停'}
              </span>
            </div>
            <Switch
              checked={status.stockPollRespectGlobalGate ?? true}
              onCheckedChange={handleToggleStockPollRespectGate}
              disabled={setStockPollRespectGate.isPending}
              aria-label="切换轮询是否遵循全局总闸"
            />
          </div>
        )}

        <div className="flex flex-wrap items-center gap-2">
        {/* 测试推送与写入地址都走卖家的 webhook 管理 API，没这能力的家一律隐藏 */}
        {caps?.webhookManage && (
          <>
            <Button
              variant="outline"
              size="sm"
              onClick={handleTest}
              disabled={testWebhook.isPending}
            >
              <Send className="mr-1.5 h-3.5 w-3.5" />
              测试推送
            </Button>
            <Button
              variant="outline"
              size="sm"
              onClick={() => {
                setWebhookInput(profile?.webhookUrl ?? '')
                setWebhookOpen(true)
              }}
            >
              <Upload className="mr-1.5 h-3.5 w-3.5" />
              写入卖家
            </Button>
          </>
        )}
        {caps?.redeem && (
          <Button variant="outline" size="sm" onClick={() => setRedeemOpen(true)}>
            <Ticket className="mr-1.5 h-3.5 w-3.5" />
            兑换充值
          </Button>
        )}
        {caps?.genLogs && (
          <Button variant="outline" size="sm" onClick={() => setGenLogsOpen(true)}>
            <History className="mr-1.5 h-3.5 w-3.5" />
            开号记录
            {status?.genLogs?.avg_interval_min != null && (
              <span className="ml-1.5 text-xs text-muted-foreground">
                均 {formatDuration(status.genLogs.avg_interval_min * 60)}
              </span>
            )}
          </Button>
        )}
        <Button
          size="sm"
          onClick={() => {
            const zones = status?.stock?.zones?.filter((z) => z.enabled && z.available > 0) ?? []
            const picked = zones.length > 0
              ? zones.reduce((best, z) =>
                  (z.unitPrice ?? Infinity) < (best.unitPrice ?? Infinity) ||
                  ((z.unitPrice ?? Infinity) === (best.unitPrice ?? Infinity) && z.available > best.available)
                    ? z
                    : best
                ).zone
              : ''
            setDirectCount(String(status?.stock?.available && status.stock.available > 0 ? 1 : 1))
            setDirectZone(picked)
            setDirectOpen(true)
          }}
        >
          <ShoppingCart className="mr-1.5 h-3.5 w-3.5" />
          直接提取
        </Button>
        </div>
      </div>

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard
          icon={<Wallet className="h-3.5 w-3.5" />}
          label="卖家余额"
          value={status?.profileError ? '—' : (profile?.balance ?? profile?.quota ?? '—')}
          hint={
            status?.profileError
              ? status.profileError
              : profile
                ? `总额度 ${profile.quota ?? '-'} / 已用 ${profile.usedQuota ?? '-'}`
                : undefined
          }
          tone={status?.profileError ? 'warn' : 'normal'}
        />
        <StatCard
          icon={<PackageOpen className="h-3.5 w-3.5" />}
          label="本轮可提取"
          value={
            status?.stockError
              ? '—'
              : status?.stock?.zones && status.stock.zones.length > 0
                ? status.stock.zones
                    .filter((z) => z.enabled)
                    .map((z) => `${z.label ?? z.zone} ${z.available}`)
                    .join(' / ')
                : status?.stock?.available ?? status?.stockMax ?? '—'
          }
          hint={status?.stockError ?? '已综合余额、库存与每母号上限'}
          sub={
            status?.stockError
              ? undefined
              : status?.stock?.zones
                  ?.filter((z) => z.enabled && describeZoneBatch(z))
                  .map((z) => `${z.label ?? z.zone}：${describeZoneBatch(z)}`)
                  .join('　') || undefined
          }
          tone={status?.stockError ? 'warn' : 'normal'}
        />
        <StatCard
          icon={<ShoppingCart className="h-3.5 w-3.5" />}
          label="当前单价"
          value={
            status?.stockError
              ? '—'
              : status?.stock?.zones && status.stock.zones.length > 0
                ? status.stock.zones
                    .filter((z) => z.enabled && z.available > 0 && z.unitPrice != null)
                    .map((z) => `${z.label ?? z.zone} ${z.unitPrice}`)
                    .join(' / ') || '—'
                : status?.stock?.priceMin != null && status?.stock?.priceMax != null
                  ? status.stock.priceMin === status.stock.priceMax
                    ? status.stock.priceMin
                    : `${status.stock.priceMin}~${status.stock.priceMax}`
                  : '—'
          }
          hint={
            status?.capabilities?.tieredPricing && !status?.stockError
              ? '阶梯定价，价格随提取数量变化'
              : status?.capabilities?.zonedPurchase && !status?.stockError
                ? '各区单价独立'
                : undefined
          }
          tone="normal"
        />
        {/* 卖家系统状态是 legacy 独有；没这能力时后端压根不发请求，卡里只会是空值 */}
        {caps?.systemStatus && (
        <StatCard
          icon={<Boxes className="h-3.5 w-3.5" />}
          label="卖家存货 Key"
          value={status?.systemError ? '—' : (system?.keys_stock ?? '—')}
          hint={
            status?.systemError
              ? status.systemError
              : system
                ? `存活 ${system.keys_active ?? '-'} / 失效 ${system.keys_dead ?? '-'}${
                    system.keys_total != null ? ` / 累计 ${system.keys_total}` : ''
                  }${system.generating ? ' · 正在生成' : ''}`
                : undefined
          }
          sub={status?.systemError ? undefined : describeUptime(system)}
          tone={
            status?.systemError
              ? 'warn'
              : system?.keys_stock === 0
                ? 'warn'
                : 'normal'
          }
        />
        )}
      </div>

      <GenLogsDialog status={status} open={genLogsOpen} onOpenChange={setGenLogsOpen} />

      {/* 兑换充值 */}
      <Dialog open={redeemOpen} onOpenChange={setRedeemOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>兑换码充值</DialogTitle>
            <DialogDescription>
              形如 KM-XXXXX-XXXXX-XXXXX，大小写 / 空格 / 连字符会自动规整，可整段粘贴。
              同一张码重复提交不会重复充值。
            </DialogDescription>
          </DialogHeader>
          <Input
            value={code}
            onChange={(e) => setCode(e.target.value)}
            placeholder="KM-A2B3C-D4E5F-G6H7J"
            autoFocus
          />
          <DialogFooter>
            <Button variant="outline" onClick={() => setRedeemOpen(false)}>
              取消
            </Button>
            <Button onClick={handleRedeem} disabled={redeem.isPending}>
              {redeem.isPending ? '兑换中…' : '兑换'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 写入卖家 webhook 地址 */}
      <Dialog open={webhookOpen} onOpenChange={setWebhookOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>写入卖家侧 Webhook 地址</DialogTitle>
            <DialogDescription>
              需填完整地址，含路径 token，例如
              <code className="mx-1 text-xs">https://你的域名/webhook/vendor/whk_xxx</code>
              。token 只存在服务端配置里，此处需手动补全。
            </DialogDescription>
          </DialogHeader>
          <Input
            value={webhookUrl}
            onChange={(e) => setWebhookInput(e.target.value)}
            placeholder="https://rs.example.com/webhook/vendor/whk_xxx"
            autoFocus
          />
          <DialogFooter>
            <Button variant="outline" onClick={() => setWebhookOpen(false)}>
              取消
            </Button>
            <Button onClick={handleSetWebhook} disabled={setWebhookUrl.isPending}>
              {setWebhookUrl.isPending ? '提交中…' : '写入'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 直接提取 */}
      <Dialog open={directOpen} onOpenChange={setDirectOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>直接提取</DialogTitle>
            <DialogDescription>
              不依赖 webhook 事件，订单号由服务端生成。会真实扣费，提交前需二次确认。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <div>
              <label className="text-xs text-muted-foreground">提取数量</label>
              <Input
                type="number"
                min={1}
                value={directCount}
                onChange={(e) => setDirectCount(e.target.value)}
                className="mt-1"
                autoFocus
              />
              <div className="mt-1 text-xs text-muted-foreground">
                本轮可提取上限 {status?.stock?.available ?? status?.stockMax ?? '未知'}
                {profile?.balance != null ? `，当前余额 ${profile.balance}` : ''}
              </div>
            </div>

            {status?.stock?.zones && status.stock.zones.length > 0 && (
              <div>
                <label className="text-xs text-muted-foreground">提取区域</label>
                <Select value={directZone} onValueChange={setDirectZone}>
                  <SelectTrigger className="mt-1">
                    <SelectValue placeholder="自动选择最便宜的区" />
                  </SelectTrigger>
                  <SelectContent>
                    {status.stock.zones
                      .filter((z) => z.enabled && z.available > 0)
                      .map((z) => (
                        <SelectItem key={z.zone} value={z.zone}>
                          {z.label ?? z.zone} · {z.available} 个可提
                          {z.unitPrice != null && ` · ${z.unitPrice} 积分/个`}
                        </SelectItem>
                      ))}
                  </SelectContent>
                </Select>
                <div className="mt-1.5 text-xs text-muted-foreground">
                  各区单价独立。直接提取不落库，重试时需手动带上响应回显的 zone。
                </div>
              </div>
            )}

            <div className="rounded-md bg-muted/50 p-2.5 text-xs text-muted-foreground">
              入库参数：分组{' '}
              {status?.defaultGroups?.length ? status.defaultGroups.join(' / ') : '无'}
              ，RPM {status?.defaultRpmLimit ?? 10}
              ，Region {formatVendorRegion(status)}
              （在 config.json 的 vendor 段调整）
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDirectOpen(false)}>
              取消
            </Button>
            <Button
              onClick={handleDirectPurchase}
              disabled={purchaseAdHoc.isPending}
            >
              {purchaseAdHoc.isPending ? '提取中…' : '提取'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}
