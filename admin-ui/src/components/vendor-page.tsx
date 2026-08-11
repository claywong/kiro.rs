import { useState } from 'react'
import { toast } from 'sonner'
import {
  RefreshCw, PackagePlus, SkullIcon, ChevronDown, ChevronRight, Check, CheckCheck, Repeat,
  Zap, Hand, CalendarCheck, Truck,
} from 'lucide-react'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/tabs'
import { VendorStatusBar } from '@/components/vendor-status-bar'
import { VendorPoolGate } from '@/components/vendor-pool-gate'
import { VendorPurchaseDialog } from '@/components/vendor-purchase-dialog'
import {
  useVendorList, useVendorStatus, useVendorEvents, useVendorOrders, useAckVendorEvents,
} from '@/hooks/use-vendor'
import { extractErrorMessage } from '@/lib/utils'
import type { VendorEvent } from '@/types/api'

function formatTime(ts?: string): string {
  if (!ts) return '—'
  const d = new Date(ts)
  if (Number.isNaN(d.getTime())) return ts
  return d.toLocaleString('zh-CN', { hour12: false })
}

/** 事件展示窗口：只看最近 24 小时，更早的记录对排障已无参考价值 */
const EVENT_WINDOW_MS = 24 * 60 * 60 * 1000

/**
 * 事件时间戳转毫秒。解析不出来返回 null —— 调用方一律按「不做时间判断」处理，
 * 免得因为一条脏时间把事件藏掉或错误开放提取按钮。
 */
function eventTime(ts?: string): number | null {
  if (!ts) return null
  const t = new Date(ts).getTime()
  return Number.isNaN(t) ? null : t
}

/** 事件类型标签 */
function EventTypeBadge({ type }: { type: string }) {
  if (type === 'new_keys_available') {
    return (
      <Badge className="border-blue-500/40 bg-blue-500/10 text-blue-600 dark:text-blue-400">
        <PackagePlus className="mr-1 h-3 w-3" />
        新 Key 就绪
      </Badge>
    )
  }
  if (type === 'all_keys_dead') {
    return (
      <Badge className="border-destructive/40 bg-destructive/10 text-destructive">
        <SkullIcon className="mr-1 h-3 w-3" />
        全部失效
      </Badge>
    )
  }
  if (type === 'reservation_created') {
    return (
      <Badge className="border-cyan-500/40 bg-cyan-500/10 text-cyan-700 dark:text-cyan-400">
        <CalendarCheck className="mr-1 h-3 w-3" />
        预定成功
      </Badge>
    )
  }
  if (type === 'reservation_delivered') {
    return (
      <Badge className="border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-400">
        <Truck className="mr-1 h-3 w-3" />
        卖家发货
      </Badge>
    )
  }
  return <Badge variant="secondary">{type}</Badge>
}

/** 自动 / 手动触发标记。未提取过的事件不显示 */
function TriggerTag({ trigger }: { trigger?: string }) {
  if (!trigger) return null
  const auto = trigger === 'auto'
  return (
    <span
      className="ml-1.5 inline-flex items-center gap-0.5 rounded bg-muted px-1 py-0.5 text-[10px] text-muted-foreground"
      title={auto ? '由自动模式触发' : '手动触发'}
    >
      {auto ? <Zap className="h-2.5 w-2.5" /> : <Hand className="h-2.5 w-2.5" />}
      {auto ? '自动' : '手动'}
    </span>
  )
}

/**
 * 失效确认结论（仅 all_keys_dead 事件）。
 *
 * 这一列决定了下一轮新 Key 能不能自动提取：只有「已确认失效」才授权自动扣费，
 * 且该授权用掉一次后失效，故一并标出是否已被使用。
 */
function ValidationCell({ event }: { event: VendorEvent }) {
  const s = event.validationStatus
  if (!s) return <span className="text-xs text-muted-foreground">—</span>

  const map = {
    pending: { text: '确认中', cls: 'text-amber-600 dark:text-amber-500' },
    confirmed_dead: { text: '已确认失效', cls: 'text-emerald-600 dark:text-emerald-500' },
    still_alive: { text: '仍有健康 Key', cls: 'text-muted-foreground' },
    inconclusive: { text: '无法确认', cls: 'text-muted-foreground' },
  } as const
  const { text, cls } = map[s]

  return (
    <div className="text-xs">
      <span className={`font-medium ${cls}`}>{text}</span>
      {s === 'confirmed_dead' && (
        <span className="ml-1.5 text-[10px] text-muted-foreground">
          {event.validationUsed ? '已用于自动提取' : '可授权一次自动提取'}
        </span>
      )}
      {event.validationDetail && (
        <div className="mt-0.5 text-muted-foreground">{event.validationDetail}</div>
      )}
    </div>
  )
}

