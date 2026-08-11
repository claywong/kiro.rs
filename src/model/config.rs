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

    /// 协议风味，决定路径前缀、鉴权头与响应字段映射。缺省 `legacy`。
    ///
    /// | 值 | 卖家 | 路径 + 鉴权 |
    /// |---|---|---|
    /// | `legacy` | 首家 | `/api/my/*` + `X-API-Key` |
    /// | `kiroapp` | kiroapp**.io** | `/api/me/*` + `Authorization: Bearer` |
    /// | `kiroapp-cc` | kiroapp**.cc** | `/openapi/*` + `Authorization: Bearer` |
    /// | `drop` | drop.kiro.ss | `/api/my/*` + `X-API-Key`，人民币计价 |
    /// | `kiromarket` | api.91kiro.com | `/api/my/*` + `X-API-Key`，逐张实付 |
    /// | `kirored` | kiro.red | email + 密码登录，请求签名，响应 AES 加密 |
    /// | `kiro-ooo` | kiro.ooo | `/api/my/*` + `X-API-Key`，**余额在 `credits`** |
    ///
    /// 拼错时**直接报错而非静默回退** —— 被当成默认值会对着错误的路径和鉴权头
    /// 发请求，症状是一片 401/404。可选值见
    /// [`crate::vendor::protocol::VendorFlavor::all_names`]。
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

    /// 提取入库时写入凭据的调度优先级。**数值越小越优先**（选号取 priority 最小者）。
    ///
    /// 不配时按 flavor 取缺省：车次制的 kiro.red 给 10（拼车号存活短，排在自有号
    /// 之后当兜底），其余家给 0（与本配置项引入前的行为一致）。取值见
    /// [`Self::effective_default_priority`]。
    #[serde(default)]
    pub default_priority: Option<u32>,

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

    /// 逐渠道补货：本家只看**自己**有没有存活 Key，没有就补，不看池子总量。
    ///
    /// 逐家独立配置，故意做成**不对称**的：
    ///
    /// | 本家设置 | 判据 | 别家补货对本家的影响 |
    /// |---|---|---|
    /// | `true` | 本家存活 == 0 | 无 —— 只看自己 |
    /// | `false`（默认） | 池中存活 < `autoPurchasePoolTarget` | 有 —— 别家的号占用总量 |
    ///
    /// 开着的家彼此独立、各自保底；关着的家仍按全局总量判，而那个总量**包含**
    /// 开着的那些家买来的号。混合配置时要注意：`poolTarget` 必须大于「开了本项
    /// 的家数」，否则那些家常驻的号会把总量占满，关着的家永远轮不到补货
    /// （A 开且常驻 1 张时，`poolTarget=1` 会让 B 恒被挡住，得配 2）。
    ///
    /// 为什么它仍然是有界的：兜底路径（就地盘点）不消费卖家额度、会反复成立，
    /// 原本唯一的刹车是全局阈值。本项换了另一个刹车 —— **本家自己的盘点**：
    /// 买到一张后本家 `alive == 1`，下一条推送即被 `StillAlive` 拒掉。故上限是
    /// 「本家常驻 `autoPurchaseMaxCount` 张」，不会无限扣费。
    ///
    /// 代价要清楚：开着的每家都各自维持库存，账号消耗约等于开着的家数倍。
    /// 且买来的号若立刻被封（`Suspended`），本家又回到 `alive == 0`，会再买一张 ——
    /// 这是本项的**预期语义**（渠道无可用即补），封号率高时消耗会明显上升。
    #[serde(default)]
    pub auto_purchase_per_channel: bool,

    /// kiro.red 自动预定下一批次。默认关闭；只控制**创建新的付费预定单**，不控制
    /// 已付款订单发货后的取卡与入库。
    ///
    /// 预定与现货自动提取是两套独立策略：本项开启后，只要卖家侧没有任何待发货
    /// 预定单，就预定 1 件名称以 `Kiro拼车`（忽略空白）开头的最便宜可预定商品。
    /// 它刻意不看本地是否还有可用 Key，也不受 `autoPurchasePoolTarget` 限制，因为
    /// 目标是在旧 Key 仍可用时提前排队。
    ///
    /// 新预定会立即扣积分，故仍遵循库存轮询的总闸语义：
    /// `stockPollRespectGlobalGate=true` 时受顶层 `autoPurchaseEnabled` 控制；为 false
    /// 时与现有轮询下单一样越过总闸。轮询间隔为 0 时没有执行器，本项不会生效。
    #[serde(default)]
    pub auto_reserve: bool,

    /// 登录密码，仅 `kirored`（kiro.red）用。该家不用静态 Key，而是用
    /// email + 密码登录换 JWT —— email 复用 `api_key` 字段，密码放这里。
    ///
    /// 单独成行、放在结构体末尾且带 `#[serde(default)]`：既不打扰上游按序排列的
    /// 字段块，其余家不配也不受影响（缺省空串）。
    #[serde(default)]
    pub vendor_password: String,

    /// 轮询发现新车并自动提取的间隔（秒）。**0 = 关闭（默认）**。
    ///
    /// 给**没有 webhook 的卖家**（`kirored` / `kiroapp-cc`）用。这些家的
    /// `autoPurchase` 单独开是无效的：自动提取只由入站事件触发，而它们压根不推 ——
    /// 开关能开、面板显示「自动提取」，却一次都不会动，且不留任何跳过记录。
    /// 本项补上缺失的那一环：定时查库存，发现新车就**合成**一条
    /// `new_keys_available` 事件塞进同一条管线，下游判定与幂等完全复用。
    ///
    /// 与 `autoPurchase` 是 **AND** 关系，且 `autoPurchase` 关着时**连库存都不查**：
    /// 本项非 0 只是让轮询器跑起来，它每轮先看提取模式，手动模式整轮跳过。
    ///
    /// 早期版本在手动模式下仍会查库存并把发现的车落成事件（供「先观察、不扣费」）。
    /// 该用法已取消：观察目的用面板状态条的「X 前发车」就能达到，而落库的事件只有
    /// 面板红点、没有外部推送，等人看到时车早过期了 —— 换不来一次真实提取，白付
    /// 每分钟一次出站。
    ///
    /// 于是「停掉本家的轮询」有两种粒度：关 `autoPurchase`（轮询器仍在，转入待机，
    /// 切回自动后最迟一个周期恢复），或把本项改成 0（轮询器根本不起，改回要重启）。
    ///
    /// **有下限**：小于 [`MIN_STOCK_POLL_INTERVAL_SECS`] 的非 0 值会被抬到该值并
    /// 告警。kiro.red 查一次库存要登录 + 签名 + 解密，间隔太密等于持续压卖家接口，
    /// 且我方每一轮都要走一遍授权判定。
    #[serde(default)]
    pub stock_poll_interval_secs: u64,

    /// 轮询是否遵循**全局自动提取总闸**（顶层 `autoPurchaseEnabled`）。默认 `true`。
    ///
    /// | 本项 | 总闸关闭时的行为 |
    /// |---|---|
    /// | `true`（默认） | 轮询直接跳过，连库存都不查 |
    /// | `false` | 继续发现新车，**并且照样自动下单** |
    ///
    /// **改成 `false` 等于让本家越过全局急停。** 总闸的价值在于它是一个能一键停掉
    /// 全部自动扣费的地方；本项关掉后，总闸对本家这条轮询链路整体失效，包括
    /// `try_auto_purchase` 那一步的扣费。且总闸会被健康联动自动翻转，所以这相当于
    /// 把本家的花钱从那套自动逻辑里摘出来。想停掉本家只有两条路：关本家的
    /// `auto_purchase`，或把 [`Self::stock_poll_interval_secs`] 改成 0。
    ///
    /// 绕过的范围**只有总闸、且只有轮询触发的那条路** ——
    /// 卖家 webhook 推送触发的自动提取一律仍受总闸管，见
    /// [`AutoPurchaseSource`](crate::vendor::service::AutoPurchaseSource)。
    /// 池闸（`auto_purchase_pool_target`）、失效授权判定、并发锁都不绕，仍然有界。
    ///
    /// 什么时候该关：总闸常态关闭（例如靠健康联动自动开关），但你要这一家不受
    /// 那套联动影响、该补货就补。开着总闸时本项没有区别。
    #[serde(default = "default_true")]
    pub stock_poll_respect_global_gate: bool,
}

