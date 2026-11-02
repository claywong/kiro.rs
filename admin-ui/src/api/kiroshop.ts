/**
 * KiroShop 商城面板 API 客户端（`/api/admin/kiroshop/*`）
 *
 * 错误文案：后端把卖家错误原样透出（{"error": "..."}），此处统一解析。
 */
import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  AccountInfo, BuyOutcome, KiroProduct, KiroshopSettings, KiroshopStatus, LocalEvent,
  OrderDetailResponse, Quote, ShopOrder, TrackedOrder,
} from '@/types/kiroshop'

const api = axios.create({
  baseURL: '/api/admin/kiroshop',
  timeout: 120000,
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) config.headers['x-api-key'] = apiKey
  return config
})

/** 卖家/本模块错误统一文案 */
export function kiroshopErrorMessage(error: unknown, fallback = '操作失败'): string {
  if (!error || typeof error !== 'object') return fallback
  const resp = (error as Record<string, unknown>).response as
    | Record<string, unknown>
    | undefined
  const data = resp?.data as Record<string, unknown> | undefined
  const detail = typeof data?.error === 'string' ? data.error.trim() : ''
  if (detail) return detail
  const msg = (error as Record<string, unknown>).message
  return typeof msg === 'string' && msg ? msg : fallback
}

async function getJson<T>(path: string, params?: Record<string, unknown>): Promise<T> {
  const { data } = await api.get<T>(path, { params })
  return data
}

export async function getStatus(): Promise<KiroshopStatus> {
  return getJson('/status')
}

export async function getProducts(): Promise<KiroProduct[]> {
  const r = await getJson<{ data: KiroProduct[] }>('/products')
  return r.data ?? []
}

export interface QuoteParams {
  productId: number
  quantity: number
  optionKey?: string
  couponCode?: string
  allowBackorder?: boolean
}

export async function getQuote(p: QuoteParams): Promise<Quote> {
  const { data } = await api.post<{ data: Quote }>('/quote', {
    productId: p.productId,
    quantity: p.quantity,
    optionKey: p.optionKey || undefined,
    couponCode: p.couponCode || undefined,
    allowBackorder: p.allowBackorder ?? false,
  })
  return data.data
}

export interface BuyParams {
  productId: number
  quantity: number
  optionKey?: string
  couponCode?: string
  maxAmount?: number
  allowBackorder?: boolean
  clientRef?: string
  idempotencyKey?: string
}

export async function buy(p: BuyParams): Promise<BuyOutcome> {
  const { data } = await api.post<{ data: BuyOutcome }>('/buy', {
    productId: p.productId,
    quantity: p.quantity,
    optionKey: p.optionKey || undefined,
    couponCode: p.couponCode || undefined,
    maxAmount: p.maxAmount,
    allowBackorder: p.allowBackorder ?? false,
    clientRef: p.clientRef || undefined,
    idempotencyKey: p.idempotencyKey || undefined,
  })
  return data.data
}

export async function getOrders(): Promise<{ tracked: TrackedOrder[]; shop: ShopOrder[] }> {
  return getJson('/orders')
}

export async function getOrderDetail(id: string): Promise<OrderDetailResponse> {
  return getJson(`/orders/${encodeURIComponent(id)}`)
}

export async function payOrder(id: string): Promise<ShopOrder> {
  const { data } = await api.post<{ data: ShopOrder }>(`/orders/${encodeURIComponent(id)}/pay`)
  return data.data
}

export async function cancelOrder(id: string): Promise<void> {
  await api.post(`/orders/${encodeURIComponent(id)}/cancel`)
}

/** 手动取号 + 增量入库 */
export async function pickupOrder(id: string): Promise<BuyOutcome> {
  const { data } = await api.post<{ data: BuyOutcome }>(`/orders/${encodeURIComponent(id)}/pickup`)
  return data.data
}

/** 只做增量拿号入库（不取号） */
export async function importDelivery(id: string): Promise<{ imported: number; duplicated: number; failed: number; lastError?: string }> {
  const { data } = await api.post<{ data: { imported: number; duplicated: number; failed: number; lastError?: string } }>(
    `/orders/${encodeURIComponent(id)}/import`,
  )
  return data.data
}

export async function pickupSupplement(orderId: string, supplementId: number): Promise<BuyOutcome> {
  const { data } = await api.post<{ data: BuyOutcome }>(
    `/orders/${encodeURIComponent(orderId)}/supplements/${supplementId}/pickup`,
  )
  return data.data
}

export async function createAfterSale(orderId: string, kind: string, reason?: string): Promise<void> {
  await api.post(`/orders/${encodeURIComponent(orderId)}/after-sale`, { kind, reason })
}

export async function getLocalEvents(limit = 100, unackedOnly = false): Promise<{ data: LocalEvent[]; unacked: number }> {
  return getJson('/events', { limit, unackedOnly })
}

export async function ackLocalEvents(eventId?: number): Promise<void> {
  await api.post('/events/ack', { eventId: eventId ?? null })
}

export async function getAccount(): Promise<AccountInfo> {
  const r = await getJson<{ data: AccountInfo }>('/account')
  return r.data
}

export async function getBalanceLogs(page = 1, size = 20): Promise<Record<string, unknown>> {
  return getJson('/account/balance-logs', { page, size })
}

export async function setDailyLimit(v: number): Promise<void> {
  await api.put('/account/daily-limit', { dailyPayLimit: v })
}

export async function getWebhook(): Promise<Record<string, unknown>> {
  return getJson('/webhook')
}

export async function putWebhook(url: string, events?: string[]): Promise<Record<string, unknown>> {
  const { data } = await api.put('/webhook', { url, events, enabled: true })
  return data
}

export async function deleteWebhook(): Promise<void> {
  await api.delete('/webhook')
}

export async function testWebhook(): Promise<Record<string, unknown>> {
  const { data } = await api.post('/webhook/test')
  return data
}

export async function rotateWebhookSecret(): Promise<Record<string, unknown>> {
  const { data } = await api.post('/webhook/rotate-secret')
  return data
}

export async function getWebhookDeliveries(status?: string): Promise<Record<string, unknown>> {
  return getJson('/webhook/deliveries', { status, page: 1, size: 20 })
}

export async function retryWebhookDelivery(id: number): Promise<void> {
  await api.post(`/webhook/deliveries/${id}/retry`)
}

export async function getSettings(): Promise<KiroshopSettings> {
  const r = await getJson<{ data: KiroshopSettings }>('/settings')
  return r.data
}

/** 字段级更新：只传要改的键 */
export async function updateSettings(patch: Record<string, unknown>): Promise<void> {
  await api.put('/settings', patch)
}
