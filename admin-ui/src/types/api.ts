// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  /** 优先级模式下的当前优先凭据 ID；均衡模式为 0 */
  currentId: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
  /** 每分钟请求数上限（0 = 不限速） */
  rpmLimit: number
  /** 当前 60 秒滑动窗口内已使用的请求数 */
  rpmCurrent: number
  disabled: boolean
  failureCount: number
  /** 累计失败次数（所有失败类型，只增不减，仅手动重置归零） */
  totalFailureCount: number
  /** 是否为优先级模式下的当前优先凭据；均衡模式恒为 false */
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  provider?: string | null
  hasProfileArn: boolean
  email?: string
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  refreshFailureCount: number
  disabledReason?: string
  /** 账号级风控冷却剩余秒数（>0 表示冷却中） */
  throttledRemainingSecs?: number
  endpoint: string
  /** 账号所属分组（可属于多个分组） */
  groups?: string[]
  /** 账号来源渠道（纯备注） */
  sourceChannel?: string
  /** 各模型 TTFT EWMA 的均值（毫秒），无样本时缺省 */
  ttftEwmaMs?: number
  /** 后端缓存的最近一次余额（5 分钟内） */
  balance?: BalanceResponse
  /** 余额缓存的更新时间（Unix 秒） */
  balanceUpdatedAt?: number
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
  /** 用户是否当前开启了超额 */
  overageEnabled?: boolean
  /** 账号订阅是否可以开启超额 */
  overageCapable?: boolean
  /** 上游 overageCapability 原始字符串，用于排查"未知"状态 */
  overageCapabilityRaw?: string
}

// 某凭据当前可用的模型列表响应
export interface AvailableModelsResponse {
  id: number
  selectionMode: 'specified' | 'priority' | 'balanced'
  models: AvailableModelItem[]
}

// 单个可用模型
export interface AvailableModelItem {
  modelId: string
  modelName?: string
  description?: string
  maxInputTokens?: number
  maxOutputTokens?: number
}