fn default_true() -> bool {
    true
}

impl VendorConfig {
    /// 库存轮询的**实际生效间隔**（秒），0 表示未启用。
    ///
    /// 与配置原值的区别是已抬过下限（见 [`MIN_STOCK_POLL_INTERVAL_SECS`]）。
    /// 轮询器与面板都必须用这个值，否则面板显示 10 秒而实际按 60 秒跑，
    /// 会让人误判「怎么没按我配的频率查」。
    ///
    /// **0 不受下限影响** —— 0 是「关闭」，被抬成 60 等于替用户开了一条扣费路径。
    pub fn effective_stock_poll_interval(&self) -> u64 {
        match self.stock_poll_interval_secs {
            0 => 0,
            v => v.max(MIN_STOCK_POLL_INTERVAL_SECS),
        }
    }
}

/// 轮询间隔下限（秒）。见 [`VendorConfig::stock_poll_interval_secs`]。
///
/// 定在 1 分钟。车次存活时间以十分钟计（kiro.red 实测「16 分钟 47 秒」这个量级），
/// 1 分钟的分辨率足够抢到刚发的车。再密就没有意义了 —— kiro.red 查一次库存要
/// 登录换 JWT + 请求签名 + 响应解密，秒级轮询等于持续压卖家接口。
pub const MIN_STOCK_POLL_INTERVAL_SECS: u64 = 60;

