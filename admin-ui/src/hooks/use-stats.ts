import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { getByCredential, getByModel, getOverview, getTimeSeries } from '@/api/stats'
import type { StatsFilter, StatsTimeFilter } from '@/types/api'

/**
 * 统计接口共用配置
 *
 * - `staleTime: 25_000`：30s 自动刷新前不再触发后台 refetch（防止跨 Tab 切换抖动）
 * - `placeholderData: keepPreviousData`：切换 range 或 tab 期间保留上次数据，
 *   chart 组件输入引用稳定 → 不会卸载重挂
 * - `refetchOnWindowFocus: false`：Admin 面板长时间挂着时减少瞬时压力
 */
const COMMON = {
  refetchInterval: 30_000,
  staleTime: 25_000,
  placeholderData: keepPreviousData,
  refetchOnWindowFocus: false,
} as const

export function useOverview() {
  return useQuery({
    queryKey: ['stats', 'overview'],
    queryFn: getOverview,
    ...COMMON,
  })
}

/**
 * 概览的近窗口健康指标（最近 1/5 分钟报错、重试）专用。
 *
 * 复用同一个 /stats/overview 接口，但刷新更快：COMMON 的 30s 间隔对"最近 1 分钟"
 * 这种窗口来说太慢（数据最多能陈旧半个窗口），这里收到 10s。queryKey 与
 * useOverview 区分开，避免两者共享缓存后互相拉高/拉低刷新频率。
 */
export function useRecentHealth() {
  return useQuery({
    queryKey: ['stats', 'overview', 'recent-health'],
    queryFn: getOverview,
    refetchInterval: 10_000,
    staleTime: 8_000,
    placeholderData: keepPreviousData,
    refetchOnWindowFocus: false,
  })
}

function timeKey(time: StatsTimeFilter) {
  return [
    time.range ?? 'custom',
    time.startDate ?? '',
    time.endDate ?? '',
    time.granularity,
  ] as const
}

export function useTimeSeries(time: StatsTimeFilter, filter?: StatsFilter) {
  return useQuery({
    queryKey: ['stats', 'timeseries', ...timeKey(time), filter?.keyId ?? 'all', filter?.group ?? 'all'],
    queryFn: () => getTimeSeries(time, filter),
    ...COMMON,
  })
}

export function useByModel(time: StatsTimeFilter, filter?: StatsFilter) {
  return useQuery({
    queryKey: ['stats', 'by-model', ...timeKey(time), filter?.keyId ?? 'all', filter?.group ?? 'all'],
    queryFn: () => getByModel(time, filter),
    ...COMMON,
  })
}

export function useByCredential(time: StatsTimeFilter, filter?: StatsFilter) {
  return useQuery({
    queryKey: ['stats', 'by-credential', ...timeKey(time), filter?.keyId ?? 'all', filter?.group ?? 'all'],
    queryFn: () => getByCredential(time, filter),
    ...COMMON,
  })
}