/** 提取结果列 */
function PurchaseStatusCell({ event }: { event: VendorEvent }) {
  if (event.eventType === 'reservation_created') {
    return <span className="text-xs text-muted-foreground">等待发货</span>
  }
  if (event.eventType === 'reservation_delivered') {
    return <span className="text-xs font-medium text-emerald-600 dark:text-emerald-500">已发货</span>
  }
  if (!event.purchaseStatus) {
    return <span className="text-xs text-muted-foreground">未提取</span>
  }
  if (event.purchaseStatus === 'skipped') {
    return (
      <div className="text-xs">
        <span className="font-medium text-muted-foreground">自动跳过</span>
        <TriggerTag trigger={event.purchaseTrigger} />
        {event.lastError && (
          <div className="mt-0.5 break-all text-muted-foreground">{event.lastError}</div>
        )}
        <div className="mt-0.5 text-muted-foreground/80">数量未绑定，仍可手动提取</div>
      </div>
    )
  }
  if (event.purchaseStatus === 'failed') {
    return (
      <div className="text-xs">
        <span className="font-medium text-destructive">提取失败</span>
        <TriggerTag trigger={event.purchaseTrigger} />
        {event.lastError && (
          <div className="mt-0.5 break-all text-muted-foreground">{event.lastError}</div>
        )}
      </div>
    )
  }
  const parts = [`提了 ${event.purchased ?? 0}`, `入库 ${event.imported ?? 0}`]
  if (event.duplicated) parts.push(`重复 ${event.duplicated}`)
  if (event.failed) parts.push(`失败 ${event.failed}`)
  return (
    <div className="text-xs">
      <span className="font-medium text-emerald-600 dark:text-emerald-500">
        {parts.join(' / ')}
      </span>
      <TriggerTag trigger={event.purchaseTrigger} />
      <div className="mt-0.5 text-muted-foreground">{formatTime(event.processedAt)}</div>
    </div>
  )
}