fn default_vendor_auto_max_count() -> u32 {
    1
}

fn default_vendor_rpm_limit() -> u32 {
    300
}

/// kiro.red 入库凭据的缺省优先级。数值越小越优先，10 表示排在自有号（0）之后。
pub const DEFAULT_KIRORED_PRIORITY: u32 = 10;

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
            // 该家非车次制，按 flavor 缺省取 0
            default_priority: None,
            default_api_region: String::new(),
            default_auth_region: String::new(),
            auto_purchase: false,
            auto_purchase_max_count: default_vendor_auto_max_count(),
            auto_purchase_schedule: Vec::new(),
            // 不开自动提取的家谈不上逐渠道补货，与上面 auto_purchase 保持一致
            auto_purchase_per_channel: false,
            auto_reserve: false,
            // kiroapp.cc 走静态 Key，不用登录密码
            vendor_password: String::new(),
            // 存量配置不擅自开轮询：那会带来扣费行为，必须用户显式配
            stock_poll_interval_secs: 0,
            stock_poll_respect_global_gate: true,
        }
    }
}

impl VendorConfig {
    /// 入库凭据的调度优先级。显式配了就用配的，否则按 flavor 取缺省。
    ///
    /// kiro.red 缺省 10 而非 0：那家是拼车车次，号的存活时长以分钟计（实测多为
    /// 半小时到一小时），排在自有号之后当兜底更合适。缺省值放在代码里而不是要求
    /// 写进配置文件，是为了让「不配也对」—— 新加这家的人不必知道要补这一项。
    pub fn effective_default_priority(&self) -> u32 {
        if let Some(p) = self.default_priority {
            return p;
        }
        match self.flavor {
            crate::vendor::protocol::VendorFlavor::Kirored => DEFAULT_KIRORED_PRIORITY,
            _ => 0,
        }
    }

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

    /// 出站接口是否可用（base_url 与 api_key 均非空）。
    ///
    /// kirored（kiro.red）额外要求 `vendor_password` 非空 —— 该家用
    /// email（存在 `api_key`）+ 密码登录，缺密码则登录必失败，提前判为不可用。
    pub fn outbound_enabled(&self) -> bool {
        if self.normalized_base_url().is_empty() || self.api_key.trim().is_empty() {
            return false;
        }
        if self.flavor == crate::vendor::protocol::VendorFlavor::Kirored {
            return !self.vendor_password.trim().is_empty();
        }
        true
    }

    /// 入站 webhook 是否可用（出站可用且路径 token 非空）
    pub fn inbound_enabled(&self) -> bool {
        self.outbound_enabled() && !self.webhook_path_token.trim().is_empty()
    }
}

