import axios from 'axios'
import { storage } from '@/lib/storage'

// 本地新增：会话亲和总开关与触顶处理。独立成文件，避免与上游改动 credentials.ts 时冲突。
const api = axios.create({
  baseURL: '/api/admin/config/session-affinity',
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) config.headers['x-api-key'] = apiKey
  return config
})

export interface SessionAffinityState {
  enabled: boolean
  /** 亲和命中时 RPM 允许突破到的倍数（凭据级 rpmLimit 与池级 accountRpmLimit 各自放宽） */
  rpmMultiplier: number
  /** 亲和触顶时服务端等待名额释放的上限（毫秒），0 = 不等 */
  capMaxWaitMs: number
  /** 亲和触顶且无法短等时是否溢出到其它凭据 */
  overflowEnabled: boolean
}

/** 与后端 SESSION_AFFINITY_CAP_MAX_WAIT_LIMIT_MS 保持一致 */
export const CAP_MAX_WAIT_LIMIT_MS = 60000
/** 与后端 SESSION_AFFINITY_RPM_MULTIPLIER_RANGE 保持一致 */
export const RPM_MULTIPLIER_MIN = 1
export const RPM_MULTIPLIER_MAX = 10

// 获取会话亲和总开关
export async function getSessionAffinity(): Promise<SessionAffinityState> {
  const { data } = await api.get<SessionAffinityState>('')
  return data
}

// 设置会话亲和（各字段可选，缺省表示不变）
export async function setSessionAffinity(
  patch: Partial<SessionAffinityState>,
): Promise<SessionAffinityState> {
  const { data } = await api.put<SessionAffinityState>('', patch)
  return data
}
