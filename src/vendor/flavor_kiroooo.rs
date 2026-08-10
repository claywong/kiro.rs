//! 第七家卖家 kiro.ooo 的协议实现：`/api/my/*` + `X-API-Key: usr-xxx`
//!
//! 本模块只负责「原始 DTO → 中立结构」的翻译，不发请求（HTTP 在 [`super::client`]）。
//!
//! 与 [`super::flavor_legacy`] 同路径前缀、同鉴权头，但**不能复用它**，差异如下
//! （全部经真实 Key 实测，非照文档推断）：
//!
//! - **余额字段完全不同**。本家 `/my/profile` 的 `quota` / `remaining` / `used_quota`
//!   **恒为 0**（该家不用这套配额模型），真实余额是 `credits`。照 legacy 映射
//!   `balance ← remaining` 会让面板显示余额 0，且自动提取算出的可提数量恒为 0 ——
//!   整家静默不可用。这是本家必须独立成 flavor 的首要理由。
//! - **提货路径是 `/my/keys/claim`** 而非 `/my/purchase`。
//! - **分区在独立端点**：`/my/stock` 是扁平结构、没有 `zones[]`，双区货架要另取
//!   `/my/stock/regions`（见 [`RegionsResponse`]）；claim 的区域参数名是
//!   **`region`**（不是首家的 `zone`），且**不传默认 `us-east-1`**。这一条是
//!   2026-08-10 卖家改版后新增的，早期版本确实不分区。
//!   参数名与默认值都要留意：默认区常常正是关停 0 库存的那个。
//! - **无开号记录**：`/my/gen-logs` 实测 404。
//! - **`/status` 有真实数据**（且免鉴权），但字段名是 `uptime_secs`，
//!   legacy 的 DTO 叫 `uptime_seconds`，映射时要补。
//! - 独有：`/my/credits` 的 `ledger[]` 是积分流水，`/my/keys` **给密钥正文**
//!   （多数家只给前缀，故本家可与本地凭据池对账）。
//!
//! @author wangzhong

use chrono::NaiveDateTime;
use serde::Deserialize;

use super::protocol::{
    EarliestKeyInfo, LedgerEntry, OrderInfo, Paged, ProfileInfo, PurchaseResult, PurchasedKey,
    RedeemResult, StockInfo, VendorKeyInfo, ZoneStock,
};

/// 路径前缀。账号维度接口都在 `/api/my` 下，系统状态在 `/api/status`（免鉴权）。
pub const PATH_STOCK: &str = "/api/my/stock";
/// 双区货架。`/my/stock` 只给一份扁平数字（实测是「当前开放那个区」的），
/// 要判断哪个区有货必须查这里。
pub const PATH_STOCK_REGIONS: &str = "/api/my/stock/regions";
/// 提货。注意不是 `/api/my/purchase` —— 本家没有那个路由。
pub const PATH_CLAIM: &str = "/api/my/keys/claim";
pub const PATH_PROFILE: &str = "/api/my/profile";
pub const PATH_STATUS: &str = "/api/status";
pub const PATH_ORDERS: &str = "/api/my/purchase-orders";
/// 积分余额 + 流水。本家的余额权威来源。
pub const PATH_CREDITS: &str = "/api/my/credits";
pub const PATH_KEYS: &str = "/api/my/keys";
pub const PATH_KEYS_CREATED_AT: &str = "/api/my/keys/created-at";
pub const PATH_WEBHOOK: &str = "/api/my/webhook";
pub const PATH_WEBHOOK_TEST: &str = "/api/my/webhook/test";
/// 兑换码充值。**文档的端点表里没有这一项**，但实测 `GET` 返回
/// 405 `allow: POST`，说明路由确实存在。故开放该能力，形态未知处一律给足别名。
pub const PATH_REDEEM: &str = "/api/my/redeem";

/// 单次提货上限（文档：单次上限 500）。
pub const MAX_CLAIM_COUNT: u32 = 500;

// ============ 库存 ============

/// `GET /my/stock` 响应。**扁平结构，无分区**。
///
/// 实测样本：
/// ```json
/// {"afford":1,"can_buy":true,"claimable":2,"credits":45,"max":2,
///  "remaining":0,"short_credits":0,"stock":2,"unit_price":45}
/// ```
///
/// 四个数量字段语义不同，取小才安全（见 [`StockResponse::effective_available`]）。
/// `remaining` 在本家是**剩余配额**而非余额，且实测为 0，故不当余额用。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StockResponse {
    /// 可领上限（卖家按「每母号上限 − 已领」算）
    #[serde(default)]
    pub claimable: Option<u32>,
    /// 我方当前可取的仓库存货数
    #[serde(default)]
    pub stock: Option<u32>,
    /// 按当前积分**买得起**几个。积分不足时它比 claimable 小。
    #[serde(default)]
    pub afford: Option<u32>,
    /// 卖家给的聚合上限
    #[serde(default)]
    pub max: Option<u32>,
    /// 账户积分余额。**本家的余额就是它**，不是 `remaining`。
    #[serde(default)]
    pub credits: Option<f64>,
    /// 当前单价（积分/个）
    #[serde(default)]
    pub unit_price: Option<f64>,
    /// 剩余配额。本家实测恒 0，**不是余额**，故只建模不使用。
    #[serde(default)]
    pub remaining: Option<f64>,
    /// 卖家自己的「能不能买」结论。为 false 时即便上面几个数非 0 也提不出来。
    #[serde(default = "default_true")]
    pub can_buy: bool,
}

fn default_true() -> bool {
    true
}

impl StockResponse {
    /// 实际可提数量：`claimable` / `stock` / `afford` / `max` 中**给了值的取最小**。
    ///
    /// 为什么必须取小而不是只读 `claimable`：文档说 claim 实际取
    /// `min(count, 剩余配额, 我方可取库存)`，而 `afford` 是「按现有积分买得起几个」，
    /// 卖家并不把它算进 `claimable`。实测 `claimable=2` 而 `afford=1`（45 积分、
    /// 单价 45），此时报 2 会让面板显示一个提不到的数，自动提取也会按 2 下单后
    /// 只成交 1 个（或直接被拒）。
    ///
    /// `can_buy` 为 false 时直接归 0 —— 那是卖家自己的结论，比任何数量字段权威。
    pub fn effective_available(&self) -> u32 {
        if !self.can_buy {
            return 0;
        }
        [self.claimable, self.stock, self.afford, self.max]
            .into_iter()
            .flatten()
            .min()
            // 四个字段一个都没给：无从判断，按 0 处理。宁可少提，
            // 也不凭空造一个数出来触发扣费。
            .unwrap_or(0)
    }
}

impl From<StockResponse> for StockInfo {
    fn from(r: StockResponse) -> Self {
        let available = r.effective_available();
        Self {
            available,
            // 单一定价，最低最高同值。缺货时不给报价 —— 面板显示一个提不到的价
            // 会误导（与首家 / kiro-market 同样处置）。
            price_min: r.unit_price.filter(|_| available > 0),
            price_max: r.unit_price.filter(|_| available > 0),
            // 关键：余额取 credits。本家的 remaining 是剩余配额且恒 0，
            // 拿它当余额会让面板显示 0 且自动提取算不出可提数量。
            balance: r.credits,
            // 分区在 /my/stock/regions，本端点给不出来。**留空是有意的**：
            // 只有 client 层取到货架后才填（见 VendorClient::stock 的本家分支），
            // 在这里凭空造一个「默认区」会让 pick_zone 选到一个未经核实的区。
            zones: Vec::new(),
        }
    }
}

// ============ 双区货架 ============

/// claim 不传 `region` 时卖家采用的默认区（文档明写）。
///
/// 建这个常量不是为了拿它当下单参数用，而是为了在注释与测试里指名道姓：
/// **默认区经常正是关停 0 库存的那个**，所以下单必须显式带区。
pub const DEFAULT_REGION: &str = "us-east-1";

/// `GET /my/stock/regions` 的 `regions[]` 单项。
///
/// 实测样本（2026-08-10 18:00）：
/// ```json
/// {"region":"eu-central-1","label":"欧洲区","open":true,"claimable":13,
///  "stock":13,"afford":2,"unit_price":50,"short_credits":0,
///  "batches":[{"count":8,"time":"2026-08-10 17:41:00"}],
///  "dispatches":[{"alive":8,"dead":0,"delivered":0,"running":true,"time":"..."}]}
/// ```
///
/// 卖家侧的时刻格式：`2026-08-10 18:19:00`，**不带时区**。
///
/// 实测卖家时钟是 UTC+8（`fleet_now` 比本机 UTC 快 8 小时），但这个偏移
/// **不能硬编码** —— 一律拿同一响应里的 [`RegionsResponse::fleet_now`] 做基准算差值，
/// 差值是时区无关的。见 [`parse_naive`] 与 [`StockRegion::into_zone`]。
const TIME_FMT: &str = "%Y-%m-%d %H:%M:%S";

/// 解析卖家的无时区时刻串。空串与畸形串都归 `None`（卖家用空串表示「无」）。
fn parse_naive(raw: &str) -> Option<NaiveDateTime> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    NaiveDateTime::parse_from_str(s, TIME_FMT).ok()
}

/// 一趟车。`dispatches[]` 单项。
///
/// 实测样本（同一区的相邻两趟）：
/// ```json
/// {"alive":1,"dead":0,"dead_at":"","delivered":8,"running":true,"time":"2026-08-10 18:19:00"}
/// {"alive":0,"dead":10,"dead_at":"2026-08-10 18:23:53","delivered":0,"running":false,
///  "time":"2026-08-10 18:04:00"}
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Dispatch {
    /// 发车时刻（无时区，见 [`TIME_FMT`]）
    #[serde(default)]
    pub time: String,
    /// 整车报废时刻。**空串表示车还活着** —— 这是判断死活最可靠的信号。
    #[serde(default)]
    pub dead_at: String,
    /// 卖家自己的「这车还在跑吗」结论
    #[serde(default)]
    pub running: bool,
    /// 本车当前存活的 Key 数
    #[serde(default)]
    pub alive: Option<u32>,
    /// 本车已死的 Key 数
    #[serde(default)]
    pub dead: Option<u32>,
    /// 本车已发放（被人提走）的 Key 数
    #[serde(default)]
    pub delivered: Option<u32>,
}

