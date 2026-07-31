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
    ///
    /// `inboundToken` 是别名：早期文档与 `config.example.json` 用的是这个名字，
    /// 而本结构没有 `deny_unknown_fields`，不加别名会让按旧文档配的人被静默
    /// 忽略成空串 —— 症状是 webhook 一直 404，极难定位。
    #[serde(default, alias = "inboundToken")]
    pub webhook_path_token: String,

    /// 手动提取入库时默认写入的凭据分组（可选）。提取弹窗仅展示，不在弹窗内编辑。
    #[serde(default)]
    pub default_groups: Vec<String>,

    /// 手动提取入库时默认的每分钟请求数上限（默认 300，与新增凭据保持一致）
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
    ///
    /// `autoPurchaseWindows` 是别名，原因同 [`VendorConfig::webhook_path_token`]。
    #[serde(default, alias = "autoPurchaseWindows")]
    pub auto_purchase_schedule: Vec<crate::vendor::schedule::AutoPurchaseWindow>,
}

fn default_vendor_auto_max_count() -> u32 {
    1
}

fn default_vendor_rpm_limit() -> u32 {
    300
}

/// 单供应商时期的隐式 id。存量事件按它回填，故不能改。
pub const DEFAULT_VENDOR_ID: &str = "default";

fn default_vendor_id() -> String {
    DEFAULT_VENDOR_ID.to_string()
}

/// **已废弃**的 kiroapp.cc 独立配置块，仅为兼容存量 `config.json` 保留。
///
/// 历史背景：kiroapp.cc 最早是作为「次级卖家」单独接入的，走自己的客户端与
/// 服务层，配置写在顶层 `kiroapp` 键下。后来协议抽象层落地，它成了普通的
/// [`VendorFlavor::KiroappCc`](crate::vendor::protocol::VendorFlavor::KiroappCc)，
/// 与其余卖家共用一套实现，独立那条路径已删除。
///
/// 顶层键名 `kiroapp` 是这段历史留下的**命名陷阱**：它指的是 kiroapp**.cc**，
/// 而 `flavor: "kiroapp"` 指的是 kiroapp**.io**，两者是不同的卖家。新配置请直接
/// 写进 `vendors` 数组并显式声明 `"flavor": "kiroapp-cc"`，不要再用这个块。
///
/// 存量配置由 [`Config::resolved_vendors`] 自动转换，无需手工迁移。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyKiroappCcConfig {
    /// API base URL（如 `https://kiroapp.cc`，末尾斜杠会被规整掉）
    pub base_url: String,

    /// 账号 API Key，以 `Authorization: Bearer` 发送
    pub api_key: String,

    /// 提取入库时默认写入的凭据分组（可选）
    #[serde(default)]
    pub default_groups: Vec<String>,

    /// 提取入库时默认的每分钟请求数上限（默认 300，与新增凭据保持一致）
    #[serde(default = "default_vendor_rpm_limit")]
    pub default_rpm_limit: u32,
}

/// 存量 kiroapp.cc 配置转换后占用的卖家 id。
///
/// 固定值而非 `default`：`default` 属于单供应商时期的存量事件，两者混用会让
/// 事件归属错乱。
pub const LEGACY_KIROAPP_CC_VENDOR_ID: &str = "kiroapp-cc";

impl LegacyKiroappCcConfig {
    /// 规整后的 base URL（去掉末尾斜杠）
    pub fn normalized_base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// 是否可用（base_url 与 api_key 均非空）
    pub fn enabled(&self) -> bool {
        !self.normalized_base_url().is_empty() && !self.api_key.trim().is_empty()
    }

    /// 转成标准 [`VendorConfig`]。
    ///
    /// kiroapp.cc 没有 webhook，故 `webhook_path_token` 留空（入站不启用）；
    /// 也不开自动提取 —— 对方 claim 无幂等键，保持与独立路径时期一致的保守默认。
    pub fn to_vendor_config(&self) -> VendorConfig {
        VendorConfig {
            id: LEGACY_KIROAPP_CC_VENDOR_ID.to_string(),
            name: "kiroapp.cc".to_string(),
            flavor: crate::vendor::protocol::VendorFlavor::KiroappCc,
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            webhook_path_token: String::new(),
            default_groups: self.default_groups.clone(),
            default_rpm_limit: self.default_rpm_limit,
            default_api_region: String::new(),
            default_auth_region: String::new(),
            auto_purchase: false,
            auto_purchase_max_count: default_vendor_auto_max_count(),
            auto_purchase_schedule: Vec::new(),
        }
    }
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

