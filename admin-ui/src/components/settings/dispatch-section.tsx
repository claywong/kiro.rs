import {
  useAccountThrottleConfig,
  useSetAccountThrottleConfig,
  useAccountRpmLimitConfig,
  useSetAccountRpmLimitConfig,
  useRateLimitSameCredentialConfig,
  useSetRateLimitSameCredentialConfig,
  useLoadBalancingMode,
  useSetLoadBalancingMode,
  useSelfHealConfig,
  useSetSelfHealConfig,
  useHealthGateState,
  useSetHealthGateEnabled,
  useTrafficIngressState,
  useSetTrafficIngressEnabled,
  useConcurrencyGateState,
  useSetConcurrencyGateConfig,
} from '@/hooks/use-credentials'
import {
  SettingGroup,
  SettingNumber,
  SettingReadout,
  SettingSegments,
  SettingSwitch,
  useFieldSaver,
} from '@/components/console/setting-row'
import { reportSaveError } from '@/components/settings/report-error'

const SECS_PER_MIN = 60
// 与后端 SetAccountRpmLimitConfigRequest 的校验区间保持一致
const MIN_RPM_LIMIT = 1
const MAX_RPM_LIMIT = 100000

/**
 * 调度分区：凭据怎么选、失败怎么转、禁用怎么恢复。
 *
 * 三组配置放在一屏里是有意的 —— 它们互相牵制，分开看容易配出自相矛盾的组合。
 * 最典型的是「自愈开着 + 冷却间隔为 0」：403 持续时会陷入 全禁 → 自愈 → 403 → 再禁
 * 的死循环（0.7.4 修的就是这个）。摆在一起，间隔和上限这两个刹车就跟自愈开关同时在视野里。
 */
export function DispatchSection() {
  return (
    <div className="space-y-6">
      <LoadBalancingGroup />
      <ThrottleGroup />
      <SameCredentialRetryGroup />
      <RpmLimitGroup />
      <SelfHealGroup />
      <TrafficIngressGroup />
      <HealthGateGroup />
      <ConcurrencyGateGroup />
    </div>
  )
}

function appliedText(value: boolean | null | undefined) {
  if (value == null) return '尚未同步'
  return value ? '可调度' : '不可调度'
}

function hostOf(url: string | undefined) {
  if (!url) return '未配置'
  try {
    return new URL(url).host
  } catch {
    return url
  }
}

function LoadBalancingGroup() {
  const { data, isLoading } = useLoadBalancingMode()
  const { mutate } = useSetLoadBalancingMode()
  const saver = useFieldSaver(mutate, reportSaveError)

  return (
    <SettingGroup title="凭据选择">
      <SettingSegments
        label="负载均衡模式"
        hint={
          data?.mode === 'balanced'
            ? '按用量动态挑选凭据，把请求摊平到整个池子'
            : '按优先级数字从小到大用：先用完 0 号，再换 1 号'
        }
        value={data?.mode ?? 'priority'}
        options={[
          { value: 'priority', label: '按优先级', hint: '小数字先用，顺序耗尽' },
          { value: 'balanced', label: '均衡负载', hint: '按用量动态摊平' },
        ]}
        onChange={(next) => saver.save('mode', next)}
        pending={saver.isSaving('mode')}
        saved={saver.isSaved('mode')}
        disabled={isLoading}
      />
    </SettingGroup>
  )
}

function ThrottleGroup() {
  const { data, isLoading } = useAccountThrottleConfig()
  const { mutate } = useSetAccountThrottleConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const failover = data?.failover ?? true
  const cooldownSecs = data?.cooldownSecs ?? 30 * SECS_PER_MIN

  return (
    <SettingGroup
      title="账号级风控"
      description="上游对单个账号触发临时限速（429 + suspicious activity）时怎么处理"
    >
      <SettingSwitch
        label="故障转移"
        hint={
          failover
            ? '冷却该凭据并立即切到下一个可用凭据'
            : '仅按瞬态错误重试，不切换凭据'
        }
        checked={failover}
        onChange={(next) => saver.save('failover', { failover: next })}
        pending={saver.isSaving('failover')}
        saved={saver.isSaved('failover')}
        disabled={isLoading}
      />
      <SettingNumber
        label="冷却时长"
        hint="被风控的凭据要静默多久才重新参与调度"
        value={cooldownSecs}
        toDisplay={(secs) => Math.round(secs / SECS_PER_MIN)}
        fromDisplay={(min) => min * SECS_PER_MIN}
        onCommit={(secs) => saver.save('cooldown', { cooldownSecs: secs })}
        min={1}
        max={1440}
        unit="分钟"
        presets={[5, 15, 30, 60]}
        pending={saver.isSaving('cooldown')}
        saved={saver.isSaved('cooldown')}
        disabled={isLoading || !failover}
      />
    </SettingGroup>
  )
}

