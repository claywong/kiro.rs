import { useSessionAffinity, useSetSessionAffinity } from '@/hooks/use-session-affinity'
import {
  SettingGroup,
  SettingNumber,
  SettingSwitch,
  useFieldSaver,
} from '@/components/console/setting-row'
import { reportSaveError } from '@/components/settings/report-error'
import {
  CAP_MAX_WAIT_LIMIT_MS,
  RPM_MULTIPLIER_MAX,
  RPM_MULTIPLIER_MIN,
} from '@/api/session-affinity'

/**
 * 会话亲和总开关（本地新增，独立成文件以免与上游的 dispatch-section 改动冲突）。
 *
 * 关闭只是让新请求不再绑定 / 命中，现有绑定不会被清空，靠 TTL 自然过期；
 * TTL 内重新打开，旧绑定继续生效。
 *
 * 亲和命中达到 RPM 放宽上限（rpmLimit × 倍数）时：名额在「触顶等待上限」内释放就原地等，
 * 否则开了溢出就本次换号（主绑定不变），都不行才 429。
 */
export function SessionAffinityGroup() {
  const { data, isLoading } = useSessionAffinity()
  const { mutate } = useSetSessionAffinity()
  const saver = useFieldSaver(mutate, reportSaveError)
  const enabled = data?.enabled ?? true
  const rpmMultiplier = data?.rpmMultiplier ?? 3
  const capMaxWaitMs = data?.capMaxWaitMs ?? 3000
  const overflowEnabled = data?.overflowEnabled ?? true

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
        onChange={(next) => saver.save('enabled', { enabled: next })}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading}
      />
      <SettingNumber
        label="RPM 放宽倍数"
        hint="亲和命中时凭据可超出 RPM 限制到「限制 × 倍数」（凭据级与池级各自放宽），达到即为触顶；1 表示不放宽，可填小数"
        value={rpmMultiplier}
        onCommit={(m) => saver.save('rpmMultiplier', { rpmMultiplier: m })}
        min={RPM_MULTIPLIER_MIN}
        max={RPM_MULTIPLIER_MAX}
        unit="倍"
        presets={[1.5, 2, 3, 4]}
        pending={saver.isSaving('rpmMultiplier')}
        saved={saver.isSaved('rpmMultiplier')}
        disabled={isLoading || !enabled}
      />
      <SettingNumber
        label="触顶等待上限"
        hint="亲和凭据达到 RPM 放宽上限时，名额在该时间内释放就原地等待后重试原号（保住缓存）；0 表示不等"
        value={capMaxWaitMs}
        onCommit={(ms) => saver.save('capMaxWaitMs', { capMaxWaitMs: ms })}
        min={0}
        max={CAP_MAX_WAIT_LIMIT_MS}
        unit="毫秒"
        presets={[0, 1000, 3000, 5000]}
        pending={saver.isSaving('capMaxWaitMs')}
        saved={saver.isSaved('capMaxWaitMs')}
        disabled={isLoading || !enabled}
      />
      <SettingSwitch
        label="触顶溢出"
        hint={
          overflowEnabled
            ? '等不到名额时本次临时换到空闲凭据（优先复用该会话上次的溢出号），主绑定不变'
            : '已关闭：等不到名额直接返回 429，由客户端退避后回原号'
        }
        checked={overflowEnabled}
        onChange={(next) => saver.save('overflowEnabled', { overflowEnabled: next })}
        pending={saver.isSaving('overflowEnabled')}
        saved={saver.isSaved('overflowEnabled')}
        disabled={isLoading || !enabled}
      />
    </SettingGroup>
  )
}
