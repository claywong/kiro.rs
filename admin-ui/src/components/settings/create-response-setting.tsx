import { Loader2 } from 'lucide-react'
import { toast } from 'sonner'
import type { CreateResponseMode } from '@/api/credentials'
import { SettingGroup } from '@/components/console/setting-row'
import { Button } from '@/components/ui/button'
import { useCreateResponseMode, useSetCreateResponseMode } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

const OPTIONS: { value: CreateResponseMode; label: string }[] = [
  { value: 'auto', label: 'auto（推荐）' },
  { value: 'always', label: 'always' },
  { value: 'off', label: 'off' },
]

export function CreateResponseSetting() {
  const query = useCreateResponseMode()
  const mutation = useSetCreateResponseMode()
  const mode = query.data?.mode
  const disabled = mutation.isPending || query.isError || !mode

  const changeMode = (next: CreateResponseMode) => {
    if (disabled || next === mode) return
    mutation.mutate(next, {
      onSuccess: () => toast.success(`CreateResponse 模式已切换为 ${next}，对新请求生效`),
    })
  }

  return (
    <SettingGroup
      title="CreateResponse 接口"
      description="官方 Kiro IDE 的主聊天已改走 CreateResponse（runtime.<region>.kiro.dev/v1/responses），旧的 generateAssistantResponse 会被上游单独限流（429 USER_REQUEST_RATE_EXCEEDED），与账号额度无关。保存后对新请求立即生效，无需重启。"
    >
      <div className="space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <p className="text-sm" role="status">
            {query.isError ? '当前模式读取失败' : mode ? <>当前生效：<code>{mode}</code></> : '正在读取当前模式…'}
          </p>
          <div className="flex flex-wrap gap-2" role="group" aria-label="选择 CreateResponse 模式">
            {OPTIONS.map((option) => (
              <Button
                key={option.value}
                type="button"
                size="sm"
                variant={mode === option.value ? 'default' : 'outline'}
                aria-pressed={mode === option.value}
                disabled={disabled}
                onClick={() => changeMode(option.value)}
              >
                {option.label}
              </Button>
            ))}
          </div>
        </div>
        <div className="space-y-1 text-xs leading-relaxed text-muted-foreground">
          <p><code>auto</code>：先走旧接口，仅当返回 429 USER_REQUEST_RATE_EXCEEDED 时在同一账号上回退到 CreateResponse；回退失败再按原有逻辑重试或换号。</p>
          <p><code>always</code>：聊天请求一律走 CreateResponse，不再先试旧接口，被限流的账号少一次无效往返。</p>
          <p><code>off</code>：只用旧接口，行为与升级前一致。</p>
          <p>该开关同样作用于「模型测试」。CreateResponse 对同一账号有并发窗口，连发时偶尔会 429，网关会等待 1.5 秒重试一次。</p>
        </div>
        {mutation.isPending && (
          <p className="flex items-center gap-2 text-xs text-muted-foreground" role="status">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />正在保存…
          </p>
        )}
        {query.isError && (
          <div className="flex flex-wrap items-center gap-2 text-sm text-destructive" role="alert">
            <span>读取失败：{extractErrorMessage(query.error)}</span>
            <Button type="button" size="sm" variant="outline" disabled={query.isFetching} onClick={() => void query.refetch()}>重试</Button>
          </div>
        )}
        {mutation.isError && (
          <p className="text-sm text-destructive" role="alert">保存失败：{extractErrorMessage(mutation.error)}。请重试；如连接中断，请刷新确认服务器当前模式。</p>
        )}
      </div>
    </SettingGroup>
  )
}