// 真实模型请求测试结果
export interface ModelTestResponse {
  modelId: string
  credentialId: number
  latencyMs: number
  responseText: string
  creditUsage?: number
  creditUnit?: string
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

// 添加凭据请求
export interface AddCredentialRequest {
  refreshToken?: string
  accessToken?: string
  profileArn?: string
  expiresAt?: string
  authMethod?: 'social' | 'idc' | 'api_key' | 'external_idp'
  provider?: string
  clientId?: string
  clientSecret?: string
  startUrl?: string
  /** 企业 SSO (external_idp) 的 OAuth2 Token 端点（external_idp 必填） */
  tokenEndpoint?: string
  /** 企业 SSO 的 OIDC Issuer URL（可选） */
  issuerUrl?: string
  /** 企业 SSO 授予的 scopes（空格分隔，可选） */
  scopes?: string
  priority?: number
  /** 每分钟请求数上限（默认 10；0 表示不限速） */
  rpmLimit?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  kiroApiKey?: string
  endpoint?: string
  email?: string
  groups?: string[]
  sourceChannel?: string
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}

// 更新凭据请求（字段为 undefined 表示不修改，空字符串表示清除）
export interface UpdateCredentialRequest {
  email?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  /** 账号所属分组（undefined 表示不修改，数组表示整体替换） */
  groups?: string[]
  /** 账号来源渠道（undefined 表示不修改，空串表示清除） */
  sourceChannel?: string
  /** 每分钟请求数上限（undefined 表示不修改，0 表示不限速） */
  rpmLimit?: number
}

// 更新 refreshToken 请求
export interface UpdateRefreshTokenRequest {
  refreshToken: string
  accessToken?: string
  expiresAt?: string
}

// 代理健康状态
export type ProxyHealth = 'unknown' | 'healthy' | 'unhealthy'

// 代理池条目
export interface ProxyPoolEntry {
  id: number
  url: string
  label?: string
  enabled: boolean
  credentialCount: number
  health: ProxyHealth
  latencyMs?: number
  lastCheckedAt?: string
  consecutiveFailures: number
  autoDisabled: boolean
}

// 代理池列表响应
export interface ProxyPoolResponse {
  total: number
  proxies: ProxyPoolEntry[]
}

// 添加代理请求
export interface AddProxyRequest {
  url: string
  label?: string
}

// 批量添加代理请求
export interface BatchAddProxyRequest {
  urls: string[]
}

// 分配代理给凭据请求
export interface AssignProxyRequest {
  proxyId?: number | null
}

// 批量添加代理响应
export interface BatchAddProxyResponse {
  added: number
  errors: number
  proxies: ProxyPoolEntry[]
  errorMessages: string[]
}

// 单个代理健康检查响应
export interface ProxyCheckResponse {
  id: number
  health: ProxyHealth
  latencyMs?: number
  lastCheckedAt?: string
  enabled: boolean
  autoDisabled: boolean
}

// 全量健康检查响应
export interface ProxyCheckAllResponse {
  healthy: number
  unhealthy: number
  autoDisabled: number
}

// 轮询批量分配请求
export interface AssignRoundRobinRequest {
  credentialIds?: number[] | null
}

// 轮询批量分配响应
export interface AssignRoundRobinResponse {
  assigned: number
  proxyCount: number
}

// 全局代理配置
export interface GlobalProxyResponse {
  proxyUrl: string | null
}

export interface SetGlobalProxyRequest {
  proxyUrl: string | null
}

// 在线更新配置
export interface UpdateConfigResponse {
  /** 上一次更新前正在运行的版本号（带 v 前缀）；存在时可调用回退接口 */
  previousVersion?: string
  /** 上一次成功完成在线更新的时间（RFC3339） */
  lastAppliedAt?: string
  /** 是否已配置 GitHub Token（仅返回布尔，不回明文） */
  githubTokenSet: boolean
  /** 是否开启无人值守自动更新 */
  autoApply: boolean
  /** 自动更新触发时间（本地时区，HH:MM 24 小时制） */
  autoApplyTime: string
}

export interface SetUpdateConfigRequest {
  /** GitHub Personal Access Token；空字符串表示清除 */
  githubToken?: string
  autoApply?: boolean
  autoApplyTime?: string
}

/** GitHub API 限流状态（含 token 验证结果） */
export interface GitHubRateLimitInfo {
  /** 提供的 token 是否有效（无 token 时为 false 但仍能查到匿名限额） */
  valid: boolean
  /** 是否带 token 调用（false = 匿名查询） */
  authenticated: boolean
  /** 限流上限（匿名 60，认证 5000） */
  limit: number
  /** 剩余可用次数 */
  remaining: number
  /** 已用次数 */
  used: number
  /** 限流窗口重置时间（Unix 秒） */
  reset: number
  /** token 对应的用户名（可能为空） */
  login?: string
  /** 失败时的提示信息 */
  warning?: string
}

export interface ImageUpdateResponse {
  success: boolean
  message: string
  output?: string
  applied: boolean
  needRestart: boolean
}

export interface UpdateCheckInfo {
  currentVersion: string
  latestVersion: string
  hasUpdate: boolean
  buildType: string
  releaseName?: string
  releaseNotes?: string
  releaseUrl?: string
  publishedAt?: string
  checkedAt: string
  cached: boolean
  warning?: string
}

// 登录API密钥修改（adminApiKey —— 管理面板登录密钥）
export interface UpdateAdminKeyRequest {
  newKey: string
}

// IdC 设备授权登录
export interface StartIdcLoginRequest {
  region: string
  startUrl?: string
  priority?: number
  email?: string
  proxyUrl?: string
}

export interface StartIdcLoginResponse {
  sessionId: string
  userCode: string
  verificationUri: string
  verificationUriComplete?: string
  expiresAt: string
  pollInterval: number
}

export type PollIdcLoginResponse =
  | { status: 'pending' }
  | { status: 'success'; credentialId: number }
  | { status: 'expired' }

// Social 登录（Portal PKCE OAuth）
export interface StartSocialLoginRequest {
  priority?: number
  email?: string
  proxyUrl?: string
  authEndpoint?: string
}

/** 远程访问时手动完成 Social 登录：从浏览器地址栏粘贴的回调 URL 中提取参数 */
export interface CompleteSocialLoginRequest {
  code: string
  state: string
  loginOption?: string
  path?: string
}

export interface StartSocialLoginResponse {
  sessionId: string
  portalUrl: string
  expiresAt: string
}

export type PollSocialLoginResponse = PollIdcLoginResponse

// ============ 客户端 API Key 分发 ============

export interface ClientKeyItem {
  id: number
  /** 脱敏后的 Key（仅展示） */
  maskedKey: string
  name: string
  description?: string
  disabled: boolean
  createdAt: string
  lastUsedAt?: string
  totalCalls: number
  totalInputTokens: number
  totalOutputTokens: number
  totalCacheCreationTokens: number
  totalCacheReadTokens: number
  /** 绑定的账号分组（未绑定时为 undefined） */
  group?: string
  /** 是否系统密钥（由 config.json apiKey 同步，不可删除、可轮换） */
  isSystem: boolean
}

export interface ClientKeysResponse {
  total: number
  keys: ClientKeyItem[]
}

export interface CreateClientKeyRequest {
  name: string
  description?: string
  group?: string
}

/** 创建响应：明文 Key 仅在此处返回一次 */
export interface CreateClientKeyResponse {
  id: number
  key: string
  name: string
  createdAt: string
}

export interface UpdateClientKeyRequest {
  name?: string
  description?: string
  group?: string
}

// ============ 用量统计 ============

export type StatsRange = '24h' | '7d' | '30d'
export type StatsGranularity = 'hour' | 'day'

export interface StatsTimeFilter {
  range?: StatsRange
  startDate?: string
  endDate?: string
  granularity: StatsGranularity
}

export interface StatsFilter {
  /** 不传 = 全部；其它值 = 客户端 Key id */
  keyId?: number
  /** 按账号分组筛选（仅影响 timeseries / by-credential，by-model 不支持） */
  group?: string
}

export interface OverviewStats {
  todayCalls: number
  todayInputTokens: number
  todayOutputTokens: number
  todayErrors: number
  todayCredits: number
  weekCalls: number
  weekInputTokens: number
  weekOutputTokens: number
  weekCredits: number
  activeClientKeys: number
  activeCredentials: number
  /** 最近 1 / 5 分钟报错数（整条请求最终失败，来自 trace 库） */
  errors1m: number
  errors5m: number
  /** 最近 1 / 5 分钟重试数（首次尝试之外的重投跳数） */
  retries1m: number
  retries5m: number
  /** trace 是否启用；关闭时上面 4 个近窗口计数不再更新 */
  traceEnabled: boolean
}

export interface TimeSeriesPoint {
  ts: string
  inputTokens: number
  outputTokens: number
  cacheCreationTokens: number
  cacheReadTokens: number
  calls: number
  errors: number
  credits: number
}

export interface ModelDistribution {
  model: string
  calls: number
  inputTokens: number
  outputTokens: number
}

export interface CredentialDistribution {
  credentialId: number
  email?: string
  calls: number
  inputTokens: number
  outputTokens: number
  errors: number
}

// ============ 请求链路追踪 ============

/** 单次上游尝试 */
export interface TraceAttempt {
  attempt: number
  credentialId: number
  email?: string | null
  endpoint: string
  /** 上游 HTTP 状态码；null = 网络层失败 */
  httpStatus: number | null
  /** success / quota_exhausted / account_throttled / auth_failed / transient / network_error / bad_request / unknown */
  outcome: string
  /** 上游错误体片段（已截断） */
  errorSnippet: string | null
  durationMs: number
}

/** 一个外部请求的完整链路 */
export interface TraceRecord {
  traceId: string
  ts: string
  keyId: number
  /** masterApiKey = 历史 master 调用（已下线）；clientKey = 客户端 Key */
  keySource: 'masterApiKey' | 'clientKey'
  /** 发起请求的客户端 Key 名称（master 表示主 apiKey；管理员业务 Key 可为 null） */
  keyName?: string | null
  model: string
  isStream: boolean
  /** success / error / interrupted */
  finalStatus: string
  finalCredentialId: number
  finalEmail?: string | null
  errorType: string | null
  errorMessage: string | null
  totalAttempts: number
  durationMs: number
  /** 流式中断时已发送字节数 */
  interruptedAfterBytes: number | null
  /** 输入 token */
  inputTokens?: number
  /** 输出 token */
  outputTokens?: number
  /** 缓存创建 token */
  cacheCreationTokens?: number
  /** 缓存读取 token */
  cacheReadTokens?: number
  /** 总 token = input + output + cache_creation + cache_read */
  totalTokens?: number
  /** 费用（credits） */
  credits?: number
  /** 首 Token 延迟（毫秒，仅流式有值） */
  firstTokenMs?: number | null
  /** 推理思考级别（low / medium / high / max / xhigh，仅 effort 请求时有值） */
  effort?: string | null
  attempts: TraceAttempt[]
}

/** 链路查询参数 */
export interface TraceQuery {
  status?: string
  errorType?: string
  credentialId?: number
  /** 按发起请求的客户端 Key 筛选（0 = master apiKey） */
  keyId?: number
  /** 该凭据在某一跳失败过（即便 trace 最终成功）——用于凭据失败详情 */
  failedAttemptCredentialId?: number
  model?: string
  /** 按账号分组名筛选（只返回 final_credential_id 属于该分组的 trace） */
  group?: string
  onlyFailed?: boolean
  limit?: number
  offset?: number
}

/** 分页响应 */
export interface TracePage {
  records: TraceRecord[]
  total: number
}

/** 单凭据失败分类计数（鉴权 / 账号风控 / 其他） */
export interface FailureStats {
  auth: number
  throttle: number
  other: number
}

/** credentialId(字符串) → 失败分类计数 */
export type FailureStatsMap = Record<string, FailureStats>

// ============ 账号分组（独立实体）============

export interface GroupItem {
  name: string
  description?: string
  createdAt: string
  /** 引用计数：有多少个凭据带这个分组 */
  credentialCount: number
  /** 引用计数：有多少把客户端 Key 绑定这个分组 */
  clientKeyCount: number
}

export interface GroupsResponse {
  total: number
  groups: GroupItem[]
}

export interface CreateGroupRequest {
  name: string
  description?: string
}

export interface UpdateGroupRequest {
  /** 新名字；不传或与原名一致则不改名 */
  newName?: string
  /** 新备注；空字符串清除；undefined 保留原值 */
  description?: string
}

// ============ 卖家（Key 供应商）对接 ============

/** 单个卖家支持的能力集。前端据此决定是否展示对应卡片。 */
export interface VendorCapabilities {
  systemStatus: boolean
  genLogs: boolean
  webhookManage: boolean
  purchaseOrders: boolean
  redeem: boolean
  ledger: boolean
  myKeys: boolean
  earliestKey: boolean
  batchScopedPurchase: boolean
  tieredPricing: boolean
  /** 分区库存：库存按区隔离、各区单价独立，下单需指定 zone */
  zonedPurchase: boolean
}

/** 单个区域的库存与报价（仅 zonedPurchase 能力的卖家有） */
export interface VendorZoneStock {
  /** 区域代码，下单时原样回传，如 us / eu */
  zone: string
  /** 人类可读名称，如「美国区」。缺失时回退显示 zone。 */
  label?: string
  /** 本区当前可提取数量 */
  available: number
  /** 本区仓库存货数，可能大于 available（受单次上限压制） */
  stock?: number
  /** 本区单价。各区独立设置，不要硬编码。 */
  unitPrice?: number
  /** 本区是否开放。关闭的区即使有存货也提不出来。 */
  enabled: boolean
}

/** 卖家清单项 */
export interface VendorListItem {
  vendorId: string
  name: string
  /** kiroapp = kiroapp.io；kiroapp-cc = kiroapp.cc；drop = drop.kiro.ss；kiromarket = api.91kiro.com；kirored = kiro.red；kiro-ooo = kiro.ooo，互为不同卖家 */
  flavor: 'legacy' | 'kiroapp' | 'kiroapp-cc' | 'drop' | 'kiromarket' | 'kirored' | 'kiro-ooo'
  capabilities: VendorCapabilities
  inboundEnabled: boolean
  autoPurchase: boolean
  /** 逐渠道补货（逐家独立）：true = 只看本家存活，false = 按全局阈值判总量 */
  perChannel: boolean
  unacked: number
}

/** `GET /api/admin/vendor/vendors` 响应 */
export interface VendorListResponse {
  vendors: VendorListItem[]
  defaultVendorId?: string
  /**
   * 全局提取限制：池中存活的卖家 Key 达到此数即不再自动补货，0 = 不启用。
   *
   * 跨供应商共享，故随清单一起返回而不在按家查的 `/status` 里。
   */
  poolTarget?: number
}

/** 卖家账号档案（已统一为 camelCase） */
export interface VendorProfile {
  name?: string
  email?: string
  balance?: number
  quota?: number
  usedQuota?: number
  minPurchase?: number
  maxPurchase?: number
  webhookUrl?: string
  createdAt?: string
}

/** 顶部状态条（单个卖家的） */
export interface VendorStatus {
  vendorId: string
  name: string
  /** kiroapp = kiroapp.io；kiroapp-cc = kiroapp.cc；drop = drop.kiro.ss；kiromarket = api.91kiro.com；kirored = kiro.red；kiro-ooo = kiro.ooo，互为不同卖家 */
  flavor: 'legacy' | 'kiroapp' | 'kiroapp-cc' | 'drop' | 'kiromarket' | 'kirored' | 'kiro-ooo'
  capabilities: VendorCapabilities
  /** baseUrl + apiKey 均已配置，出站接口可用 */
  configured: boolean
  /** 额外配了路径 token，入站 webhook 可用 */
  inboundEnabled: boolean
  /** 未点「已知悉」的事件数 */
  unacked: number
  /** 提取入库时写入的默认分组 */
  defaultGroups: string[]
  defaultRpmLimit: number
  /** 提取入库时写入凭据的 apiRegion；空串表示沿用全局 region */
  defaultApiRegion?: string
  /** 提取入库时写入凭据的 authRegion；空串表示沿用全局 region */
  defaultAuthRegion?: string
  /** 提取模式：true = 自动，false = 手动。运行时值，切换后立即生效 */
  autoPurchase: boolean
  /**
   * 当前时刻实际生效的单次提取上限（已应用时段表）。
   * 实际数量 = min(newKeys, stockMax, 本值)
   */
  autoPurchaseMaxCount: number
  /** 未命中任何时段时的兜底上限，即 config 里的 autoPurchaseMaxCount */
  autoPurchaseBaseMaxCount?: number
  /** 当前命中的时段描述，如 `14:00–23:00`；未配时段表或未命中为 null */
  autoPurchaseWindow?: string | null
  /**
   * 逐渠道补货（运行时值，逐家独立）。
   *
   * - `true`：本家只看**自己**有没有存活 Key，没有就补，不看全局池量
   * - `false`：按 `poolTarget` 判池子总量，而该总量**包含**开着本项的那些家
   *
   * 不对称是刻意的。混合配置时 `poolTarget` 要大于「开着本项的家数」，
   * 否则那些家常驻的号会占满总量，关着的家永远轮不到补货。
   */
  autoPurchasePerChannel?: boolean
  profile?: VendorProfile
  /** 拉余额失败时的原因（不影响其余字段） */
  profileError?: string
  /** 库存与报价。stock 是新结构(带价格区间)，stockMax 是兼容字段 */
  stock?: {
    /**
     * 分区卖家这里是**各区之和**。它大于 0 只说明某个区有货，
     * 不代表任一指定区有货 —— 判断能否提取要看 zones。
     */
    available: number
    priceMin?: number
    priceMax?: number
    balance?: number
    /** 分区库存。为空表示该卖家不分区。 */
    zones?: VendorZoneStock[]
  }
  stockMax?: number
  stockError?: string
  /** 卖家 /api/status：存活 / 失效 / 存货 Key 数（仅部分卖家支持） */
  system?: VendorSystemStatus
  systemError?: string
  /** 卖家近期开号批次与平均间隔（仅部分卖家支持） */
  genLogs?: VendorGenLogs
  genLogsError?: string
  /** 最早密钥时间（仅部分卖家支持） */
  earliestKey?: { createdAt?: string; count?: number }
  earliestKeyError?: string
}


/** 切换提取模式的结果 */
export interface VendorModeChange {
  autoPurchase: boolean
  /** 是否已写回 config.json；false 表示重启后会回退到文件里的值 */
  persisted: boolean
  /** 持久化失败原因 */
  warning?: string
}

/** 设置全局提取限制的结果 */
export interface VendorPoolTargetChange {
  /** 设置后的阈值（运行时已生效）。0 = 不启用 */
  poolTarget: number
  /** 是否已写回 config.json；false 表示重启后会回退到文件里的值 */
  persisted: boolean
  /** 持久化失败原因 */
  warning?: string
}

/** 设置逐渠道补货模式的结果 */
export interface VendorPerChannelChange {
  /** 设置后的模式（运行时已生效） */
  perChannel: boolean
  /** 是否已写回 config.json；false 表示重启后会回退到文件里的值 */
  persisted: boolean
  /** 持久化失败原因 */
  warning?: string
}

/** 卖家 `/api/status` 返回的 Key 数量分布 */
export interface VendorSystemStatus {
  keys_active?: number | null
  keys_dead?: number | null
  /** 卖家侧尚未售出的存货 Key */
  keys_stock?: number | null
  /** 卖家侧 Key 累计总数（含已失效） */
  keys_total?: number | null
  /** 卖家侧是否正在生成新 Key */
  generating?: boolean | null
  /** 卖家侧已运行秒数 */
  uptime_seconds?: number | null
  /** 卖家侧启动时刻，形如 `2026-07-25 20:59:33`（无时区标记） */
  started_at?: string | null
  auto_check?: boolean | null
  auto_generate?: boolean | null
  /** 自动检测间隔，卖家用字符串给（如 "20"） */
  check_interval?: string | null
  /** 卖家未建模字段，后端 flatten 透传 */
  [key: string]: unknown
}

/** 卖家 `/api/my/gen-logs` 的一条开号批次 */
export interface VendorGenLogEntry {
  /** 开号时刻，形如 `2026-07-28 23:27:36`（无时区标记） */
  created_at?: string | null
  /** 该批开出的 Key 数 */
  count?: number | null
  /** 卖家侧状态，如 "done" */
  status?: string | null
}

/** 卖家 `/api/my/gen-logs` 返回 —— 近期开号批次与平均间隔 */
export interface VendorGenLogs {
  /** 相邻两批的平均间隔（分钟），不足两批时可能缺失 */
  avg_interval_min?: number | null
  items?: VendorGenLogEntry[]
}

/** 卖家推来的一条 webhook 事件 */
export interface VendorEvent {
  eventId: string
  /** new_keys_available / all_keys_dead / unknown */
  eventType: string
  /** 提取用订单号，必须原样作为 client_order_id */
  purchaseOrderId?: string
  message?: string
  newKeys?: number
  dead?: number
  receivedAt: string
  /** 同一事件被推送的次数，> 1 说明对方在重投 */
  deliveryCount: number
  acked: boolean
  /** 首次提交提取时绑定的数量；非空即不可更改 */
  boundCount?: number
  /** 首次提交时绑定的区域；与 boundCount 一起锁死，换区重试会再扣一次积分 */
  boundZone?: string
  /** done / failed / skipped；未提取过则为空 */
  purchaseStatus?: string
  purchased?: number
  imported?: number
  duplicated?: number
  failed?: number
  lastError?: string
  processedAt?: string
  /** manual / auto；未提取过则为空 */
  purchaseTrigger?: string
  /** 失效确认结论，仅 all_keys_dead 事件有值 */
  validationStatus?: 'pending' | 'confirmed_dead' | 'still_alive' | 'inconclusive'
  /** 确认结论的依据说明 */
  validationDetail?: string
  validatedAt?: string
  /** 该确认是否已被某次自动提取用掉 */
  validationUsed: boolean
}

export interface VendorEventsResponse {
  events: VendorEvent[]
  unacked: number
}

/** 卖家侧提取订单（对账用） */
export interface VendorOrder {
  clientOrderId?: string
  orderId?: string
  requested?: number
  purchased?: number
  totalDebit?: number
  createdAt?: string
}

export interface VendorOrdersResponse {
  orders: VendorOrder[]
}

/** 提取 + 入库结果 */
export interface VendorPurchaseResult {
  /** 本次绑定并提交的数量 */
  count: number
  /** 卖家回显的请求数。余额不足时 purchased < requested */
  requested?: number
  /** 卖家实际出 Key 数 */
  purchased: number
  /** 成功入库数 */
  imported: number
  /** 本地已存在而跳过 */
  duplicated: number
  failed: number
  /** 提取后卖家侧剩余余额 */
  remaining?: number
  /** 本单实际扣费总额（阶梯定价下的权威数字） */
  totalDebit?: number
  /** 本单实际均价 */
  unitPrice?: number
  /** 卖家侧订单 / 批次 id */
  orderId?: string
  /** 本单实际成交的区域。各区单价不同，必须展示。 */
  zone?: string
  /** true 表示本次是幂等重放，卖家未重复扣款 */
  replayed?: boolean
  /** 逐张 Key 的元数据（不含明文） */
  keys?: Array<{
    account?: string
    issuerUrl?: string
    price?: number
    hasPassword?: boolean
  }>
  error?: string
  /** 直接提取时服务端生成的订单号 */
  clientOrderId?: string
  /** 提取来自哪一家（直接提取时回显） */
  vendorId?: string
}

/** 兑换码充值结果 */
export interface VendorRedeemResult {
  code?: string
  quota?: number
  previous_quota?: number
  balance?: number
  created_by_name?: string
  redeemed_at?: string
  /** true 表示此前已兑换过，本次未改动余额 */
  replayed: boolean
}

// ============ 近 1 分钟额度消耗（本地扩展）============

/** 各凭据近 windowSecs 秒内消耗的 credits；spend 的 key 为 credentialId 字符串 */
export interface RecentSpendResponse {
  windowSecs: number
  spend: Record<string, number>
}
