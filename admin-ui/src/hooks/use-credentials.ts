import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getCredentials,
  setCredentialDisabled,
  setCredentialPriority,
  resetCredentialFailure,
  forceRefreshToken,
  clearThrottle,
  getCredentialBalance,
  getCredentialModels,
  getCurrentCredentialModels,
  testModel,
  addCredential,
  deleteCredential,
  updateCredential,
  updateRefreshToken,
  getLoadBalancingMode,
  setLoadBalancingMode,
  getAccountThrottleConfig,
  setAccountThrottleConfig,
  getHealthGateState,
  getTrafficIngressState,
  getSelfHealConfig,
  setHealthGateEnabled,
  setTrafficIngressEnabled,
  setSelfHealConfig,
  getLogGovernanceConfig,
  setLogGovernanceConfig,
  resetSuccessCount,
  resetAllSuccessCount,
} from '@/api/credentials'
// 本地新增接口单独成行，避免上游改动同一 import 块时反复冲突。
import { getRecentSpend } from '@/api/credentials'
import type { AddCredentialRequest, UpdateCredentialRequest, UpdateRefreshTokenRequest } from '@/types/api'

// 查询凭据列表
export function useCredentials() {
  return useQuery({
    queryKey: ['credentials'],
    queryFn: getCredentials,
    refetchInterval: 30000, // 每 30 秒刷新一次
  })
}

/**
 * 查询各凭据近 1 分钟消耗的额度（credits）
 *
 * 这是个瞬时观测量，后端窗口只有 60 秒，刷新间隔取 15 秒，保证窗口内至少采到几次；
 * 不做 placeholderData，读数宁可短暂空缺也不要显示过期值。
 */
export function useRecentSpend() {
  return useQuery({
    queryKey: ['credentials', 'recent-spend'],
    queryFn: getRecentSpend,
    refetchInterval: 15_000,
    staleTime: 5_000,
    refetchOnWindowFocus: false,
  })
}

// 查询凭据余额
export function useCredentialBalance(id: number | null) {
  return useQuery({
    queryKey: ['credential-balance', id],
    queryFn: () => getCredentialBalance(id!),
    enabled: id !== null,
    retry: false, // 余额查询失败时不重试（避免重复请求被封禁的账号）
  })
}

// 查询凭据当前可用的模型列表（按需实时查询上游）
export function useCredentialModels(id: number | null) {
  return useQuery({
    queryKey: ['credential-models', id],
    queryFn: () => getCredentialModels(id!),
    enabled: id !== null,
    retry: false, // 失败不重试，避免对被封禁/异常账号反复请求
  })
}

// 使用账号池当前选中的可用凭据查询模型列表
export function useCurrentCredentialModels(enabled: boolean) {
  return useQuery({
    queryKey: ['current-credential-models'],
    queryFn: getCurrentCredentialModels,
    enabled,
    retry: false,
  })
}

// 对模型发送真实请求；credentialId 存在时定向测试该凭据
export function useTestModel() {
  return useMutation({
    mutationFn: ({
      modelId,
      credentialId,
    }: {
      modelId: string
      credentialId?: number | null
    }) => testModel(modelId, credentialId),
  })
}

// 设置禁用状态
export function useSetDisabled() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      setCredentialDisabled(id, disabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置优先级
export function useSetPriority() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, priority }: { id: number; priority: number }) =>
      setCredentialPriority(id, priority),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置失败计数
export function useResetFailure() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetCredentialFailure(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 强制刷新 Token
export function useForceRefreshToken() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => forceRefreshToken(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 解除账号级风控冷却
export function useClearThrottle() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => clearThrottle(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 添加新凭据
export function useAddCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: AddCredentialRequest) => addCredential(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 删除凭据
export function useDeleteCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteCredential(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置单个凭据的成功次数
export function useResetSuccessCount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetSuccessCount(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置所有凭据的成功次数
export function useResetAllSuccessCount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: () => resetAllSuccessCount(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 更新已禁用凭据的 refreshToken
export function useUpdateRefreshToken() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: UpdateRefreshTokenRequest }) =>
      updateRefreshToken(id, req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 更新凭据可编辑字段
export function useUpdateCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: UpdateCredentialRequest }) =>
      updateCredential(id, req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 获取负载均衡模式
export function useLoadBalancingMode() {
  return useQuery({
    queryKey: ['loadBalancingMode'],
    queryFn: getLoadBalancingMode,
  })
}

// 设置负载均衡模式
export function useSetLoadBalancingMode() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setLoadBalancingMode,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['loadBalancingMode'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      queryClient.invalidateQueries({ queryKey: ['current-credential-models'] })
    },
  })
}

// 获取账号级风控故障转移配置
export function useAccountThrottleConfig() {
  return useQuery({
    queryKey: ['accountThrottleConfig'],
    queryFn: getAccountThrottleConfig,
  })
}

// 更新账号级风控故障转移配置
export function useSetAccountThrottleConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setAccountThrottleConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['accountThrottleConfig'] })
    },
  })
}

// 获取自愈治理配置（30s 刷新以便观测 consecutiveRounds/totalCount 变化）
export function useSelfHealConfig() {
  return useQuery({
    queryKey: ['selfHealConfig'],
    queryFn: getSelfHealConfig,
    refetchInterval: 30_000,
  })
}

// 更新自愈治理配置
export function useSetSelfHealConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setSelfHealConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['selfHealConfig'] })
    },
  })
}

/**
 * 健康联动总开关状态。30s 刷新以便观测判定与已推送值的变化
 * （看门狗默认 30s 一轮，对齐它的节奏）。
 */
export function useHealthGateState() {
  return useQuery({
    queryKey: ['healthGateState'],
    queryFn: getHealthGateState,
    refetchInterval: 30_000,
  })
}

// 切健康联动总开关
export function useSetHealthGateEnabled() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setHealthGateEnabled,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['healthGateState'] })
    },
  })
}

// 独立流量入口状态；轮询用于更新异步推送结果。
export function useTrafficIngressState() {
  return useQuery({
    queryKey: ['trafficIngressState'],
    queryFn: getTrafficIngressState,
    refetchInterval: 10_000,
  })
}

export function useSetTrafficIngressEnabled() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setTrafficIngressEnabled,
    onSuccess: (state) => {
      queryClient.setQueryData(['trafficIngressState'], state)
      queryClient.invalidateQueries({ queryKey: ['trafficIngressState'] })
    },
  })
}

// 获取日志治理配置
export function useLogGovernanceConfig() {
  return useQuery({
    queryKey: ['logGovernanceConfig'],
    queryFn: getLogGovernanceConfig,
  })
}

// 更新日志治理配置
export function useSetLogGovernanceConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setLogGovernanceConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['logGovernanceConfig'] })
    },
  })
}
