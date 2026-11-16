import { Gauge } from 'lucide-react'
import { Progress } from '@/components/ui/progress'
import { cn } from '@/lib/utils'
import type { CredentialQuotaSummary, CredentialRpmSummary } from '@/types/api'

/**
 * 账号池额度信息：剩余额度 / 整体使用率 / 采样个数 / RPM 汇总。
 *
 * 额度来自凭据列表接口的 quotaSummary：后端对全部未禁用凭据的缓存余额求和，
 * 与列表每行显示的余额同源，新增凭据查到余额后随列表刷新立即计入，
 * 不必等后台整轮余额刷新。没有缓存余额的凭据不计入总量，用「采样」标出覆盖率。
 * 额度告警仍按完整余额轮次计算，口径独立。
 */
export function QuotaOverviewSummary({
  rpm,
  quota,
}: {
  rpm?: CredentialRpmSummary
  quota?: CredentialQuotaSummary
}) {
  const hasData = quota != null && quota.limit > 0
  const used = hasData ? Math.max(quota.limit - quota.remaining, 0) : 0
  const pct = hasData ? (used / quota.limit) * 100 : null
  const partial = quota != null && quota.covered < quota.active

  const fmt = (v: number) => v.toLocaleString('en-US', { maximumFractionDigits: 0 })

  return (
    <section
      className="mb-4 overflow-hidden rounded-xl border border-border bg-card shadow-xs"
      aria-label="额度信息"
    >
      <div className="flex flex-wrap items-center gap-x-6 gap-y-3 px-4 py-3">
        <div className="flex items-center gap-3">
          <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-primary/10 text-primary">
            <Gauge className="h-4 w-4" aria-hidden="true" />
          </span>
          <div className="min-w-0">
            <h2 className="text-sm font-semibold">额度信息</h2>
            <p className="mt-0.5 text-[11px] text-muted-foreground">
              {!quota
                ? '额度数据加载中'
                : partial
                  ? '部分凭据尚未查到余额，总量偏小'
                  : '启用账号 · 实时缓存余额'}
            </p>
          </div>
        </div>

        <Metric label="整体使用率" title="汇总已用 ÷ 汇总额度（不平均各账号百分比）">
          <div className="flex items-center gap-2">
            <strong
              className={cn(
                'font-mono text-xl font-semibold tabular-nums tracking-tight',
                pct == null
                  ? 'text-muted-foreground'
                  : pct >= 90
                    ? 'text-red-600 dark:text-red-400'
                    : pct >= 80
                      ? 'text-amber-600 dark:text-amber-400'
                      : 'text-primary',
              )}
            >
              {pct != null ? `${pct.toFixed(2)}%` : '—'}
            </strong>
            {pct != null && <Progress value={Math.min(pct, 100)} className="h-1.5 w-16" />}
          </div>
        </Metric>

        <Metric
          label="剩余额度"
          title={
            hasData
              ? `剩余 = 总额度 − 已用（credits，单个账号超额按 0 计）\n已用 ${fmt(used)} / 总额度 ${fmt(quota.limit)}`
              : '剩余 = 总额度 − 已用（credits）'
          }
        >
          <span className="font-mono text-sm font-semibold tabular-nums">
            {hasData ? (
              <>
                {fmt(quota.remaining)}
                <span className="text-muted-foreground"> / {fmt(quota.limit)}</span>
              </>
            ) : '—'}
          </span>
        </Metric>

        <Metric label="采样个数" title="已查到余额的未禁用凭据 / 未禁用凭据；未查到余额的不计入总量">
          <span
            className={cn(
              'font-mono text-sm font-semibold tabular-nums',
              partial && 'text-amber-600 dark:text-amber-400',
            )}
          >
            {quota ? (
              <>
                {quota.covered}
                <span className="text-muted-foreground"> / {quota.active}</span>
              </>
            ) : '—'}
          </span>
        </Metric>

        <Metric
          label="RPM"
          title={`未禁用凭据当前 60 秒已用请求数 / RPM 上限之和${rpm?.unlimitedCount ? `；另有 ${rpm.unlimitedCount} 个不限，未计入上限` : ''}`}
        >
          <span className="font-mono text-sm font-semibold tabular-nums">
            {rpm ? (
              <>
                {fmt(rpm.current)}
                <span className="text-muted-foreground"> / {rpm.limit > 0 ? fmt(rpm.limit) : '不限'}</span>
                {rpm.unlimitedCount > 0 && rpm.limit > 0 && (
                  <span className="ml-1 text-[11px] font-normal text-muted-foreground">
                    +{rpm.unlimitedCount} 不限
                  </span>
                )}
              </>
            ) : '—'}
          </span>
        </Metric>
      </div>
    </section>
  )
}

function Metric({
  label,
  title,
  children,
}: {
  label: string
  title?: string
  children: React.ReactNode
}) {
  return (
    <div className="flex flex-col gap-0.5" title={title}>
      <span className="text-[11px] text-muted-foreground">{label}</span>
      {children}
    </div>
  )
}
