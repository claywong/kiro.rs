import { useState } from 'react'
import {
  Activity, RefreshCw, UploadCloud, Key, Wand2, Eye, EyeOff, Copy,
  MoreHorizontal, ShieldAlert, ShieldCheck, Boxes, HeartPulse, HeartCrack,
  Link2, Link2Off, Power, PowerOff,
} from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { storage } from '@/lib/storage'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent,
  DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator,
} from '@/components/ui/dropdown-menu'
import {
  useLoadBalancingMode, useSetLoadBalancingMode,
  useAccountThrottleConfig, useSetAccountThrottleConfig,
  useSelfHealConfig, useSetSelfHealConfig,
  useHealthGateState, useSetHealthGateEnabled,
  useTrafficIngressState, useSetTrafficIngressEnabled,
} from '@/hooks/use-credentials'
import { useUpdateCheck } from '@/hooks/use-update-check'
import { updateAdminKey, type SelfHealConfigPatch } from '@/api/credentials'
import { extractErrorMessage, generateApiKey } from '@/lib/utils'
import { ImageUpdateDialog } from '@/components/image-update-dialog'
import { AvailableModelsDialog } from '@/components/available-models-dialog'

/**
 * 顶栏右侧通用工具栏：负载均衡切换、可用模型、刷新、在线更新、设置（Key 管理）。
 *
 * 与原 Dashboard 中的工具按钮等价，但全局 Tab 都可访问。刷新按钮会失效
 * 凭据/客户端 Key/统计三类查询，覆盖三个 Tab 的主要数据源。
 */
interface TopbarToolsProps {
  compact?: boolean
}