/// 健康联动：把本地「近 1 分钟报错数」反向映射为外部系统的账号调度开关。
///
/// 语义刻意是**反的**：本地稳（报错 < 阈值）就把外部账号的调度**关掉**，本地一旦
/// 不稳（报错 >= 阈值）再把它**打开**。外部账号在这里的角色是兜底池——平时不让它
/// 接量（省额度 / 保它的账号健康度），只在本地扛不住时放进来接一段。
///
/// 判据取 `traces.db` 的 60 秒窗口报错数（同概览页「报错 · 近 1 分钟」那张卡）。
/// trace 关闭时该计数不再更新，此时整个联动会跳过而非按残留读数误判。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthGateConfig {
    /// 总开关。默认关闭 —— 这是本地运维特性，不配就完全不跑。
    #[serde(default)]
    pub enabled: bool,

    /// 外部系统基址，如 `https://4code.us`。末尾斜杠会被自动去掉。
    #[serde(default)]
    pub base_url: String,

    /// 外部系统的 Admin Token。
    #[serde(default)]
    pub token: String,

    /// 传 token 用的请求头名（默认 `X-API-Key`）。
    ///
    /// 4code.us 实测认证走 `X-API-Key`：同一个 token 用 `Authorization: Bearer` 会被
    /// 回 401 `INVALID_TOKEN`。做成可配是为了对方换认证方式时不用改代码——填
    /// `Authorization` 时需自行在 token 里带上 `Bearer ` 前缀。
    #[serde(default = "default_health_gate_auth_header")]
    pub auth_header: String,

    /// 要联动开关的外部账号 ID 列表。空列表等于没启用。
    #[serde(default)]
    pub account_ids: Vec<u64>,

    /// 不稳定判定阈值：近 1 分钟报错数 **>=** 此值即视为不稳定（默认 10）。
    #[serde(default = "default_health_gate_error_threshold")]
    pub error_threshold: u64,

    /// 轮询间隔（秒，默认 30）。判据窗口固定 60 秒，间隔取其一半，
    /// 保证任何一分钟的异常至少被看到一次。
    #[serde(default = "default_health_gate_interval_secs")]
    pub check_interval_secs: u64,

    /// 连续多少次判定一致才真正切换开关（默认 2）。
    ///
    /// 防抖用。报错数在阈值上下抖动时，单次读数就切会导致反复推开关，既刷对方
    /// 审计日志也让调度状态来回跳。要求连续几个周期口径一致再动。
    #[serde(default = "default_health_gate_confirmations")]
    pub confirmations: u32,

    /// 状态没变时也按当前判定重推一次的间隔（秒，默认 300 = 5 分钟）。
    ///
    /// 为什么需要：本地只记「上次推成功的值」，不去读对方当前状态。若有人在对方
    /// 后台手动改了开关，本地记录就与实际脱节，且因为状态"没变"而永远不再推，
    /// 一直错到下次健康度翻转。定期重推让这种漂移自愈。开关接口幂等，重推同值无副作用。
    ///
    /// `0` 表示只在翻转时推、不做定期兜底。
    #[serde(default = "default_health_gate_reaffirm_interval_secs")]
    pub reaffirm_interval_secs: u64,

    /// 单次推送失败后的重试次数（默认 3，含首发共 3 次尝试）。
    ///
    /// 只对网络错误与对方 5xx 重试。4xx（token 失效 / 账号不存在）重试无意义，
    /// 直接放弃并留给下个周期——那类问题得改配置，不是等一等就好。
    #[serde(default = "default_health_gate_max_attempts")]
    pub max_attempts: u32,

    // ── 以下为「不依赖流量的判据」相关配置 ────────────────────────────────
    // 报错绝对条数在闭环里会失效（兜底一开、流量被分走，分子塌了，「没量」和
    // 「健康」读数一样），所以补两类零流量下依然有效的判据：凭据池存量、主动探测。
    /// 可用凭据比例低于此值即判不稳定。**默认 0，即不启用该维度。**
    ///
    /// 注意这个判据容易误报，默认关闭是刻意的：`available_count()` 把限流冷却中
    /// （`throttled_until` 未到期）的凭据也算作不可用，而账号级 429 冷却是正常
    /// 运行中的预期行为、不是故障。流量一大就有大批凭据在冷却里轮转，比例天然
    /// 很低，此时系统完全健康。方向还是反的：流量越大 → 冷却越多 → 比例越低
    /// → 越倾向判不稳定，会在系统最正常忙碌的时候误报。
    ///
    /// 10 张里只有 1 张可用也可能完全正常——能不能扛住取决于当前流量和这张的
    /// 剩余配额，与另外 9 张在冷却无关。
    ///
    /// 无论此项如何配置，「`available == 0`（一张可用的都没有）」始终判不稳定，
    /// 那条是底线，不受这里影响。
    #[serde(default = "default_health_gate_min_available_ratio")]
    pub min_available_ratio: f64,

    /// 是否开启主动探测（默认关闭）。
    ///
    /// 探测发的是**真实推理请求、会计费**，所以默认不开。它覆盖的是存量信号的
    /// 盲区：凭据全好但推理接口坏了的时候 available 是满的，只能真出一次货才知道。
    #[serde(default)]
    pub probe_enabled: bool,

    /// 探测间隔（秒，默认 30）。同时是「窗口内有成功请求则跳过本轮」的窗口长度。
    ///
    /// 因为有成功就跳过，探测频率天然与流量成反比：忙时一次不发，闲时才发，
    /// 而闲时正是存量信号覆盖不到、真正需要探测的时刻。
    #[serde(default = "default_health_gate_probe_interval_secs")]
    pub probe_interval_secs: u64,

    /// 探测用的模型 ID（默认 `claude-opus-5`）。
    ///
    /// 用用户真实在用的模型探测，测出来的健康度才有意义，不会出现「便宜模型通了
    /// 但主力模型挂了」的假阳性。代价是这是最贵的档，且可能有独立额度——
    /// 配置前请确认高频探测不会啃掉用户真正要用的配额。
    #[serde(default = "default_health_gate_probe_model")]
    pub probe_model: String,

    /// 连续多少次探测失败才判不稳定（默认 2）。单次网络抖动不下结论。
    #[serde(default = "default_health_gate_probe_failures")]
    pub probe_failures: u32,
}

