import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import { AlertTriangle, Lock } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from '@/components/ui/select'
import { usePurchaseForEvent } from '@/hooks/use-vendor'
import { extractErrorMessage } from '@/lib/utils'
// 本地新增：Region 展示文案，单独成行避免与上游 import 块相撞。
import { formatVendorRegion } from '@/lib/vendor-region'
import type { VendorEvent, VendorStatus } from '@/types/api'

/**
 * 按事件提取入库的弹窗。
 *
 * 核心约束：卖家侧对「同订单号 + 同 count」幂等重放，改 count 会返回 409。
 * 而订单号由卖家给定、重投不变，所以首次提交的数量一旦绑定就永久锁死。
 * 因此：
 * - 未绑定过 → 数量可改，默认取 newKeys 与当前可提取上限的较小值，并醒目提示「提交后永久绑定」
 * - 已绑定过 → 数量框置灰锁死，只能按绑定值重试
 */
export function VendorPurchaseDialog({
  event,
  status,
  open,
  onOpenChange,
  vendorId,
}: {
  event: VendorEvent | null
  status?: VendorStatus
  open: boolean
  onOpenChange: (open: boolean) => void
  vendorId?: string
}) {
  const purchase = usePurchaseForEvent(vendorId)
  const [count, setCount] = useState('')
  const [zone, setZone] = useState<string>('')

  const locked = event?.boundCount != null
  const boundCount = event?.boundCount
  const boundZone = event?.boundZone

  const zones = status?.stock?.zones?.filter((z) => z.enabled && z.available > 0) ?? []
  const hasZones = zones.length > 0
  // 自动选一个：优先用已绑定的，否则按单价最低（同价按 available 大的）
  const pickZone = () => {
    if (boundZone) return boundZone
    if (zones.length === 0) return undefined
    return zones.reduce((best, z) =>
      (z.unitPrice ?? Infinity) < (best.unitPrice ?? Infinity) ||
      ((z.unitPrice ?? Infinity) === (best.unitPrice ?? Infinity) && z.available > best.available)
        ? z
        : best
    ).zone
  }

  useEffect(() => {
    if (!event) return
    // 已绑定则强制显示绑定值；首次提取取事件声明数量与当前上限的较小值
    const picked = pickZone()
    const zoneMax = picked ? zones.find((z) => z.zone === picked)?.available : (status?.stock?.available ?? status?.stockMax)
    const availableCount = Math.min(event.newKeys ?? 1, zoneMax ?? Infinity)
    setCount(String(boundCount ?? availableCount))
    setZone(picked ?? '')
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [event, boundCount, boundZone, status?.stock?.available, status?.stockMax])

  const handleSubmit = async () => {
    if (!event) return
    const n = locked ? boundCount! : Number(count)
    if (!Number.isInteger(n) || n <= 0) {
      toast.error('提取数量需为正整数')
      return
    }
    try {
      const r = await purchase.mutateAsync({
        eventId: event.eventId,
        count: n,
        zone: hasZones && zone ? zone : undefined,
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
      onOpenChange(false)
    } catch (e) {
      toast.error(extractErrorMessage(e))
    }
  }

  if (!event) return null

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{locked ? '重试提取' : '提取入库'}</DialogTitle>
          <DialogDescription>{event.message ?? '提取该事件对应的 Key'}</DialogDescription>
        </DialogHeader>

        <div className="space-y-3">
          <div>
            <label className="text-xs text-muted-foreground">订单号（不可改）</label>
            <div className="mt-1 break-all rounded-md bg-muted/50 px-2.5 py-2 font-mono text-xs">
              {event.purchaseOrderId ?? '（该事件无订单号）'}
            </div>
          </div>

          <div>
            <label className="text-xs text-muted-foreground">提取数量</label>
            <Input
              type="number"
              min={1}
              value={count}
              onChange={(e) => setCount(e.target.value)}
              disabled={locked}
              className="mt-1"
              autoFocus={!locked}
            />
            {locked ? (
              <div className="mt-1.5 flex items-start gap-1.5 text-xs text-muted-foreground">
                <Lock className="mt-0.5 h-3 w-3 shrink-0" />
                <span>该订单已绑定 {boundCount} 个，只能按此数量重试。</span>
              </div>
            ) : (
              <div className="mt-1.5 flex items-start gap-1.5 rounded-md border border-amber-500/40 bg-amber-500/5 px-2.5 py-2 text-xs text-amber-700 dark:text-amber-500">
                <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                <span>
                  数量提交后与该订单号永久绑定，卖家侧不允许改数量重试。
                  {event.newKeys != null && `本次事件声明 ${event.newKeys} 个`}
                  {(status?.stock?.available != null || status?.stockMax != null) && `，当前可提取上限 ${status.stock?.available ?? status.stockMax}`}。
                </span>
              </div>
            )}
          </div>

          {hasZones && (
            <div>
              <label className="text-xs text-muted-foreground">
                提取区域{locked && boundZone ? '（已绑定）' : ''}
              </label>
              <Select value={zone} onValueChange={setZone} disabled={locked && !!boundZone}>
                <SelectTrigger className="mt-1">
                  <SelectValue placeholder="自动选择最便宜的区" />
                </SelectTrigger>
                <SelectContent>
                  {zones.map((z) => (
                    <SelectItem key={z.zone} value={z.zone}>
                      {z.label ?? z.zone} · {z.available} 个可提
                      {z.unitPrice != null && ` · ${z.unitPrice} 积分/个`}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              {locked && boundZone && (
                <div className="mt-1.5 flex items-start gap-1.5 text-xs text-muted-foreground">
                  <Lock className="mt-0.5 h-3 w-3 shrink-0" />
                  <span>该订单已绑定区域 {boundZone}，重试必须用同一区。</span>
                </div>
              )}
              {!locked && (
                <div className="mt-1.5 text-xs text-muted-foreground">
                  各区单价独立。区域与数量一起绑定，换区重试会被当成新订单再扣一次积分。
                </div>
              )}
            </div>
          )}

          <div className="rounded-md bg-muted/50 p-2.5 text-xs text-muted-foreground">
            入库参数：分组{' '}
            {status?.defaultGroups?.length ? status.defaultGroups.join(' / ') : '无'}
            ，RPM {status?.defaultRpmLimit ?? 10}
            ，Region {formatVendorRegion(status)}
            （在 config.json 的 vendor 段调整）
          </div>

          {event.lastError && (
            <div className="rounded-md border border-destructive/40 bg-destructive/5 px-2.5 py-2 text-xs text-destructive">
              上次失败：{event.lastError}
            </div>
          )}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            取消
          </Button>
          <Button
            onClick={handleSubmit}
            disabled={purchase.isPending || !event.purchaseOrderId}
          >
            {purchase.isPending
              ? '提取中…'
              : locked
                ? `按 ${boundCount} 个重试`
                : '提取入库'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