/** 账号类型键 → 中文标签，与后端 metadata schema 的 oneOf 标题一致 */
const CREDENTIAL_TYPE_LABELS: Record<string, string> = {
  normal: '正常号',
  boom: '炸弹号',
  long_speed: '长速刷',
  short_speed: '短速刷',
}

/**
 * 用户级 429「原号重试」。
 *
 * 放在「账号级风控」之后、「主动限流」之前，是按限流处置的时间顺序排的：
 * 撞上用户级 429 之后先决定要不要原地再试（这里），再决定换号还是冷却。
 *
 * 注意与上一组的区别：这里只管用户级限流（USER_REQUEST_RATE_EXCEEDED），
 * 不影响账号级风控（suspicious activity）—— 后者原地重试只会延长风控。
 */
function SameCredentialRetryGroup() {
  const { data, isLoading } = useRateLimitSameCredentialConfig()
  const { mutate } = useSetRateLimitSameCredentialConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const retries = data?.retries ?? {}
  const knownTypes = data?.knownTypes ?? []
  const maxPerType = data?.maxRetriesPerType ?? 10
  const retryDelayMs = data?.retryDelayMs ?? 200
  const anyEnabled = Object.values(retries).some((n) => n > 0)

  // retries 是整表替换语义：改一个类型要把整表带上，否则其余类型会被清零。
  const saveType = (type: string, next: number) =>
    saver.save(type, { retries: { ...retries, [type]: next } })

  return (
    <SettingGroup
      title="原号重试（用户级 429）"
      description="撞上用户级限流时，先在同一个账号上重试几次再换号。速刷号配额恢复快，原地等一下通常比换号划算。0 = 立即换号"
    >
      {knownTypes.map((type) => (
        <SettingNumber
          key={type}
          label={CREDENTIAL_TYPE_LABELS[type] ?? type}
          hint={
            (retries[type] ?? 0) > 0
              ? `限流后在原号上最多再试 ${retries[type]} 次，用尽才换号`
              : '限流后立即换号，不在原号上重试'
          }
          value={retries[type] ?? 0}
          onCommit={(next) => saveType(type, next)}
          min={0}
          max={maxPerType}
          unit="次"
          presets={[0, 1, 2, 3]}
          pending={saver.isSaving(type)}
          saved={saver.isSaved(type)}
          disabled={isLoading}
        />
      ))}
      <SettingNumber
        label="重试间隔"
        hint="原号两次重试之间固定等待这么久；间隔太短等于没重试，太长不如换号"
        value={retryDelayMs}
        onCommit={(next) => saver.save('delay', { retryDelayMs: next })}
        min={50}
        max={60000}
        unit="毫秒"
        presets={[100, 200, 500, 1000]}
        pending={saver.isSaving('delay')}
        saved={saver.isSaved('delay')}
        disabled={isLoading || !anyEnabled}
      />
    </SettingGroup>
  )
}

/**
 * 单账号 RPM 主动限流。
 *
 * 紧跟「账号级风控」是有意的：两者都是账号级限速，区别只在谁先动手 ——
 * 风控是上游 429 之后的被动补救，这里是我们自己先掐住不让它撞上去。
 * 摆在一起，配了主动限流还在等风控兜底这种误解就不容易发生。
 */
function RpmLimitGroup() {
  const { data, isLoading } = useAccountRpmLimitConfig()
  const { mutate } = useSetAccountRpmLimitConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const enabled = data?.enabled ?? false
  const limit = data?.limit ?? 60

  return (
    <SettingGroup
      title="单账号限流"
      description="主动掐住单个账号的每分钟请求数，别等上游风控才反应"
    >
      <SettingSwitch
        label="启用 RPM 限流"
        hint={
          enabled
            ? '每个凭据独立计 60 秒滑动窗口，超限的临时跳过并切到下一个可用凭据'
            : '关闭时不计数、不影响调度'
        }
        checked={enabled}
        onChange={(next) => saver.save('enabled', { enabled: next })}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading}
      />
      <SettingNumber
        label="每分钟上限"
        hint="单个凭据 60 秒内最多放行多少请求。所有凭据都超限时请求返回 429"
        value={limit}
        onCommit={(n) => saver.save('limit', { limit: n })}
        min={MIN_RPM_LIMIT}
        max={MAX_RPM_LIMIT}
        unit="次/分钟"
        presets={[10, 30, 60, 120, 300]}
        pending={saver.isSaving('limit')}
        saved={saver.isSaved('limit')}
        disabled={isLoading || !enabled}
      />
    </SettingGroup>
  )
}