/// 一批可提的货。`batches[]` 单项。
///
/// 与 `dispatches[]` 的区别：`batches` 只列**此刻还能提**的批次（提空即消失），
/// `dispatches` 是历史车次流水。故「这批货是哪趟车的」要看 `batches`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Batch {
    /// 该批次所属车次的发车时刻，与 `dispatches[].time` 同值可对上
    #[serde(default)]
    pub time: String,
    /// 本批可提数量
    #[serde(default)]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StockRegion {
    /// 区域代码。**取值是完整 AWS 区域标识**（`us-east-1` / `eu-central-1`），
    /// 不是首家那种 `us` / `eu` 短码 —— claim 时原样回传。
    #[serde(default)]
    pub region: String,
    /// 中文区名，如「美国区」
    #[serde(default)]
    pub label: Option<String>,
    /// 本区是否开放。关停的区即使有存货也提不出来。
    #[serde(default)]
    pub open: bool,
    /// 本区可领上限
    #[serde(default)]
    pub claimable: Option<u32>,
    /// 本区仓库存货
    #[serde(default)]
    pub stock: Option<u32>,
    /// 按现有积分在**本区单价**下买得起几个。各区单价不同，故这个数逐区不同。
    #[serde(default)]
    pub afford: Option<u32>,
    /// 本区单价。实测美区 80、欧区 50，**差 60%，不能混用**。
    #[serde(default)]
    pub unit_price: Option<f64>,
    /// 卖家自己的「本区能不能买」结论
    #[serde(default = "default_true")]
    pub can_buy: bool,
    /// 历史车次流水，**最新的在前**（实测按 `time` 降序）
    #[serde(default)]
    pub dispatches: Vec<Dispatch>,
    /// 此刻还能提的批次。空表示本区无货（此时 `dispatches` 仍有历史车次）。
    #[serde(default)]
    pub batches: Vec<Batch>,
}

impl StockRegion {
    /// 本区实际可提数量。与 [`StockResponse::effective_available`] 同规则：
    /// `claimable` / `stock` / `afford` 取小，`open` 或 `can_buy` 为假则归 0。
    ///
    /// 这里**必须带上 `afford`**：它按本区单价算，美区 80 / 欧区 50 的差价下，
    /// 同样的积分在两区买得起的个数不同。漏掉它会让面板报一个买不起的数，
    /// 自动提取也会按那个数下单。
    pub fn effective_available(&self) -> u32 {
        if !self.open || !self.can_buy {
            return 0;
        }
        [self.claimable, self.stock, self.afford]
            .into_iter()
            .flatten()
            .min()
            // 一个数量字段都没给：无从判断，按 0 处理，不凭空造数触发扣费
            .unwrap_or(0)
    }
}

impl StockRegion {
    /// 挑出「本区当前该展示哪趟车」。
    ///
    /// 规则：**优先 `batches[]` 里最新那批对应的车** —— 那是此刻真能提到的货，
    /// 「这车跑了多久了」问的就是它。本区无货（`batches` 空）时退回 `dispatches[]`
    /// 最新那趟，用来回答「上一趟什么时候发的」。
    ///
    /// 不假设卖家的数组顺序（实测降序，但那是卖家的实现细节，改了不会报错、
    /// 只会让面板显示一趟老车），故一律按解析出的时刻取最大。
    fn current_dispatch(&self) -> Option<&Dispatch> {
        let latest_batch_time = self.batches.iter().filter_map(|b| parse_naive(&b.time)).max();
        if let Some(t) = latest_batch_time {
            // 能和车次对上就用那趟车 —— 它带 dead_at / running，批次没有
            if let Some(d) = self
                .dispatches
                .iter()
                .find(|d| parse_naive(&d.time) == Some(t))
            {
                return Some(d);
            }
        }
        self.dispatches
            .iter()
            .filter(|d| parse_naive(&d.time).is_some())
            .max_by_key(|d| parse_naive(&d.time))
    }

    /// 转中立结构。
    ///
    /// `fleet_now` 是**卖家自己的当前时刻**，用来把无时区的时刻串换成时区无关的
    /// 差值；缺它时车次相关的两个字段一律留空 —— 宁可不显示，也不显示一个错数。
    pub fn into_zone(self, fleet_now: Option<NaiveDateTime>) -> ZoneStock {
        let available = self.effective_available();

        // 存活时长 =（整车报废时刻 或 卖家当前时刻）− 发车时刻。
        //
        // 两端都取自卖家自己的时钟，**差值与时区无关** —— 这就是不必知道卖家在哪个
        // 时区也能算对的原因（实测其时钟为 UTC+8，但一点都不依赖这个事实）。
        // 语义随车况而变，与 ZoneStock::alive_secs 的约定一致：车活着是「已跑多久」
        // 且会随时间增长，车已死是「总共跑了多久」的终值。
        let (departed_at, alive_secs) = match self
            .current_dispatch()
            .and_then(|d| parse_naive(&d.time).map(|t| (t, parse_naive(&d.dead_at))))
        {
            Some((departed, dead_at)) => {
                let alive = fleet_now
                    // 车已死就用 dead_at 封顶，否则死了的车存活时长还会一直涨
                    .map(|now| dead_at.unwrap_or(now).signed_duration_since(departed))
                    .map(|d| d.num_seconds())
                    // 负数说明卖家两个时刻自相矛盾（时钟回拨 / 字段错位），
                    // 报一个负的存活时长不如不报
                    .filter(|s| *s >= 0);
                // 发车时刻前端要 Unix 秒，而我们只有卖家的无时区串。拿 fleet_now
                // 当锚点换算成「本机此刻 − 已过去多久」，同样只用差值。
                let departed_unix = fleet_now.and_then(|now| {
                    let ago = now.signed_duration_since(departed).num_seconds();
                    // 未来时刻（卖家预告下一趟车？）不当发车时间用：
                    // 前端按 now - departedAt 算「多久前发车」，会显示成负数
                    (ago >= 0).then(|| chrono::Utc::now().timestamp() - ago)
                });
                (departed_unix.filter(|t| *t > 0), alive)
            }
            None => (None, None),
        };

        ZoneStock {
            zone: self.region,
            label: self.label.filter(|s| !s.trim().is_empty()),
            available,
            stock: self.stock,
            unit_price: self.unit_price,
            // 卖家的 open 与 can_buy 都得为真才算开放
            enabled: self.open && self.can_buy,
            departed_at,
            alive_secs,
            // 本家不给存活时长文案（kiro.red 才给），留空让前端按 alive_secs 自己格式化
            alive_text: None,
        }
    }
}

/// `GET /my/stock/regions` 响应。
///
/// 顶层还给 `fleet_active` / `fleet_now` / `fleet_started_at`（发车状态）、
/// `ok` / `remaining`，中立结构没有位置，不建模。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegionsResponse {
    #[serde(default)]
    pub regions: Vec<StockRegion>,
    /// 账户积分余额。与 `/my/stock` 的 `credits` 同源，取到就不必再查一次。
    #[serde(default)]
    pub credits: Option<f64>,
    /// **卖家自己的当前时刻**（无时区），如 `2026-08-10 18:28:12`。
    ///
    /// 这个字段是车次存活时长能算准的全部依据：卖家给的时刻串都不带时区，
    /// 但只要拿同一响应里的它做基准，`now − 发车时刻` 就是时区无关的差值。
    /// 实测其时钟为 UTC+8，**但不要硬编码这个偏移** —— 卖家换机房就错了，
    /// 而症状是存活时长整体偏移 8 小时（看着像「刚发车」或「跑了半天」）。
    #[serde(default)]
    pub fleet_now: String,
    /// 本轮发车是否正在进行
    #[serde(default)]
    pub fleet_active: bool,
}

impl From<RegionsResponse> for StockInfo {
    fn from(r: RegionsResponse) -> Self {
        let credits = r.credits;
        // 卖家不给 fleet_now（老版本或字段改名）时，车次字段一律留空而非猜时区
        let fleet_now = parse_naive(&r.fleet_now);
        if fleet_now.is_none() && !r.regions.is_empty() {
            tracing::debug!(
                raw = %r.fleet_now,
                "kiro.ooo 货架未给可解析的 fleet_now，本次不透出发车时间与存活时长"
            );
        }
        let zones: Vec<ZoneStock> = r
            .regions
            .into_iter()
            .map(|z| z.into_zone(fleet_now))
            .collect();
        // 报价只算「开放且有货」的区：把关停区的价算进来，面板就会显示一个
        // 实际提不到的价位（实测美区 open=false 却仍标 80）
        let prices: Vec<f64> = zones
            .iter()
            .filter(|z| z.enabled && z.available > 0)
            .filter_map(|z| z.unit_price)
            .collect();
        Self {
            // 各区之和。注意这个数**不代表任何单一区能提这么多** ——
            // 各区严格隔离，下单必须按区走，见 StockInfo::pick_zone。
            available: zones.iter().map(|z| z.available).sum(),
            price_min: prices.iter().copied().reduce(f64::min),
            price_max: prices.iter().copied().reduce(f64::max),
            balance: credits,
            zones,
        }
    }
}

// ============ 账号档案 ============

