use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// 工具兼容模式。
///
/// - `ClaudeCode`（默认）：把 Claude Code 内置工具（Write/Edit/Bash/Read/Glob/Grep/LS/WebSearch）
///   的工具名与入参双向适配为 Kiro 内置工具（fs_write/str_replace/... ），并替换为 Kiro 内置 schema。
/// - `Raw`：保留旧行为，直接透传客户端工具名/schema，用于排障。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCompatibilityMode {
    #[default]
    ClaudeCode,
    Raw,
}

/// 自定义模型定义。
///
/// 用户在 `config.json` 的 `customModels` 数组里声明客户端模型别名到 Kiro 后端
/// 模型 ID 的映射及元数据。运行期由 [`crate::model::custom_models`] 全局注册表按
/// `id`（大小写不敏感）精确匹配，优先于内置的模糊映射逻辑——既能新增模型，也能
/// 覆盖内置模型的映射。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModel {
    /// 客户端请求时使用的模型名（别名）。匹配大小写不敏感。
    pub id: String,

    /// 映射到的 Kiro 后端模型 ID（实际下发给上游）。
    pub backend_id: String,

    /// `/v1/models` 展示名（可选，缺省用 `id`）。
    #[serde(default)]
    pub display_name: Option<String>,

    /// 上下文窗口大小（可选，缺省 200000）。
    #[serde(default)]
    pub context_window: Option<i32>,

    /// 单次响应最大 token 数，用于 `/v1/models` 展示（可选，缺省 64000）。
    #[serde(default)]
    pub max_tokens: Option<i32>,

    /// 是否支持原生 reasoning / `output_config`（可选，缺省 false）。
    /// 命中的自定义模型置 true 时，会按 backend_id 放行 `additionalModelRequestFields`。
    #[serde(default)]
    pub supports_reasoning: Option<bool>,

    /// `/v1/models` 的 `owned_by` 字段（可选，缺省 "custom"）。
    #[serde(default)]
    pub owned_by: Option<String>,
}

