import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { getSessionAffinity, setSessionAffinity } from '@/api/session-affinity'

// 获取会话亲和总开关
export function useSessionAffinity() {
  return useQuery({
    queryKey: ['sessionAffinity'],
    queryFn: getSessionAffinity,
  })
}

// 设置会话亲和总开关
export function useSetSessionAffinity() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setSessionAffinity,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['sessionAffinity'] })
    },
  })
}