/// `GET /my/profile` 响应。
///
/// 实测样本（**注意 quota / remaining / used_quota 全是 0**）：
/// ```json
/// {"name":"claywong","username":"claywong","user_no":"U100167","quota":0,
///  "remaining":0,"used_quota":0,"webhook_url":"","claimable":1,
///  "auto_fleet":false,"reserve_count":0,"min_reserve":1,"is_fleet_owner":false,
///  "needs_2fa":false,"twofa_ok":true,"risk_flag":0,"risk_rate":0,...}
/// ```
///
/// 本响应**不含 credits**，故余额要另取 `/my/credits`（见
/// [`super::client::VendorClient::profile`] 的本家分支）。风控字段
/// （`risk_flag` / `risk_rate` / `risk_threshold`）与自动车配置
/// （`auto_fleet` / `reserve_count`）中立结构里没有位置，不建模。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfileResponse {
    #[serde(default)]
    pub name: Option<String>,
    /// 登录名。`name` 缺失时用它兜底。
    #[serde(default)]
    pub username: Option<String>,
    /// 积分余额。本响应实测**不给**这个字段，留着是因为卖家日后补上就能直接用，
    /// 省掉一次 `/my/credits`。
    #[serde(default)]
    pub credits: Option<f64>,
    /// 卖家侧保存的 webhook 地址（空串表示未配）
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// 当前可领数量。可当「单次最大购买数」给面板的提取弹窗用。
    #[serde(default)]
    pub claimable: Option<u32>,
}

impl From<ProfileResponse> for ProfileInfo {
    fn from(r: ProfileResponse) -> Self {
        Self {
            name: r
                .name
                .filter(|s| !s.trim().is_empty())
                .or(r.username),
            email: None,
            // 档案里通常没有 credits，此时留空由 client 层用 /my/credits 补。
            // **绝不回退到 remaining** —— 本家那个字段是剩余配额且恒 0。
            balance: r.credits,
            // 刻意不映射 quota / used_quota：本家这两个恒为 0，映射过来
            // 面板会显示「配额 0 / 已用 0」这种无意义读数，比留空更误导。
            quota: None,
            used_quota: None,
            min_purchase: None,
            // 可领上限即单次可提上限，给面板的提取弹窗限制输入
            max_purchase: r.claimable,
            webhook_url: r.webhook_url.filter(|s| !s.trim().is_empty()),
            created_at: None,
        }
    }
}

/// `GET /my/credits` 响应：余额 + 流水。**本家余额的权威来源。**
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreditsResponse {
    #[serde(default)]
    pub credits: Option<f64>,
    /// 买断一台母号的价格。中立结构没有位置，不映射，仅注明其存在。
    #[serde(default)]
    pub master_price: Option<f64>,
    #[serde(default)]
    pub ledger: Vec<LedgerRow>,
}

/// `/my/credits` 的 `ledger[]` 单条。
///
/// 实测样本：
/// ```json
/// {"id":217,"kind":"claim_key","amount":-45,"balance_after":45,
///  "ref_id":"21a68cccb15074980ffa96dc3a050b3d","note":"美国区自助提货 Key #3886",
///  "created_at":"2026-08-06 23:14:12"}
/// ```
///
/// `kind` 已见取值：`claim_key`（提货扣费）、`recharge`（充值到账）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LedgerRow {
    #[serde(default)]
    pub id: Option<i64>,
    /// 变动类型。本家叫 `kind`，其余家叫 `type` / `reason`。
    #[serde(default, alias = "type", alias = "reason")]
    pub kind: Option<String>,
    /// 带符号金额（提货为负）
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default, alias = "balance")]
    pub balance_after: Option<f64>,
    #[serde(default, alias = "memo", alias = "detail")]
    pub note: Option<String>,
    /// 关联单号：提货是 `client_order_id`，充值是支付流水号。
    /// 中立结构没有独立位置，并进 `memo` —— 对账时要靠它把流水与订单对上。
    #[serde(default)]
    pub ref_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl From<LedgerRow> for LedgerEntry {
    fn from(l: LedgerRow) -> Self {
        // ref_id 并进 memo：中立结构没有「关联单号」的位置，而对账时必须靠它
        // 把流水与本地订单对上。拼在备注里是唯一不改共用结构的透出方式。
        let memo = match (l.note.filter(|s| !s.trim().is_empty()), l.ref_id) {
            (Some(note), Some(rid)) if !rid.trim().is_empty() => {
                Some(format!("{note}（单号 {rid}）"))
            }
            (Some(note), _) => Some(note),
            (None, Some(rid)) if !rid.trim().is_empty() => Some(format!("单号 {rid}")),
            (None, _) => None,
        };
        Self {
            seq: l.id,
            entry_type: l.kind,
            amount: l.amount,
            balance_after: l.balance_after,
            memo,
            created_at: l.created_at,
        }
    }
}

/// 把 `/my/credits` 的流水包装成分页信封。本家返回定长数组（`?limit=` 控制条数），
/// 无分页元数据，故统一按单页处理。
pub fn ledger_to_paged(rows: Vec<LedgerRow>) -> Paged<LedgerEntry> {
    Paged::from_vec(rows.into_iter().map(LedgerEntry::from).collect())
}

// ============ 提货 ============

/// claim 响应里 `keys[]` 的单项。**两种形态都认。**
///
/// 文档的示例脚本用 `jq -r ".keys[]"` 取值，暗示是**字符串数组**；而
/// `/my/keys` 返回的是带 `key` / `region` 等字段的**对象数组**。两处不一致，
/// 而 claim 一旦返回 2xx 积分就已经扣了 —— 猜错形态等于把付过费的 Key 丢掉。
/// 故用 untagged 枚举同时接住，不赌哪一种。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ClaimedKey {
    /// `["ksk_xxx", "ksk_yyy"]`
    Plain(String),
    /// `[{"key":"ksk_xxx","region":"us-east-1",...}]`
    Detailed(ClaimedKeyDetail),
}

/// 对象形态的 Key 条目。字段名按 `/my/keys` 的实测形态建模。
#[derive(Debug, Clone, Deserialize)]
pub struct ClaimedKeyDetail {
    pub key: String,
    /// 该张 Key 所属 AWS 区域，形如 `us-east-1` / `eu-central-1`。
    /// **这是本家唯一的区域来源** —— 下单不能选区。
    #[serde(default)]
    pub region: Option<String>,
    /// 这一张实际扣的积分（若卖家逐张给）
    #[serde(default, alias = "paid", alias = "unit_price")]
    pub price: Option<f64>,
}

impl ClaimedKey {
    /// 密钥正文
    pub fn key(&self) -> &str {
        match self {
            Self::Plain(s) => s.trim(),
            Self::Detailed(d) => d.key.trim(),
        }
    }

    /// 所属区域（字符串形态没有）
    pub fn region(&self) -> Option<&str> {
        match self {
            Self::Plain(_) => None,
            Self::Detailed(d) => d.region.as_deref().filter(|s| !s.trim().is_empty()),
        }
    }

    /// 逐张实付（字符串形态没有）
    pub fn price(&self) -> Option<f64> {
        match self {
            Self::Plain(_) => None,
            Self::Detailed(d) => d.price,
        }
    }
}

/// `POST /my/keys/claim` 响应。
///
/// 文档只保证有 `keys[]`（示例脚本 `jq -r ".keys[]"`），其余字段名未给，故几个
/// 合理别名都接。缺了它们不影响入库，只影响面板上的扣费回显。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClaimResponse {
    #[serde(default)]
    pub keys: Vec<ClaimedKey>,
    /// 实际成交数
    #[serde(default, alias = "claimed", alias = "purchased", alias = "granted")]
    pub count: Option<u32>,
    /// 我方请求数（卖家若回显）
    #[serde(default)]
    pub requested: Option<u32>,
    /// 提货后账户积分余额
    #[serde(default, alias = "remaining_credits", alias = "balance")]
    pub credits: Option<f64>,
    /// 本单实扣积分。这是对账的权威值。
    #[serde(default, alias = "total_credits", alias = "total_cost", alias = "charged")]
    pub cost: Option<f64>,
    /// 本单单价
    #[serde(default)]
    pub unit_price: Option<f64>,
    /// 我方发过去的幂等键，卖家回显
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// true 表示本次命中了卖家侧的幂等重放，未重复扣积分。
    /// 文档说同一 `client_order_id` 重复提交返回上次那批 Key，但没说带不带标志位。
    #[serde(default, alias = "replay", alias = "idempotent")]
    pub replayed: bool,
}

/// 一批 Key 的共同区域。**全部相同才返回 Some。**
///
/// 卖家逐 Key 给区域，而中立结构 [`PurchaseResult`] 只有一个 `zone`
/// （其余六家都是「整单一个区」）。取舍：
///
/// 自 2026-08-10 起 claim 带 `region` 下单，一单只会来自一个区，混区实际不再发生；
/// 但这里的兜底**不删** —— 它是对「卖家真回了混区」的防线，删掉就变成静默错落区。
///
/// - **全单同区**（单区提货是常态）→ 返回该区，`service::import_purchased` 会把它
///   写进凭据的 `api_region`，请求就会正确打到 `q.{region}.amazonaws.com`。
/// - **混区** → 返回 `None`，整单退回全局默认区入库。这是**已知的能力缺口**：
///   混区单里非默认区的那几张，请求会打到错误的区域端点而失败。修它需要给
///   `PurchasedKey` 加逐张区域并改 `import_keys` 签名（波及全部六家的构造点），
///   属于独立的一件事，本次不做。故此处必须告警，让人能从日志看出原因 ——
///   否则症状是「某几张 Key 莫名不可用」，极难定位。
fn uniform_region(keys: &[ClaimedKey]) -> Option<String> {
    let mut regions = keys.iter().filter_map(|k| k.region());
    let first = regions.next()?;
    if regions.all(|r| r == first) {
        return Some(first.to_string());
    }
    let all: Vec<&str> = keys.iter().filter_map(|k| k.region()).collect();
    tracing::warn!(
        regions = ?all,
        "kiro.ooo 本单混了多个区域，无法逐张设置 api_region，将全部按全局默认区入库；\
         非默认区的 Key 请求会打到错误的区域端点而不可用，需人工在凭据里改 apiRegion"
    );
    None
}