/// 卖家（Key 供应商）对接配置。
///
/// 覆盖两个方向：
/// - **入站**：卖家把 `new_keys_available` / `all_keys_dead` 事件 POST 到
///   `/webhook/vendor/{webhook_path_token}`。对方推送端不带签名，故用不可猜测的
///   路径段做唯一凭证，比对不上直接 404。入站只负责落库 + 告警，不触发任何扣费动作。
/// - **出站**：kiro.rs 拿 `api_key` 调卖家提取接口提取 Key、查库存余额、兑换充值。
///   路径前缀与鉴权头形态由 `flavor` 决定，详见 [`crate::vendor::protocol::VendorFlavor`]。
///
/// 可配置多家：用 `vendors` 数组，每家一个本配置对象。旧的单例 `vendor` 字段
/// 仍然可用（等价于只有一家），见 [`Config::resolved_vendors`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VendorConfig {
    /// 供应商标识。事件按它归属，面板按它切换标签页，**配好后不要再改** ——
    /// 改了会让历史事件与新配置对不上。缺省为 `default`，与单供应商时期的
    /// 存量事件保持一致。
    #[serde(default = "default_vendor_id")]
    pub id: String,

    /// 面板上展示的名字。缺省用 `id`。
    #[serde(default)]
    pub name: String,

    /// 协议风味：`legacy`（`/api/my/*` + `X-API-Key`）或
    /// `kiroapp`（`/api/me/*` + `Authorization: Bearer`）。缺省 `legacy`。
    #[serde(default)]
    pub flavor: crate::vendor::protocol::VendorFlavor,

    /// 卖家 API base URL（如 `https://vendor.example.com`，末尾斜杠会被规整掉）
    pub base_url: String,

    /// 卖家账号密钥。发送形态由 `flavor` 决定：`legacy` 用 `X-API-Key: usr-xxx`，
    /// `kiroapp` 用 `Authorization: Bearer km_xxx`。
    pub api_key: String,

    /// 入站 webhook 路径 token。请求路径需完整匹配，否则 404。
    /// 为空视为入站未启用（出站接口仍可用）。
    #[serde(default)]
    pub webhook_path_token: String,

    /// 手动提取入库时默认写入的凭据分组（可选）。提取弹窗仅展示，不在弹窗内编辑。
    #[serde(default)]
    pub default_groups: Vec<String>,

    /// 手动提取入库时默认的每分钟请求数上限（默认 10，与新增凭据保持一致）
    #[serde(default = "default_vendor_rpm_limit")]
    pub default_rpm_limit: u32,

    /// 提取入库时写入凭据的 API Region（默认不写）。
    ///
    /// 卖家 Key 是 API Key 凭据，`effective_api_region` 只看凭据的 `apiRegion`
    /// 再回退全局 config，**不回退凭据的 `region` 字段**，所以想让入库的 Key 固定
    /// 落在某个区域，必须显式配这一项。缺省/空串表示不写该字段、沿用全局 region，
    /// 与未引入本配置项时的行为一致。
    #[serde(default)]
    pub default_api_region: String,

    /// 提取入库时写入凭据的 Auth Region（默认不写）。
    ///
    /// 用于 Token 刷新域名（`prod.{region}.auth.desktop.kiro.dev`）。纯 API Key
    /// 凭据不走刷新流程，这一项对它们不起作用；显式配置是为了让凭据的区域归属
    /// 完整、且卖家日后改发 OAuth 凭据时行为一致。缺省/空串表示不写、沿用全局。
    #[serde(default)]
    pub default_auth_region: String,

    /// 提取模式：true = 自动，false = 手动（默认）。
    ///
    /// 默认手动 —— 提取会真实扣费，且提取数量一旦提交就与订单号永久绑定
    /// （卖家侧改数量返回 409），把这个决策交给程序需要用户显式开启。
    /// 运行时可在管理面板切换，切换会写回本文件。
    #[serde(default)]
    pub auto_purchase: bool,

    /// 自动模式下单次提取的数量上限（默认 1）。
    ///
    /// 实际提取数量 = `min(事件声明的 newKeys, 卖家当前可提取上限, 本值)`。
    /// 取最小值是因为自动模式没有人工复核，而数量一旦绑定就无法改小。
    ///
    /// 配了 `autoPurchaseSchedule` 且当前时刻命中某段时，以该段的 `maxCount`
    /// 为准，本值退化为未命中时段时的兜底。
    #[serde(default = "default_vendor_auto_max_count")]
    pub auto_purchase_max_count: u32,

    /// 按时段调整自动提取上限（可选）。空表示全天都用 `autoPurchaseMaxCount`。
    ///
    /// 用于「下午与晚上压力大、需多持有一张 Key」这类规律性需求。时刻按**本地
    /// 时区**判定（同 usageStats，容器内需正确设置 `TZ`）。
    #[serde(default)]
    pub auto_purchase_schedule: Vec<crate::vendor::schedule::AutoPurchaseWindow>,
}

fn default_vendor_auto_max_count() -> u32 {
    1
}

fn default_vendor_rpm_limit() -> u32 {
    10
}

/// 单供应商时期的隐式 id。存量事件按它回填，故不能改。
pub const DEFAULT_VENDOR_ID: &str = "default";

fn default_vendor_id() -> String {
    DEFAULT_VENDOR_ID.to_string()
}

impl VendorConfig {
    /// 规整后的 base URL（去掉末尾斜杠）
    pub fn normalized_base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// 规整后的供应商 id。为空时回退到默认 id。
    pub fn vendor_id(&self) -> &str {
        let trimmed = self.id.trim();
        if trimmed.is_empty() {
            DEFAULT_VENDOR_ID
        } else {
            trimmed
        }
    }

    /// 面板展示名。未配置时回退到 id。
    pub fn display_name(&self) -> &str {
        let trimmed = self.name.trim();
        if trimmed.is_empty() {
            self.vendor_id()
        } else {
            trimmed
        }
    }

    /// 出站接口是否可用（base_url 与 api_key 均非空）
    pub fn outbound_enabled(&self) -> bool {
        !self.normalized_base_url().is_empty() && !self.api_key.trim().is_empty()
    }

    /// 入站 webhook 是否可用（出站可用且路径 token 非空）
    pub fn inbound_enabled(&self) -> bool {
        self.outbound_enabled() && !self.webhook_path_token.trim().is_empty()
    }
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 上一次成功更新前正在运行的版本号，用于在前端展示「回退到 vX.Y.Z」按钮。
    /// 实际回退动作通过 `<exe>.backup` 文件完成，无需访问网络。
    #[serde(default)]
    pub update_previous_version: Option<String>,

