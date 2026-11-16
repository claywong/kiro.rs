export interface CacheTokenUsage {
  inputTokens: number
  cacheCreationTokens?: number
  cacheReadTokens?: number
}

function validCounter(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0
}

export function hasCacheCounters(
  usage: CacheTokenUsage | undefined,
): usage is Required<CacheTokenUsage> {
  return (
    !!usage &&
    validCounter(usage.inputTokens) &&
    validCounter(usage.cacheCreationTokens) &&
    validCounter(usage.cacheReadTokens)
  )
}

/** Output tokens never enter the prompt-cache denominator. null means no usable input. */
export function cacheHitRate(
  usage: CacheTokenUsage | undefined,
): number | null {
  if (!hasCacheCounters(usage)) return null
  const prompt =
    usage.inputTokens + usage.cacheCreationTokens + usage.cacheReadTokens
  return Number.isSafeInteger(prompt) && prompt > 0
    ? (usage.cacheReadTokens / prompt) * 100
    : null
}

/** Preserve the distinction between no hits, partial hits, and a complete hit. */
export function formatCacheHitRate(rate: number | null): string {
  if (rate === null || !Number.isFinite(rate) || rate < 0 || rate > 100)
    return '—'
  if (rate > 0 && rate < 0.1) return '<0.1%'
  if (rate > 99.9 && rate < 100) return '>99.9%'
  return `${rate.toFixed(1)}%`
}

/** Aggregate token counts first, rather than averaging per-account percentages. */
export function sumCacheUsage(
  rows: readonly CacheTokenUsage[],
): Required<CacheTokenUsage> | undefined {
  const total = { inputTokens: 0, cacheCreationTokens: 0, cacheReadTokens: 0 }
  for (const row of rows) {
    if (!hasCacheCounters(row)) return undefined
    total.inputTokens += row.inputTokens
    total.cacheCreationTokens += row.cacheCreationTokens
    total.cacheReadTokens += row.cacheReadTokens
  }
  return hasCacheCounters(total) ? total : undefined
}

export interface StandardCostUsage {
  standardCostUsd?: number
  unpricedCalls?: number
}

/** undefined means the server did not report cost (older backend). */
export function sumStandardCost(
  rows: readonly StandardCostUsage[],
): { usd: number; unpricedCalls: number } | undefined {
  let usd = 0
  let unpricedCalls = 0
  for (const row of rows) {
    if (typeof row.standardCostUsd !== 'number' || !Number.isFinite(row.standardCostUsd))
      return undefined
    usd += row.standardCostUsd
    unpricedCalls += row.unpricedCalls ?? 0
  }
  return { usd, unpricedCalls }
}

export function formatUsd(usd: number | undefined): string {
  if (usd === undefined || !Number.isFinite(usd) || usd < 0) return '—'
  if (usd > 0 && usd < 0.01) return '<$0.01'
  return `$${usd.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`
}
