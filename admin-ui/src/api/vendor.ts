import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  VendorStatus,
  VendorEventsResponse,
  VendorOrdersResponse,
  VendorPurchaseResult,
  VendorRedeemResult,
} from '@/types/api'

const api = axios.create({
  baseURL: '/api/admin/vendor',
  // 提取 Key 需卖家现场生成 + 本地逐条验活，给足时间
  timeout: 180000,
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) config.headers['x-api-key'] = apiKey
  return config
})

/** 顶部状态条：配置状态 + 余额 + 本轮可提取量 + 未确认事件数 */
export async function getVendorStatus(): Promise<VendorStatus> {
  const { data } = await api.get<VendorStatus>('/status')
  return data
}

/** 事件列表（倒序）+ 未确认数 */
export async function listVendorEvents(limit = 200): Promise<VendorEventsResponse> {
  const { data } = await api.get<VendorEventsResponse>('/events', { params: { limit } })
  return data
}

/** 卖家侧最近 50 条提取订单，用于对账 */
export async function listVendorOrders(): Promise<VendorOrdersResponse> {
  const { data } = await api.get<VendorOrdersResponse>('/orders')
  return data
}

/**
 * 按事件提取并入库。
 *
 * 数量一旦提交就与该订单号永久绑定：卖家侧对「同订单号 + 同 count」幂等重放，
 * 换数量会返回 409。若事件已绑定过其它数量，后端直接返回 409 且带 boundCount，
 * 不会白撞一次卖家接口。
 */
export async function purchaseForEvent(
  eventId: string,
  count: number,
): Promise<VendorPurchaseResult> {
  const { data } = await api.post<VendorPurchaseResult>(
    `/events/${encodeURIComponent(eventId)}/purchase`,
    { count },
  )
  return data
}

/** 不依赖事件的直接提取（服务端生成订单号）。会真实扣费，调用前需二次确认。 */
export async function purchaseAdHoc(count: number): Promise<VendorPurchaseResult> {
  const { data } = await api.post<VendorPurchaseResult>('/purchase', { count })
  return data
}

/** 标记事件已知悉（消红点）。不传 eventId 表示全部标记。 */
export async function ackVendorEvents(eventId?: string): Promise<{ acked: number }> {
  const { data } = await api.post<{ acked: number }>('/events/ack', {
    event_id: eventId,
  })
  return data
}

/** 兑换码充值。`replayed=true` 表示这张码此前已兑换，本次未改动余额。 */
export async function redeemVendorCode(code: string): Promise<VendorRedeemResult> {
  const { data } = await api.post<VendorRedeemResult>('/redeem', { code })
  return data
}

/** 让卖家往已保存的 URL 推一条测试消息 */
export async function testVendorWebhook(): Promise<Record<string, unknown>> {
  const { data } = await api.post<Record<string, unknown>>('/webhook/test')
  return data
}

/** 把本机 webhook 地址写到卖家侧 */
export async function setVendorWebhookUrl(webhookUrl: string): Promise<{ ok: boolean }> {
  const { data } = await api.put<{ ok: boolean }>('/webhook', { webhookUrl })
  return data
}
