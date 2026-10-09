import axios from 'axios'
import { storage } from '@/lib/storage'

// 本地新增：会话亲和总开关。独立成文件，避免与上游改动 credentials.ts 时冲突。
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
}

// 获取会话亲和总开关
export async function getSessionAffinity(): Promise<SessionAffinityState> {
  const { data } = await api.get<SessionAffinityState>('')
  return data
}

// 设置会话亲和总开关
export async function setSessionAffinity(enabled: boolean): Promise<SessionAffinityState> {
  const { data } = await api.put<SessionAffinityState>('', { enabled })
  return data
}
