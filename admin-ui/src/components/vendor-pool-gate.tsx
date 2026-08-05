import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { vendorErrorMessage } from '@/api/vendor'
import {
  useVendorList,
  useSetVendorPoolTarget,
  useSetVendorPerChannel,
} from '@/hooks/use-vendor'

/**
 * 全局提取限制。跨所有卖家共享，故摆在供应商标签页之外。
 *
 * 为什么需要它：各家的失效判定按设计互不可见（A 家推「全部失效」时若把 B 家
 * 健康的 Key 算进来，A 的补货会被 B 挡死）。代价是多家 Key 同期失效时，
 * 三家各自都得出「池子空了」的结论，于是各提一份、各扣一次费。
 *
 * 阈值刻意不做成开关 + 数字两个控件：那会产生「开关开着但阈值为 0」这种无意义
 * 组合，语义上等于永久禁止自动补货，几乎肯定是误填。用单一数字 + 0 表示不启用，
 * 与后端 `autoPurchaseSchedule` 的 `maxCount: 0` 是同一套约定。
 *
 * 「逐渠道补货」是另一套判据，不是这个阈值的修饰项：打开后判据换成「本家有没有
 * 存活 Key」，阈值**完全不参与**。故它打开时把阈值一栏整体置灰 —— 若只是加一行
 * 说明而让输入框看着照常可用，会让人以为两者叠加生效。
 */
export function VendorPoolGate() {
  const { data: vendorList } = useVendorList()
  const setPoolTarget = useSetVendorPoolTarget()
  const setPerChannel = useSetVendorPerChannel()

  const saved = vendorList?.poolTarget ?? 0
  const perChannel = vendorList?.perChannel ?? false
  const [input, setInput] = useState(String(saved))

  // 服务端值变化时同步到输入框（首次加载完成、或保存后拉回实际值）。
  // 不放在 useState 初值里：清单是异步来的，初次渲染时还是 undefined。
  useEffect(() => {
    setInput(String(saved))
  }, [saved])

  const parsed = Number(input)
  const valid = Number.isInteger(parsed) && parsed >= 0
  const dirty = valid && parsed !== saved

  const handleSave = async () => {
    if (!valid) {
      toast.error('请填写 0 或正整数')
      return
    }
    try {
      const r = await setPoolTarget.mutateAsync(parsed)
      const what =
        r.poolTarget === 0
          ? '已关闭全局提取限制'
          : `全局提取限制已设为 ${r.poolTarget}`
      // 持久化失败仍是 200：运行时已生效，但重启会回退，这一点必须说清
      if (r.persisted) {
        toast.success(what)
      } else {
        toast.warning(`${what}，但未能写入配置文件`, {
          description: r.warning
            ? `${r.warning}。重启后会回退到文件里的值。`
            : '重启后会回退到文件里的值。',
        })
      }
    } catch (e) {
      toast.error(vendorErrorMessage(e, '设置全局提取限制失败'))
    }
  }

  const handleToggle = async (next: boolean) => {
    try {
      const r = await setPerChannel.mutateAsync(next)
      const what = r.perChannel
        ? '已开启逐渠道补货'
        : '已关闭逐渠道补货，恢复按全局阈值判断'
      if (r.persisted) {
        toast.success(what)
      } else {
        toast.warning(`${what}，但未能写入配置文件`, {
          description: r.warning
            ? `${r.warning}。重启后会回退到文件里的值。`
            : '重启后会回退到文件里的值。',
        })
      }
    } catch (e) {
      toast.error(vendorErrorMessage(e, '设置逐渠道补货失败'))
    }
  }

  return (
    <Card>
      <CardContent className="flex flex-col gap-3 px-4 py-3">
        <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
          <div className="text-sm font-medium">逐渠道补货</div>
          <Switch
            checked={perChannel}
            onCheckedChange={handleToggle}
            disabled={setPerChannel.isPending}
            aria-label="只看本渠道有没有存活 Key，没有就补货"
          />
          <div className="min-w-0 flex-1 text-xs text-muted-foreground">
            {perChannel ? (
              <>
                判据为「<strong>本渠道</strong>无存活 Key 即补货」，各家互不影响。
                每家会各自维持库存，故账号消耗约为渠道数倍 —— 买到的号若很快被封，
                本渠道又回到无存活，会再补一张。
              </>
            ) : (
              <>
                关闭时按下方全局阈值判断：只看池子总量，不区分渠道。
                开启后改为每家各自维持库存，可用性更高但账号消耗更大。
              </>
            )}
          </div>
        </div>

        <div
          className={`flex flex-wrap items-center gap-x-4 gap-y-2 border-t pt-3 ${
            perChannel ? 'opacity-50' : ''
          }`}
        >
          <div className="text-sm font-medium">全局提取限制</div>

          <div className="flex items-center gap-2">
            <Input
              type="number"
              min={0}
              step={1}
              value={input}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && dirty) handleSave()
              }}
              disabled={perChannel}
              className="h-8 w-20 tabular-nums"
              aria-label="池中存活卖家 Key 的上限，0 表示不限制"
            />
            <Button
              size="sm"
              onClick={handleSave}
              disabled={perChannel || !dirty || setPoolTarget.isPending}
            >
              {setPoolTarget.isPending ? '保存中…' : '保存'}
            </Button>
          </div>

          <div className="min-w-0 flex-1 text-xs text-muted-foreground">
            {perChannel ? (
              <>
                逐渠道补货已开启，<strong>本项不参与判断</strong>
                （保留的值会在关闭逐渠道后重新生效）。
              </>
            ) : (
              <>
                {saved === 0 ? (
                  <>当前不限制。多家卖家的 Key 同期失效时，每家会各自补一次货。</>
                ) : (
                  <>池中存活的卖家 Key 达到 {saved} 个时，各家都不再自动补货。</>
                )}
                <span className="ml-1">
                  填 0 表示不限制。此项与各家自己的「单次提取上限」是两层限制：后者管一笔提多少，此项管池子总量。
                </span>
              </>
            )}
          </div>
        </div>
      </CardContent>
    </Card>
  )
}
