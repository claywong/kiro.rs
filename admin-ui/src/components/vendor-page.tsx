import { useState } from 'react'
import { toast } from 'sonner'
import {
  RefreshCw, PackagePlus, SkullIcon, ChevronDown, ChevronRight, Check, CheckCheck, Repeat,
} from 'lucide-react'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { VendorStatusBar } from '@/components/vendor-status-bar'
import { VendorPurchaseDialog } from '@/components/vendor-purchase-dialog'
import {
  useVendorStatus, useVendorEvents, useVendorOrders, useAckVendorEvents,
} from '@/hooks/use-vendor'
import { extractErrorMessage } from '@/lib/utils'
import type { VendorEvent } from '@/types/api'

function formatTime(ts?: string): string {
  if (!ts) return '—'
  const d = new Date(ts)
  if (Number.isNaN(d.getTime())) return ts
  return d.toLocaleString('zh-CN', { hour12: false })
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
  return <Badge variant="secondary">{type}</Badge>
}

/** 提取结果列 */
function PurchaseStatusCell({ event }: { event: VendorEvent }) {
  if (!event.purchaseStatus) {
    return <span className="text-xs text-muted-foreground">未提取</span>
  }
  if (event.purchaseStatus === 'failed') {
    return (
      <div className="text-xs">
        <span className="font-medium text-destructive">提取失败</span>
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
      <div className="mt-0.5 text-muted-foreground">{formatTime(event.processedAt)}</div>
    </div>
  )
}

/** 订单历史折叠面板：展开时才拉数据 */
function OrdersPanel() {
  const [open, setOpen] = useState(false)
  const { data, isLoading, refetch, isFetching } = useVendorOrders(open)
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
                      <tr key={o.client_order_id ?? i} className="border-b last:border-0">
                        <td className="px-4 py-2 font-mono text-xs">
                          {o.client_order_id ?? '—'}
                        </td>
                        <td className="px-4 py-2 text-right tabular-nums">
                          {o.requested ?? '—'}
                        </td>
                        <td className="px-4 py-2 text-right tabular-nums">
                          {o.purchased ?? '—'}
                        </td>
                        <td className="px-4 py-2 text-xs text-muted-foreground">
                          {o.created_at ?? '—'}
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
 * - 入站 webhook 只落库不花钱，所有提取动作在此页显式触发
 * - 提取数量与订单号永久绑定（卖家侧改数量会 409），弹窗内锁定处理
 * - 未确认事件整行高亮 + 顶部计数，点「已知悉」消除
 */
export function VendorPage() {
  const { data: status } = useVendorStatus()
  const { data, isLoading, isFetching, refetch } = useVendorEvents()
  const ack = useAckVendorEvents()

  const [purchaseTarget, setPurchaseTarget] = useState<VendorEvent | null>(null)
  const [purchaseOpen, setPurchaseOpen] = useState(false)

  const events = data?.events ?? []
  const unacked = data?.unacked ?? 0

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
      <VendorStatusBar />

      <Card>
        <CardContent className="p-0">
          <div className="flex items-center justify-between px-4 py-3">
            <div className="flex items-center gap-2 text-sm font-medium">
              卖家事件
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
                还没收到任何事件。
                {status?.inboundEnabled
                  ? '可点上方「测试推送」验证链路。'
                  : '入站 webhook 未启用，请先配置 vendor.webhookPathToken。'}
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
                        <td className="px-4 py-2.5">
                          <PurchaseStatusCell event={e} />
                        </td>
                        <td className="whitespace-nowrap px-4 py-2.5 text-right">
                          <div className="flex items-center justify-end gap-1.5">
                            {e.eventType === 'new_keys_available' && e.purchaseOrderId && (
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

      <OrdersPanel />

      <VendorPurchaseDialog
        event={purchaseTarget}
        status={status}
        open={purchaseOpen}
        onOpenChange={setPurchaseOpen}
      />
    </div>
  )
}