    /// GitHub Personal Access Token（可选）。设置后 GitHub Releases 接口会带上
    /// `Authorization: Bearer <token>`，把限流从匿名 60/h 提到认证 5000/h。
    /// 仅需 `public_repo` 读取权限即可。
    #[serde(default)]
    pub github_token: Option<String>,

    /// 上一次成功完成在线更新的时间（RFC3339）。前端用于显示「上次更新于 …」。
    #[serde(default)]
    pub update_last_applied_at: Option<String>,

    /// 是否启用无人值守自动更新。开启后服务会在每天的 `update_auto_apply_time`
    /// 时刻检查 GitHub Releases，发现新版本即自动下载二进制并替换重启。
    #[serde(default)]
    pub update_auto_apply: bool,

    /// 自动更新的每日触发时间（本地时区，`HH:MM` 24 小时制）。
    /// 默认 03:00 凌晨执行，对在线服务影响最小。
    #[serde(default = "default_update_auto_apply_time")]
    pub update_auto_apply_time: String,

    /// 负载均衡模式（"priority" 或 "balanced"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 账号级 429 风控触发时是否对当前凭据进入冷却并故障转移（默认 true）。
    ///
    /// 关闭后：429 + suspicious activity 仍按普通瞬态错误重试，不切换凭据。
    /// 开启后：识别到 suspicious activity 字符串时，把当前凭据冷却 `account_throttle_cooldown_secs` 秒，
    /// 立即切换到下一个可用凭据。
    #[serde(default = "default_account_throttle_failover")]
    pub account_throttle_failover: bool,

    /// 账号级风控冷却时长（秒，默认 1800 = 30 分钟）。
    #[serde(default = "default_account_throttle_cooldown_secs")]
    pub account_throttle_cooldown_secs: u64,

    /// 是否识别 403 账号封禁文案并立即禁用凭据（默认 true）。
    ///
    /// 开启后：某凭据收到 403 且响应体命中明确封禁文案（同时含 "suspended" 与
    /// "locked your account"）时，立即标记为 `Suspended` 并禁用。这类凭据**不参与
    /// 自愈**，需人工联系客服核实后手动重置，从根上打断持续 403 死循环（issue #51）。
    ///
    /// 只匹配这两个高特异短语同时出现的情形，不影响普通 403（权限/WAF/区域抖动），
    /// 后者仍按既有 `report_failure` 累计路径处理。关闭后：完全回退旧行为。
    #[serde(default = "default_suspended_detection_enabled")]
    pub suspended_detection_enabled: bool,

    /// 是否启用凭据自愈（默认 true）。
    ///
    /// 当前请求的 model/group 作用域没有可用凭据时，只恢复该作用域内因
    /// `TooManyFailures` 被自动禁用且仍满足冷却/上限的凭据。
    #[serde(default = "default_self_heal_enabled")]
    pub self_heal_enabled: bool,

    /// 同一凭据两次自愈之间的最小冷却间隔（秒，默认 300 = 5 分钟）。
    ///
    /// 冷却窗口内即使再次全灭也不触发自愈。这是打断 issue #51「全禁 → 自愈 →
    /// 403 → 再禁」死循环的关键：持续故障时自愈频率被限到每 5 分钟一次，
    /// 而非每个请求都重置刷屏并无效打上游。
    #[serde(default = "default_self_heal_min_interval_secs")]
    pub self_heal_min_interval_secs: u64,

    /// 连续自愈的最大轮数（默认 5，`0` 表示不限）。
    ///
    /// 同一凭据连续自愈达到此值且同一模型期间没有成功调用时，停止自愈并记录
    /// 错误日志提示人工介入。其它凭据、分组或模型的成功不会清零该计数。
    #[serde(default = "default_self_heal_max_consecutive_rounds")]
    pub self_heal_max_consecutive_rounds: u32,

    /// 按凭据缓存上游可用模型列表的 TTL（秒，默认 3600）。
    #[serde(default = "default_model_cache_ttl_secs")]
    pub model_cache_ttl_secs: u64,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 工具兼容模式。默认 `claude-code`：把 Claude Code 内置工具名/入参双向适配为
    /// Kiro 内置工具；`raw` 保留旧行为、直接透传客户端工具 schema，用于排障。
    #[serde(default = "default_tool_compatibility_mode")]
    pub tool_compatibility_mode: ToolCompatibilityMode,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 是否启用请求链路追踪（写 traces.db）。默认 true。
    ///
    /// 关闭后：不再写入 trace 记录、不走 TraceSink，但 `GET /api/admin/traces`
    /// 仍可查询历史已存记录。适合隐私敏感或磁盘紧张的场景。
    #[serde(default = "default_trace_enabled")]
    pub trace_enabled: bool,