impl Default for HealthGateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            token: String::new(),
            auth_header: default_health_gate_auth_header(),
            account_ids: Vec::new(),
            error_threshold: default_health_gate_error_threshold(),
            check_interval_secs: default_health_gate_interval_secs(),
            confirmations: default_health_gate_confirmations(),
            reaffirm_interval_secs: default_health_gate_reaffirm_interval_secs(),
            max_attempts: default_health_gate_max_attempts(),
            min_available_ratio: default_health_gate_min_available_ratio(),
            probe_enabled: false,
            probe_interval_secs: default_health_gate_probe_interval_secs(),
            probe_model: default_health_gate_probe_model(),
            probe_failures: default_health_gate_probe_failures(),
        }
    }
}

impl HealthGateConfig {
    /// 配置是否完整可用：开关开着，且基址 / token / 账号列表都给全了。
    /// 缺任一项都当没启用处理 —— 半配状态下静默不跑比每周期报错刷屏好。
    pub fn is_usable(&self) -> bool {
        self.enabled && self.is_configured()
    }

    /// 配置是否齐全（不看 `enabled`）。
    ///
    /// 与 [`Self::is_usable`] 的分工：本方法答「填全了吗」，`is_usable` 答
    /// 「填全了且现在开着吗」。看门狗按本方法决定要不要**起任务** —— 起了之后
    /// `enabled` 由面板运行时切换，若按 `is_usable` 起任务，启动时是关的就压根
    /// 没有循环在跑，面板打开开关后要等到重启才生效。
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.token.trim().is_empty()
            && !self.account_ids.is_empty()
    }

    /// 去掉末尾斜杠的基址，供拼接路径使用。
    pub fn normalized_base_url(&self) -> &str {
        self.base_url.trim().trim_end_matches('/')
    }

    /// 认证头名，空配置时回落到默认值（空头名会让 reqwest 直接 panic）。
    pub fn auth_header(&self) -> &str {
        let h = self.auth_header.trim();
        if h.is_empty() { "X-API-Key" } else { h }
    }
}

fn default_health_gate_auth_header() -> String {
    "X-API-Key".to_string()
}

fn default_health_gate_error_threshold() -> u64 {
    10
}

fn default_health_gate_interval_secs() -> u64 {
    30
}

fn default_health_gate_confirmations() -> u32 {
    2
}