impl From<ClaimResponse> for PurchaseResult {
    fn from(r: ClaimResponse) -> Self {
        // 先算区域：下面 into_iter() 会把 keys 移走
        let zone = uniform_region(&r.keys);
        let unit_price = r.unit_price;

        // 逐张实付之和。**只在每张都给了价时才可用** —— 缺一张就少算一份，
        // 会把实扣报少（报少比报错更难发现）。
        let paid_sum = if !r.keys.is_empty() && r.keys.iter().all(|k| k.price().is_some()) {
            Some(r.keys.iter().filter_map(|k| k.price()).sum::<f64>())
        } else {
            None
        };

        let keys: Vec<PurchasedKey> = r
            .keys
            .into_iter()
            .filter(|k| !k.key().is_empty())
            .map(|k| PurchasedKey {
                price: k.price().or(unit_price),
                key: k.key().to_string(),
                // 本家不下发子号的网页登录凭据，只给 API Key 正文
                account: None,
                password: None,
                issuer_url: None,
                // 本家的区域是订单级的（zone），逐张不带区
                region: None,
            })
            .collect();
        // 卖家回显数与实际条数不一致时取较大者，避免漏入库
        let purchased = r.count.unwrap_or(0).max(keys.len() as u32);
        Self {
            purchased,
            requested: r.requested,
            // 本家 claim 回显的是账户积分余额（与首家同语义）
            remaining: r.credits,
            unit_price,
            // 权威值优先：卖家给的实扣 → 逐张之和 → 单价 × 成交数
            total_debit: r
                .cost
                .or(paid_sum)
                .or_else(|| unit_price.map(|p| p * purchased as f64)),
            // 本家没有独立的卖家侧订单号，回显的就是我方发去的幂等键
            order_id: r.client_order_id,
            keys,
            replayed: r.replayed,
            zone,
        }
    }
}

// ============ 系统状态 ============

/// `GET /status` 响应（**免鉴权**）。
///
/// 实测样本：
/// ```json
/// {"keys_active":417,"keys_alive":1166,"keys_dead":515,"keys_stock":2,
///  "keys_suspect":265,"keys_total":1946,"generating":false,"auto_mode":false,
///  "started_at":"2026-08-10 07:40:58","uptime_secs":18822,
///  "announce":{"enabled":false,"text":"","level":"info",...}}
/// ```
///
/// 与首家 `VendorSystemStatus` 的差异：本家叫 `uptime_secs`（首家是
/// `uptime_seconds`）、多了 `keys_alive` / `keys_suspect` / `auto_mode` / `announce`。
/// 单独建 DTO 再映射，**不给首家的 DTO 加别名** —— 那是上游文件，改它等于
/// 把本家的形态混进别家的真相来源。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SystemStatusResponse {
    #[serde(default)]
    pub keys_active: Option<u32>,
    #[serde(default)]
    pub keys_dead: Option<u32>,
    #[serde(default)]
    pub keys_stock: Option<u32>,
    #[serde(default)]
    pub keys_total: Option<u32>,
    /// 存活总数（含 suspect）。首家没有这个维度，映射时并进 extra 透出。
    #[serde(default)]
    pub keys_alive: Option<u32>,
    /// 疑似已死、待复检的数量。同上，走 extra。
    #[serde(default)]
    pub keys_suspect: Option<u32>,
    #[serde(default)]
    pub generating: Option<bool>,
    /// 本家的字段名。首家叫 `uptime_seconds`。
    #[serde(default)]
    pub uptime_secs: Option<f64>,
    #[serde(default)]
    pub started_at: Option<String>,
    /// 卖家侧是否处于自动开号模式
    #[serde(default)]
    pub auto_mode: Option<bool>,
}

impl From<SystemStatusResponse> for super::flavor_legacy::VendorSystemStatus {
    fn from(r: SystemStatusResponse) -> Self {
        // 本家独有的三个维度放进 extra（该字段就是为「卖家新增字段不必改后端」
        // 准备的），面板据此仍能看到存活总数与待复检数
        let mut extra = serde_json::Map::new();
        if let Some(v) = r.keys_alive {
            extra.insert("keys_alive".to_string(), serde_json::json!(v));
        }
        if let Some(v) = r.keys_suspect {
            extra.insert("keys_suspect".to_string(), serde_json::json!(v));
        }
        if let Some(v) = r.auto_mode {
            extra.insert("auto_mode".to_string(), serde_json::json!(v));
        }
        Self {
            keys_active: r.keys_active,
            keys_dead: r.keys_dead,
            keys_stock: r.keys_stock,
            keys_total: r.keys_total,
            generating: r.generating,
            // 字段改名的落点：本家 uptime_secs → 首家 uptime_seconds
            uptime_seconds: r.uptime_secs,
            started_at: r.started_at,
            // 本家 /status 不给这两个（它们是首家的自动检测配置）
            auto_check: None,
            auto_generate: r.auto_mode,
            check_interval: None,
            extra,
        }
    }
}

// ============ 订单 ============

/// `GET /my/purchase-orders` 单条。**裸数组返回**（与首家同形态）。
///
/// 实测样本：
/// ```json
/// [{"client_order_id":"21a68cccb15074980ffa96dc3a050b3d","requested":1,
///   "purchased":1,"created_at":"2026-08-06 23:14:12","source":"api"}]
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PurchaseOrderRow {
    #[serde(default)]
    pub client_order_id: Option<String>,
    #[serde(default)]
    pub requested: Option<u32>,
    #[serde(default)]
    pub purchased: Option<u32>,
    #[serde(default)]
    pub created_at: Option<String>,
    /// 下单来源：`api` / 网页等。中立结构没有位置，不映射。
    #[serde(default)]
    pub source: Option<String>,
}

impl From<PurchaseOrderRow> for OrderInfo {
    fn from(o: PurchaseOrderRow) -> Self {
        Self {
            // 本家没有独立的卖家侧订单号，client_order_id 就是订单标识，
            // 两个字段都填上：面板的订单列表按 order_id 展示，对账按前者
            order_id: o.client_order_id.clone(),
            client_order_id: o.client_order_id,
            requested: o.requested,
            purchased: o.purchased,
            // 订单列表不给扣费额，要对账得查 /my/credits 的流水
            total_debit: None,
            created_at: o.created_at,
        }
    }
}

/// 裸数组包装成分页信封（本家无分页参数，固定最近 50 条）
pub fn orders_to_paged(rows: Vec<PurchaseOrderRow>) -> Paged<OrderInfo> {
    Paged::from_vec(rows.into_iter().map(OrderInfo::from).collect())
}

// ============ 我的密钥 ============

/// `GET /my/keys` 单条。**本家给密钥正文**（多数家只给前缀）。
///
/// 实测样本：
/// ```json
/// {"id":3886,"key":"ksk_SAMPLE8EAPFIsTKZBg06PJrfjhTFnaGq","region":"us-east-1",
///  "status":"dead","created_at":"2026-08-06 22:59:17","dispatched_at":"2026-08-06 23:14:12",
///  "order_id":"21a68...","master_id":"772763741994","dead_reason":"临时风控锁(母号失效)",
///  "current_usage":1319,"usage_limit":10000,"usage_rate":0,"last_probe":"...",
///  "listing_price":0,"on_sale":false}
/// ```
///
/// 卖家还给 `master_id` / `dead_reason` / `current_usage` / `usage_limit` /
/// `last_probe` / `listing_price` / `on_sale` / `region`，[`VendorKeyInfo`] 都没有
/// 对应位置，故不映射。用量与死因值得展示，但那要先给中立结构加位置，是独立的事。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MyKeyRow {
    #[serde(default)]
    pub id: Option<i64>,
    /// 密钥正文。可与本地凭据池对账。
    #[serde(default)]
    pub key: Option<String>,
    /// `dead` 已失效 / `alive` 或 `active` 存活 / `suspect` 疑似已死待复检
    #[serde(default)]
    pub status: Option<String>,
    /// 该 Key 被开出来的时刻
    #[serde(default)]
    pub created_at: Option<String>,
    /// 发车（分配给我）的时刻，即我方购得时间
    #[serde(default)]
    pub dispatched_at: Option<String>,
}

impl From<MyKeyRow> for VendorKeyInfo {
    fn from(k: MyKeyRow) -> Self {
        Self {
            id: k.id.map(|v| v.to_string()),
            // 本家真给正文，填上 —— 这是能与本地凭据池逐张对账的少数几家之一
            key_value: k.key.filter(|s| !s.trim().is_empty()),
            // 本家不下发子号账号名
            account: None,
            status: k.status,
            purchased_at: k.dispatched_at,
            created_at: k.created_at,
        }
    }
}

/// `GET /my/keys` 响应
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MyKeysResponse {
    #[serde(default)]
    pub keys: Vec<MyKeyRow>,
    /// 总条数（含已失效）
    #[serde(default)]
    pub count: Option<u32>,
    /// 其中存活数。中立结构没有位置，不映射。
    #[serde(default)]
    pub active: Option<u32>,
}

impl MyKeysResponse {
    /// 转成中立分页结构。本家无分页参数，按单页处理，`total` 用卖家给的 `count`。
    pub fn into_paged(self) -> Paged<VendorKeyInfo> {
        let total = self.count.unwrap_or(self.keys.len() as u32);
        let items: Vec<VendorKeyInfo> = self.keys.into_iter().map(VendorKeyInfo::from).collect();
        Paged {
            page_size: Some((items.len() as u32).max(1)),
            items,
            total: Some(total),
            page: Some(1),
            pages: Some(1),
        }
    }
}