    /// 请求链路追踪记录保留天数（默认 7）。后台任务每天清理超期记录。
    #[serde(default = "default_trace_retention_days")]
    pub trace_retention_days: u32,

    /// 请求用量日志（usage_log.*.jsonl + 聚合桶）保留天数（默认 31）。
    #[serde(default = "default_usage_log_retention_days")]
    pub usage_log_retention_days: u32,

    /// 卖家（Key 供应商）对接配置 —— 单供应商写法，保留兼容。
    /// 多家请用 `vendors`；两者同时存在时本字段等价于 `vendors` 的第一项之前，
    /// 合并规则见 [`Config::resolved_vendors`]。未配置时 webhook 端点与出站接口均不启用。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<VendorConfig>,

    /// 多卖家对接配置。每家一个对象，靠 `id` 区分、`flavor` 决定协议形态。
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub vendors: Vec<VendorConfig>,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 自定义模型映射表。
    ///
    /// 每条把一个客户端模型别名映射到 Kiro 后端模型 ID 并附带元数据。默认空数组
    /// （完全向后兼容）。启动时装入 [`crate::model::custom_models`] 全局注册表，
    /// 供 `map_model` / `get_context_window_size` / `/v1/models` 查询。
    #[serde(default)]
    pub custom_models: Vec<CustomModel>,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "2.3.0".to_string()
}

fn default_system_version() -> String {
    "macos".to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    "priority".to_string()
}

fn default_account_throttle_failover() -> bool {
    true
}

fn default_account_throttle_cooldown_secs() -> u64 {
    30 * 60
}

fn default_suspended_detection_enabled() -> bool {
    true
}

fn default_self_heal_enabled() -> bool {
    true
}

fn default_self_heal_min_interval_secs() -> u64 {
    5 * 60
}

fn default_self_heal_max_consecutive_rounds() -> u32 {
    5
}

fn default_model_cache_ttl_secs() -> u64 {
    60 * 60
}

fn default_update_auto_apply_time() -> String {
    "03:00".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_tool_compatibility_mode() -> ToolCompatibilityMode {
    ToolCompatibilityMode::ClaudeCode
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

fn default_trace_enabled() -> bool {
    true
}

fn default_trace_retention_days() -> u32 {
    7
}

fn default_usage_log_retention_days() -> u32 {
    31
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            update_previous_version: None,
            github_token: None,
            update_last_applied_at: None,
            update_auto_apply: false,
            update_auto_apply_time: default_update_auto_apply_time(),
            load_balancing_mode: default_load_balancing_mode(),
            account_throttle_failover: default_account_throttle_failover(),
            account_throttle_cooldown_secs: default_account_throttle_cooldown_secs(),
            suspended_detection_enabled: default_suspended_detection_enabled(),
            self_heal_enabled: default_self_heal_enabled(),
            self_heal_min_interval_secs: default_self_heal_min_interval_secs(),
            self_heal_max_consecutive_rounds: default_self_heal_max_consecutive_rounds(),
            model_cache_ttl_secs: default_model_cache_ttl_secs(),
            extract_thinking: default_extract_thinking(),
            tool_compatibility_mode: default_tool_compatibility_mode(),
            default_endpoint: default_endpoint(),
            trace_enabled: default_trace_enabled(),
            trace_retention_days: default_trace_retention_days(),
            usage_log_retention_days: default_usage_log_retention_days(),
            vendor: None,
            vendors: Vec::new(),
            endpoints: HashMap::new(),
            custom_models: Vec::new(),
            config_path: None,
        }
    }
}

