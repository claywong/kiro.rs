// 卖家提取入库的 Region 展示文案（本地扩展）。
// 独立成文件，避免往上游的 utils.ts 里塞函数。

import type { VendorStatus } from '@/types/api'

// 入库参数里 Region 一栏的展示值。
// 两个 region 通常配成同一个，相同时只显示一次；不同才拆开标注，
// 避免操作者误以为只有一个区域生效。空值显示「全局」表示沿用 config 的 region。
export function formatVendorRegion(status?: VendorStatus | null): string {
  const api = status?.defaultApiRegion?.trim() || ''
  const auth = status?.defaultAuthRegion?.trim() || ''
  if (api === auth) return api || '全局'
  return `API ${api || '全局'} / Auth ${auth || '全局'}`
}