/** 订单历史折叠面板：展开时才拉数据 */
function OrdersPanel({ vendorId }: { vendorId?: string }) {
  const [open, setOpen] = useState(false)
  const { data, isLoading, refetch, isFetching } = useVendorOrders(vendorId)
  const orders = data?.orders ?? []

  return (
    <Card>
      <CardContent className="p-0">
        <button
          className="flex w-full items-center justify-between px-4 py-3 text-sm font-medium hover:bg-muted/40"
          onClick={() => setOpen((v) => !v)}
        >
          <span className="flex items-center gap-1.5">
            {open ? (
              <ChevronDown className="h-4 w-4" />
            ) : (
              <ChevronRight className="h-4 w-4" />
            )}
            卖家订单历史
            <span className="text-xs font-normal text-muted-foreground">
              最近 50 条，用于对账查漏
            </span>
          </span>
          {open && (
            <Button
              variant="ghost"
              size="sm"
              onClick={(e) => {
                e.stopPropagation()
                refetch()
              }}
              disabled={isFetching}
            >
              <RefreshCw className={`h-3.5 w-3.5 ${isFetching ? 'animate-spin' : ''}`} />
            </Button>
          )}
        </button>

        {open && (
          <div className="border-t">
            {isLoading ? (
              <div className="p-4 text-sm text-muted-foreground">加载中…</div>
            ) : orders.length === 0 ? (
              <div className="p-4 text-sm text-muted-foreground">暂无订单记录</div>
            ) : (
              <div className="overflow-x-auto">
                <table className="w-full text-sm">
                  <thead className="text-xs text-muted-foreground">
                    <tr className="border-b">
                      <th className="px-4 py-2 text-left font-medium">订单号</th>
                      <th className="px-4 py-2 text-right font-medium">请求</th>
                      <th className="px-4 py-2 text-right font-medium">实际交付</th>
                      <th className="px-4 py-2 text-left font-medium">时间</th>
                    </tr>
                  </thead>
                  <tbody>
                    {orders.map((o, i) => (
                      <tr key={o.clientOrderId ?? i} className="border-b last:border-0">
                        <td className="px-4 py-2 font-mono text-xs">
                          {o.clientOrderId ?? '—'}
                        </td>
                        <td className="px-4 py-2 text-right tabular-nums">
                          {o.requested ?? '—'}
                        </td>
                        <td className="px-4 py-2 text-right tabular-nums">
                          {o.purchased ?? '—'}
                        </td>
                        <td className="px-4 py-2 text-xs text-muted-foreground">
                          {o.createdAt ?? '—'}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  )
}

/**
 * 供应商页：卖家 webhook 事件驾驶舱。
 *
 * 设计要点：
 * - 多供应商支持：顶部标签页切换，各家独立状态与事件
 * - 入站 webhook 只落库不花钱，所有提取动作在此页显式触发
 * - 提取数量与订单号永久绑定（卖家侧改数量会 409），弹窗内锁定处理
 * - 未确认事件整行高亮 + 顶部计数，点「已知悉」消除
 */
export function VendorPage() {
  const { data: vendorList } = useVendorList()
  const vendors = vendorList?.vendors ?? []
  const defaultVendorId = vendorList?.defaultVendorId

  // 当前选中的供应商 id，缺省用配置里的第一家
  const [currentVendorId, setCurrentVendorId] = useState<string | undefined>(undefined)
  const activeVendorId = currentVendorId ?? defaultVendorId

  const { data: status } = useVendorStatus(activeVendorId)
  const { data, isLoading, isFetching, refetch } = useVendorEvents(200, activeVendorId)
  const ack = useAckVendorEvents(activeVendorId)

  const [purchaseTarget, setPurchaseTarget] = useState<VendorEvent | null>(null)
  const [purchaseOpen, setPurchaseOpen] = useState(false)

  const allEvents = data?.events ?? []
  const unacked = data?.unacked ?? 0

  // 只展示最近 24 小时。时间解析失败的行保留展示，宁可多显示也不要静默丢事件。
  const cutoff = Date.now() - EVENT_WINDOW_MS
  const events = allEvents.filter((e) => {
    const t = eventTime(e.receivedAt)
    return t == null || t >= cutoff
  })
  const hiddenCount = allEvents.length - events.length

  /**
   * 最近一条「全部失效」的时间。它之前的 `new_keys_available` 对应的 Key 已经
   * 随这轮全灭作废，再提取就是白扣费，故不给按钮。
   */
  const latestDeadAt = allEvents.reduce<number | null>((acc, e) => {
    if (e.eventType !== 'all_keys_dead') return acc
    const t = eventTime(e.receivedAt)
    if (t == null) return acc
    return acc == null || t > acc ? t : acc
  }, null)

  /** 该事件是否仍值得提取：非「全部失效」之前的新 Key 事件 */
  const isPurchasable = (e: VendorEvent): boolean => {
    if (e.eventType !== 'new_keys_available' || !e.purchaseOrderId) return false
    if (latestDeadAt == null) return true
    const t = eventTime(e.receivedAt)
    // 时间不可解析时保守放开，避免因脏数据锁死唯一的提取入口
    return t == null || t > latestDeadAt
  }

  const handleAck = async (eventId?: string) => {
    try {
      await ack.mutateAsync(eventId)
      toast.success(eventId ? '已标记为已知悉' : `已确认全部 ${unacked} 条事件`)
    } catch (e) {
      toast.error(extractErrorMessage(e))
    }
  }

  const openPurchase = (event: VendorEvent) => {
    setPurchaseTarget(event)
    setPurchaseOpen(true)
  }

  return (
    <div className="space-y-4">
      {/* 多供应商标签页：配置了多家时显示切换器 */}
      {vendors.length > 1 && (
        <Tabs value={activeVendorId} onValueChange={setCurrentVendorId}>
          <TabsList>
            {vendors.map((v) => (
              <TabsTrigger key={v.vendorId} value={v.vendorId} className="relative">
                {v.name}
                {v.unacked > 0 && (
                  <span className="ml-1.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-destructive px-1 text-[10px] font-medium text-destructive-foreground">
                    {v.unacked}
                  </span>
                )}
              </TabsTrigger>
            ))}
          </TabsList>
        </Tabs>
      )}

      {/* 全局提取限制：跨所有卖家共享，故摆在标签页与单家状态条之间 */}
      <VendorPoolGate />

      <VendorStatusBar vendorId={activeVendorId} />


      <Card>
        <CardContent className="p-0">
          <div className="flex items-center justify-between px-4 py-3">
            <div className="flex items-center gap-2 text-sm font-medium">
              卖家事件
              <span className="text-xs font-normal text-muted-foreground">
                最近 24 小时
                {hiddenCount > 0 ? ` · 已折叠 ${hiddenCount} 条更早记录` : ''}
              </span>
              {unacked > 0 && (
                <Badge className="border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-500">
                  {unacked} 条未处理
                </Badge>
              )}
            </div>
            <div className="flex items-center gap-2">
              {unacked > 0 && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => handleAck()}
                  disabled={ack.isPending}
                >
                  <CheckCheck className="mr-1.5 h-3.5 w-3.5" />
                  全部已知悉
                </Button>
              )}
              <Button variant="ghost" size="sm" onClick={() => refetch()} disabled={isFetching}>
                <RefreshCw className={`h-3.5 w-3.5 ${isFetching ? 'animate-spin' : ''}`} />
              </Button>
            </div>
          </div>

          <div className="border-t">
            {isLoading ? (
              <div className="p-4 text-sm text-muted-foreground">加载中…</div>
            ) : events.length === 0 ? (
              <div className="p-8 text-center text-sm text-muted-foreground">
                {hiddenCount > 0 ? (
                  `最近 24 小时没有新事件（更早的 ${hiddenCount} 条不再展示）。`
                ) : (
                  <>
                    还没收到任何事件。
                    {status?.inboundEnabled
                      ? '可点上方「测试推送」验证链路。'
                      : '入站 webhook 未启用，请先配置 vendor.webhookPathToken。'}
                  </>
                )}
              </div>
            ) : (
              <div className="overflow-x-auto">
                <table className="w-full text-sm">
                  <thead className="text-xs text-muted-foreground">
                    <tr className="border-b">
                      <th className="px-4 py-2 text-left font-medium">接收时间</th>
                      <th className="px-4 py-2 text-left font-medium">类型</th>
                      <th className="px-4 py-2 text-left font-medium">消息</th>
                      <th className="px-4 py-2 text-right font-medium">数量</th>
                      <th className="px-4 py-2 text-left font-medium">失效确认</th>
                      <th className="px-4 py-2 text-left font-medium">提取结果</th>
                      <th className="px-4 py-2 text-right font-medium">操作</th>
                    </tr>
                  </thead>
                  <tbody>
                    {events.map((e) => (
                      <tr
                        key={e.eventId}
                        className={`border-b last:border-0 ${
                          e.acked ? '' : 'bg-amber-500/5'
                        }`}
                      >
                        <td className="whitespace-nowrap px-4 py-2.5 text-xs">
                          {formatTime(e.receivedAt)}
                          {e.deliveryCount > 1 && (
                            <span
                              className="ml-1.5 inline-flex items-center gap-0.5 rounded bg-muted px-1 py-0.5 text-[10px] text-muted-foreground"
                              title={`卖家重投了 ${e.deliveryCount} 次`}
                            >
                              <Repeat className="h-2.5 w-2.5" />×{e.deliveryCount}
                            </span>
                          )}
                        </td>
                        <td className="px-4 py-2.5">
                          <EventTypeBadge type={e.eventType} />
                        </td>
                        <td className="max-w-[22rem] px-4 py-2.5 text-xs">
                          <div className="truncate" title={e.message ?? ''}>
                            {e.message ?? '—'}
                          </div>
                          <div className="mt-0.5 font-mono text-[10px] text-muted-foreground">
                            {e.purchaseOrderId ?? e.eventId}
                          </div>
                        </td>
                        <td className="px-4 py-2.5 text-right tabular-nums">
                          {e.newKeys ?? e.dead ?? '—'}
                        </td>
                        <td className="max-w-[16rem] px-4 py-2.5">
                          <ValidationCell event={e} />
                        </td>
                        <td className="px-4 py-2.5">
                          <PurchaseStatusCell event={e} />
                        </td>
                        <td className="whitespace-nowrap px-4 py-2.5 text-right">
                          <div className="flex items-center justify-end gap-1.5">
                            {isPurchasable(e) ? (
                              <Button
                                size="sm"
                                variant={e.purchaseStatus === 'done' ? 'outline' : 'default'}
                                onClick={() => openPurchase(e)}
                              >
                                {e.purchaseStatus === 'failed'
                                  ? `重试（${e.boundCount ?? '?'}）`
                                  : e.purchaseStatus === 'done'
                                    ? '再次提取'
                                    : '提取入库'}
                              </Button>
                            ) : (
                              e.eventType === 'new_keys_available' &&
                              e.purchaseOrderId && (
                                <span
                                  className="text-xs text-muted-foreground"
                                  title="此后已收到「全部失效」，这批 Key 已作废，提取只会白扣费"
                                >
                                  已作废
                                </span>
                              )
                            )}
                            {!e.acked && (
                              <Button
                                size="sm"
                                variant="ghost"
                                onClick={() => handleAck(e.eventId)}
                                disabled={ack.isPending}
                              >
                                <Check className="mr-1 h-3.5 w-3.5" />
                                已知悉
                              </Button>
                            )}
                          </div>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        </CardContent>
      </Card>

      <OrdersPanel vendorId={activeVendorId} />

      <VendorPurchaseDialog
        event={purchaseTarget}
        status={status}
        open={purchaseOpen}
        onOpenChange={setPurchaseOpen}
        vendorId={activeVendorId}
      />
    </div>
  )
}