export function TopbarTools({ compact = false }: TopbarToolsProps) {
  const queryClient = useQueryClient()
  const { data: loadBalancingData, isLoading: isLoadingMode } = useLoadBalancingMode()
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } = useSetLoadBalancingMode()
  const { data: throttleConfig, isLoading: isLoadingThrottle } = useAccountThrottleConfig()
  const { mutate: setThrottleConfig, isPending: isSettingThrottle } = useSetAccountThrottleConfig()
  const { data: updateCheck } = useUpdateCheck()

  const [imageUpdateOpen, setImageUpdateOpen] = useState(false)
  const [modelsDialogOpen, setModelsDialogOpen] = useState(false)
  const [keyDialogOpen, setKeyDialogOpen] = useState(false)
  const [newKey, setNewKey] = useState('')
  const [showPlain, setShowPlain] = useState(false)
  const [updating, setUpdating] = useState(false)

  const handleRefresh = () => {
    queryClient.invalidateQueries({ queryKey: ['credentials'] })
    queryClient.invalidateQueries({ queryKey: ['client-keys'] })
    queryClient.invalidateQueries({ queryKey: ['stats'] })
    queryClient.invalidateQueries({ queryKey: ['current-credential-models'] })
    queryClient.invalidateQueries({ queryKey: ['credential-models'] })
    toast.success('已刷新')
  }

  const handleToggleLoadBalancing = () => {
    const cur = loadBalancingData?.mode || 'priority'
    const next = cur === 'priority' ? 'balanced' : 'priority'
    setLoadBalancingMode(next, {
      onSuccess: () => toast.success(`已切换到${next === 'priority' ? '优先级模式' : '均衡负载模式'}`),
      onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  const handleToggleFailover = () => {
    const cur = throttleConfig?.failover ?? true
    const next = !cur
    setThrottleConfig({ failover: next }, {
      onSuccess: () => toast.success(next ? '已开启账号级风控故障转移' : '已关闭账号级风控故障转移'),
      onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  const openKeyDialog = () => {
    setNewKey('')
    setShowPlain(false)
    setKeyDialogOpen(true)
  }

  const handleUpdateKey = async (e: React.FormEvent) => {
    e.preventDefault()
    const key = newKey.trim()
    if (!key) {
      toast.error('新登录API密钥不能为空')
      return
    }
    setUpdating(true)
    try {
      await updateAdminKey({ newKey: key })
      storage.setApiKey(key)
      toast.success('登录API密钥已更新，已自动切换到新 Key')
      setKeyDialogOpen(false)
      setNewKey('')
    } catch (err) {
      toast.error(`更新失败: ${extractErrorMessage(err)}`)
    } finally {
      setUpdating(false)
    }
  }

  const controls = {
    handleRefresh,
    handleToggleFailover,
    handleToggleLoadBalancing,
    isLoadingMode,
    isLoadingThrottle,
    isSettingMode,
    isSettingThrottle,
    loadBalancingMode: loadBalancingData?.mode,
    openImageUpdate: () => setImageUpdateOpen(true),
    openModels: () => setModelsDialogOpen(true),
    openKeyDialog,
    throttleConfig,
    updateCheck,
    updateCooldown: (secs: number) =>
      setThrottleConfig({ cooldownSecs: secs }, {
        onSuccess: () =>
          toast.success(`冷却时长已设为 ${Math.round(secs / 60)} 分钟`),
        onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
      }),
  }

  return (
    <>
      {compact ? <CompactTools controls={controls} /> : <FullTools controls={controls} />}
      <ImageUpdateDialog open={imageUpdateOpen} onOpenChange={setImageUpdateOpen} />
      <AvailableModelsDialog
        open={modelsDialogOpen}
        onOpenChange={setModelsDialogOpen}
      />

      <Dialog
        open={keyDialogOpen}
        onOpenChange={(open) => { if (!updating) setKeyDialogOpen(open) }}
      >
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <Key className="h-4 w-4" />
              修改登录API密钥
            </DialogTitle>
            <DialogDescription>
              用于登录此管理面板。修改后将自动更新本地存储的 Key，无需重新登录。
            </DialogDescription>
          </DialogHeader>
          <form onSubmit={handleUpdateKey} className="space-y-4 py-2">
            <div className="relative">
              <Input
                type={showPlain ? 'text' : 'password'}
                placeholder="输入或生成新的登录API密钥"
                value={newKey}
                onChange={(e) => setNewKey(e.target.value)}
                disabled={updating}
                autoFocus
                className="pr-20 font-mono text-[13px]"
              />
              <div className="pointer-events-none absolute inset-y-0 right-0 flex items-center pr-1.5">
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="pointer-events-auto h-7 w-7"
                  onClick={() => setShowPlain((v) => !v)}
                  disabled={updating}
                  title={showPlain ? '隐藏' : '显示'}
                >
                  {showPlain ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                </Button>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="pointer-events-auto h-7 w-7"
                  onClick={async () => {
                    if (!newKey.trim()) {
                      toast.error('请先输入或生成 Key 再复制')
                      return
                    }
                    try {
                      await navigator.clipboard.writeText(newKey)
                      toast.success('已复制到剪贴板')
                    } catch {
                      toast.error('复制失败，请手动选择文本')
                    }
                  }}
                  disabled={updating}
                  title="复制"
                >
                  <Copy className="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>
            <div className="flex items-center justify-between gap-2">
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => {
                  const key = generateApiKey('sk-admin-')
                  setNewKey(key)
                  setShowPlain(true)
                }}
                disabled={updating}
              >
                <Wand2 className="h-3.5 w-3.5" />生成随机 Key
              </Button>
              <p className="text-[11px] text-muted-foreground">
                建议生成后立即复制保存，确认更新后即生效。
              </p>
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setKeyDialogOpen(false)} disabled={updating}>
                取消
              </Button>
              <Button type="submit" disabled={updating || !newKey.trim()}>
                {updating ? '更新中…' : '确认更新'}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </>
  )
}

interface ToolControls {
  handleRefresh: () => void
  handleToggleFailover: () => void
  handleToggleLoadBalancing: () => void
  isLoadingMode: boolean
  isLoadingThrottle: boolean
  isSettingMode: boolean
  isSettingThrottle: boolean
  loadBalancingMode?: 'priority' | 'balanced'
  openImageUpdate: () => void
  openKeyDialog: () => void
  openModels: () => void
  throttleConfig?: { failover: boolean; cooldownSecs: number }
  updateCheck?: { hasUpdate: boolean; latestVersion: string; currentVersion: string }
  updateCooldown: (secs: number) => void
}

/**
 * 桌面端顶栏工具：只保留三个控件，避免与左侧 Tab 争抢横向空间。
 * - 「运行策略」下拉：负载均衡 / 账号级风控故障转移 / 凭据自愈（原三个独立按钮）
 * - 可用模型、刷新：高频动作，保持一键可达
 * - 「更多」下拉：镜像在线更新 / GitHub / 密钥管理
 */
function FullTools({ controls }: { controls: ToolControls }) {
  return (
    <>
      <StrategyMenu controls={controls} />
      <ModelsButton onOpen={controls.openModels} />
      <RefreshButton onRefresh={controls.handleRefresh} />
      <MoreMenu controls={controls} />
    </>
  )
}

function StrategyMenu({ controls }: { controls: ToolControls }) {
  const { data: selfHeal } = useSelfHealConfig()
  const { data: gate } = useHealthGateState()
  const throttle = readThrottleState(controls.throttleConfig)
  const selfHealOn = selfHeal?.enabled ?? true
  // 未配置时传 undefined，让 tooltip 干脆不提这一项
  const gateOn = gate?.configured ? gate.enabled : undefined
  const balanced = controls.loadBalancingMode === 'balanced'

  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button
          variant="outline"
          size="sm"
          title={strategyTitle(controls.loadBalancingMode, throttle, selfHealOn, gateOn)}
        >
          <Activity className="h-3.5 w-3.5" />
          <span className="hidden md:inline">
            {controls.isLoadingMode ? '加载中…' : balanced ? '均衡负载' : '优先级'}
          </span>
          {/* 两个开关的状态用小图标挂在按钮上，不展开也能一眼看到 */}
          {throttle.failover ? (
            <ShieldCheck className="h-3 w-3 text-emerald-600" />
          ) : (
            <ShieldAlert className="h-3 w-3 text-amber-500" />
          )}
          {selfHealOn ? (
            <HeartPulse className="h-3 w-3 text-emerald-600" />
          ) : (
            <HeartCrack className="h-3 w-3 text-amber-500" />
          )}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="max-h-[80vh] w-72 overflow-y-auto">
        <DropdownMenuLabel>负载均衡</DropdownMenuLabel>
        <DropdownMenuItem
          disabled={controls.isLoadingMode || controls.isSettingMode}
          onSelect={controls.handleToggleLoadBalancing}
        >
          <Activity />
          {controls.isLoadingMode
            ? '负载均衡加载中'
            : balanced
              ? '切换到优先级'
              : '切换到均衡负载'}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <ThrottlePanels
          config={controls.throttleConfig}
          loading={controls.isLoadingThrottle}
          saving={controls.isSettingThrottle}
          onToggleFailover={controls.handleToggleFailover}
          onChangeCooldown={controls.updateCooldown}
        />
        <DropdownMenuSeparator />
        <SelfHealPanels />
        <DropdownMenuSeparator />
        <TrafficIngressPanels />
        <DropdownMenuSeparator />
        <HealthGatePanels />
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function MoreMenu({ controls }: { controls: ToolControls }) {
  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" title="更多" className="relative">
          <MoreHorizontal className="h-4 w-4" />
          {controls.updateCheck?.hasUpdate && <UpdateDot />}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-60">
        <DropdownMenuLabel>系统</DropdownMenuLabel>
        <DropdownMenuItem onSelect={controls.openImageUpdate}>
          <UploadCloud />
          {controls.updateCheck?.hasUpdate
            ? `镜像在线更新（v${controls.updateCheck.latestVersion}）`
            : '镜像在线更新'}
        </DropdownMenuItem>
        <DropdownMenuItem asChild>
          <a
            href="https://github.com/ZyphrZero/kiro.rs"
            target="_blank"
            rel="noopener noreferrer"
          >
            <GithubIcon className="size-4" />GitHub 仓库
          </a>
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuLabel>密钥管理</DropdownMenuLabel>
        <DropdownMenuItem onSelect={controls.openKeyDialog}>
          <Key />修改登录API密钥（管理面板登录）
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function GithubIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" className={className} aria-hidden="true">
      <path d="M12 .5C5.65.5.5 5.65.5 12.02c0 5.1 3.29 9.42 7.86 10.95.58.11.79-.25.79-.55 0-.27-.01-.99-.02-1.95-3.2.7-3.87-1.54-3.87-1.54-.52-1.32-1.27-1.67-1.27-1.67-1.04-.71.08-.7.08-.7 1.15.08 1.76 1.18 1.76 1.18 1.02 1.76 2.69 1.25 3.34.95.1-.74.4-1.25.72-1.54-2.55-.29-5.24-1.28-5.24-5.69 0-1.26.45-2.29 1.18-3.09-.12-.29-.51-1.46.11-3.05 0 0 .96-.31 3.16 1.18a10.95 10.95 0 0 1 5.75 0c2.2-1.49 3.16-1.18 3.16-1.18.62 1.59.23 2.76.12 3.05.74.8 1.18 1.83 1.18 3.09 0 4.42-2.69 5.39-5.26 5.68.41.36.78 1.06.78 2.14 0 1.55-.01 2.79-.01 3.17 0 .31.21.67.8.55A11.51 11.51 0 0 0 23.5 12.02C23.5 5.65 18.35.5 12 .5Z" />
    </svg>
  )
}

function strategyTitle(
  mode: ToolControls['loadBalancingMode'],
  throttle: ThrottleState,
  selfHealOn: boolean,
  /** 健康联动：undefined = 未配置，此时不在 tooltip 里提，免得像是坏了 */
  gateOn?: boolean,
) {
  const modeText = mode === 'balanced' ? '均衡负载' : '优先级'
  const throttleText = throttle.failover
    ? `故障转移开（冷却 ${throttle.cooldownMin}m）`
    : '故障转移关'
  const gateText = gateOn === undefined ? '' : ` · 联动${gateOn ? '开' : '关'}`
  return `运行策略：${modeText} · ${throttleText} · 自愈${selfHealOn ? '开' : '关'}${gateText}`
}

/** 「运行策略」下拉里的故障转移区块（含冷却时长），自带自定义输入的局部状态 */
function ThrottlePanels(props: ThrottleConfigButtonProps) {
  const { loading, saving, onToggleFailover, onChangeCooldown } = props
  const [customMin, setCustomMin] = useState('')
  const state = readThrottleState(props.config)
  const busy = loading || saving

  const submitCustom = (e: React.FormEvent) => {
    e.preventDefault()
    const min = parseInt(customMin, 10)
    if (invalidCooldownMinutes(min)) {
      toast.error('请输入 1-1440 之间的分钟数')
      return
    }
    onChangeCooldown(min * SECONDS_PER_MINUTE)
    setCustomMin('')
  }

  return (
    <>
      <ThrottleStatusPanel
        saving={busy}
        state={state}
        onToggleFailover={onToggleFailover}
      />
      <ThrottleCooldownPanel
        customMin={customMin}
        saving={busy}
        state={state}
        onChangeCooldown={onChangeCooldown}
        onCustomMinChange={setCustomMin}
        onSubmitCustom={submitCustom}
      />
    </>
  )
}

function CompactTools({ controls }: { controls: ToolControls }) {
  const throttleProps = {
    config: controls.throttleConfig,
    loading: controls.isLoadingThrottle,
    saving: controls.isSettingThrottle,
    onToggleFailover: controls.handleToggleFailover,
    onChangeCooldown: controls.updateCooldown,
  }

  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button variant="outline" size="icon" title="更多操作">
          <MoreHorizontal className="h-4 w-4" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-64">
        <DropdownMenuLabel>系统操作</DropdownMenuLabel>
        <DropdownMenuItem
          disabled={controls.isLoadingMode || controls.isSettingMode}
          onSelect={controls.handleToggleLoadBalancing}
        >
          <Activity />
          {controls.isLoadingMode
            ? '负载均衡加载中'
            : controls.loadBalancingMode === 'priority'
              ? '切换到均衡负载'
              : '切换到优先级'}
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={controls.handleRefresh}>
          <RefreshCw />刷新数据
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={controls.openModels}>
          <Boxes />可用模型
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={controls.openImageUpdate}>
          <UploadCloud />镜像在线更新
        </DropdownMenuItem>
        <ThrottleCompactItems {...throttleProps} />
        <SelfHealCompactItems />
        <TrafficIngressCompactItems />
        <HealthGateCompactItems />
        <DropdownMenuLabel>密钥管理</DropdownMenuLabel>
        <DropdownMenuItem onSelect={controls.openKeyDialog}>
          <Key />修改登录API密钥（管理面板登录）
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function ModelsButton({ onOpen }: { onOpen: () => void }) {
  return (
    <Button variant="ghost" size="icon" onClick={onOpen} title="可用模型">
      <Boxes className="h-4 w-4" />
    </Button>
  )
}

function RefreshButton({ onRefresh }: { onRefresh: () => void }) {
  return (
    <Button variant="ghost" size="icon" onClick={onRefresh} title="刷新">
      <RefreshCw className="h-4 w-4" />
    </Button>
  )
}

function UpdateDot() {
  return (
    <span className="absolute right-1 top-1 inline-flex h-2 w-2 items-center justify-center">
      <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-red-400 opacity-75" />
      <span className="relative inline-flex h-2 w-2 rounded-full bg-red-500" />
    </span>
  )
}

interface ThrottleConfigButtonProps {
  config?: { failover: boolean; cooldownSecs: number }
  loading: boolean
  saving: boolean
  onToggleFailover: () => void
  onChangeCooldown: (secs: number) => void
}

interface ThrottleState {
  cooldownMin: number
  cooldownSecs: number
  failover: boolean
}

interface CustomCooldownFormProps {
  cooldownMin: number
  customMin: string
  disabled: boolean
  onCustomMinChange: (value: string) => void
  onSubmit: (e: React.FormEvent) => void
}

const COOLDOWN_PRESETS = [
  { label: '5 分钟', secs: 5 * 60 },
  { label: '15 分钟', secs: 15 * 60 },
  { label: '30 分钟', secs: 30 * 60 },
  { label: '1 小时', secs: 60 * 60 },
  { label: '2 小时', secs: 2 * 60 * 60 },
]

const DEFAULT_COOLDOWN_SECS = 30 * 60
const SECONDS_PER_MINUTE = 60
const MIN_CUSTOM_COOLDOWN_MINUTES = 1
const MAX_CUSTOM_COOLDOWN_MINUTES = 1440

/**
 * 故障转移开关 + 冷却时长设置（紧凑下拉）
 *
 * 主按钮文案显示当前状态；下拉里:
 * - 顶部一个 Switch 切换 failover
 * - 5 个预设时长 + 一个自定义输入（分钟）
 */
function ThrottleStatusPanel({
  saving, state, onToggleFailover,
}: {
  saving: boolean
  state: ThrottleState
  onToggleFailover: () => void
}) {
  return (
    <>
      <DropdownMenuLabel>账号级风控故障转移</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <ThrottleStatusText failover={state.failover} />
          <Switch
            checked={state.failover}
            disabled={saving}
            onCheckedChange={() => onToggleFailover()}
          />
        </div>
      </div>
    </>
  )
}

function ThrottleStatusText({ failover }: { failover: boolean }) {
  return (
    <div className="text-xs">
      <div className="font-medium text-foreground">
        {failover ? '开启' : '关闭'}
      </div>
      <div className="text-muted-foreground leading-snug">
        {failover
          ? '上游对当前账号触发临时限速时，自动冷却该凭据并切换到下一个可用凭据'
          : '上游对当前账号触发临时限速时，仅按瞬态错误重试，不切换凭据'}
      </div>
    </div>
  )
}

function ThrottleCooldownPanel({
  customMin, saving, state, onChangeCooldown, onCustomMinChange, onDone, onSubmitCustom,
}: {
  customMin: string
  saving: boolean
  state: ThrottleState
  onChangeCooldown: (secs: number) => void
  onCustomMinChange: (value: string) => void
  onDone?: () => void
  onSubmitCustom: (e: React.FormEvent) => void
}) {
  const disabled = saving || !state.failover

  return (
    <>
      <DropdownMenuLabel className="pt-1">冷却时长</DropdownMenuLabel>
      <div className={cooldownPanelClassName(state.failover)}>
        <CooldownPresetButtons
          cooldownSecs={state.cooldownSecs}
          disabled={disabled}
          onChangeCooldown={onChangeCooldown}
          onDone={onDone}
        />
        <CustomCooldownForm
          cooldownMin={state.cooldownMin}
          customMin={customMin}
          disabled={disabled}
          onCustomMinChange={onCustomMinChange}
          onSubmit={onSubmitCustom}
        />
      </div>
    </>
  )
}

function CustomCooldownForm({
  cooldownMin, customMin, disabled, onCustomMinChange, onSubmit,
}: CustomCooldownFormProps) {
  return (
    <form onSubmit={onSubmit} className="mt-2 flex items-center gap-1.5">
      <Input
        type="number"
        min={MIN_CUSTOM_COOLDOWN_MINUTES}
        max={MAX_CUSTOM_COOLDOWN_MINUTES}
        placeholder={`自定义（当前 ${cooldownMin}）`}
        value={customMin}
        onChange={(e) => onCustomMinChange(e.target.value)}
        disabled={disabled}
        className="h-7 text-xs"
      />
      <span className="text-xs text-muted-foreground">分钟</span>
      <Button
        type="submit"
        size="sm"
        variant="outline"
        className="h-7 text-xs"
        disabled={disabled || !customMin.trim()}
      >
        保存
      </Button>
    </form>
  )
}

function ThrottleCompactItems(props: ThrottleConfigButtonProps) {
  const { loading, saving, onToggleFailover, onChangeCooldown } = props
  const [customMin, setCustomMin] = useState('')
  const state = readThrottleState(props.config)
  const busy = loading || saving

  const submitCustom = (e: React.FormEvent) => {
    e.preventDefault()
    const min = parseInt(customMin, 10)
    if (invalidCooldownMinutes(min)) {
      toast.error('请输入 1-1440 之间的分钟数')
      return
    }
    onChangeCooldown(min * SECONDS_PER_MINUTE)
    setCustomMin('')
  }

  return (
    <>
      <DropdownMenuLabel>故障转移</DropdownMenuLabel>
      <DropdownMenuItem
        disabled={busy}
        onSelect={onToggleFailover}
      >
        {state.failover ? <ShieldCheck /> : <ShieldAlert />}
        {compactThrottleText(loading, state)}
      </DropdownMenuItem>
      <ThrottleCooldownPanel
        customMin={customMin}
        saving={busy}
        state={state}
        onChangeCooldown={onChangeCooldown}
        onCustomMinChange={setCustomMin}
        onSubmitCustom={submitCustom}
      />
    </>
  )
}

// ============ 自愈治理 ============

const SELF_HEAL_INTERVAL_PRESETS = [
  { label: '不冷却', secs: 0 },
  { label: '1 分钟', secs: 60 },
  { label: '5 分钟', secs: 5 * 60 },
  { label: '15 分钟', secs: 15 * 60 },
  { label: '30 分钟', secs: 30 * 60 },
]

/**
 * 自愈治理设置（下拉）：
 * - 开关：是否启用凭据自愈
 * - 冷却间隔：两次自愈的最小间隔（打断持续 403 死循环的关键）
 * - 连续上限：连续自愈达到该轮数且期间无成功则停止（0=不限）
 * - 只读观测：凭据最大连续轮数 / 累计恢复凭据次数
 */
function SelfHealPanels() {
  const { data: config, isLoading } = useSelfHealConfig()
  const { mutate, isPending } = useSetSelfHealConfig()
  const [roundsInput, setRoundsInput] = useState('')

  const enabled = config?.enabled ?? true
  const busy = isLoading || isPending

  const save = (patch: SelfHealConfigPatch, msg: string) => {
    mutate(patch, {
      onSuccess: () => toast.success(msg),
      onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
    })
  }

  const submitRounds = (e: React.FormEvent) => {
    e.preventDefault()
    const n = parseInt(roundsInput, 10)
    if (Number.isNaN(n) || n < 0 || n > 1000) {
      toast.error('请输入 0-1000 之间的轮数（0=不限）')
      return
    }
    save({ maxConsecutiveRounds: n }, n === 0 ? '连续自愈已设为不限' : `连续自愈上限已设为 ${n} 轮`)
    setRoundsInput('')
  }

  return (
    <>
        <DropdownMenuLabel>凭据自愈</DropdownMenuLabel>
        <div className="px-2 pb-2">
          <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
            <div className="text-xs">
              <div className="font-medium">{enabled ? '已启用' : '已关闭'}</div>
              <div className="text-muted-foreground">
                当前请求池全灭时按作用域恢复凭据
              </div>
            </div>
            <Switch
              checked={enabled}
              disabled={busy}
              onCheckedChange={(v) => save({ enabled: v }, v ? '已开启凭据自愈' : '已关闭凭据自愈')}
            />
          </div>
          {config && (
            <div className="mt-2 flex items-center justify-between rounded-md bg-secondary/20 px-2.5 py-1.5 text-xs text-muted-foreground">
              <span>连续 {config.consecutiveRounds} 轮</span>
              <span>累计恢复 {config.totalCount} 次</span>
            </div>
          )}
        </div>

        <DropdownMenuLabel className="pt-1">403 封禁识别</DropdownMenuLabel>
        <div className="px-2 pb-2">
          <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
            <div className="text-xs">
              <div className="font-medium">
                {config?.suspendedDetectionEnabled ?? true ? '已启用' : '已关闭'}
              </div>
              <div className="text-muted-foreground">
                命中封禁文案的 403 立即禁用，不参与自愈
              </div>
            </div>
            <Switch
              checked={config?.suspendedDetectionEnabled ?? true}
              disabled={busy}
              onCheckedChange={(v) =>
                save({ suspendedDetectionEnabled: v }, v ? '已开启 403 封禁识别' : '已关闭 403 封禁识别')
              }
            />
          </div>
        </div>

        <DropdownMenuLabel className="pt-1">自愈冷却间隔</DropdownMenuLabel>
        <div className={cooldownPanelClassName(enabled)}>
          <div className="grid grid-cols-3 gap-1.5">
            {SELF_HEAL_INTERVAL_PRESETS.map((p) => (
              <Button
                key={p.secs}
                size="sm"
                variant={config?.minIntervalSecs === p.secs ? 'default' : 'outline'}
                className="h-7 text-xs"
                disabled={busy || !enabled}
                onClick={() => save({ minIntervalSecs: p.secs }, `自愈冷却已设为「${p.label}」`)}
              >
                {p.label}
              </Button>
            ))}
          </div>

          <DropdownMenuLabel className="px-0 pt-2">连续自愈上限（0=不限）</DropdownMenuLabel>
          <form onSubmit={submitRounds} className="mt-1 flex items-center gap-1.5">
            <Input
              type="number"
              min={0}
              max={1000}
              placeholder={`当前 ${config?.maxConsecutiveRounds ?? 5} 轮`}
              value={roundsInput}
              onChange={(e) => setRoundsInput(e.target.value)}
              disabled={busy || !enabled}
              className="h-7 text-xs"
            />
            <span className="text-xs text-muted-foreground">轮</span>
            <Button
              type="submit"
              size="sm"
              variant="outline"
              className="h-7 text-xs"
              disabled={busy || !enabled || !roundsInput.trim()}
            >
              保存
            </Button>
          </form>
        </div>
    </>
  )
}

// ============ 流量入口 ============

/** 独立控制 g7e6ai.com 指定账号的 schedulable，不参与健康判定。 */
function TrafficIngressPanels() {
  const { data: state, isLoading } = useTrafficIngressState()
  const { mutate, isPending } = useSetTrafficIngressEnabled()

  const configured = state?.configured ?? false
  const enabled = state?.enabled ?? false
  const busy = isLoading || isPending

  const toggle = (next: boolean) => {
    mutate(next, {
      onSuccess: () =>
        toast.success(next ? '流量入口已开启，正在同步外部账号' : '流量入口已关闭，正在同步外部账号'),
      onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  return (
    <>
      <DropdownMenuLabel>流量入口</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <div className="min-w-0 text-xs">
            <div className="font-medium">
              {!configured ? '未配置' : enabled ? '已开启' : '已关闭'}
            </div>
            <div className="truncate text-muted-foreground">
              {configured ? '手动控制指定外部账号接量' : '需配置 trafficIngress 的 token / 账号'}
            </div>
          </div>
          <Switch
            checked={enabled}
            disabled={busy || !configured}
            onCheckedChange={toggle}
            aria-label="流量入口开关"
          />
        </div>

        {configured && state && (
          <div className="mt-2 space-y-1 rounded-md bg-secondary/20 px-2.5 py-1.5 text-xs text-muted-foreground">
            <div className="flex items-center justify-between gap-2">
              <span className="truncate">{hostOf(state.baseUrl)}</span>
              <span>{state.accountCount} 个账号</span>
            </div>
            <div className="flex items-center justify-between gap-2">
              <span>期望：{enabled ? '可调度' : '不可调度'}</span>
              <span>已同步：{appliedText(state.appliedSchedulable)}</span>
            </div>
          </div>
        )}
      </div>
    </>
  )
}

// ============ 健康联动 ============

/**
 * 健康联动总开关（下拉）。
 *
 * 语义与同栏其余开关不同，值得注意：这里控制的是**往外部系统推调度开关**，
 * 不是本地怎么调度请求。本地稳则关闭外部调度、不稳则打开（反向映射，因为
 * 外部账号是兜底池，平时闲着更好）。
 *
 * 关掉时后端停止健康判定，并把外部账号设为不可调度。推送异步执行，失败会按
 * 检查周期重试；「已推送」展示最近一次成功值。
 *
 * 未配置与「配好了但关着」分开展示：前者改了也没用（没有循环在跑），
 * 故置灰并说明缺什么。
 */
function HealthGatePanels() {
  const { data: state, isLoading } = useHealthGateState()
  const { mutate, isPending } = useSetHealthGateEnabled()

  const configured = state?.configured ?? false
  const enabled = state?.enabled ?? false
  const busy = isLoading || isPending

  const toggle = (v: boolean) => {
    mutate(v, {
      onSuccess: () =>
        toast.success(
          v
            ? '已开启健康联动'
            : '已关闭健康联动，正在将外部账号设为不可调度',
        ),
      onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
    })
  }

  return (
    <>
      <DropdownMenuLabel>健康联动</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <div className="min-w-0 text-xs">
            <div className="font-medium">
              {!configured ? '未配置' : enabled ? '已启用' : '已关闭'}
            </div>
            <div className="truncate text-muted-foreground">
              {configured ? (
                <>本地不稳时放外部兜底池接量</>
              ) : (
                <>需先配 healthGate 的地址 / token / 账号</>
              )}
            </div>
          </div>
          <Switch
            checked={enabled}
            disabled={busy || !configured}
            onCheckedChange={toggle}
            aria-label="健康联动总开关"
          />
        </div>

        {configured && state && (
          <div className="mt-2 space-y-1 rounded-md bg-secondary/20 px-2.5 py-1.5 text-xs text-muted-foreground">
            <div className="flex items-center justify-between gap-2">
              <span className="truncate">{hostOf(state.baseUrl)}</span>
              <span>{state.accountCount} 个账号</span>
            </div>
            <div className="flex items-center justify-between gap-2">
              <span>判定：{state.verdict ?? '未判定'}</span>
              <span>已推送：{appliedText(state.appliedSchedulable)}</span>
            </div>
            {!enabled && (
              <div className="pt-0.5 text-[11px] leading-snug">
                已停止健康判定，外部账号将保持不可调度。
              </div>
            )}
          </div>
        )}
      </div>
    </>
  )
}

/** 已推送值的中文说明。null = 本进程还没推过，对方可能残留上次运行的值 */
function appliedText(applied: boolean | null | undefined): string {
  if (applied === null || applied === undefined) return '未知'
  return applied ? '可调度' : '不可调度'
}

/** 只取主机名，下拉里宽度有限，完整 URL 会挤掉右边的账号数 */
function hostOf(url: string): string {
  try {
    return new URL(url).host
  } catch {
    return url || '—'
  }
}

/** 紧凑模式（下拉菜单内）的自愈开关项 */
function SelfHealCompactItems() {
  const { data: config, isLoading } = useSelfHealConfig()
  const { mutate, isPending } = useSetSelfHealConfig()
  const enabled = config?.enabled ?? true
  const busy = isLoading || isPending

  return (
    <>
      <DropdownMenuLabel>凭据自愈</DropdownMenuLabel>
      <DropdownMenuItem
        disabled={busy}
        onSelect={() =>
          mutate(
            { enabled: !enabled },
            {
              onSuccess: () => toast.success(!enabled ? '已开启凭据自愈' : '已关闭凭据自愈'),
              onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
            },
          )
        }
      >
        {enabled ? <HeartPulse /> : <HeartCrack />}
        {isLoading
          ? '自愈加载中'
          : enabled
            ? `关闭自愈（连续 ${config?.consecutiveRounds ?? 0} 轮）`
            : '开启全账号自愈'}
      </DropdownMenuItem>
    </>
  )
}

/** 紧凑模式的独立流量入口开关；未配置时不占菜单空间。 */
function TrafficIngressCompactItems() {
  const { data: state, isLoading } = useTrafficIngressState()
  const { mutate, isPending } = useSetTrafficIngressEnabled()

  if (!state?.configured) return null

  const enabled = state.enabled
  const busy = isLoading || isPending

  return (
    <>
      <DropdownMenuLabel>流量入口</DropdownMenuLabel>
      <DropdownMenuItem
        disabled={busy}
        onSelect={() =>
          mutate(!enabled, {
            onSuccess: () =>
              toast.success(!enabled ? '流量入口已开启，正在同步外部账号' : '流量入口已关闭，正在同步外部账号'),
            onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
          })
        }
      >
        {enabled ? <Power /> : <PowerOff />}
        {enabled
          ? `关闭入口（已同步${appliedText(state.appliedSchedulable)}）`
          : `开启入口（已同步${appliedText(state.appliedSchedulable)}）`}
      </DropdownMenuItem>
    </>
  )
}

/**
 * 紧凑模式（窄屏）的健康联动开关项。
 *
 * 未配置时整项不渲染 —— 紧凑菜单空间有限，摆一个点不动的项没有意义
 * （宽屏那版会显示"未配置"并说明缺什么，那里有地方写）。
 */
function HealthGateCompactItems() {
  const { data: state, isLoading } = useHealthGateState()
  const { mutate, isPending } = useSetHealthGateEnabled()

  if (!state?.configured) return null

  const enabled = state.enabled
  const busy = isLoading || isPending

  return (
    <>
      <DropdownMenuLabel>健康联动</DropdownMenuLabel>
      <DropdownMenuItem
        disabled={busy}
        onSelect={() =>
          mutate(!enabled, {
            onSuccess: () =>
              toast.success(
                !enabled
                  ? '已开启健康联动'
                  : '已关闭健康联动，正在将外部账号设为不可调度',
              ),
            onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
          })
        }
      >
        {enabled ? <Link2 /> : <Link2Off />}
        {enabled
          ? `关闭联动（当前${state.verdict ?? '未判定'}）`
          : `开启联动（外部${appliedText(state.appliedSchedulable)}）`}
      </DropdownMenuItem>
    </>
  )
}

function CooldownPresetButtons({
  cooldownSecs, disabled, onChangeCooldown, onDone,
}: {
  cooldownSecs: number
  disabled: boolean
  onChangeCooldown: (secs: number) => void
  onDone?: () => void
}) {
  return (
    <div className="grid grid-cols-3 gap-1">
      {COOLDOWN_PRESETS.map((preset) => (
        <CooldownPresetButton
          key={preset.secs}
          active={preset.secs === cooldownSecs}
          disabled={disabled}
          label={preset.label}
          secs={preset.secs}
          onChangeCooldown={onChangeCooldown}
          onDone={onDone}
        />
      ))}
    </div>
  )
}

function CooldownPresetButton({
  active, disabled, label, secs, onChangeCooldown, onDone,
}: {
  active: boolean
  disabled: boolean
  label: string
  secs: number
  onChangeCooldown: (secs: number) => void
  onDone?: () => void
}) {
  return (
    <Button
      type="button"
      size="sm"
      variant={active ? 'default' : 'outline'}
      className="h-7 text-xs"
      disabled={disabled}
      onClick={() => {
        if (!active) onChangeCooldown(secs)
        onDone?.()
      }}
    >
      {label}
    </Button>
  )
}

function secondsToMinutes(seconds: number) {
  return Math.round(seconds / SECONDS_PER_MINUTE)
}

function readThrottleState(
  config: ThrottleConfigButtonProps['config'],
): ThrottleState {
  const cooldownSecs = config?.cooldownSecs ?? DEFAULT_COOLDOWN_SECS
  return {
    cooldownMin: secondsToMinutes(cooldownSecs),
    cooldownSecs,
    failover: config?.failover ?? true,
  }
}

function compactThrottleText(loading: boolean, state: ThrottleState) {
  if (loading) return '故障转移加载中'
  if (!state.failover) return '开启故障转移'
  return `关闭故障转移 · ${state.cooldownMin}m`
}

function invalidCooldownMinutes(minutes: number) {
  return (
    Number.isNaN(minutes) ||
    minutes < MIN_CUSTOM_COOLDOWN_MINUTES ||
    minutes > MAX_CUSTOM_COOLDOWN_MINUTES
  )
}

function cooldownPanelClassName(failover: boolean) {
  return `px-2 pb-2 ${failover ? '' : 'opacity-60'}`
}