impl Config {
    /// 合并 `vendor`（单例，兼容）与 `vendors`（多家）为最终生效列表。
    ///
    /// 规则：
    /// 1. 单例 `vendor` 排在前面 —— 它是存量配置，其事件已按 `default` id 落库。
    /// 2. 按 `id` 去重，保留先出现的。两家共用一个 id 会让事件互相污染、
    ///    webhook 也无法区分来源，故重复项直接丢弃并告警，而不是静默合并。
    /// 3. 出站配置不完整（base_url / api_key 为空）的项一并丢弃 —— 留着只会
    ///    在面板上多一个永远报错的标签页。
    pub fn resolved_vendors(&self) -> Vec<VendorConfig> {
        let mut out: Vec<VendorConfig> = Vec::new();
        let candidates = self.vendor.iter().chain(self.vendors.iter());

        for cfg in candidates {
            if !cfg.outbound_enabled() {
                tracing::warn!(
                    vendor_id = cfg.vendor_id(),
                    "卖家配置不完整（baseUrl / apiKey 为空），已跳过"
                );
                continue;
            }
            if out.iter().any(|c| c.vendor_id() == cfg.vendor_id()) {
                tracing::warn!(
                    vendor_id = cfg.vendor_id(),
                    "卖家 id 重复，已跳过后出现的那一项（事件归属需唯一）"
                );
                continue;
            }
            out.push(cfg.clone());
        }
        out
    }

    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());

        // 用户手工把字符串字段清空（如 `"updateAutoApplyTime": ""`）时，serde 默认值不会
        // 介入；这里把"看起来像空"的关键字段回退到默认值，避免后续业务用到
        // 空字符串导致难以诊断的错误。
        if config.update_auto_apply_time.trim().is_empty() {
            config.update_auto_apply_time = default_update_auto_apply_time();
        }

        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(path, content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn model_cache_ttl_defaults_for_existing_configs() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.model_cache_ttl_secs, 3600);
        assert_eq!(Config::default().model_cache_ttl_secs, 3600);
    }

    #[test]
    fn model_cache_ttl_accepts_explicit_value() {
        let config: Config = serde_json::from_str(r#"{"modelCacheTtlSecs":120}"#).unwrap();
        assert_eq!(config.model_cache_ttl_secs, 120);
    }

    #[test]
    fn self_heal_config_defaults_for_existing_configs() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.suspended_detection_enabled);
        assert!(config.self_heal_enabled);
        assert_eq!(config.self_heal_min_interval_secs, 300);
        assert_eq!(config.self_heal_max_consecutive_rounds, 5);

        let default = Config::default();
        assert!(default.suspended_detection_enabled);
        assert!(default.self_heal_enabled);
        assert_eq!(default.self_heal_min_interval_secs, 300);
        assert_eq!(default.self_heal_max_consecutive_rounds, 5);
    }

    #[test]
    fn self_heal_config_accepts_explicit_values() {
        let config: Config = serde_json::from_str(
            r#"{
                "suspendedDetectionEnabled": false,
                "selfHealEnabled": false,
                "selfHealMinIntervalSecs": 60,
                "selfHealMaxConsecutiveRounds": 0
            }"#,
        )
        .unwrap();
        assert!(!config.suspended_detection_enabled);
        assert!(!config.self_heal_enabled);
        assert_eq!(config.self_heal_min_interval_secs, 60);
        assert_eq!(config.self_heal_max_consecutive_rounds, 0);
    }
}

/// 本地新增：卖家提取入库的 Region 配置。单独成块，避免与上游测试模块相撞。
#[cfg(test)]
mod vendor_api_region_tests {
    use super::VendorConfig;

    #[test]
    fn 未配置时不写region沿用全局() {
        let cfg: VendorConfig =
            serde_json::from_str(r#"{"baseUrl":"https://v.example.com","apiKey":"usr-x"}"#).unwrap();
        assert!(cfg.default_api_region.is_empty());
        assert!(cfg.default_auth_region.is_empty());
    }

    #[test]
    fn auth与api的region可分别配置() {
        let cfg: VendorConfig = serde_json::from_str(
            r#"{"baseUrl":"https://v.example.com","apiKey":"usr-x","defaultAuthRegion":"us-east-1"}"#,
        )
        .unwrap();
        assert_eq!(cfg.default_auth_region, "us-east-1");
        // 未显式配置的一项保持不写
        assert!(cfg.default_api_region.is_empty());
    }

    #[test]
    fn 显式配置覆盖默认值() {
        let cfg: VendorConfig = serde_json::from_str(
            r#"{"baseUrl":"https://v.example.com","apiKey":"usr-x","defaultApiRegion":"us-east-1"}"#,
        )
        .unwrap();
        assert_eq!(cfg.default_api_region, "us-east-1");
    }

    #[test]
    fn 显式空串同样表示沿用全局region() {
        let cfg: VendorConfig = serde_json::from_str(
            r#"{"baseUrl":"https://v.example.com","apiKey":"usr-x","defaultApiRegion":""}"#,
        )
        .unwrap();
        assert!(cfg.default_api_region.is_empty());
    }
}