function SelfHealGroup() {
  const { data, isLoading } = useSelfHealConfig()
  const { mutate } = useSetSelfHealConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const enabled = data?.enabled ?? true

  return (
    <SettingGroup
      title="凭据自愈"
      description="请求池全灭时自动把禁用的凭据放回来重试"
    >
      <SettingSwitch
        label="启用自愈"
        hint="当前作用域内已无可用凭据时，按作用域批量恢复被禁用的凭据"
        checked={enabled}
        onChange={(next) => saver.save('enabled', { enabled: next })}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading}
      />
      <SettingSwitch
        label="403 封禁识别"
        hint="命中封禁文案的 403 直接禁用且不参与自愈，避免为已封账号反复重试"
        checked={data?.suspendedDetectionEnabled ?? true}
        onChange={(next) =>
          saver.save('suspended', { suspendedDetectionEnabled: next })
        }
        pending={saver.isSaving('suspended')}
        saved={saver.isSaved('suspended')}
        disabled={isLoading}
      />
      <SettingNumber
        label="自愈冷却间隔"
        hint="两次自愈之间的最小间隔。设 0 表示不冷却 —— 上游持续 403 时这是唯一能打断死循环的刹车，不建议设 0"
        value={data?.minIntervalSecs ?? 0}
        toDisplay={(secs) => Math.round(secs / SECS_PER_MIN)}
        fromDisplay={(min) => min * SECS_PER_MIN}
        onCommit={(secs) => saver.save('interval', { minIntervalSecs: secs })}
        min={0}
        max={1440}
        unit="分钟"
        presets={[0, 1, 5, 15]}
        pending={saver.isSaving('interval')}
        saved={saver.isSaved('interval')}
        disabled={isLoading || !enabled}
      />
      <SettingNumber
        label="连续自愈上限"
        hint="连续自愈达到该轮数且期间无任何成功请求则停止自愈。0 = 不限"
        value={data?.maxConsecutiveRounds ?? 5}
        onCommit={(n) => saver.save('rounds', { maxConsecutiveRounds: n })}
        min={0}
        max={1000}
        unit="轮"
        pending={saver.isSaving('rounds')}
        saved={saver.isSaved('rounds')}
        disabled={isLoading || !enabled}
      />
      <SettingReadout
        label="运行状态"
        hint="当前连续自愈轮数 / 累计恢复凭据次数"
      >
        连续 {data?.consecutiveRounds ?? 0} 轮 · 累计恢复 {data?.totalCount ?? 0} 次
      </SettingReadout>
    </SettingGroup>
  )
}

function TrafficIngressGroup() {
  const { data, isLoading } = useTrafficIngressState()
  const { mutate } = useSetTrafficIngressEnabled()
  const saver = useFieldSaver(mutate, reportSaveError)
  const configured = data?.configured ?? false
  const enabled = data?.enabled ?? false

  return (
    <SettingGroup
      title="流量入口"
      description="独立控制外部账号是否可调度，并受本地 RPM 容量判据约束"
    >
      <SettingSwitch
        label="启用流量入口"
        hint={
          configured
            ? enabled && data?.rpmOk === false
              ? '入口已开启，但当前 RPM 容量不足，外部账号会保持不可调度'
              : '切换后异步同步到受控外部账号'
            : '需先在 config.json 配置 trafficIngress 的地址、令牌和账号'
        }
        checked={enabled}
        onChange={(next) => saver.save('enabled', next)}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading || !configured}
      />
      <SettingReadout label="运行状态" hint="目标系统 / 账号数 / 最近一次成功同步值">
        {hostOf(data?.baseUrl)} · {data?.accountCount ?? 0} 个账号 · 已同步{' '}
        {appliedText(data?.appliedSchedulable)}
      </SettingReadout>
      <SettingReadout label="RPM 容量判据">
        {data?.rpmOk == null ? '未判定' : data.rpmOk ? '充足' : '不足'}
      </SettingReadout>
    </SettingGroup>
  )
}