    /// **已废弃**：kiroapp.cc 的独立配置块，仅兼容存量 `config.json`。
    ///
    /// 注意键名 `kiroapp` 指的是 kiroapp**.cc**，而非 `flavor: "kiroapp"` 对应的
    /// kiroapp**.io**。由 [`Config::resolved_vendors`] 自动转成 `kiroapp-cc` flavor
    /// 的普通卖家项。新配置请直接写 `vendors`，详见 [`LegacyKiroappCcConfig`]。
    #[serde(default, rename = "kiroapp")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_kiroapp_cc: Option<LegacyKiroappCcConfig>,

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
            legacy_kiroapp_cc: None,
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
    /// 4. 存量的顶层 `kiroapp` 块（其实是 kiroapp.cc，见 [`LegacyKiroappCcConfig`]）
    ///    转成 `kiroapp-cc` flavor 的一项，排在最后。若 `vendors` 里已显式配了同 id
    ///    的项，按第 2 条由显式配置胜出。
    pub fn resolved_vendors(&self) -> Vec<VendorConfig> {
        let mut out: Vec<VendorConfig> = Vec::new();
        // 存量 kiroapp.cc 块放在最后：显式的 vendors 配置应当覆盖自动迁移的结果。
        let migrated = self
            .legacy_kiroapp_cc
            .as_ref()
            .filter(|c| c.enabled())
            .map(|c| c.to_vendor_config());
        let candidates = self
            .vendor
            .iter()
            .chain(self.vendors.iter())
            .chain(migrated.iter());

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

/// 本地新增：卖家配置的字段别名与 kiroapp.cc 存量配置迁移。
/// 单独成块，避免插进上游 `mod tests` 中间引发合并冲突。
#[cfg(test)]
mod vendor_config_compat_tests {
    use super::{Config, LEGACY_KIROAPP_CC_VENDOR_ID, VendorConfig};
    use crate::vendor::protocol::VendorFlavor;

    /// 旧文档与 config.example.json 用的是 `inboundToken`，不加别名会被静默忽略，
    /// 症状是 webhook 一直 404。
    #[test]
    fn inbound_token_别名生效() {
        let cfg: VendorConfig = serde_json::from_str(
            r#"{"baseUrl":"https://x","apiKey":"k","inboundToken":"whk_abc"}"#,
        )
        .unwrap();
        assert_eq!(cfg.webhook_path_token, "whk_abc");
        assert!(cfg.inbound_enabled(), "配了 token 就该启用入站");
    }

    /// 正名仍然优先可用
    #[test]
    fn webhook_path_token_正名生效() {
        let cfg: VendorConfig = serde_json::from_str(
            r#"{"baseUrl":"https://x","apiKey":"k","webhookPathToken":"whk_xyz"}"#,
        )
        .unwrap();
        assert_eq!(cfg.webhook_path_token, "whk_xyz");
    }

    #[test]
    fn 未配token时入站不启用() {
        let cfg: VendorConfig =
            serde_json::from_str(r#"{"baseUrl":"https://x","apiKey":"k"}"#).unwrap();
        assert!(!cfg.inbound_enabled());
        assert!(cfg.outbound_enabled(), "出站仍应可用");
    }

    #[test]
    fn auto_purchase_schedule_正名生效() {
        let cfg: VendorConfig = serde_json::from_str(
            r#"{"baseUrl":"https://x","apiKey":"k",
                "autoPurchaseSchedule":[{"from":"09:00","to":"12:00","maxCount":3}]}"#,
        )
        .unwrap();
        assert_eq!(cfg.auto_purchase_schedule.len(), 1);
    }

    /// 旧文档写的是 `autoPurchaseWindows` + `start`/`end`，三处名字都得兼容，
    /// 否则要么被静默丢弃、要么直接启动失败。
    #[test]
    fn auto_purchase_windows_旧名整套生效() {
        let cfg: VendorConfig = serde_json::from_str(
            r#"{"baseUrl":"https://x","apiKey":"k",
                "autoPurchaseWindows":[{"start":"09:00","end":"12:00","maxCount":3}]}"#,
        )
        .unwrap();
        assert_eq!(cfg.auto_purchase_schedule.len(), 1, "时段表不能被静默丢弃");
        let w = &cfg.auto_purchase_schedule[0];
        assert_eq!(w.from, "09:00");
        assert_eq!(w.to, "12:00");
        assert_eq!(w.max_count, 3);
    }

    /// 存量顶层 `kiroapp` 块（其实是 kiroapp.cc）应自动变成 kiroapp-cc flavor 的卖家
    #[test]
    fn 存量kiroapp_cc配置自动迁移() {
        let config: Config = serde_json::from_str(
            r#"{"kiroapp":{"baseUrl":"https://kiroapp.cc","apiKey":"km-x",
                "defaultGroups":["g1"],"defaultRpmLimit":7}}"#,
        )
        .unwrap();
        let resolved = config.resolved_vendors();
        assert_eq!(resolved.len(), 1);
        let v = &resolved[0];
        assert_eq!(v.vendor_id(), LEGACY_KIROAPP_CC_VENDOR_ID);
        assert_eq!(v.flavor, VendorFlavor::KiroappCc, "必须是 .cc 而非 .io");
        assert_eq!(v.normalized_base_url(), "https://kiroapp.cc");
        assert_eq!(v.default_groups, vec!["g1".to_string()]);
        assert_eq!(v.default_rpm_limit, 7);
        // kiroapp.cc 没有 webhook，也不该默认开自动提取
        assert!(!v.inbound_enabled());
        assert!(!v.auto_purchase);
    }

    #[test]
    fn 存量kiroapp_cc配置不完整时跳过() {
        let config: Config =
            serde_json::from_str(r#"{"kiroapp":{"baseUrl":"","apiKey":""}}"#).unwrap();
        assert!(config.resolved_vendors().is_empty());
    }

    /// vendors 里显式配了同 id 时，显式配置胜出（迁移项排在最后）
    #[test]
    fn 显式配置覆盖存量迁移项() {
        let config: Config = serde_json::from_str(
            r#"{"vendors":[{"id":"kiroapp-cc","flavor":"kiroapp-cc",
                 "baseUrl":"https://explicit","apiKey":"k2"}],
                "kiroapp":{"baseUrl":"https://kiroapp.cc","apiKey":"km-x"}}"#,
        )
        .unwrap();
        let resolved = config.resolved_vendors();
        assert_eq!(resolved.len(), 1, "同 id 只保留先出现的");
        assert_eq!(resolved[0].normalized_base_url(), "https://explicit");
    }

    /// config.example.json 必须能真正被解析出预期的卖家。
    ///
    /// 这个测试是补上历史缺口：示例里曾把字段名写成 `inboundToken` /
    /// `autoPurchaseWindows`，与结构体对不上却没人发现，因为没有任何测试
    /// 拿真实结构体解析过它。
    #[test]
    fn 示例配置能解析且卖家齐全() {
        let raw = include_str!("../../config.example.json");
        let config: Config = serde_json::from_str(raw).expect("config.example.json 必须能解析");
        let resolved = config.resolved_vendors();
        // 刻意不断言家数：往示例里加一家就得改一次测试，而下面逐家的
        // `expect` 已经能抓出「某家被静默丢掉」——那才是本测试要防的问题。

        let io = resolved
            .iter()
            .find(|v| v.flavor == VendorFlavor::Kiroapp)
            .expect("缺 kiroapp.io");
        assert!(io.normalized_base_url().contains("kiroapp.io"));
        assert!(io.inbound_enabled(), "示例里的 webhook token 必须真正生效");
        assert_eq!(io.auto_purchase_schedule.len(), 1, "示例时段表必须真正生效");

        let cc = resolved
            .iter()
            .find(|v| v.flavor == VendorFlavor::KiroappCc)
            .expect("缺 kiroapp.cc");
        assert!(cc.normalized_base_url().contains("kiroapp.cc"));

        let drop = resolved
            .iter()
            .find(|v| v.flavor == VendorFlavor::Drop)
            .expect("缺 Kiro Drop");
        assert!(drop.normalized_base_url().contains("drop.kiro.ss"));
        assert!(drop.inbound_enabled(), "示例里的 webhook token 必须真正生效");
    }

    /// 示例配置里**不能有任何被 serde 静默忽略的键**。
    ///
    /// 「能解析」不等于「配置生效」：未知键不会报错，只会被丢掉。历史上
    /// `inboundToken` / `autoPurchaseWindows` 就是这样静静失效的。本测试反序列化
    /// 后再序列化回来逐键比对，被忽略的键在回程里会消失，从而被抓住。
    ///
    /// 同时也校验往返值稳定 —— 面板切换提取模式会把整个 config.json 写回，
    /// 若序列化形态与用户手写的不一致（如 `kiroapp-cc` 被改写成 `kiroappCc`），
    /// 文件会被悄悄改动。
    #[test]
    fn 示例配置无静默忽略的键且往返稳定() {
        let raw = include_str!("../../config.example.json");
        let orig: serde_json::Value = serde_json::from_str(raw).unwrap();
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let back = serde_json::to_value(&cfg).unwrap();

        let mut problems = Vec::new();
        diff_json(&orig, &back, "", &mut problems);
        assert!(
            problems.is_empty(),
            "config.example.json 与 Config 结构体不一致：\n{}",
            problems.join("\n")
        );
    }

    /// 逐键比对原始 JSON 与往返后的 JSON，记录「键被忽略」与「值被改写」。
    ///
    /// 只检查原始里出现过的键 —— 回程多出的键是 serde 默认值补全，属正常。
    fn diff_json(
        orig: &serde_json::Value,
        back: &serde_json::Value,
        path: &str,
        out: &mut Vec<String>,
    ) {
        match (orig, back) {
            (serde_json::Value::Object(o), serde_json::Value::Object(b)) => {
                for (k, v) in o {
                    let p = format!("{path}.{k}");
                    match b.get(k) {
                        None => out.push(format!("  {p} —— 该键被 serde 静默忽略，配置不会生效")),
                        Some(bv) => diff_json(v, bv, &p, out),
                    }
                }
            }
            (serde_json::Value::Array(o), serde_json::Value::Array(b)) => {
                for (i, v) in o.iter().enumerate() {
                    match b.get(i) {
                        None => out.push(format!("  {path}[{i}] —— 元素丢失")),
                        Some(bv) => diff_json(v, bv, &format!("{path}[{i}]"), out),
                    }
                }
            }
            (a, b) if a != b => {
                out.push(format!("  {path} —— 往返后值变了: {a} -> {b}"));
            }
            _ => {}
        }
    }

    /// flavor 的序列化形态必须与 `as_str()` / 文档 / 报错提示一致
    #[test]
    fn flavor序列化用连字符形态() {
        let json = serde_json::to_string(&VendorFlavor::KiroappCc).unwrap();
        assert_eq!(json, r#""kiroapp-cc""#, "不能写成 kiroappCc");
        assert_eq!(serde_json::to_string(&VendorFlavor::Kiroapp).unwrap(), r#""kiroapp""#);
        assert_eq!(serde_json::to_string(&VendorFlavor::Legacy).unwrap(), r#""legacy""#);
        // 往返稳定
        for f in [VendorFlavor::Legacy, VendorFlavor::Kiroapp, VendorFlavor::KiroappCc] {
            let s = serde_json::to_string(&f).unwrap();
            let back: VendorFlavor = serde_json::from_str(&s).unwrap();
            assert_eq!(back, f);
        }
    }

    /// `kiroapp` 指 .io、`kiroapp-cc` 指 .cc，两者必须解析成不同 flavor
    #[test]
    fn 两个kiroapp不能混淆() {
        assert_eq!(VendorFlavor::parse("kiroapp"), Some(VendorFlavor::Kiroapp));
        assert_eq!(
            VendorFlavor::parse("kiroapp.io"),
            Some(VendorFlavor::Kiroapp)
        );
        assert_eq!(
            VendorFlavor::parse("kiroapp-cc"),
            Some(VendorFlavor::KiroappCc)
        );
        assert_eq!(
            VendorFlavor::parse("kiroapp.cc"),
            Some(VendorFlavor::KiroappCc)
        );
        assert_ne!(VendorFlavor::Kiroapp, VendorFlavor::KiroappCc);
    }
}