fn default_health_gate_reaffirm_interval_secs() -> u64 {
    300
}

fn default_health_gate_max_attempts() -> u32 {
    3
}

fn default_health_gate_min_available_ratio() -> f64 {
    // 0 = 不启用比例判据，只保留 available == 0 那条底线。
    // 理由见 `min_available_ratio` 字段注释：限流冷却会让比例在系统健康时也很低。
    0.0
}

fn default_health_gate_probe_interval_secs() -> u64 {
    30
}

fn default_health_gate_probe_model() -> String {
    "claude-opus-5".to_string()
}

fn default_health_gate_probe_failures() -> u32 {
    2
}

/// 手动流量入口：直接控制外部系统指定账号是否参与调度。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrafficIngressConfig {
    /// 期望的流量入口状态。true = 可调度，false = 不可调度。
    #[serde(default)]
    pub enabled: bool,

    /// 外部系统基址，默认指向 g7e6ai.com。
    #[serde(default = "default_traffic_ingress_base_url")]
    pub base_url: String,

    /// 外部系统的 Admin Token。
    #[serde(default)]
    pub token: String,

    /// 传 token 用的请求头名，协议与健康联动一致。
    #[serde(default = "default_health_gate_auth_header")]
    pub auth_header: String,

    /// 需要随入口开关一起切换的外部账号 ID。
    #[serde(default)]
    pub account_ids: Vec<u64>,

    /// 整轮推送失败后的重试间隔。
    #[serde(default = "default_health_gate_interval_secs")]
    pub retry_interval_secs: u64,

    /// 单个账号一次推送最多尝试次数，含首发。
    #[serde(default = "default_health_gate_max_attempts")]
    pub max_attempts: u32,
}

impl Default for TrafficIngressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_traffic_ingress_base_url(),
            token: String::new(),
            auth_header: default_health_gate_auth_header(),
            account_ids: Vec::new(),
            retry_interval_secs: default_health_gate_interval_secs(),
            max_attempts: default_health_gate_max_attempts(),
        }
    }
}

impl TrafficIngressConfig {
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.token.trim().is_empty()
            && !self.account_ids.is_empty()
    }

    pub fn normalized_base_url(&self) -> &str {
        self.base_url.trim().trim_end_matches('/')
    }

    pub fn auth_header(&self) -> &str {
        let header = self.auth_header.trim();
        if header.is_empty() { "X-API-Key" } else { header }
    }
}