function HealthGateGroup() {
  const { data, isLoading } = useHealthGateState()
  const { mutate } = useSetHealthGateEnabled()
  const saver = useFieldSaver(mutate, reportSaveError)
  const configured = data?.configured ?? false
  const enabled = data?.enabled ?? false

  return (
    <SettingGroup
      title="健康联动"
      description="本地不稳定时放外部兜底池接量；关闭后外部账号保持不可调度"
    >
      <SettingSwitch
        label="启用健康联动"
        hint={
          configured
            ? '持续判定本地健康度，并把反向调度状态同步到外部账号'
            : '需先在 config.json 配置 healthGate 的地址、令牌和账号'
        }
        checked={enabled}
        onChange={(next) => saver.save('enabled', next)}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading || !configured}
      />
      <SettingReadout label="运行状态" hint="目标系统 / 账号数 / 最近一次健康判定">
        {hostOf(data?.baseUrl)} · {data?.accountCount ?? 0} 个账号 ·{' '}
        {data?.verdict ?? '未判定'}
      </SettingReadout>
      <SettingReadout label="外部账号状态">
        {appliedText(data?.appliedSchedulable)}
      </SettingReadout>
    </SettingGroup>
  )
}

function ConcurrencyGateGroup() {
  const { data, isLoading } = useConcurrencyGateState()
  const { mutate } = useSetConcurrencyGateConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const configured = data?.configured ?? false
  const enabled = data?.enabled ?? false
  const manual = data?.manualConcurrency ?? null
  const mode = manual == null ? 'auto' : 'manual'

  return (
    <SettingGroup
      title="并发联动"
      description="把本地有效凭据的 RPM 总量换算成外部账号并发上限"
    >
      <SettingSwitch
        label="启用并发联动"
        hint={
          configured
            ? '启用后持续把期望并发同步到受控外部账号'
            : '需先在 config.json 配置 concurrencyGate 的地址、令牌和账号'
        }
        checked={enabled}
        onChange={(next) => saver.save('enabled', { enabled: next })}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading || !configured}
      />
      <SettingSegments
        label="并发计算模式"
        hint={
          mode === 'auto'
            ? '按 RPM 总量除以换算除数自动计算'
            : '使用固定并发值，忽略 RPM 换算结果'
        }
        value={mode}
        options={[
          { value: 'auto', label: '自动换算' },
          { value: 'manual', label: '手动固定' },
        ]}
        onChange={(next) =>
          saver.save('mode', {
            manualConcurrency:
              next === 'auto' ? null : (data?.resolvedConcurrency ?? 0),
          })
        }
        pending={saver.isSaving('mode')}
        saved={saver.isSaved('mode')}
        disabled={isLoading || !configured}
      />
      <SettingNumber
        label="RPM 换算除数"
        hint={`当前 RPM 总量 ${data?.rpmTotal ?? 0}，自动模式下向下取整后再夹到配置范围`}
        value={data?.divisor ?? 6}
        onCommit={(next) => saver.save('divisor', { divisor: next })}
        min={1}
        max={100000}
        presets={[3, 6, 10, 20]}
        pending={saver.isSaving('divisor')}
        saved={saver.isSaved('divisor')}
        disabled={isLoading || !configured || mode !== 'auto'}
      />
      <SettingNumber
        label="手动并发"
        hint="仅手动固定模式生效，最终值仍受后端 min/max 范围约束"
        value={manual ?? data?.resolvedConcurrency ?? 0}
        onCommit={(next) =>
          saver.save('manualConcurrency', { manualConcurrency: next })
        }
        min={0}
        max={1000000}
        pending={saver.isSaving('manualConcurrency')}
        saved={saver.isSaved('manualConcurrency')}
        disabled={isLoading || !configured || mode !== 'manual'}
      />
      <SettingReadout label="同步状态" hint="目标系统 / 账号数 / 期望值 / 最近已推送值">
        {hostOf(data?.baseUrl)} · {data?.accountCount ?? 0} 个账号 · 期望{' '}
        {data?.resolvedConcurrency ?? 0} · 已推送 {data?.appliedConcurrency ?? '未知'}
      </SettingReadout>
      {(data?.unlimitedCredentials ?? 0) > 0 && (
        <SettingReadout label="不限速凭据">
          {data?.unlimitedCredentials} 个，RPM 总量包含折算估值
        </SettingReadout>
      )}
    </SettingGroup>
  )
}
