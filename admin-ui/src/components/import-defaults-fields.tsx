/**
 * 导入对话框的公共「默认值」字段：RPM / 优先级 / 代理。
 *
 * 批量导入与 KAM 导入都需要这三项，且语义完全一致，故抽成独立模块：
 * 一是避免两处 JSX 与解析逻辑重复，二是把本地新增代码从上游的两个
 * 对话框文件里挪出来，减少后续合并冲突面。
 *
 * 语义：这里填的是**应用到本次所有导入行**的默认值。
 * 优先级为「单行显式值 > 此处默认值 > 硬编码兜底」，
 * 所以批量导入 JSON 里已写 rpmLimit 的行不会被这里覆盖。
 */

import { Input } from '@/components/ui/input'

/** 三项默认值的原始输入（全部按字符串存，空串表示"不指定"） */
export interface ImportDefaults {
  rpmLimit: string
  priority: string
  proxyUrl: string
  proxyUsername: string
  proxyPassword: string
}

export const EMPTY_IMPORT_DEFAULTS: ImportDefaults = {
  rpmLimit: '',
  priority: '',
  proxyUrl: '',
  proxyUsername: '',
  proxyPassword: '',
}

/** 解析结果：未填的项为 undefined，交由调用方回退到各自的兜底值 */
export interface ResolvedImportDefaults {
  rpmLimit?: number
  priority?: number
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
}

/**
 * 把原始输入解析为可直接塞进 AddCredentialRequest 的值。
 *
 * 返回 `error` 时调用方应中止导入并提示——数字填错就导入会静默落一批
 * 错配置进库，比拦下来麻烦得多。
 */
export function resolveImportDefaults(
  d: ImportDefaults,
): { resolved: ResolvedImportDefaults; error?: string } {
  const resolved: ResolvedImportDefaults = {}

  const rpmRaw = d.rpmLimit.trim()
  if (rpmRaw) {
    const n = Number(rpmRaw)
    if (!Number.isInteger(n) || n < 0) {
      return { resolved, error: 'RPM 必须是 ≥ 0 的整数（0 表示不限速）' }
    }
    resolved.rpmLimit = n
  }

  const priorityRaw = d.priority.trim()
  if (priorityRaw) {
    const n = Number(priorityRaw)
    if (!Number.isInteger(n) || n < 0) {
      return { resolved, error: '优先级必须是 ≥ 0 的整数' }
    }
    resolved.priority = n
  }

  resolved.proxyUrl = d.proxyUrl.trim() || undefined
  resolved.proxyUsername = d.proxyUsername.trim() || undefined
  resolved.proxyPassword = d.proxyPassword.trim() || undefined

  return { resolved }
}

interface Props {
  value: ImportDefaults
  onChange: (next: ImportDefaults) => void
  disabled?: boolean
  /** 代理池里可用条目数，用于说明留空时的行为 */
  enabledProxyCount?: number
}

/** 三项默认值的输入区。受控组件，状态由调用方持有。 */
export function ImportDefaultsFields({
  value,
  onChange,
  disabled,
  enabledProxyCount = 0,
}: Props) {
  const set = <K extends keyof ImportDefaults>(key: K, v: ImportDefaults[K]) =>
    onChange({ ...value, [key]: v })

  return (
    <div className="space-y-3 rounded-xl border border-input/60 bg-secondary/20 p-3">
      <div className="text-sm font-medium">导入默认值（应用到本次所有账号）</div>

      <div className="grid grid-cols-2 gap-2">
        <div className="space-y-1.5">
          <label htmlFor="importDefaultRpm" className="text-xs text-muted-foreground">
            每分钟请求上限（RPM）
          </label>
          <Input
            id="importDefaultRpm"
            type="number"
            min={0}
            step={1}
            placeholder="留空用 300"
            value={value.rpmLimit}
            onChange={(e) => set('rpmLimit', e.target.value)}
            disabled={disabled}
          />
        </div>
        <div className="space-y-1.5">
          <label htmlFor="importDefaultPriority" className="text-xs text-muted-foreground">
            优先级
          </label>
          <Input
            id="importDefaultPriority"
            type="number"
            min={0}
            step={1}
            placeholder="留空用 0"
            value={value.priority}
            onChange={(e) => set('priority', e.target.value)}
            disabled={disabled}
          />
        </div>
      </div>
      <p className="text-xs text-muted-foreground">
        RPM 填 0 表示不限速；优先级数值越小越优先，仅「优先级」调度模式下生效
      </p>

      <div className="space-y-2">
        <label htmlFor="importDefaultProxy" className="text-xs text-muted-foreground">
          代理
        </label>
        <Input
          id="importDefaultProxy"
          placeholder={
            enabledProxyCount > 0
              ? `留空则从代理池随机分配（当前 ${enabledProxyCount} 个可用）`
              : '代理 URL（留空使用全局配置，"direct" 不使用代理）'
          }
          value={value.proxyUrl}
          onChange={(e) => set('proxyUrl', e.target.value)}
          disabled={disabled}
        />
        <div className="grid grid-cols-2 gap-2">
          <Input
            id="importDefaultProxyUser"
            placeholder="代理用户名"
            value={value.proxyUsername}
            onChange={(e) => set('proxyUsername', e.target.value)}
            disabled={disabled}
          />
          <Input
            id="importDefaultProxyPass"
            type="password"
            placeholder="代理密码"
            value={value.proxyPassword}
            onChange={(e) => set('proxyPassword', e.target.value)}
            disabled={disabled}
          />
        </div>
        <p className="text-xs text-muted-foreground">
          填了就对所有账号用这个代理，不再走代理池随机分配。输入 "direct" 可显式不使用代理
        </p>
      </div>
    </div>
  )
}
