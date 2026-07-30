import { toast } from 'sonner'
import { PackageOpen, Wallet, ShoppingCart } from 'lucide-react'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { useKiroappStatus, useClaimKiroappKey } from '@/hooks/use-vendor'
import { vendorErrorMessage } from '@/api/vendor'

/**
 * 次级卖家 kiroapp 卡片。
 *
 * 与主卖家的状态区分开呈现：对方只有查库存 / 查余额 / 提取三个接口，没有
 * webhook、没有订单号、没有数量参数（一次一个），故这里只有一个「提取 1 个」按钮。
 *
 * 未配置时整块不渲染 —— 没配就说明没在用这家，不需要占位提示。
 */
export function KiroappCard() {
  const { data: status, isLoading } = useKiroappStatus()
  const claim = useClaimKiroappKey()
  const confirm = useConfirm()

  // 未配置不渲染。isLoading 期间也不渲染，避免闪一下又消失。
  if (isLoading || !status?.configured) return null

  const available = status.stock?.availableKeys
  const price = status.stock?.keyPrice
  const balance = status.balance?.balance

  const hasStock = typeof available === 'number' && available > 0
  // 余额不足时不让点：对方会直接拒，白撞一次接口
  const affordable =
    typeof balance !== 'number' || typeof price !== 'number' || balance >= price

  const handleClaim = async () => {
    const ok = await confirm({
      title: '从 kiroapp 提取 1 个 Key？',
      description:
        `会真实扣费${typeof price === 'number' ? `（单价 ${price}）` : ''}。` +
        '对方接口没有幂等键，失败时不会自动重试，请勿连续点击。',
      confirmText: '提取',
    })
    if (!ok) return

    try {
      const r = await claim.mutateAsync()
      // 一个 Key 都没识别出来：可能已扣费但没入库，必须让用户去核对
      if (r.claimed === 0) {
        toast.warning(r.error ?? '未识别出 Key，请到卖家侧核对是否已扣费', {
          duration: 10000,
        })
        return
      }
      toast.success(
        [
          `提取 ${r.claimed} 个`,
          `入库 ${r.imported}`,
          r.duplicated ? `重复 ${r.duplicated}` : '',
          r.failed ? `失败 ${r.failed}` : '',
        ]
          .filter(Boolean)
          .join('，'),
      )
    } catch (e) {
      toast.error(vendorErrorMessage(e, '提取失败'))
    }
  }

  return (
    <Card>
      <CardContent className="flex flex-wrap items-center justify-between gap-3 p-4">
        <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
          <div className="text-sm font-medium">kiroapp</div>

          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <PackageOpen className="h-3.5 w-3.5" />
            <span>可售</span>
            <span
              className={`text-sm font-semibold tabular-nums ${
                status.stockError
                  ? 'text-amber-600 dark:text-amber-500'
                  : hasStock
                    ? 'text-emerald-600 dark:text-emerald-500'
                    : 'text-foreground'
              }`}
            >
              {status.stockError ? '—' : (available ?? '—')}
            </span>
            {typeof price === 'number' && <span>· 单价 {price}</span>}
          </div>

          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Wallet className="h-3.5 w-3.5" />
            <span>余额</span>
            <span className="text-sm font-semibold tabular-nums text-foreground">
              {status.balanceError ? '—' : (balance ?? '—')}
            </span>
          </div>

          {(status.stockError || status.balanceError) && (
            <div className="text-xs text-amber-600 dark:text-amber-500">
              {status.stockError ?? status.balanceError}
            </div>
          )}
        </div>

        <div className="flex items-center gap-2">
          <div className="text-xs text-muted-foreground">
            入库：分组{' '}
            {status.defaultGroups?.length ? status.defaultGroups.join(' / ') : '无'}
            ，RPM {status.defaultRpmLimit}
          </div>
          <Button
            size="sm"
            onClick={handleClaim}
            disabled={claim.isPending || !hasStock || !affordable}
            title={
              !hasStock
                ? '当前无可售 Key'
                : !affordable
                  ? '余额不足'
                  : undefined
            }
          >
            <ShoppingCart className="mr-1.5 h-3.5 w-3.5" />
            {claim.isPending ? '提取中…' : '提取 1 个'}
          </Button>
        </div>
      </CardContent>
    </Card>
  )
}
