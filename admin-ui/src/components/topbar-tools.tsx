import { useState } from 'react'
import {
  Activity,
  RefreshCw,
  MoreHorizontal,
  Boxes,
} from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
} from '@/components/ui/dropdown-menu'
import {
  useLoadBalancingMode,
  useSetLoadBalancingMode,
} from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'
import { AvailableModelsDialog } from '@/components/available-models-dialog'

/**
 * 顶栏只保留负载均衡、模型和刷新三个高频入口。
 * 故障转移、自愈和在线更新统一归「设置」Tab，避免同一配置出现两套入口。
 */
interface TopbarToolsProps {
  compact?: boolean
}

// 顶栏“刷新数据”只刷新数据查询；配置查询有各自的保存/轮询语义。
const NON_DATA_QUERY_ROOTS = new Set([
  'loadBalancingMode',
  'accountThrottleConfig',
  'accountRpmLimitConfig',
  'selfHealConfig',
  'logGovernanceConfig',
  'global-proxy',
  'custom-models',
  'update-config',
  'system-update-check',
])

/** 一个开关的完整描述，compact / full 两种排布共用 */
interface ToggleSpec {
  key: string
  /** 当前是否开启 */
  on: boolean
  busy: boolean
  /** full 模式的按钮文案 */
  label: string
  /** compact 模式的菜单项文案（说明这次点击会做什么） */
  menuLabel: string
  title: string
  icon: React.ReactNode
  onToggle: () => void
}

export function TopbarTools({ compact = false }: TopbarToolsProps) {
  const queryClient = useQueryClient()
  const { data: lbData, isLoading: lbLoading } = useLoadBalancingMode()
  const { mutate: setLb, isPending: lbSaving } = useSetLoadBalancingMode()

  const [modelsOpen, setModelsOpen] = useState(false)

  const handleRefresh = () => {
    // 刷新所有数据查询；配置查询不属于“刷新数据”，也不会因此触发上游检查。
    queryClient.invalidateQueries({
      predicate: ({ queryKey }) => {
        const root = queryKey[0]
        return typeof root === 'string' && !NON_DATA_QUERY_ROOTS.has(root)
      },
    })
    toast.success('已刷新')
  }

  const onError = (err: unknown) =>
    toast.error('切换失败：' + extractErrorMessage(err))

  const balanced = lbData?.mode === 'balanced'

  const toggles: ToggleSpec[] = [
    {
      key: 'lb',
      on: balanced,
      busy: lbLoading || lbSaving,
      label: lbLoading ? '加载中…' : balanced ? '均衡负载' : '按优先级',
      menuLabel: balanced ? '切换到按优先级' : '切换到均衡负载',
      title: balanced
        ? '调度模式：均衡负载 —— 按用量动态摊平到整个池子'
        : '调度模式：按优先级 —— 数字越小越先用，用完再换下一个',
      icon: <Activity className="h-3.5 w-3.5" />,
      onToggle: () =>
        setLb(balanced ? 'priority' : 'balanced', {
          onSuccess: () =>
            toast.success(
              balanced ? '已切换到按优先级调度' : '已切换到均衡负载',
            ),
          onError,
        }),
    },
  ]

  return (
    <>
      {compact ? (
        <CompactTools
          toggles={toggles}
          onRefresh={handleRefresh}
          onOpenModels={() => setModelsOpen(true)}
        />
      ) : (
        <FullTools
          toggles={toggles}
          onRefresh={handleRefresh}
          onOpenModels={() => setModelsOpen(true)}
        />
      )}
      <AvailableModelsDialog open={modelsOpen} onOpenChange={setModelsOpen} />
    </>
  )
}

interface ToolsProps {
  toggles: ToggleSpec[]
  onRefresh: () => void
  onOpenModels: () => void
}

function FullTools({
  toggles,
  onRefresh,
  onOpenModels,
}: ToolsProps) {
  return (
    <>
      {toggles.map((t) => (
        <Button
          key={t.key}
          variant="outline"
          size="sm"
          onClick={t.onToggle}
          disabled={t.busy}
          title={t.title}
        >
          {t.icon}
          <span className="hidden md:inline">{t.label}</span>
        </Button>
      ))}
      <Button variant="ghost" size="icon" onClick={onOpenModels} title="可用模型">
        <Boxes className="h-4 w-4" />
      </Button>
      <Button variant="ghost" size="icon" onClick={onRefresh} title="刷新数据">
        <RefreshCw className="h-4 w-4" />
      </Button>
    </>
  )
}

function CompactTools({
  toggles,
  onRefresh,
  onOpenModels,
}: ToolsProps) {
  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" title="更多操作">
          <MoreHorizontal className="h-4 w-4" />
        </Button>
      </DropdownMenuTrigger>
      {/* 窄屏兜底：菜单项随调度开关增加时不撑出视口，超出即在菜单内滚动 */}
      <DropdownMenuContent
        align="end"
        className="max-h-[calc(100dvh-4.5rem)] w-56 max-w-[calc(100dvw-1rem)] overflow-x-hidden overflow-y-auto overscroll-contain"
      >
        <DropdownMenuLabel>调度</DropdownMenuLabel>
        {toggles.map((t) => (
          <DropdownMenuItem key={t.key} disabled={t.busy} onSelect={t.onToggle}>
            {t.icon}
            {t.menuLabel}
          </DropdownMenuItem>
        ))}
        <DropdownMenuLabel>操作</DropdownMenuLabel>
        <DropdownMenuItem onSelect={onRefresh}>
          <RefreshCw />
          刷新数据
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={onOpenModels}>
          <Boxes />
          可用模型
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}
