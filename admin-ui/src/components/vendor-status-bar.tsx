import { useState } from 'react'
import { toast } from 'sonner'
import {
  Wallet, PackageOpen, Webhook, BellRing, Send, Ticket, Upload, ShoppingCart,
  Boxes, CalendarClock, Zap, Hand,
} from 'lucide-react'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { Switch } from '@/components/ui/switch'
import {
  useVendorStatus, useRedeemVendorCode, useTestVendorWebhook,
  useSetVendorWebhookUrl, usePurchaseAdHoc, useSetVendorMode,
} from '@/hooks/use-vendor'
import { isRateLimited, vendorErrorMessage } from '@/api/vendor'
import type { VendorStatus } from '@/types/api'

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
 * 账号起点展示。卖家返回 `YYYY-MM-DD HH:mm:ss`（无时区标记），Safari 对该形式
 * 直接 new Date 会得到 Invalid Date，故先补 'T' 再解析；解析不出来就原样显示。
 */
function describeAccountStart(status?: VendorStatus): {
  value: string
  hint: string
  tone: 'normal' | 'warn'
} {
  if (status?.keysCreatedAtError) {
    return { value: '—', hint: status.keysCreatedAtError, tone: 'warn' }
  }
  const info = status?.keysCreatedAt
  const raw = info?.created_at?.trim()
  const countHint = `历史 Key 记录 ${info?.key_count ?? 0} 条`
  if (!raw) {
    return {
      value: '—',
      hint: info ? `账号名下暂无 Key 记录，${countHint}` : '暂无数据',
      tone: 'normal',
    }
  }
  const d = parseVendorTime(raw)
  if (!d) {
    return { value: raw, hint: countHint, tone: 'normal' }
  }
  const days = Math.floor((Date.now() - d.getTime()) / 86400000)
  return {
    value: d.toLocaleDateString('zh-CN'),
    hint: `${d.toLocaleString('zh-CN', { hour12: false })}，已 ${days} 天 · ${countHint}`,
    tone: 'normal',
  }
}

/** 本机应有的 webhook 地址。路径 token 只存在后端，前端拿不到，故只做「是否已配置」提示。 */
function describeWebhook(status?: VendorStatus): {
  value: string
  tone: 'normal' | 'warn' | 'good'
  hint: string
} {
  if (!status?.inboundEnabled) {
    return {
      value: '未启用',
      tone: 'warn',
      hint: '配置 vendor.webhookPathToken 后启用入站接收',
    }
  }
  const saved = status.profile?.webhook_url?.trim()
  if (!saved) {
    return { value: '卖家侧未保存', tone: 'warn', hint: '点「写入卖家」提交本机地址' }
  }
  return { value: '已连接', tone: 'good', hint: saved }
}

export function VendorStatusBar() {
  const { data: status, isLoading } = useVendorStatus()
  const redeem = useRedeemVendorCode()
  const testWebhook = useTestVendorWebhook()
  const setWebhookUrl = useSetVendorWebhookUrl()
  const purchaseAdHoc = usePurchaseAdHoc()
  const setMode = useSetVendorMode()
  const confirm = useConfirm()

  const [redeemOpen, setRedeemOpen] = useState(false)
  const [code, setCode] = useState('')
  const [webhookOpen, setWebhookOpen] = useState(false)
  const [webhookUrl, setWebhookInput] = useState('')
  const [directOpen, setDirectOpen] = useState(false)
  const [directCount, setDirectCount] = useState('1')

  const profile = status?.profile
  const webhookInfo = describeWebhook(status)
  const system = status?.system
  const accountStart = describeAccountStart(status)

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
          `之后收到「新 Key 就绪」才自动下单，每次最多 ${
            status?.autoPurchaseMaxCount ?? 1
          } 个（还会受事件声明数量与卖家上限限制）。` +
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
    const balance = profile?.remaining ?? profile?.quota
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
      const r = await purchaseAdHoc.mutateAsync(n)
      toast.success(`提取完成：出 ${r.purchased} 个，入库 ${r.imported} 个`, {
        description: [
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
                  } 个`
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

        <div className="flex flex-wrap items-center gap-2">
        <Button variant="outline" size="sm" onClick={handleTest} disabled={testWebhook.isPending}>
          <Send className="mr-1.5 h-3.5 w-3.5" />
          测试推送
        </Button>
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            setWebhookInput(profile?.webhook_url ?? '')
            setWebhookOpen(true)
          }}
        >
          <Upload className="mr-1.5 h-3.5 w-3.5" />
          写入卖家
        </Button>
        <Button variant="outline" size="sm" onClick={() => setRedeemOpen(true)}>
          <Ticket className="mr-1.5 h-3.5 w-3.5" />
          兑换充值
        </Button>
        <Button
          size="sm"
          onClick={() => {
            setDirectCount(String(status?.stockMax && status.stockMax > 0 ? 1 : 1))
            setDirectOpen(true)
          }}
        >
          <ShoppingCart className="mr-1.5 h-3.5 w-3.5" />
          直接提取
        </Button>
        </div>
      </div>

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        <StatCard
          icon={<Wallet className="h-3.5 w-3.5" />}
          label="卖家余额"
          value={status?.profileError ? '—' : (profile?.remaining ?? profile?.quota ?? '—')}
          hint={
            status?.profileError
              ? status.profileError
              : profile
                ? `总额度 ${profile.quota ?? '-'} / 已用 ${profile.used_quota ?? '-'}`
                : undefined
          }
          tone={status?.profileError ? 'warn' : 'normal'}
        />
        <StatCard
          icon={<PackageOpen className="h-3.5 w-3.5" />}
          label="本轮可提取"
          value={status?.stockError ? '—' : (status?.stockMax ?? '—')}
          hint={status?.stockError ?? '已综合余额、库存与每母号上限'}
          tone={status?.stockError ? 'warn' : 'normal'}
        />
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
        <StatCard
          icon={<CalendarClock className="h-3.5 w-3.5" />}
          label="账号起始时间"
          value={accountStart.value}
          hint={accountStart.hint}
          tone={accountStart.tone}
        />
        <StatCard
          icon={<Webhook className="h-3.5 w-3.5" />}
          label="入站 Webhook"
          value={webhookInfo.value}
          hint={<span className="break-all">{webhookInfo.hint}</span>}
          tone={webhookInfo.tone}
        />
        <StatCard
          icon={<BellRing className="h-3.5 w-3.5" />}
          label="未处理事件"
          value={status?.unacked ?? 0}
          hint={status?.unacked ? '下方列表点「已知悉」消除' : '全部已处理'}
          tone={status?.unacked ? 'warn' : 'good'}
        />
      </div>

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
                本轮可提取上限 {status?.stockMax ?? '未知'}
                {profile?.remaining != null ? `，当前余额 ${profile.remaining}` : ''}
              </div>
            </div>
            <div className="rounded-md bg-muted/50 p-2.5 text-xs text-muted-foreground">
              入库参数：分组{' '}
              {status?.defaultGroups?.length ? status.defaultGroups.join(' / ') : '无'}
              ，成本 {status?.defaultPurchaseCost ?? '未设'}，RPM{' '}
              {status?.defaultRpmLimit ?? 10}
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
