import {
  Cpu,
  FolderTree,
  Gauge,
  Globe,
  KeyRound,
  ScrollText,
  PackageOpen,
  ShieldCheck,
  SlidersHorizontal,
  Store,
  Tags,
} from 'lucide-react'
import { PageHeader } from '@/components/console/page-header'
import { Card, CardContent } from '@/components/ui/card'
import { useUrlState } from '@/hooks/use-url-state'
import { useVendorUnackedCount } from '@/hooks/use-vendor'
import { cn } from '@/lib/utils'
import { DispatchSection } from '@/components/settings/dispatch-section'
import { NetworkSection } from '@/components/settings/network-section'
import { LogSection } from '@/components/settings/log-section'
import { SystemSection } from '@/components/settings/system-section'
import { SecuritySection } from '@/components/settings/security-section'
import { MetadataSection } from '@/components/settings/metadata-section'
import { ModelsSection } from '@/components/settings/models-section'
import { GroupsPage } from '@/components/groups-page'
import { ClientKeysPage } from '@/components/client-keys-page'
import { VendorPage } from '@/components/vendor-page'

/**
 * 设置页 —— 把此前散在三处的 7 个配置端点收拢到一处。
 *
 * 改造前它们分别住在：顶栏按钮（负载均衡）、顶栏两个下拉（风控故障转移、自愈）、
 * 顶栏设置菜单（登录密钥）、日志页下拉（日志治理）、代理池弹窗内（全局代理）、
 * 镜像更新弹窗内（更新配置）。同一类东西分在六个地方，找一个配置得先记住它藏在哪。
 *
 * 顶栏**保留**三个快捷开关（负载均衡 / 故障转移 / 自愈），因为它们是运维高频动作，
 * 一次点击就该切换完；但参数（冷却时长、连续上限、保留天数这些）全部移到这里 ——
 * 下拉菜单里塞数字输入框本来就不是它该干的事。
 */
type SectionKey =
  | 'dispatch'
  | 'metadata'
  | 'network'
  | 'log'
  | 'models'
  | 'system'
  | 'security'
  | 'groups'
  | 'keys'
  | 'vendor'

/** 页面级分区：渲染整页组件（自带 PageHeader 与布局），不套设置卡片的壳 */
const PAGE_SECTIONS: SectionKey[] = ['groups', 'keys', 'vendor']

const SECTIONS: {
  key: SectionKey
  label: string
  icon: React.ReactNode
}[] = [
  {
    key: 'dispatch',
    label: '调度',
    icon: <Gauge className="h-4 w-4" />,
  },
  {
    key: 'metadata',
    label: '凭据字段',
    icon: <Tags className="h-4 w-4" />,
  },
  {
    key: 'network',
    label: '网络',
    icon: <Globe className="h-4 w-4" />,
  },
  {
    key: 'log',
    label: '日志',
    icon: <ScrollText className="h-4 w-4" />,
  },
  {
    key: 'models',
    label: '模型',
    icon: <Cpu className="h-4 w-4" />,
  },
  {
    key: 'system',
    label: '系统',
    icon: <PackageOpen className="h-4 w-4" />,
  },
  {
    key: 'security',
    label: '安全',
    icon: <ShieldCheck className="h-4 w-4" />,
  },
  {
    key: 'groups',
    label: '分组管理',
    icon: <FolderTree className="h-4 w-4" />,
  },
  {
    key: 'keys',
    label: '客户端 Key',
    icon: <KeyRound className="h-4 w-4" />,
  },
  {
    key: 'vendor',
    label: '供应商',
    icon: <Store className="h-4 w-4" />,
  },
]

export function SettingsPage() {
  const [urlState, patchUrl] = useUrlState('settings', { s: 'dispatch' })
  const active = (SECTIONS.some((x) => x.key === urlState.s)
    ? urlState.s
    : 'dispatch') as SectionKey
  // 供应商分区搬进来后，「未确认事件」红点跟着挂到侧栏该分区上
  const { data: vendorUnacked } = useVendorUnackedCount()

  // 页面级分区自带 PageHeader，设置页自己的页头此时不再叠一个
  const isPageSection = PAGE_SECTIONS.includes(active)

  return (
    <div className="console-scope space-y-4">
      {!isPageSection && (
        <PageHeader
          icon={<SlidersHorizontal className="h-4 w-4" />}
          title="设置"
          description="改动即时生效并写入 config.json，无需重启。"
        />
      )}

      <div className="flex flex-col gap-4 lg:flex-row">
        {/* 分区导航：桌面端竖排侧栏，窄屏横向滚动的胶囊行 */}
        <nav
          className="flex shrink-0 gap-1 overflow-x-auto px-1 py-1 lg:w-48 lg:flex-col lg:overflow-visible lg:px-0 lg:py-0 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
          aria-label="设置分区"
        >
          {SECTIONS.map((s) => (
            <button
              key={s.key}
              type="button"
              onClick={() => patchUrl({ s: s.key })}
              aria-current={active === s.key ? 'page' : undefined}
              className={cn(
                'inline-flex shrink-0 items-center gap-2 rounded-lg px-3 py-2 text-left text-[13px] transition-colors',
                'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40',
                active === s.key
                  ? 'bg-primary/12 font-medium text-foreground ring-1 ring-primary/25'
                  : 'text-muted-foreground hover:bg-accent/60 hover:text-foreground',
              )}
            >
              {s.icon}
              <span className="whitespace-nowrap">{s.label}</span>
              {s.key === 'vendor' && vendorUnacked ? (
                <span className="ml-auto inline-flex h-4 min-w-4 items-center justify-center rounded-full bg-destructive px-1 text-[10px] font-medium leading-none text-destructive-foreground">
                  {vendorUnacked}
                </span>
              ) : null}
            </button>
          ))}
        </nav>

        {isPageSection ? (
          <div className="min-w-0 flex-1">
            {active === 'groups' && <GroupsPage />}
            {active === 'keys' && <ClientKeysPage />}
            {active === 'vendor' && <VendorPage />}
          </div>
        ) : (
          <Card className="min-w-0 flex-1">
            <CardContent className="p-4 sm:p-5">
              {active === 'dispatch' && <DispatchSection />}
              {active === 'metadata' && <MetadataSection />}
              {active === 'models' && <ModelsSection />}
              {active === 'network' && <NetworkSection />}
              {active === 'log' && <LogSection />}
              {active === 'system' && <SystemSection />}
              {active === 'security' && <SecuritySection />}
            </CardContent>
          </Card>
        )}
      </div>
    </div>
  )
}
