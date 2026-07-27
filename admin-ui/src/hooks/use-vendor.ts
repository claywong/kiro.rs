import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getVendorStatus,
  listVendorEvents,
  listVendorOrders,
  purchaseForEvent,
  purchaseAdHoc,
  ackVendorEvents,
  redeemVendorCode,
  setVendorMode,
  testVendorWebhook,
  setVendorWebhookUrl,
} from '@/api/vendor'

/** 状态条：余额 / 库存要打卖家接口，30s 刷新一次，别太频繁 */
export function useVendorStatus() {
  return useQuery({
    queryKey: ['vendor-status'],
    queryFn: getVendorStatus,
    refetchInterval: 30000,
    staleTime: 10000,
  })
}

/** 事件列表：webhook 随时可能推过来，15s 刷新 */
export function useVendorEvents(limit = 200) {
  return useQuery({
    queryKey: ['vendor-events', limit],
    queryFn: () => listVendorEvents(limit),
    refetchInterval: 15000,
    staleTime: 5000,
  })
}

/**
 * 未确认事件数。给 tab 红点和概览页横幅用。
 *
 * 复用 vendor-events 的查询键会拖上整个列表，这里单独走 status（响应更小），
 * 且 status 本身已含 unacked。
 */
export function useVendorUnacked(): number {
  const { data } = useVendorStatus()
  return data?.unacked ?? 0
}

/** 订单历史：折叠面板展开时才拉 */
export function useVendorOrders(enabled: boolean) {
  return useQuery({
    queryKey: ['vendor-orders'],
    queryFn: listVendorOrders,
    enabled,
    staleTime: 30000,
  })
}

/** 提取后凭据池变了，凭据列表与状态条一并失效 */
function invalidateAfterPurchase(qc: ReturnType<typeof useQueryClient>) {
  qc.invalidateQueries({ queryKey: ['vendor-events'] })
  qc.invalidateQueries({ queryKey: ['vendor-status'] })
  qc.invalidateQueries({ queryKey: ['vendor-orders'] })
  qc.invalidateQueries({ queryKey: ['credentials'] })
}

export function usePurchaseForEvent() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: ({ eventId, count }: { eventId: string; count: number }) =>
      purchaseForEvent(eventId, count),
    onSuccess: () => invalidateAfterPurchase(qc),
    // 失败也要刷：后端已把失败原因和绑定数量写回事件行
    onError: () => invalidateAfterPurchase(qc),
  })
}

export function usePurchaseAdHoc() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (count: number) => purchaseAdHoc(count),
    onSuccess: () => invalidateAfterPurchase(qc),
  })
}

export function useAckVendorEvents() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (eventId?: string) => ackVendorEvents(eventId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['vendor-events'] })
      qc.invalidateQueries({ queryKey: ['vendor-status'] })
    },
  })
}

/** 切换提取模式。成功后刷 status 拿回服务端确认的值，不做乐观更新 */
export function useSetVendorMode() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (autoPurchase: boolean) => setVendorMode(autoPurchase),
    onSettled: () => qc.invalidateQueries({ queryKey: ['vendor-status'] }),
  })
}

export function useRedeemVendorCode() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (code: string) => redeemVendorCode(code),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['vendor-status'] }),
  })
}

export function useTestVendorWebhook() {
  return useMutation({ mutationFn: testVendorWebhook })
}

export function useSetVendorWebhookUrl() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (url: string) => setVendorWebhookUrl(url),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['vendor-status'] }),
  })
}
