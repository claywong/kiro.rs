import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  VendorStatus,
  VendorEventsResponse,
  VendorModeChange,
  VendorOrdersResponse,
  VendorPurchaseResult,
  VendorRedeemResult,
  VendorListResponse,
  VendorPoolTargetChange,
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

/** 取出 axios 错误里的 HTTP 状态码 */
function errorStatus(error: unknown): number | undefined {
  if (!error || typeof error !== 'object') return undefined
  const resp = (error as Record<string, unknown>).response as
    | Record<string, unknown>
    | undefined
  return typeof resp?.status === 'number' ? resp.status : undefined
}

/** 是否被限流。后端原样透出卖家状态码，故 429 可直接判定。 */
export function isRateLimited(error: unknown): boolean {
  return errorStatus(error) === 429
}

/**
 * 卖家接口错误提示。后端把卖家的 `{"error":"..."}` 原样透出，并保留其状态码，
 * 通用的 extractErrorMessage 读不到这个形状，只会给出「Request failed with
 * status code 429」这类无信息量的文案，故单独解析。
 */
export function vendorErrorMessage(error: unknown, fallback = '操作失败'): string {
  if (!error || typeof error !== 'object') return fallback
  const resp = (error as Record<string, unknown>).response as
    | Record<string, unknown>
    | undefined
  const data = resp?.data as Record<string, unknown> | undefined
  const status = errorStatus(error)
  const detail = typeof data?.error === 'string' ? data.error.trim() : ''

  // 429 一律按限流解释：卖家对测试推送等接口有频率限制
  if (status === 429) {
    return detail ? `请求过于频繁：${detail}` : '请求过于频繁，请稍后再试'
  }
  if (detail) return detail
  if (status) return `${fallback}（HTTP ${status}）`
  const msg = (error as Record<string, unknown>).message
  return typeof msg === 'string' && msg ? msg : fallback
}

/** 卖家清单与能力集（不发任何出站请求，保证标签页在卖家不可用时也能渲染） */
export async function listVendors(): Promise<VendorListResponse> {
  const { data } = await api.get<VendorListResponse>('/vendors')
  return data
}

/** 状态条（单个卖家的余额 / 库存 / 配置状态）。vendorId 缺省时用配置里的第一家。 */
export async function getVendorStatus(vendorId?: string): Promise<VendorStatus> {
  const { data } = await api.get<VendorStatus>('/status', {
    params: vendorId ? { vendorId } : {},
  })
  return data
}

/** 事件列表（倒序）+ 未确认数 */
export async function listVendorEvents(
  limit = 200,
  vendorId?: string
): Promise<VendorEventsResponse> {
  const { data } = await api.get<VendorEventsResponse>('/events', {
    params: { limit, ...(vendorId && { vendorId }) },
  })
  return data
}

/** 卖家侧最近提取订单，用于对账 */
export async function listVendorOrders(vendorId?: string): Promise<VendorOrdersResponse> {
  const { data } = await api.get<VendorOrdersResponse>('/orders', {
    params: vendorId ? { vendorId } : {},
  })
  return data
}

/**
 * 按事件提取并入库。
 *
 * `count` 为本次希望提取的数量；若该事件此前已绑定过其它数量，返回 409 并带
 * `boundCount`，调用方提示用户按该值重试。
 */
export async function purchaseForEvent(
  eventId: string,
  count: number,
  vendorId?: string
): Promise<VendorPurchaseResult> {
  const { data } = await api.post<VendorPurchaseResult>(
    `/events/${eventId}/purchase`,
    { count, vendorId },
    { params: vendorId ? { vendorId } : {} }
  )
  return data
}

/** 不依赖事件的主动提取（自行生成或指定订单号） */
export async function purchaseAdHoc(
  count: number,
  clientOrderId?: string,
  vendorId?: string
): Promise<VendorPurchaseResult> {
  const { data } = await api.post<VendorPurchaseResult>(
    '/purchase',
    { count, clientOrderId, vendorId },
    { params: vendorId ? { vendorId } : {} }
  )
  return data
}

/**
 * 标记事件已知悉（消红点）。
 *
 * - 指定 eventId：该卖家的单条标记
 * - eventId 为空：该卖家全部标记
 * - vendorId 也为空：所有卖家全部标记
 */
export async function ackVendorEvents(
  eventId?: string,
  vendorId?: string
): Promise<{ acked: number }> {
  const { data } = await api.post<{ acked: number }>('/events/ack', {
    eventId,
    vendorId,
  })
  return data
}

/** 兑换码充值。`replayed=true` 表示这张码此前已兑换，本次未改动余额。 */
export async function redeemVendorCode(
  code: string,
  vendorId?: string
): Promise<VendorRedeemResult> {
  const { data } = await api.post<VendorRedeemResult>(
    '/redeem',
    { code, vendorId },
    { params: vendorId ? { vendorId } : {} }
  )
  return data
}

/** 切换提取模式 */
export async function setVendorMode(
  autoPurchase: boolean,
  vendorId?: string
): Promise<VendorModeChange> {
  const { data } = await api.put<VendorModeChange>(
    '/mode',
    { autoPurchase, vendorId },
    { params: vendorId ? { vendorId } : {} }
  )
  return data
}

/**
 * 设置全局提取限制。不带 vendorId —— 阈值跨供应商共享，
 * 后端也不按家分发这个请求。
 */
export async function setVendorPoolTarget(
  poolTarget: number
): Promise<VendorPoolTargetChange> {
  const { data } = await api.put<VendorPoolTargetChange>('/pool-target', {
    poolTarget,
  })
  return data
}

/** 让卖家往已保存的 URL 推一条测试消息 */
export async function testVendorWebhook(
  vendorId?: string
): Promise<Record<string, unknown>> {
  const { data } = await api.post<Record<string, unknown>>(
    '/webhook/test',
    {},
    { params: vendorId ? { vendorId } : {} }
  )
  return data
}

/** 把本机 webhook 地址写到卖家侧 */
export async function setVendorWebhookUrl(
  webhookUrl: string,
  vendorId?: string
): Promise<{ ok: boolean }> {
  const { data } = await api.put<{ ok: boolean }>(
    '/webhook',
    { webhookUrl, vendorId },
    { params: vendorId ? { vendorId } : {} }
  )
  return data
}
