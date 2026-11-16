import { useQuery } from '@tanstack/react-query'
import { Gauge } from 'lucide-react'
import { getQuotaAlerts } from '@/api/credentials'
import { Progress } from '@/components/ui/progress'
import { cn } from '@/lib/utils'
import type { CredentialRpmSummary } from '@/types/api'

/**
 * 账号池额度信息：汇总额度 / 整体使用率 / 有效个数 / RPM 汇总。
 *
 * 额度三项直接复用额度告警的 overview（与「设置 → 额度告警 → 账号池概况」同一口径：
 * 基于最近完整余额轮次，汇总已用 ÷ 汇总额度，过期数据不参与）；RPM 来自凭据列表
 * 接口的全量 rpmSummary。
 */
export function QuotaOverviewSummary({ rpm }: { rpm?: CredentialRpmSummary }) {
  // 与设置页共用 queryKey，两处数据同源、互相复用缓存
  const { data, isPending, isError } = useQuery({
    queryKey: ['quota-alerts'],
    queryFn: getQuotaAlerts,
    refetchInterval: 30_000,
  })
  const overview = data?.overview
  const pct = overview?.percentage ?? null
  const placeholder = isPending ? '…' : '—'
  // 重启后首轮余额刷新完成前，overview 为空壳（0 / 0），显示为「统计中」而非误导性的 0
  const noRound = overview?.completedAt == null
  const shown = noRound ? undefined : overview

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
              {isError
                ? '额度数据加载失败'
                : overview && overview.completedAt == null
                  ? '首轮余额刷新进行中（服务重启后约需 5 分钟）'
                  : overview && !overview.complete
                    ? '仅有效数据小计（非完整池统计）'
                    : '启用账号 · 最近完整余额轮次'}
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
              {pct != null ? `${pct.toFixed(2)}%` : noRound && overview ? '统计中' : placeholder}
            </strong>
            {pct != null && <Progress value={Math.min(pct, 100)} className="h-1.5 w-16" />}
          </div>
        </Metric>

        <Metric label="汇总额度" title="已用 / 总额度（credits）">
          <span className="font-mono text-sm font-semibold tabular-nums">
            {shown ? (
              <>
                {fmt(shown.used)}
                <span className="text-muted-foreground"> / {fmt(shown.limit)}</span>
              </>
            ) : noRound && overview ? '统计中' : placeholder}
          </span>
        </Metric>

        <Metric label="有效个数" title="余额数据有效的账号 / 启用账号">
          <span className="font-mono text-sm font-semibold tabular-nums">
            {shown ? (
              <>
                {shown.validAccounts}
                <span className="text-muted-foreground"> / {shown.enabledAccounts}</span>
              </>
            ) : noRound && overview ? '统计中' : placeholder}
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