fn default_traffic_ingress_base_url() -> String {
    "https://g7e6ai.com".to_string()
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

    /// 健康联动：按本地近 1 分钟报错数反向控制外部系统的账号调度开关。
    /// 详见 [`HealthGateConfig`]。默认关闭。
    #[serde(default)]
    pub health_gate: HealthGateConfig,

    /// 手动流量入口：控制 g7e6ai.com 指定账号的 schedulable 开关。
    #[serde(default)]
    pub traffic_ingress: TrafficIngressConfig,

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

    /// 全局提取限制：池中存活的卖家 Key 达到此数即不再自动补货。0 = 不启用（默认）。
    ///
    /// 为什么需要它：各家的失效判定按设计互不可见（见 `vendor::auto::census` 的
    /// 注释——A 家推「全部失效」时若把 B 家健康的 Key 算进来，A 的补货会被 B 挡死）。
    /// 代价是多家 Key 同期失效时，三家各自都得出「池子空了」的结论，于是各提一份。
    /// 本值补上那个缺失的全局视图，判据是 `vendor::auto::pool_alive`。
    ///
    /// 与各家 `autoPurchaseMaxCount` 是两层闸：后者管单笔提多少，本值管池子总量。
    /// 语义刻意是「池子够用」而非「别家有存活」，否则会退回被别家挡死的老问题。
    ///
    /// 沿用本项目 0 值即关闭的既有约定（同 `autoPurchaseSchedule` 的 `maxCount: 0`），
    /// 不额外设开关，避免「开关开着但阈值为 0」这种无意义组合。
    #[serde(default)]
    pub auto_purchase_pool_target: u32,

    /// 自动提取总闸。`false` = 全局关闭，任何家都不再自动下单。默认 `true`。
    ///
    /// 与各家 `autoPurchase` 的分工：那个是**逐家**的模式选择（这一家走自动还是
    /// 手动），本值是**跨家**的一刀切。想全停时逐家去关有两个毛病 —— 家数多要点
    /// N 次，且新增一家时默认值取自它自己的配置块，很容易漏掉一家又悄悄开始下单。
    /// 故单独留一个总闸，语义是「先问它，再问各家」。
    ///
    /// 关闭时**不改各家的 `autoPurchase`**：那是用户对每家的意图，总闸只是临时
    /// 压住出站。重新打开后各家回到原来各自的模式，不需要再逐家恢复一遍。
    ///
    /// 默认 `true` 而非 `false`：存量 `config.json` 里没有这个键，反过来会让升级
    /// 后自动提取集体静默停摆，且现场几乎无从发现。
    #[serde(default = "default_auto_purchase_enabled")]
    pub auto_purchase_enabled: bool,

    // 逐渠道补货是**逐家**配置，见 [`VendorConfig::auto_purchase_per_channel`]。
    // 早期版本曾在此处放过一个同名顶层开关，是设计错误：那样一开就是全家生效，
    // 无法「A 家各自保底、B 家仍按总量控」。已移除，不保留别名 —— 该版本没发布过。
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

/// 自动提取总闸缺省开启，理由见 [`Config::auto_purchase_enabled`]
fn default_auto_purchase_enabled() -> bool {
    true
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
            health_gate: HealthGateConfig::default(),
            traffic_ingress: TrafficIngressConfig::default(),
            vendor: None,
            vendors: Vec::new(),
            auto_purchase_pool_target: 0,
            auto_purchase_enabled: default_auto_purchase_enabled(),
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
            serde_json::from_str(r#"{"baseUrl":"https://v.example.com","apiKey":"usr-x"}"#)
                .unwrap();
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

    /// 存量配置没有这个字段，必须解析为 0（不启用），否则升级后会突然挡掉自动补货
    #[test]
    fn 全局池闸默认关闭() {
        let config: Config = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(config.auto_purchase_pool_target, 0);
    }

    #[test]
    fn 全局池闸序列化回写不丢失() {
        let mut config = Config::default();
        config.auto_purchase_pool_target = 4;
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("autoPurchasePoolTarget"), "落盘要带上本字段");
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.auto_purchase_pool_target, 4);
    }

    #[test]
    fn 全局池闸读取配置值() {
        let config: Config = serde_json::from_str(r#"{"autoPurchasePoolTarget":3}"#).unwrap();
        assert_eq!(config.auto_purchase_pool_target, 3);
    }

    /// 逐渠道补货是**逐家**配置，不是顶层开关 —— 混着配是本特性的用法：
    /// 开着的家只看自己，关着的家仍按 `autoPurchasePoolTarget` 判总量。
    #[test]
    fn 逐渠道补货按家独立配置() {
        let config: Config = serde_json::from_str(
            r#"{"autoPurchasePoolTarget":2,
                "vendors":[
                  {"id":"a","baseUrl":"https://a","apiKey":"k","autoPurchasePerChannel":true},
                  {"id":"b","baseUrl":"https://b","apiKey":"k"}
                ]}"#,
        )
        .unwrap();
        let vs = config.resolved_vendors();
        let a = vs.iter().find(|v| v.vendor_id() == "a").unwrap();
        let b = vs.iter().find(|v| v.vendor_id() == "b").unwrap();
        assert!(a.auto_purchase_per_channel, "a 家开着");
        assert!(!b.auto_purchase_per_channel, "b 家默认关闭");
        // 全局阈值仍在，供 b 家使用
        assert_eq!(config.auto_purchase_pool_target, 2);
    }

    #[test]
    fn 逐渠道补货默认关闭且回写不丢() {
        let v: VendorConfig =
            serde_json::from_str(r#"{"baseUrl":"https://x","apiKey":"k"}"#).unwrap();
        assert!(
            !v.auto_purchase_per_channel,
            "默认关闭 —— 会多花钱的特性不默认开"
        );

        let mut v2 = v.clone();
        v2.auto_purchase_per_channel = true;
        let json = serde_json::to_string(&v2).unwrap();
        assert!(json.contains("autoPurchasePerChannel"), "落盘要带上本字段");
        let back: VendorConfig = serde_json::from_str(&json).unwrap();
        assert!(back.auto_purchase_per_channel);
    }

    #[test]
    fn 自动预定默认关闭且回写不丢() {
        let v: VendorConfig =
            serde_json::from_str(r#"{"baseUrl":"https://kiro.red","apiKey":"a@b.c"}"#).unwrap();
        assert!(!v.auto_reserve, "自动扣积分的功能不能在升级后默认开启");

        let mut enabled = v;
        enabled.auto_reserve = true;
        let json = serde_json::to_string(&enabled).unwrap();
        assert!(json.contains("autoReserve"));
        let back: VendorConfig = serde_json::from_str(&json).unwrap();
        assert!(back.auto_reserve);
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
        assert!(
            drop.inbound_enabled(),
            "示例里的 webhook token 必须真正生效"
        );
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
        assert_eq!(
            serde_json::to_string(&VendorFlavor::Kiroapp).unwrap(),
            r#""kiroapp""#
        );
        assert_eq!(
            serde_json::to_string(&VendorFlavor::Legacy).unwrap(),
            r#""legacy""#
        );
        // 往返稳定
        for f in [
            VendorFlavor::Legacy,
            VendorFlavor::Kiroapp,
            VendorFlavor::KiroappCc,
        ] {
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

    /// kiro.red 不配 priority 时缺省 10 —— 车次号存活短，排在自有号（0）之后兜底。
    #[test]
    fn kirored不配优先级时缺省10() {
        let cfg: VendorConfig = serde_json::from_str(
            r#"{"baseUrl":"https://kiro.red","apiKey":"a@b.c","flavor":"kirored"}"#,
        )
        .unwrap();
        assert_eq!(cfg.default_priority, None, "配置里确实没这一项");
        assert_eq!(cfg.effective_default_priority(), 10);
    }

    /// 其余家缺省 0，与本配置项引入前的行为一致（原先硬编码 0）。
    #[test]
    fn 其余家不配优先级时缺省0() {
        for flavor in ["legacy", "kiroapp", "kiroapp-cc", "drop", "kiromarket"] {
            let cfg: VendorConfig = serde_json::from_str(&format!(
                r#"{{"baseUrl":"https://x","apiKey":"k","flavor":"{flavor}"}}"#
            ))
            .unwrap();
            assert_eq!(
                cfg.effective_default_priority(),
                0,
                "flavor={flavor} 应保持原有的 0"
            );
        }
    }

    /// 显式配了就以配置为准，包括把这家改回 0。
    #[test]
    fn 显式配置覆盖缺省优先级() {
        let cfg: VendorConfig = serde_json::from_str(
            r#"{"baseUrl":"https://kiro.red","apiKey":"a@b.c","flavor":"kirored",
                "defaultPriority":0}"#,
        )
        .unwrap();
        assert_eq!(cfg.effective_default_priority(), 0);

        let cfg: VendorConfig = serde_json::from_str(
            r#"{"baseUrl":"https://x","apiKey":"k","flavor":"legacy","defaultPriority":7}"#,
        )
        .unwrap();
        assert_eq!(cfg.effective_default_priority(), 7);
    }

    /// 存量 config.json 没有 `autoPurchaseEnabled` 这个键。若默认成 false，
    /// 升级后所有家的自动提取会集体静默停摆，且现场几乎无从发现。
    #[test]
    fn 自动提取总闸缺省开启() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.auto_purchase_enabled);
        assert!(Config::default().auto_purchase_enabled);
    }

    /// 显式写 false 要能关掉 —— 总闸的整个用途就在这里
    #[test]
    fn 自动提取总闸可显式关闭() {
        let config: Config = serde_json::from_str(r#"{"autoPurchaseEnabled":false}"#).unwrap();
        assert!(!config.auto_purchase_enabled);
    }

    /// 总闸与阈值是两个独立的顶层字段，读一个不该带出另一个的默认值
    #[test]
    fn 总闸与池阈值互不干扰() {
        let config: Config =
            serde_json::from_str(r#"{"autoPurchaseEnabled":false,"autoPurchasePoolTarget":5}"#)
                .unwrap();
        assert!(!config.auto_purchase_enabled);
        assert_eq!(config.auto_purchase_pool_target, 5);
    }
}
