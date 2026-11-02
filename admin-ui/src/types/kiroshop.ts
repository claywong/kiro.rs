/**
 * KiroShop（商城）面板类型。
 *
 * 注意字段命名两套并存：
 * - 直接透传卖家 API 的 DTO（Product / ShopOrder / AccountInfo / Quote 等）是
 *   **snake_case**，与 kiro.yonghu.shop 的 openapi 一致；
 * - 本模块自己产生的结构（TrackedOrder / BuyOutcome / LocalEvent / KiroshopSettings）
 *   是 camelCase，与其余面板代码一致。
 */

// ---- 卖家 DTO（snake_case） ----

export interface ProductOption {
  key: string
  label?: string | null
  price?: number | null
  warranty_hours?: number | null
  warranty_mode?: string | null
}

export interface ProductStock {
  has_stock?: boolean | null
  backorder?: boolean | null
  eta?: string | null
  in_stock?: boolean | null
  stock_level?: string | null
  available?: number | null
}

export interface KiroProduct {
  id: number
  display_name: string
  price?: number | null
  min_quantity_per_order?: number | null
  max_quantity_per_order?: number | null
  warranty_mode?: string | null
  warranty_hours?: number | null
  options: ProductOption[]
  stock: ProductStock
}

export interface ShopOrder {
  id: string
  product_id: number
  product_name: string
  amount?: number | null
  quantity?: number | null
  unit_price?: number | null
  selected_option_key?: string | null
  status?: string | null
  status_text?: string | null
  after_sale_status?: string | null
  delivered?: number | null
  lane?: string | null
  client_ref?: string | null
  created_at?: string | null
  paid_at?: string | null
  eta?: string | null
  returned?: boolean | null
  payment?: unknown
  delivery?: unknown
}

export interface AccountInfo {
  balance?: number | null
  api_spent_today?: number | null
  daily_pay_limit?: number | null
  daily_pay_remaining?: number | null
  key_can_pay?: boolean | null
  max_pending_orders?: number | null
  pending_orders?: number | null
  customer_id?: number | null
  webhook?: { enabled?: boolean | null; url?: string | null } | null
}

export interface Quote {
  product_id: number
  option_key?: string | null
  quantity?: number | null
  unit_price?: number | null
  original_amount?: number | null
  amount?: number | null
  discount_amount?: number | null
  warranty_hours?: number | null
  lane?: string | null
  available?: number | null
  in_stock_now?: number | null
  backorder?: boolean | null
  eta?: string | null
  balance?: number | null
  balance_enough?: boolean | null
}

// ---- 本模块结构（camelCase） ----

export interface TrackedOrder {
  orderId: string
  productId: number
  productName: string
  quantity: number
  amount?: number | null
  status: string
  statusText: string
  eta?: string | null
  lane: string
  source: string
  clientRef?: string | null
  lastSeq: number
  delivered: number
  imported: number
  failedImports: number
  lastError?: string | null
  createdAt?: string | null
  updatedAt?: string | null
}

export interface BuyOutcome {
  orderId: string
  amount?: number | null
  quantity: number
  status: string
  statusText?: string | null
  imported: number
  duplicated: number
  failed: number
  lastError?: string | null
}

export interface LocalEvent {
  id: number
  kind: string
  severity: string
  message: string
  orderId?: string | null
  acked: boolean
  createdAt: string
}

export interface AutoBuyConfig {
  enabled: boolean
  poolTarget: number
  maxUnitPrice?: number | null
  maxPerOrder: number
  dailyBudget?: number | null
  allowBackorder: boolean
}

export interface KiroshopSettings {
  enabled: boolean
  baseUrl: string
  apiKeyMasked: string
  apiKeySet: boolean
  groups: string[]
  rpmLimit: number
  priority: number
  creditLimit?: number | null
  apiRegion: string
  eventsPollIntervalSecs: number
  deliveryPollIntervalSecs: number
  balanceAlertThreshold?: number | null
  autoBuy: AutoBuyConfig
  webhookPath: string
  webhookSecretSet: boolean
}

export interface KiroshopStatus {
  configured: boolean
  enabled?: boolean
  baseUrl?: string
  account?: AccountInfo | null
  products?: { id: number; displayName: string; price?: number | null; available: number; backorder?: boolean | null }[]
  pool?: { count: number; inflight: number; target: number }
  autoBuy?: AutoBuyConfig
  unackedAlerts?: number
  trackedCount?: number
  lastError?: string | null
  webhookPath?: string
}

export interface OrderDetailResponse {
  order: ShopOrder & { eta?: string | null; returned?: boolean | null }
  supplements: Record<string, unknown>[]
  afterSale: Record<string, unknown>[]
  tracked?: TrackedOrder | null
}

/** 后端错误体：{"error": "...", "code": "..."} */
export interface KiroshopApiError {
  error: string
  code?: string
}
