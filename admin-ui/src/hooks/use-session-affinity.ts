import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { getSessionAffinity, setSessionAffinity } from '@/api/session-affinity'

// 获取会话亲和配置（总开关 + 触顶处理）
export function useSessionAffinity() {
  return useQuery({
    queryKey: ['sessionAffinity'],
    queryFn: getSessionAffinity,
  })
}

// 设置会话亲和配置
export function useSetSessionAffinity() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setSessionAffinity,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['sessionAffinity'] })
    },
  })
}
