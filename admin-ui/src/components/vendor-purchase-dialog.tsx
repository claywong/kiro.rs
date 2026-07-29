import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import { AlertTriangle, Lock } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import { usePurchaseForEvent } from '@/hooks/use-vendor'
import { extractErrorMessage } from '@/lib/utils'
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
}: {
  event: VendorEvent | null
  status?: VendorStatus
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const purchase = usePurchaseForEvent()
  const [count, setCount] = useState('')

  const locked = event?.boundCount != null
  const boundCount = event?.boundCount

  useEffect(() => {
    if (!event) return
    // 已绑定则强制显示绑定值；首次提取取事件声明数量与当前上限的较小值
    const availableCount = Math.min(event.newKeys ?? 1, status?.stockMax ?? Infinity)
    setCount(String(boundCount ?? availableCount))
  }, [event, boundCount, status?.stockMax])

  const handleSubmit = async () => {
    if (!event) return
    const n = locked ? boundCount! : Number(count)
    if (!Number.isInteger(n) || n <= 0) {
      toast.error('提取数量需为正整数')
      return
    }
    try {
      const r = await purchase.mutateAsync({ eventId: event.eventId, count: n })
      toast.success(`提取完成：出 ${r.purchased} 个，入库 ${r.imported} 个`, {
        description: [
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
                  {status?.stockMax != null && `，当前可提取上限 ${status.stockMax}`}。
                </span>
              </div>
            )}
          </div>

          <div className="rounded-md bg-muted/50 p-2.5 text-xs text-muted-foreground">
            入库参数：分组{' '}
            {status?.defaultGroups?.length ? status.defaultGroups.join(' / ') : '无'}
            ，RPM {status?.defaultRpmLimit ?? 10}
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
