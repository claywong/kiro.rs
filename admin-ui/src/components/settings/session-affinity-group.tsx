import { useSessionAffinity, useSetSessionAffinity } from '@/hooks/use-session-affinity'
import { SettingGroup, SettingSwitch, useFieldSaver } from '@/components/console/setting-row'
import { reportSaveError } from '@/components/settings/report-error'

/**
 * 会话亲和总开关（本地新增，独立成文件以免与上游的 dispatch-section 改动冲突）。
 *
 * 关闭只是让新请求不再绑定 / 命中，现有绑定不会被清空，靠 TTL 自然过期；
 * TTL 内重新打开，旧绑定继续生效。
 */
export function SessionAffinityGroup() {
  const { data, isLoading } = useSessionAffinity()
  const { mutate } = useSetSessionAffinity()
  const saver = useFieldSaver(mutate, reportSaveError)
  const enabled = data?.enabled ?? true

  return (
    <SettingGroup
      title="会话亲和"
      description="按会话 ID 把请求粘在同一个凭据上，以提高上游 prompt cache 命中"
    >
      <SettingSwitch
        label="启用会话亲和"
        hint={
          enabled
            ? '同一会话复用首次选中的凭据，命中时 RPM 可放宽、遇 429 先在原号重试'
            : '已关闭：每个请求独立选号。现有绑定保留，TTL 内重新打开会继续生效'
        }
        checked={enabled}
        onChange={(next) => saver.save('enabled', next)}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading}
      />
    </SettingGroup>
  )
}
