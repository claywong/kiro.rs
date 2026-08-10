import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  listVendors,
  getVendorStatus,
  listVendorEvents,
  listVendorOrders,
  purchaseForEvent,
  purchaseAdHoc,
  ackVendorEvents,
  redeemVendorCode,
  setVendorAutoPurchaseEnabled,
  setVendorMode,
  setVendorPerChannel,
  setStockPollRespectGate,
  setVendorPoolTarget,
  testVendorWebhook,
  setVendorWebhookUrl,
} from '@/api/vendor'

/**
 * 卖家清单。首次进页面拉取，后续不主动刷新（卖家列表运行期不变）。
 *
 * 例外：响应里的 `poolTarget` 与 `autoPurchaseEnabled` 是可变的全局设置，改动后由
 * [`useSetVendorPoolTarget`] / [`useSetVendorAutoPurchaseEnabled`] 显式
 * invalidate 本 key 拉新值。
 */
export function useVendorList() {
  return useQuery({
    queryKey: ['vendor-list'],
    queryFn: listVendors,
    staleTime: Infinity,
  })
}

/** 状态条：余额 / 库存要打卖家接口，30s 刷新一次，别太频繁 */
export function useVendorStatus(vendorId?: string) {
  return useQuery({
    queryKey: ['vendor-status', vendorId],
    queryFn: () => getVendorStatus(vendorId),
    refetchInterval: 30000,
    staleTime: 10000,
  })
}

/** 事件列表：webhook 随时可能推过来，15s 刷新 */
export function useVendorEvents(limit = 200, vendorId?: string) {
  return useQuery({
    queryKey: ['vendor-events', limit, vendorId],
    queryFn: () => listVendorEvents(limit, vendorId),
    refetchInterval: 15000,
    staleTime: 5000,
  })
}

/**
 * 未确认事件数。给 tab 红点和概览页横幅用。
 *
 * 复用 events 接口而不单独读库，避免两边数据不一致导致红点与列表对不上。
 * 传 limit=1 就能拿到 unacked 计数且不拉完整列表。
 */
export function useVendorUnackedCount(vendorId?: string) {
  return useQuery({
    queryKey: ['vendor-events', 1, vendorId],
    queryFn: () => listVendorEvents(1, vendorId),
    select: (data) => data.unacked,
    refetchInterval: 30000,
    staleTime: 10000,
  })
}

/** 卖家侧订单列表（对账用） */
export function useVendorOrders(vendorId?: string) {
  return useQuery({
    queryKey: ['vendor-orders', vendorId],
    queryFn: () => listVendorOrders(vendorId),
    staleTime: 60000,
  })
}

/** 按事件提取。成功后刷 events（追加处理状态）和 status（余额 / 库存变化）。 */
export function usePurchaseForEvent(vendorId?: string) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: ({ eventId, count, zone }: { eventId: string; count: number; zone?: string }) =>
      purchaseForEvent(eventId, count, vendorId, zone),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['vendor-events', undefined, vendorId] })
      qc.invalidateQueries({ queryKey: ['vendor-status', vendorId] })
    },
  })
}

/** 直接提取（不依赖事件）。成功后只刷 status。 */
export function usePurchaseAdHoc(vendorId?: string) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: ({ count, clientOrderId, zone }: { count: number; clientOrderId?: string; zone?: string }) =>
      purchaseAdHoc(count, clientOrderId, vendorId, zone),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['vendor-status', vendorId] }),
  })
}

/** 确认事件已知悉。成功后刷 events 拿回新的 unacked。 */
export function useAckVendorEvents(vendorId?: string) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (eventId?: string) => ackVendorEvents(eventId, vendorId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['vendor-events'] })
    },
  })
}

/** 切换提取模式。成功后刷 status 拿回服务端确认的值，不做乐观更新 */
export function useSetVendorMode(vendorId?: string) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (autoPurchase: boolean) => setVendorMode(autoPurchase, vendorId),
    onSettled: () => qc.invalidateQueries({ queryKey: ['vendor-status', vendorId] }),
  })
}

/**
 * 设置全局提取限制。
 *
 * 用 `onSettled` 而非 `onSuccess`：持久化失败时后端仍返回 200（运行时已生效，
 * 只是重启会回退），失败分支也该把服务端的实际值拉回来。
 */
export function useSetVendorPoolTarget() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (poolTarget: number) => setVendorPoolTarget(poolTarget),
    onSettled: () => qc.invalidateQueries({ queryKey: ['vendor-list'] }),
  })
}

/**
 * 切换自动提取总闸。与 [`useSetVendorPoolTarget`] 同样用 `onSettled` —— 持久化
 * 失败时后端仍返回 200，失败分支也该把服务端实际值拉回来。
 *
 * 只刷 `vendor-list`：总闸不进各家的 `/status`，状态条上没有它。
 */
export function useSetVendorAutoPurchaseEnabled() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (enabled: boolean) => setVendorAutoPurchaseEnabled(enabled),
    onSettled: () => qc.invalidateQueries({ queryKey: ['vendor-list'] }),
  })
}

/**
 * 设置某一家的逐渠道补货。用 `onSettled` 而非 `onSuccess`：持久化失败时后端
 * 仍返回 200（运行时已生效，只是重启会回退），失败分支也该把实际值拉回来。
 *
 * 同时刷 status 与 list：状态条读 status，标签页那层读 list。
 */
export function useSetVendorPerChannel(vendorId?: string) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (perChannel: boolean) => setVendorPerChannel(perChannel, vendorId),
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ['vendor-status', vendorId] })
      qc.invalidateQueries({ queryKey: ['vendor-list'] })
    },
  })
}

export function useSetStockPollRespectGate(vendorId?: string) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (respect: boolean) => setStockPollRespectGate(respect, vendorId),
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ['vendor-status', vendorId] })
      qc.invalidateQueries({ queryKey: ['vendor-list'] })
    },
  })
}

export function useRedeemVendorCode(vendorId?: string) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (code: string) => redeemVendorCode(code, vendorId),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['vendor-status', vendorId] }),
  })
}

export function useTestVendorWebhook(vendorId?: string) {
  return useMutation({ mutationFn: () => testVendorWebhook(vendorId) })
}

export function useSetVendorWebhookUrl(vendorId?: string) {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (url: string) => setVendorWebhookUrl(url, vendorId),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['vendor-status', vendorId] }),
  })
}

