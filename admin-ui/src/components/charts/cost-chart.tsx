import { memo, useMemo } from 'react'
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import type { CostPoint } from '@/types/api'
import { tooltipCursorStyle } from './tooltip-style'
import { formatCurrency } from '@/lib/utils'

interface Props {
  data: CostPoint[]
  currency: string
}

const COLORS = {
  amortized: '#6366f1',
  discard: '#f43f5e',
} as const

interface ChartPoint extends CostPoint {
  label: string
}

/** date "YYYY-MM-DD" → "MM-DD" 展示（跨年时补年份由 X 轴间隔控制） */
function formatDate(date: string): string {
  const parts = date.split('-')
  if (parts.length === 3) return `${parts[1]}-${parts[2]}`
  return date
}

function pickXAxisInterval(len: number): number | 'preserveStartEnd' {
  if (len <= 12) return 0
  if (len <= 31) return Math.ceil(len / 12)
  return Math.ceil(len / 16)
}

const TOOLTIP_STYLE: React.CSSProperties = {
  background: 'rgba(20,20,20,0.94)',
  border: '1px solid rgba(255,255,255,0.08)',
  borderRadius: 10,
  boxShadow: '0 8px 24px rgba(0,0,0,0.25)',
  color: '#fff',
  fontSize: 12,
  minWidth: 180,
  padding: '10px 14px',
}

const ROW_STYLE: React.CSSProperties = {
  alignItems: 'center',
  display: 'flex',
  gap: 8,
  padding: '2px 0',
}

const SWATCH_STYLE: React.CSSProperties = {
  borderRadius: 2,
  display: 'inline-block',
  height: 10,
  width: 10,
}

const VALUE_STYLE: React.CSSProperties = {
  fontVariantNumeric: 'tabular-nums',
}

function CostTooltip({ active, payload, label, currency }: {
  active?: boolean
  payload?: ReadonlyArray<{ payload?: ChartPoint }>
  label?: string
  currency: string
}) {
  if (!active || !payload?.length) return null
  const p = payload[0]?.payload
  if (!p) return null
  return (
    <div style={TOOLTIP_STYLE}>
      <div style={{ fontWeight: 600, marginBottom: 6, color: 'rgba(255,255,255,0.92)' }}>{label}</div>
      <div style={ROW_STYLE}>
        <span style={{ ...SWATCH_STYLE, background: COLORS.amortized }} />
        <span style={{ flex: 1 }}>日常摊销:</span>
        <span style={VALUE_STYLE}>{formatCurrency(p.amortizedCost, currency)}</span>
      </div>
      {p.discardCost > 0 && (
        <div style={ROW_STYLE}>
          <span style={{ ...SWATCH_STYLE, background: COLORS.discard }} />
          <span style={{ flex: 1 }}>废弃补齐:</span>
          <span style={VALUE_STYLE}>{formatCurrency(p.discardCost, currency)}</span>
        </div>
      )}
      <div style={{ ...ROW_STYLE, borderTop: '1px solid rgba(255,255,255,0.08)', marginTop: 4, padding: '4px 0 0' }}>
        <span style={{ flex: 1, fontWeight: 600 }}>合计:</span>
        <span style={{ ...VALUE_STYLE, fontWeight: 600 }}>{formatCurrency(p.totalCost, currency)}</span>
      </div>
    </div>
  )
}

function CostChartImpl({ data, currency }: Props) {
  const formatted = useMemo<ChartPoint[]>(
    () => data.map((p) => ({ ...p, label: formatDate(p.date) })),
    [data],
  )
  const interval = useMemo(() => pickXAxisInterval(formatted.length), [formatted.length])
  const allZero = useMemo(() => formatted.every((p) => p.totalCost === 0), [formatted])

  if (formatted.length === 0) {
    return (
      <div className="flex h-[260px] items-center justify-center text-sm text-muted-foreground sm:h-[320px]">
        暂无成本数据（需为账号录入「购买成本」）
      </div>
    )
  }

  return (
    <div className="h-[260px] sm:h-[320px]">
      <ResponsiveContainer width="100%" height="100%">
        <BarChart data={formatted} margin={{ top: 16, right: 6, left: -12, bottom: 0 }}>
          <CartesianGrid strokeDasharray="3 3" className="stroke-border/50" />
          <XAxis
            dataKey="label"
            tick={{ fontSize: 11 }}
            className="fill-muted-foreground"
            interval={interval}
          />
          <YAxis
            tick={{ fontSize: 11 }}
            className="fill-muted-foreground"
            tickFormatter={(v: number) => formatCurrency(v, currency)}
            width={56}
            domain={allZero ? [0, 1] : [0, 'auto']}
            ticks={allZero ? [0] : undefined}
          />
          <Tooltip content={<CostTooltip currency={currency} />} cursor={tooltipCursorStyle} />
          <Legend verticalAlign="top" align="center" iconType="circle" wrapperStyle={{ fontSize: 12, paddingBottom: 8 }} />
          <Bar dataKey="amortizedCost" stackId="cost" fill={COLORS.amortized} name="日常摊销" radius={[0, 0, 0, 0]} isAnimationActive animationDuration={550} />
          <Bar dataKey="discardCost" stackId="cost" fill={COLORS.discard} name="废弃补齐" radius={[3, 3, 0, 0]} isAnimationActive animationDuration={550} />
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}

export const CostChart = memo(CostChartImpl)