// ============ 最早密钥时间 ============

/// `GET /my/keys/created-at` 响应。
///
/// 实测样本：`{"created_at":"2026-08-06 17:47:56","key_count":3}`
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreatedAtResponse {
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default, alias = "count")]
    pub key_count: Option<u32>,
}

impl From<CreatedAtResponse> for EarliestKeyInfo {
    fn from(r: CreatedAtResponse) -> Self {
        Self {
            created_at: r.created_at.filter(|s| !s.trim().is_empty()),
            count: r.key_count,
        }
    }
}

// ============ 兑换码 ============

/// `POST /my/redeem` 响应。
///
/// **形态未知**：文档的端点表里没有这个接口，但实测 `GET /api/my/redeem` 返回
/// 405 `allow: POST`，说明路由存在。故字段名给足别名 —— 猜错只会让面板少显示
/// 一个到账数字，而兑换本身（同账号同码幂等）已经成功。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RedeemResponse {
    /// 本次到账额度
    #[serde(default, alias = "amount", alias = "added", alias = "quota")]
    pub credits: Option<f64>,
    /// 兑换后余额
    #[serde(default, alias = "credits_after", alias = "remaining")]
    pub balance: Option<f64>,
    #[serde(default, alias = "redeemed_at")]
    pub created_at: Option<String>,
}

impl From<RedeemResponse> for RedeemResult {
    fn from(r: RedeemResponse) -> Self {
        Self {
            quota: r.credits,
            balance: r.balance,
            previous_quota: None,
            redeemed_at: r.created_at,
            // 形态未知，无法判断是否重复兑换，保守记 false（与首家外的各家一致）
            replayed: false,
        }
    }
}

// ============ Webhook 事件名归一化 ============

/// 把本家的事件名映射到中立事件名。
///
/// **本家 webhook 载荷未经实测** —— 要在卖家侧配好地址才能收到，本次没有改动
/// 用户的 webhook 配置。已知的只有文档一句「每次发车我方都会给所有配好 Webhook
/// 的用户推一条到货通知」，以及推送里带 `client_order_id`。
///
/// 事件名的候选取自本家 `/my/notify/prefs` 的开关名（`on_key_new` /
/// `on_key_dead` / `on_key_suspect` / `on_dispatch`）—— 那是卖家自己的通知语汇，
/// webhook 事件名大概率同源。宁可多认几个别名：
///
/// - 认错一个不存在的名字：无代价（永远不会收到）
/// - 漏认真实名字：事件落库成 `unknown`，自动补货完全不工作，且不报错
///
/// 返回 `None` 表示不需要改写，交由上层 [`super::store::VendorEventKind::from_str`]
/// 按标准名解析。
pub fn normalize_event_type(raw: &str) -> Option<&'static str> {
    let norm: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    match norm.as_str() {
        // 到货 / 发车 → 有新货可提
        "keynew" | "onkeynew" | "keysavailable" | "newkeys" | "dispatch" | "ondispatch"
        | "keydispatched" | "dispatchdone" | "fleetdispatched" => Some("new_keys_available"),
        // 全部失效
        "keydead" | "onkeydead" | "alldead" | "keysdead" => Some("all_keys_dead"),
        // 疑似失效：不等于「全部失效」，不能触发补货，落库告警即可。
        // 复用 kiroapp 的「密钥被回收」语义 —— 两者处置相同（只告警、需人工看）。
        "keysuspect" | "onkeysuspect" => Some("key_revoked_abuse"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 线上 `GET /my/stock` 的真实返回（2026-08-10 采样）
    const STOCK_REAL: &str = r#"{"afford":1,"can_buy":true,"claimable":2,"credits":45,
        "max":2,"remaining":0,"short_credits":0,"stock":2,"unit_price":45}"#;

    /// 线上 `GET /my/stock/regions` 的真实返回（2026-08-10 18:00 采样，
    /// 车次明细已裁剪）。注意**美区关停 0 库存、单价 80；欧区开放 13 个、单价 50**
    /// —— 而同一时刻扁平 `/my/stock` 报的是欧区那一份。
    const REGIONS_REAL: &str = r#"{"credits":145,"fleet_active":false,
        "fleet_now":"2026-08-10 18:00:14","fleet_started_at":"","ok":true,"remaining":145,
        "regions":[
          {"afford":1,"batches":[],"can_buy":false,"claimable":0,
           "dispatches":[{"alive":0,"dead":0,"dead_at":"","delivered":9,"running":true,
                          "time":"2026-08-10 17:41:00"}],
           "label":"美国区","open":false,"region":"us-east-1","short_credits":0,
           "stock":0,"unit_price":80},
          {"afford":2,"batches":[{"count":8,"time":"2026-08-10 17:41:00"}],"can_buy":true,
           "claimable":13,
           "dispatches":[{"alive":8,"dead":0,"dead_at":"","delivered":0,"running":true,
                          "time":"2026-08-10 17:41:00"}],
           "label":"欧洲区","open":true,"region":"eu-central-1","short_credits":0,
           "stock":13,"unit_price":50}]}"#;

    /// 线上 `GET /my/stock/regions` 的**车次部分**真实返回（2026-08-10 18:28:12 采样）。
    ///
    /// 这份样本的价值在于同一区里既有活着的车（`dead_at` 空）也有死掉的车
    /// （`dead_at` 有值、`running=false`），且 `batches[]` 指向的不是最新那趟历史车。
    const REGIONS_FLEET_REAL: &str = r#"{"credits":145,"fleet_active":false,
        "fleet_now":"2026-08-10 18:28:12","fleet_started_at":"","ok":true,
        "regions":[
          {"region":"us-east-1","label":"美国区","open":true,"can_buy":true,
           "claimable":1,"stock":1,"afford":1,"unit_price":80,
           "batches":[{"count":1,"time":"2026-08-10 18:19:00"}],
           "dispatches":[
             {"alive":1,"dead":0,"dead_at":"","delivered":8,"running":true,
              "time":"2026-08-10 18:19:00"},
             {"alive":0,"dead":10,"dead_at":"2026-08-10 18:23:53","delivered":0,
              "running":false,"time":"2026-08-10 18:04:00"},
             {"alive":0,"dead":0,"dead_at":"","delivered":9,"running":true,
              "time":"2026-08-10 17:41:00"}]},
          {"region":"eu-central-1","label":"欧洲区","open":true,"can_buy":true,
           "claimable":5,"stock":5,"afford":2,"unit_price":50,
           "batches":[{"count":5,"time":"2026-08-10 18:19:00"}],
           "dispatches":[
             {"alive":5,"dead":0,"dead_at":"","delivered":3,"running":true,
              "time":"2026-08-10 18:19:00"},
             {"alive":0,"dead":0,"dead_at":"","delivered":8,"running":true,
              "time":"2026-08-10 17:41:00"}]}]}"#;

    /// 线上 `GET /my/profile` 的真实返回（注意 quota / remaining / used_quota 全 0）
    const PROFILE_REAL: &str = r#"{"auto_fleet":false,"claimable":1,"is_fleet_owner":false,
        "is_super":false,"min_reserve":1,"name":"claywong","needs_2fa":false,"quota":0,
        "remaining":0,"reserve_count":0,"risk_at":"","risk_flag":0,"risk_rate":0,
        "risk_threshold":100,"role":"","twofa_ok":true,"used_quota":0,"user_no":"U100167",
        "username":"claywong","webhook_url":""}"#;

    /// 线上 `GET /status` 的真实返回
    const STATUS_REAL: &str = r#"{"announce":{"enabled":false,"text":"","level":"info"},
        "auto_mode":false,"generating":false,"keys_active":417,"keys_alive":1166,
        "keys_dead":515,"keys_stock":2,"keys_suspect":265,"keys_total":1946,
        "started_at":"2026-08-10 07:40:58","uptime_secs":18822}"#;

    // ============ 余额语义：本家最容易写错的地方 ============

    /// **本家最关键的一条断言。**
    ///
    /// 照 legacy 映射 `balance ← remaining` 会得到 0：本家 `remaining` 是剩余配额
    /// 且实测恒为 0，真实余额在 `credits`。写错的症状是面板余额显示 0、自动提取
    /// 算出的可提数量恒为 0 —— 整家静默不可用，且不报任何错。
    #[test]
    fn 库存的余额取credits而非remaining() {
        let s: StockInfo = serde_json::from_str::<StockResponse>(STOCK_REAL).unwrap().into();
        assert_eq!(s.balance, Some(45.0), "余额必须取 credits");
        assert_ne!(s.balance, Some(0.0), "取到 remaining(=0) 就是写错了");
    }

    /// 档案同理：不能回退到 remaining，且不映射恒 0 的 quota / used_quota
    #[test]
    fn 档案不把恒零的配额映射出去() {
        let p: ProfileInfo = serde_json::from_str::<ProfileResponse>(PROFILE_REAL)
            .unwrap()
            .into();
        // 档案响应里没有 credits，故余额留空由 client 层用 /my/credits 补，
        // 绝不能因为「有个 remaining 就拿来用」而填 0
        assert!(p.balance.is_none(), "档案没给 credits 时余额应留空而非填 0");
        assert!(p.quota.is_none(), "本家 quota 恒 0，映射过去是无意义读数");
        assert!(p.used_quota.is_none());
        assert_eq!(p.name.as_deref(), Some("claywong"));
        // 可领上限当单次最大购买数，供提取弹窗限制输入
        assert_eq!(p.max_purchase, Some(1));
        // 空串 webhook 要归一成 None，否则面板会显示一个空地址当"已配置"
        assert!(p.webhook_url.is_none());
    }

    #[test]
    fn 档案有credits时直接用() {
        // 卖家日后在档案里补上 credits，就能省掉一次 /my/credits 请求
        let p: ProfileInfo = serde_json::from_str::<ProfileResponse>(
            r#"{"name":"x","credits":88,"remaining":0}"#,
        )
        .unwrap()
        .into();
        assert_eq!(p.balance, Some(88.0));
    }

    #[test]
    fn 用户名兜底登录名() {
        let p: ProfileInfo =
            serde_json::from_str::<ProfileResponse>(r#"{"name":"","username":"u1"}"#)
                .unwrap()
                .into();
        assert_eq!(p.name.as_deref(), Some("u1"), "name 为空串时用 username");
    }

    // ============ 可提数量：四个字段取小 ============

    /// 实测 `claimable=2` 而 `afford=1`（45 积分、单价 45）。报 2 会让面板显示一个
    /// 提不到的数，自动提取也会按 2 下单。
    #[test]
    fn 可提数量按afford收敛() {
        let s: StockInfo = serde_json::from_str::<StockResponse>(STOCK_REAL).unwrap().into();
        assert_eq!(s.available, 1, "afford=1 时不能报 claimable 的 2");
        assert_eq!(s.price_min, Some(45.0));
        assert_eq!(s.price_max, Some(45.0));
        // 分区在 /my/stock/regions，本端点给不出来，留空（见 扁平库存不伪造分区）
        assert!(s.zones.is_empty());
    }

    // ============ 双区货架：2026-08-10 改版后新增 ============

    /// **本次改版最关键的一条断言。**
    ///
    /// 改版前本家按「不分区」实现，下单不带 `region`。而卖家默认区是
    /// `us-east-1`，实测该区 `open=false` / `stock=0`，同时扁平 `/my/stock` 报的
    /// 是欧区的 13 个 / 单价 50 —— 症状是面板显示有货、下单永远失败。
    #[test]
    fn 货架选到开放有货的欧区而非默认美区() {
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(REGIONS_REAL)
            .unwrap()
            .into();
        assert_eq!(s.zones.len(), 2, "双区都要给全，没货的区也要列出来");

        let picked = s.pick_zone().expect("欧区开放且有货，必须能选出来");
        assert_eq!(
            picked.zone, "eu-central-1",
            "必须选开放有货的欧区；选到默认的美区（open=false）会永远提不出货"
        );
        assert_ne!(
            picked.zone, DEFAULT_REGION,
            "不带 region 时卖家就用这个默认区，而它此刻是关停的"
        );
        // 区代码是完整 AWS 标识，不是首家的 us / eu 短码 —— 原样回传给 claim
        assert!(picked.zone.contains('-'), "区代码要能直接当 region 参数用");
    }

    #[test]
    fn 货架逐区映射数量与单价() {
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(REGIONS_REAL)
            .unwrap()
            .into();
        let us = s.find_zone("us-east-1").expect("美区要列出");
        let eu = s.find_zone("eu-central-1").expect("欧区要列出");

        assert!(!us.enabled, "open=false 必须落成不可用");
        assert_eq!(us.available, 0);
        assert_eq!(us.label.as_deref(), Some("美国区"));
        // 关停区的单价仍要读出来给面板展示，只是不参与报价区间
        assert_eq!(us.unit_price, Some(80.0));

        assert!(eu.enabled);
        assert_eq!(eu.stock, Some(13), "仓库存货照实给");
        assert_eq!(
            eu.available, 2,
            "claimable=13 但 afford=2（145 积分 / 单价 50），必须按 afford 收敛"
        );
        assert_eq!(eu.unit_price, Some(50.0));
        assert_eq!(eu.label.as_deref(), Some("欧洲区"));

        // 顶层：可提量是各区之和，余额取 credits（省一次 /my/credits）
        assert_eq!(s.available, 2);
        assert_eq!(s.balance, Some(145.0), "货架端点同时给余额");
        // 报价只算开放有货的区：把关停美区的 80 算进来，面板会显示一个提不到的价
        assert_eq!(s.price_min, Some(50.0));
        assert_eq!(
            s.price_max,
            Some(50.0),
            "关停区的 80 不能进报价区间，否则显示 50-80 会让人以为能按 80 提美区"
        );
    }

    // ============ 发车时间与存活时长 ============

    /// 存活时长要**只用卖家自己时钟内的差值**算，不能依赖本机时区。
    ///
    /// 样本：`fleet_now` 18:28:12，欧区当前车 18:19:00 发出且还活着
    /// （`dead_at` 空）→ 存活 9 分 12 秒 = 552 秒。
    #[test]
    fn 存活时长按卖家时钟算差值() {
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(REGIONS_FLEET_REAL)
            .unwrap()
            .into();
        let eu = s.find_zone("eu-central-1").unwrap();
        assert_eq!(
            eu.alive_secs,
            Some(552),
            "18:28:12 − 18:19:00 = 9分12秒；算错通常是拿本机时钟去减卖家时刻"
        );
        // 发车时刻要能换成 Unix 秒给前端，且落在「刚刚过去」的区间内
        let departed = eu.departed_at.expect("发车时间要透出");
        let ago = chrono::Utc::now().timestamp() - departed;
        assert!(
            (540..=600).contains(&ago),
            "距今应约 552 秒（容忍测试耗时），实际 {ago}"
        );
        // 本家不给文案，前端按 alive_secs 自己格式化
        assert!(eu.alive_text.is_none());
    }

    /// 已死的车用 `dead_at` 封顶 —— 否则死掉的车存活时长还会随时间一直涨。
    ///
    /// 构造：把美区的 `batches` 指向那趟 18:04 发车、18:23:53 报废的车，
    /// 存活应是 19 分 53 秒 = 1193 秒的**终值**，与 `fleet_now` 无关。
    #[test]
    fn 已死的车存活时长取终值() {
        let raw = r#"{"fleet_now":"2026-08-10 18:28:12","regions":[
            {"region":"us-east-1","open":true,"claimable":1,"stock":1,"afford":1,
             "batches":[{"count":1,"time":"2026-08-10 18:04:00"}],
             "dispatches":[
               {"alive":0,"dead":10,"dead_at":"2026-08-10 18:23:53","running":false,
                "time":"2026-08-10 18:04:00"},
               {"alive":1,"dead":0,"dead_at":"","running":true,
                "time":"2026-08-10 18:19:00"}]}]}"#;
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(raw).unwrap().into();
        let us = s.find_zone("us-east-1").unwrap();
        assert_eq!(
            us.alive_secs,
            Some(1193),
            "18:23:53 − 18:04:00 = 19分53秒，是终值；拿 fleet_now 去减会算成 24分12秒"
        );
    }

    /// 展示的是**能提到的那批货**所属的车，不是历史上最新那趟。
    ///
    /// 构造：最新车次是 18:19 那趟，但 `batches` 指向 17:41 那趟（18:19 的已被提空）。
    /// 问「这车跑了多久」问的是手上能提的货，故应取 17:41。
    #[test]
    fn 取可提批次对应的车而非最新历史车() {
        let raw = r#"{"fleet_now":"2026-08-10 18:28:12","regions":[
            {"region":"eu-central-1","open":true,"claimable":3,"stock":3,"afford":3,
             "batches":[{"count":3,"time":"2026-08-10 17:41:00"}],
             "dispatches":[
               {"alive":0,"dead":0,"dead_at":"","running":true,"time":"2026-08-10 18:19:00"},
               {"alive":3,"dead":0,"dead_at":"","running":true,"time":"2026-08-10 17:41:00"}]}]}"#;
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(raw).unwrap().into();
        let eu = s.find_zone("eu-central-1").unwrap();
        assert_eq!(
            eu.alive_secs,
            Some(2832),
            "18:28:12 − 17:41:00 = 47分12秒；取成 18:19 那趟就答错了问题"
        );
    }

    /// 无货的区（`batches` 空）退回最新历史车，用来回答「上一趟什么时候发的」
    #[test]
    fn 无货时退回最新历史车() {
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(REGIONS_REAL)
            .unwrap()
            .into();
        let us = s.find_zone("us-east-1").unwrap();
        // 美区 batches 空，只有 17:41 那一趟；fleet_now 18:00:14 → 19分14秒
        assert_eq!(us.alive_secs, Some(1154));
        assert!(
            us.departed_at.is_some(),
            "关停无货的区也该显示上一趟发车时间，那正是判断「下一趟大概何时」的依据"
        );
    }

    /// 卖家数组顺序是实现细节，不能依赖。乱序时仍要取时刻最大的那趟。
    #[test]
    fn 车次乱序时仍取最新() {
        let raw = r#"{"fleet_now":"2026-08-10 18:28:12","regions":[
            {"region":"us-east-1","open":true,"claimable":1,"stock":1,"afford":1,
             "batches":[],
             "dispatches":[
               {"dead_at":"","running":true,"time":"2026-08-10 17:41:00"},
               {"dead_at":"","running":true,"time":"2026-08-10 18:19:00"},
               {"dead_at":"","running":true,"time":"2026-08-10 18:04:00"}]}]}"#;
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(raw).unwrap().into();
        // 18:28:12 − 18:19:00 = 552，说明取到了最新那趟而非数组第一个
        assert_eq!(s.zones[0].alive_secs, Some(552));
    }

    /// 缺 `fleet_now` 时**不猜时区**：宁可不显示，也不显示一个偏 8 小时的数
    #[test]
    fn 缺卖家当前时刻时不猜时区() {
        let raw = r#"{"regions":[{"region":"us-east-1","open":true,"claimable":1,
            "stock":1,"afford":1,"batches":[{"count":1,"time":"2026-08-10 18:19:00"}],
            "dispatches":[{"dead_at":"","running":true,"time":"2026-08-10 18:19:00"}]}]}"#;
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(raw).unwrap().into();
        assert!(s.zones[0].alive_secs.is_none(), "没有基准时刻就不能算差值");
        assert!(s.zones[0].departed_at.is_none());
        // 其余字段照常
        assert_eq!(s.zones[0].available, 1);
    }

    /// 畸形 / 空时刻串不能让整区解析失败或算出 1970 年
    #[test]
    fn 时刻串畸形时车次字段留空() {
        let raw = r#"{"fleet_now":"2026-08-10 18:28:12","regions":[
            {"region":"us-east-1","open":true,"claimable":1,"stock":1,"afford":1,
             "batches":[{"count":1,"time":""}],
             "dispatches":[{"dead_at":"","running":true,"time":"not-a-time"}]}]}"#;
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(raw).unwrap().into();
        assert!(s.zones[0].alive_secs.is_none());
        assert!(
            s.zones[0].departed_at.is_none(),
            "解析不出来要留空，落成 0 会让前端显示 1970 年"
        );
        assert_eq!(s.zones[0].available, 1, "车次解析失败不该影响可提数量");
    }

    /// 卖家时刻自相矛盾（发车时刻晚于当前时刻）时不报负数
    #[test]
    fn 发车时刻在未来时不报负存活() {
        let raw = r#"{"fleet_now":"2026-08-10 18:00:00","regions":[
            {"region":"us-east-1","open":true,"claimable":1,"stock":1,"afford":1,
             "batches":[{"count":1,"time":"2026-08-10 19:00:00"}],
             "dispatches":[{"dead_at":"","running":true,"time":"2026-08-10 19:00:00"}]}]}"#;
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(raw).unwrap().into();
        assert!(s.zones[0].alive_secs.is_none(), "负的存活时长不如不报");
        assert!(
            s.zones[0].departed_at.is_none(),
            "前端按 now − departedAt 算「多久前发车」，未来时刻会显示负数"
        );
    }

    /// `can_buy=false` 与 `open=false` 任一为假都提不出来
    #[test]
    fn 货架的开放与可买缺一不可() {
        let mk = |open, can_buy| StockRegion {
            region: "eu-central-1".into(),
            open,
            can_buy,
            claimable: Some(5),
            stock: Some(5),
            afford: Some(5),
            ..Default::default()
        };
        assert_eq!(mk(true, true).effective_available(), 5);
        assert_eq!(mk(false, true).effective_available(), 0, "关停区提不出货");
        assert_eq!(
            mk(true, false).effective_available(),
            0,
            "卖家说不能买时比任何数量字段权威"
        );
    }

    /// 数量字段全缺时不能凭空造数 —— 那会触发一笔提不到的扣费单
    #[test]
    fn 货架数量字段全缺时按零处理() {
        let r: StockRegion =
            serde_json::from_str(r#"{"region":"us-east-1","open":true}"#).unwrap();
        assert_eq!(r.effective_available(), 0);
        // can_buy 缺失按 true（老响应没这个字段）
        assert!(r.can_buy);
    }

    /// 卖家回滚或改端点时 regions 为空：不能造出一个「默认区」
    #[test]
    fn 货架为空时不造区() {
        let s: StockInfo = serde_json::from_str::<RegionsResponse>(r#"{"credits":10}"#)
            .unwrap()
            .into();
        assert!(s.zones.is_empty(), "不知道哪个区有货时不能猜");
        assert!(
            s.pick_zone().is_none(),
            "选不出区应让上层报 NoZoneInStock 挡住下单，而不是赌默认的美区"
        );
        assert_eq!(s.available, 0);
        // 余额仍要读出来
        assert_eq!(s.balance, Some(10.0));
    }

    /// 扁平 `/my/stock` 是退路，它给不出分区 —— 必须留空而非编一个
    #[test]
    fn 扁平库存不伪造分区() {
        let s: StockInfo = serde_json::from_str::<StockResponse>(STOCK_REAL).unwrap().into();
        assert!(
            s.zones.is_empty(),
            "扁平端点没有区域信息，凭空造区会让 pick_zone 选到未经核实的区"
        );
    }

    #[test]
    fn 可提数量取四者最小() {
        let mk = |c, s, a, m| StockResponse {
            claimable: Some(c),
            stock: Some(s),
            afford: Some(a),
            max: Some(m),
            can_buy: true,
            ..Default::default()
        };
        assert_eq!(mk(9, 9, 9, 3).effective_available(), 3, "max 最小时取 max");
        assert_eq!(mk(9, 2, 9, 9).effective_available(), 2, "库存最小时取库存");
        assert_eq!(mk(0, 9, 9, 9).effective_available(), 0);
    }

    /// 卖家自己说不能买时，其余数量字段一概不算 —— 那是比任何数字都权威的结论
    #[test]
    fn can_buy为假时归零() {
        let r = StockResponse {
            claimable: Some(5),
            stock: Some(5),
            afford: Some(5),
            max: Some(5),
            can_buy: false,
            ..Default::default()
        };
        assert_eq!(r.effective_available(), 0);
    }

    /// 老版本响应或字段改名时，不能凭空造一个数出来触发扣费
    #[test]
    fn 数量字段全缺时按零处理() {
        let r: StockResponse = serde_json::from_str(r#"{"credits":100}"#).unwrap();
        assert_eq!(r.effective_available(), 0);
        // can_buy 缺失按 true（老响应没这个字段），但数量仍为 0
        assert!(r.can_buy);
        // 余额仍要读出来
        let s: StockInfo = r.into();
        assert_eq!(s.balance, Some(100.0));
        // 无货时不给报价：显示一个提不到的价位会误导
        assert!(s.price_min.is_none());
    }

    // ============ 提货：两种 keys 形态都要认 ============

    /// 文档示例脚本用 `jq -r ".keys[]"`，暗示字符串数组
    #[test]
    fn claim解析字符串数组形态() {
        let raw = r#"{"keys":["ksk_aaa","ksk_bbb"],"count":2,"credits":10,
            "cost":90,"unit_price":45,"client_order_id":"abc"}"#;
        let r: PurchaseResult = serde_json::from_str::<ClaimResponse>(raw).unwrap().into();
        assert_eq!(r.purchased, 2);
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.keys[0].key, "ksk_aaa");
        assert_eq!(r.remaining, Some(10.0), "claim 的 credits 是提货后余额");
        assert_eq!(r.total_debit, Some(90.0));
        assert_eq!(r.order_id.as_deref(), Some("abc"));
        // 字符串形态没有区域信息
        assert!(r.zone.is_none());
        // 单价能落到逐张价上（面板据此解释总额）
        assert_eq!(r.keys[0].price, Some(45.0));
    }

    /// `/my/keys` 是对象数组，claim 若同形态也要认
    #[test]
    fn claim解析对象数组形态并取区域() {
        let raw = r#"{"keys":[{"key":"ksk_aaa","region":"eu-central-1","price":40},
            {"key":"ksk_bbb","region":"eu-central-1","price":40}],
            "count":2,"credits":5}"#;
        let r: PurchaseResult = serde_json::from_str::<ClaimResponse>(raw).unwrap().into();
        assert_eq!(r.keys.len(), 2);
        assert_eq!(
            r.zone.as_deref(),
            Some("eu-central-1"),
            "全单同区时必须透出区域，否则 eu 区 Key 会按默认区入库而不可用"
        );
        // 卖家没给 cost 时按逐张实付之和兜底
        assert_eq!(r.total_debit, Some(80.0));
        assert_eq!(r.keys[0].price, Some(40.0));
    }

    /// 混区单：区域留空，退回全局默认区（已知能力缺口，见 uniform_region 注释）
    #[test]
    fn claim混区时不透出区域() {
        let raw = r#"{"keys":[{"key":"ksk_a","region":"us-east-1"},
            {"key":"ksk_b","region":"eu-central-1"}],"count":2}"#;
        let r: PurchaseResult = serde_json::from_str::<ClaimResponse>(raw).unwrap().into();
        assert_eq!(r.keys.len(), 2);
        assert!(
            r.zone.is_none(),
            "混区时不能挑一个区当整单的区 —— 那会让另一半 Key 落到错误区域"
        );
    }

    /// 两种形态混在同一个数组里也要能解析（卖家分批改造时可能出现）
    #[test]
    fn claim混合形态也能解析() {
        let raw = r#"{"keys":["ksk_plain",{"key":"ksk_obj","region":"us-east-1"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<ClaimResponse>(raw).unwrap().into();
        assert_eq!(r.keys.len(), 2);
        // 只有一张带区域，视为「全部已知区域相同」—— 另一张没有反证
        assert_eq!(r.zone.as_deref(), Some("us-east-1"));
    }

    #[test]
    fn claim的成交数取较大者并过滤空key() {
        // 卖家回显 1 但实发 2 条，按实际条数算，否则会漏入库
        let raw = r#"{"keys":["ksk_a","ksk_b","  "],"count":1}"#;
        let r: PurchaseResult = serde_json::from_str::<ClaimResponse>(raw).unwrap().into();
        assert_eq!(r.keys.len(), 2, "空白项要丢掉");
        assert_eq!(r.purchased, 2);
    }

    #[test]
    fn claim的别名字段都能接住() {
        // 卖家用另一套字段名时也要读出扣费与余额
        let raw = r#"{"keys":["ksk_a"],"claimed":1,"total_credits":45,
            "remaining_credits":55,"replay":true}"#;
        let r: PurchaseResult = serde_json::from_str::<ClaimResponse>(raw).unwrap().into();
        assert_eq!(r.purchased, 1);
        assert_eq!(r.total_debit, Some(45.0));
        assert_eq!(r.remaining, Some(55.0));
        assert!(r.replayed, "幂等重放要透出，否则面板会把它当成又扣了一次");
    }

    /// 逐张价缺了一张时不能用「之和」—— 会把实扣报少
    #[test]
    fn 逐张价不全时不按之和算总额() {
        let raw = r#"{"keys":[{"key":"ksk_a","price":45},{"key":"ksk_b"}],"count":2}"#;
        let r: PurchaseResult = serde_json::from_str::<ClaimResponse>(raw).unwrap().into();
        // 既无 cost 又无完整逐张价、又无 unit_price → 只能留空，不能报 45
        assert!(
            r.total_debit.is_none(),
            "少算一张会把实扣报少，报少比报错更难发现"
        );
    }

    // ============ 系统状态：字段改名的落点 ============

    #[test]
    fn 系统状态映射到首家结构() {
        let s: super::super::flavor_legacy::VendorSystemStatus =
            serde_json::from_str::<SystemStatusResponse>(STATUS_REAL)
                .unwrap()
                .into();
        assert_eq!(s.keys_active, Some(417));
        assert_eq!(s.keys_dead, Some(515));
        assert_eq!(s.keys_stock, Some(2));
        assert_eq!(s.keys_total, Some(1946));
        assert_eq!(s.generating, Some(false));
        // 本家叫 uptime_secs，首家叫 uptime_seconds —— 这里就是改名的落点
        assert_eq!(
            s.uptime_seconds,
            Some(18822.0),
            "uptime_secs 必须落进 uptime_seconds，否则运行时长显示为空"
        );
        assert_eq!(s.started_at.as_deref(), Some("2026-08-10 07:40:58"));
        // 本家独有的三个维度走 extra 透出
        assert_eq!(s.extra.get("keys_alive").and_then(|v| v.as_u64()), Some(1166));
        assert_eq!(s.extra.get("keys_suspect").and_then(|v| v.as_u64()), Some(265));
        // auto_mode 既进 extra 也映射到 auto_generate（语义相同）
        assert_eq!(s.auto_generate, Some(false));
        // 本家没有这两个（首家的自动检测配置）
        assert!(s.auto_check.is_none());
        assert!(s.check_interval.is_none());
    }

    // ============ 流水 / 订单 / 密钥 / 最早时间 ============

    /// 线上 `/my/credits` 的真实返回
    #[test]
    fn 流水解析真实样本() {
        let raw = r#"{"credits":45,"master_price":500,"ledger":[
            {"id":217,"kind":"claim_key","amount":-45,"balance_after":45,
             "ref_id":"21a68cccb15074980ffa96dc3a050b3d","note":"美国区自助提货 Key #3886",
             "created_at":"2026-08-06 23:14:12"},
            {"id":176,"kind":"recharge","amount":100,"balance_after":130,
             "ref_id":"KA167T1786015231548","note":"支付宝充值 100.00 元到账",
             "created_at":"2026-08-06 19:21:12"}]}"#;
        let r: CreditsResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.credits, Some(45.0));
        assert_eq!(r.master_price, Some(500.0));

        let paged = ledger_to_paged(r.ledger);
        assert_eq!(paged.total, Some(2));
        assert_eq!(paged.items[0].entry_type.as_deref(), Some("claim_key"));
        assert_eq!(paged.items[0].amount, Some(-45.0));
        assert_eq!(paged.items[0].balance_after, Some(45.0));
        // ref_id 必须并进备注：对账要靠它把流水与本地订单对上
        let memo = paged.items[0].memo.as_deref().unwrap();
        assert!(memo.contains("美国区自助提货"), "实际: {memo}");
        assert!(
            memo.contains("21a68cccb15074980ffa96dc3a050b3d"),
            "关联单号必须透出，否则流水无法与订单对账；实际: {memo}"
        );
        assert_eq!(paged.items[1].entry_type.as_deref(), Some("recharge"));
    }

    #[test]
    fn 流水缺备注时只给单号() {
        let rows: Vec<LedgerRow> =
            serde_json::from_str(r#"[{"id":1,"kind":"x","ref_id":"o1"}]"#).unwrap();
        let paged = ledger_to_paged(rows);
        assert_eq!(paged.items[0].memo.as_deref(), Some("单号 o1"));
    }

    /// 订单是裸数组，与首家同形态
    #[test]
    fn 订单裸数组包装成分页() {
        let raw = r#"[{"client_order_id":"21a68cccb15074980ffa96dc3a050b3d","requested":1,
            "purchased":1,"created_at":"2026-08-06 23:14:12","source":"api"}]"#;
        let rows: Vec<PurchaseOrderRow> = serde_json::from_str(raw).unwrap();
        let paged = orders_to_paged(rows);
        assert_eq!(paged.total, Some(1));
        let o = &paged.items[0];
        assert_eq!(
            o.client_order_id.as_deref(),
            Some("21a68cccb15074980ffa96dc3a050b3d")
        );
        // 本家没有独立卖家侧订单号，两个字段填同值
        assert_eq!(o.order_id, o.client_order_id);
        assert_eq!(o.purchased, Some(1));
        assert_eq!(o.requested, Some(1));
    }

    /// 线上 `/my/keys` 的真实返回。本家给密钥正文，是能逐张对账的少数几家之一。
    #[test]
    fn 密钥列表解析真实样本() {
        let raw = r#"{"active":0,"count":3,"keys":[
            {"created_at":"2026-08-06 22:59:17","current_usage":1319,
             "dead_reason":"临时风控锁(母号失效)","dispatched_at":"2026-08-06 23:14:12",
             "id":3886,"key":"ksk_SAMPLE8EAPFIsTKZBg06PJrfjhTFnaGq",
             "last_probe":"2026-08-07 19:02:23","listing_price":0,
             "master_id":"772763741994","on_sale":false,
             "order_id":"21a68cccb15074980ffa96dc3a050b3d","region":"us-east-1",
             "status":"dead","usage_limit":10000,"usage_rate":0}]}"#;
        let paged = serde_json::from_str::<MyKeysResponse>(raw).unwrap().into_paged();
        // total 用卖家给的 count（3），而非本页条数（1）
        assert_eq!(paged.total, Some(3), "总数要用卖家的 count");
        let k = &paged.items[0];
        assert_eq!(k.id.as_deref(), Some("3886"));
        assert_eq!(
            k.key_value.as_deref(),
            Some("ksk_SAMPLE8EAPFIsTKZBg06PJrfjhTFnaGq"),
            "本家给正文，必须填上 —— 这是能与本地凭据池对账的前提"
        );
        assert_eq!(k.status.as_deref(), Some("dead"));
        assert_eq!(k.created_at.as_deref(), Some("2026-08-06 22:59:17"));
        // 发车时刻即我方购得时间
        assert_eq!(k.purchased_at.as_deref(), Some("2026-08-06 23:14:12"));
    }

    #[test]
    fn 密钥列表为空时不给出零页宽() {
        let paged = serde_json::from_str::<MyKeysResponse>(r#"{"keys":[],"count":0}"#)
            .unwrap()
            .into_paged();
        assert_eq!(paged.total, Some(0));
        // 前端拿 page_size 做除数，0 会出 NaN
        assert_eq!(paged.page_size, Some(1));
    }

    #[test]
    fn 最早密钥时间解析真实样本() {
        let e: EarliestKeyInfo = serde_json::from_str::<CreatedAtResponse>(
            r#"{"created_at":"2026-08-06 17:47:56","key_count":3}"#,
        )
        .unwrap()
        .into();
        assert_eq!(e.created_at.as_deref(), Some("2026-08-06 17:47:56"));
        assert_eq!(e.count, Some(3));
    }

    #[test]
    fn 最早密钥时间为空串时归none() {
        let e: EarliestKeyInfo =
            serde_json::from_str::<CreatedAtResponse>(r#"{"created_at":"","key_count":0}"#)
                .unwrap()
                .into();
        // 空串会让面板显示一个空日期，归 None 才能走「暂无」分支
        assert!(e.created_at.is_none());
    }

    #[test]
    fn 兑换响应的别名都能接住() {
        for raw in [
            r#"{"credits":100,"balance":145}"#,
            r#"{"amount":100,"credits_after":145}"#,
            r#"{"quota":100,"remaining":145}"#,
        ] {
            let r: RedeemResult = serde_json::from_str::<RedeemResponse>(raw).unwrap().into();
            assert_eq!(r.quota, Some(100.0), "解析失败: {raw}");
            assert_eq!(r.balance, Some(145.0), "解析失败: {raw}");
        }
    }

    // ============ 事件名归一化 ============

    #[test]
    fn 事件名归一化到新货() {
        for raw in [
            "key_new",
            "on_key_new",
            "keys_available",
            "dispatch",
            "on_dispatch",
            "KeyNew",
            "key-new",
        ] {
            assert_eq!(
                normalize_event_type(raw),
                Some("new_keys_available"),
                "未识别: {raw}"
            );
        }
    }

    #[test]
    fn 事件名归一化到全失效() {
        for raw in ["key_dead", "on_key_dead", "all_dead", "keys_dead"] {
            assert_eq!(normalize_event_type(raw), Some("all_keys_dead"), "未识别: {raw}");
        }
    }

    /// 疑似失效**不能**当成全失效 —— 那会在旧 Key 可能还活着时触发补货扣费
    #[test]
    fn 疑似失效不触发补货() {
        assert_eq!(
            normalize_event_type("key_suspect"),
            Some("key_revoked_abuse"),
            "疑似失效只该告警，不能映射成 all_keys_dead"
        );
        assert_ne!(normalize_event_type("on_key_suspect"), Some("all_keys_dead"));
    }

    /// 标准名与未知名都返回 None，交由上层按标准名解析（标准名本就能认）
    #[test]
    fn 标准名与未知名不改写() {
        assert!(normalize_event_type("new_keys_available").is_none());
        assert!(normalize_event_type("all_keys_dead").is_none());
        assert!(normalize_event_type("test").is_none());
        assert!(normalize_event_type("某个没见过的事件").is_none());
    }
}
