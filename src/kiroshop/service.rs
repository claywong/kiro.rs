//! KiroShop 服务：在途跟踪、增量拿号入库、事件轮询、自动补货、webhook 验签
//!
//! 三条后台节奏（单 Key 限速 60 次/分钟，按此预算设计）：
//! - 事件轮询（默认 45s）：`GET /events` 追新，`order.delivery` → 增量拿号入库。
//! - 发货轮询（默认 60s）：对每张未终态的在途单 `GET /delivery?after_seq=`；
//!   顺带跑自动补货判据（池内可用号 + 在途 < 目标 → 下单）。
//! - 账户刷新（约 4.5min 一轮，搭事件轮询的车）：余额告警状态机。
//!
//! webhook 与轮询双路径收敛到同一入口 [`KiroshopService::handle_shop_event`]，
//! 以卖家事件 id 去重；处理失败会把已落库的事件行撤掉，靠卖家重试或轮询兜底。
//!
//! @author wangzhong

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::admin::AdminService;
use crate::vendor::import::import_keys;
use crate::vendor::protocol::PurchasedKey;
use crate::vendor::store::PurchaseOutcome;

use super::client::{
    AccountInfo, KiroshopClient, KiroshopError, Order, OrderDetail, OrderRequest, Product, Quote,
    Result, ShopEvent,
};
use super::config::KiroshopConfig;

use super::store::{KiroshopStore, TrackedOrder};

/// 面板购买参数
#[derive(Debug, Clone, Default)]
pub struct BuyParams {
    pub product_id: i64,
    pub quantity: u32,
    /// None → 商品默认档位（普通优先）
    pub option_key: Option<String>,
    pub coupon_code: Option<String>,
    /// 价格保护上限；None 时按报价金额自动设定（涨一分钱都会被 409 拒）
    pub max_amount: Option<f64>,
    /// 现货不足时是否转预定排队
    pub allow_backorder: bool,
    pub client_ref: Option<String>,
    /// 幂等键。None 自动生成；重试必须复用同一个
    pub idempotency_key: Option<String>,
    /// manual / auto / migrated
    pub source: String,
}

/// 购买结果摘要
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuyOutcome {
    pub order_id: String,
    pub amount: Option<f64>,
    pub quantity: u32,
    pub status: String,
    pub status_text: Option<String>,
    /// 本轮实际新到货数量（预定单通常 0，到货后由轮询入库）
    pub imported: u32,
    pub duplicated: u32,
    pub failed: u32,
    pub last_error: Option<String>,
}

pub struct KiroshopService {
    http: reqwest::Client,
    config: RwLock<KiroshopConfig>,
    store: KiroshopStore,
    admin: Arc<AdminService>,
    /// 供设置持久化的 config.json 读写入口
    persist_config: Mutex<Option<Arc<dyn ConfigPersist>>>,
    /// 最近一次账户快照（账户轮询写入，/status 直读避免每次打卖家）
    last_account: RwLock<Option<AccountInfo>>,
    /// 最近一次后台错误（面板红字）
    last_error: RwLock<Option<String>>,
    /// 自动补货失败后的冷却截止（unix 秒）
    autobuy_cooldown_until: RwLock<i64>,
    /// buy 流程串行化：同刻只允许一笔下单在飞（幂等键各不相同，串行只为控预算）
    buy_lock: tokio::sync::Mutex<()>,
}

/// config.json 持久化回调。由 main 注入 token_manager 的 update_config_file，
/// 避免 kiroshop 模块直接依赖 token_manager 的具体类型。
pub trait ConfigPersist: Send + Sync {
    fn update(&self, updater: &str, f: Box<dyn FnOnce(&mut crate::model::config::Config)>);
}

impl ConfigPersist for crate::kiro::token_manager::MultiTokenManager {
    fn update(&self, _updater: &str, f: Box<dyn FnOnce(&mut crate::model::config::Config)>) {
        if let Err(e) = self.update_config_file(f) {
            tracing::warn!("KiroShop：config.json 持久化失败: {e}");
        }
    }
}

impl KiroshopService {
    pub fn new(
        http: reqwest::Client,
        config: KiroshopConfig,
        store: KiroshopStore,
        admin: Arc<AdminService>,
    ) -> Arc<Self> {
        Arc::new(Self {
            http,
            config: RwLock::new(config),
            store,
            admin,
            persist_config: Mutex::new(None),
            last_account: RwLock::new(None),
            last_error: RwLock::new(None),
            autobuy_cooldown_until: RwLock::new(0),
            buy_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn set_config_persister(&self, p: Arc<dyn ConfigPersist>) {
        *self.persist_config.lock() = Some(p);
    }

    pub fn config(&self) -> KiroshopConfig {
        self.config.read().clone()
    }

    /// 运行时更新配置段并持久化（设置面板入口）
    pub fn update_config(&self, new_cfg: KiroshopConfig) -> Result<()> {
        let persist = self.persist_config.lock().clone();
        let for_file = new_cfg.clone();
        if let Some(p) = persist {
            p.update(
                "kiroshop/settings",
                Box::new(move |c: &mut crate::model::config::Config| {
                    c.kiroshop = Some(for_file)
                }),
            );
        } else {
            tracing::warn!("KiroShop：未注入配置持久化入口，设置仅在本进程生效");
        }
        *self.config.write() = new_cfg;
        Ok(())
    }

    fn client(&self) -> KiroshopClient {
        KiroshopClient::new(self.http.clone(), &self.config.read().clone())
    }

    /// 面板直连用（handlers 调卖家 API 的统一入口）
    pub fn api(&self) -> KiroshopClient {
        self.client()
    }

    fn ready(&self) -> bool {
        self.config.read().is_ready()
    }

    fn not_ready<T>() -> Result<T> {
        Err(KiroshopError {
            status: None,
            code: Some("not_configured".into()),
            message: "KiroShop 未配置或未启用（config.json 的 kiroshop 段缺 apiKey）".into(),
        })
    }

    pub fn store(&self) -> &KiroshopStore {
        &self.store
    }

    pub fn last_account(&self) -> Option<AccountInfo> {
        self.last_account.read().clone()
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.read().clone()
    }

    fn set_last_error(&self, e: Option<String>) {
        *self.last_error.write() = e;
    }

    // ============ 购买主流程 ============

    /// 完整下单：报价 → 建单（pay_with=balance + 价格保护 + 幂等键）→
    /// 兜底支付 → 现货则取号 → 增量拿号入库 → 落在途跟踪。
    pub async fn buy(&self, p: BuyParams) -> Result<BuyOutcome> {
        if !self.ready() {
            return Self::not_ready();
        }
        let _guard = self.buy_lock.lock().await;
        let client = self.client();

        // 1. 商品与档位
        let products = client.products().await?;
        let product = products
            .iter()
            .find(|x| x.id == p.product_id)
            .ok_or_else(|| KiroshopError {
                status: None,
                code: Some("product_not_found".into()),
                message: format!("商品 {} 不存在或已下架", p.product_id),
            })?;
        let option_key = p
            .option_key
            .clone()
            .or_else(|| product.default_option().map(|o| o.key.clone()))
            .filter(|k| !k.trim().is_empty());

        let mut req = OrderRequest {
            product_id: p.product_id,
            quantity: p.quantity.max(1),
            option_key: option_key.clone(),
            coupon_code: p.coupon_code.clone().filter(|c| !c.trim().is_empty()),
            pay_with: "balance".into(),
            backorder: if p.allow_backorder { "allow" } else { "deny" }.into(),
            max_amount: None,
            client_ref: p.client_ref.clone(),
        };

        // 2. 报价：拿精确金额与现货数。价格保护：用户给了上限就按用户的（哪怕低于
        //    报价 → 409 price_changed 拒单），没给就按报价金额锁死，涨一分都拒。
        let quote: Quote = client.quote(&req).await?;
        req.max_amount = p.max_amount.or(quote.amount);
        let in_stock_now = quote.in_stock_now.unwrap_or(0).max(0) as u32;

        // 3. 建单（幂等键复用可安全重试）
        let idem = p
            .idempotency_key
            .clone()
            .unwrap_or_else(|| format!("ks-{}", Uuid::new_v4()));
        let mut created: OrderDetail = client.create_order(&req, &idem).await?;

        // 4. 兜底支付：站方若按文档默认值把单建成待支付，这里补一刀
        //    （pay 幂等，已付过返回 already_paid 语义）
        if created.order.status.as_deref() == Some("pending") {
            match client.pay_order(&created.order.id).await {
                Ok(paid) => created = paid,
                Err(e) => {
                    // 402 余额不足等：订单已存在，把订单号带回去，别让用户换键重下
                    self.track_from_order(&created.order, &p.source);
                    return Err(KiroshopError {
                        status: e.status,
                        code: e.code.clone().or(Some("pay_failed".into())),
                        message: format!(
                            "订单 {} 已创建，但支付失败：{}。请充值后在面板对该订单点「付款」",
                            created.order.id, e
                        ),
                    });
                }
            }
        }

        let order_id = created.order.id.clone();
        let quantity = created.order.quantity.unwrap_or(req.quantity);

        // 5. 现货单立即取号；预定单（现货不足 / waiting_stock）只跟踪，到货靠轮询
        let mut pickup_note: Option<String> = None;
        if in_stock_now >= quantity {
            match client.pickup_order(&order_id).await {
                Ok(_) => {}
                Err(e) if e.code.as_deref() == Some("waiting_stock") => {
                    pickup_note = Some("现货刚被买走，订单已转预定排队，到货后自动入库".to_string());
                }
                Err(e) if e.code.as_deref() == Some("not_paid") => {
                    // 罕见：建单既没自动扣款、状态又已翻到 paid 之外。补付后重试一次
                    if let Ok(_paid) = client.pay_order(&order_id).await {
                        if let Err(e2) = client.pickup_order(&order_id).await {
                            pickup_note = Some(format!("取号未完成：{e2}（到货/补付后可在面板重试）"));
                        }
                    }
                }
                Err(e) if e.code.as_deref() == Some("pickup_checking") => {
                    // 卖家正在验货，稍候重试三次
                    let mut ok = false;
                    for _ in 0..3 {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        if client.pickup_order(&order_id).await.is_ok() {
                            ok = true;
                            break;
                        }
                    }
                    if !ok {
                        pickup_note = Some("卖家验货中，稍后由轮询自动收货".to_string());
                    }
                }
                Err(e) => {
                    pickup_note = Some(format!("取号失败：{e}；订单已保存，可在面板重试取号"));
                }
            }
        } else if p.allow_backorder {
            pickup_note = Some(format!(
                "现货 {} < 需要 {}，订单转预定排队（{}），到货自动入库",
                in_stock_now,
                quantity,
                quote.eta.as_deref().unwrap_or("eta 未知")
            ));
        }

        // 6. 落跟踪 + 立即增量收一轮货（现货单此时通常已可全量拿）
        let detail = client.order(&order_id).await.unwrap_or(created);
        self.upsert_tracked(&detail.order, &p.source);
        let outcome = self.poll_delivery_for_order(&order_id).await;

        let mut result = BuyOutcome {
            order_id: order_id.clone(),
            amount: detail.order.amount.or(quote.amount),
            quantity,
            status: detail.order.status.clone().unwrap_or_default(),
            status_text: detail.order.status_text.clone(),
            imported: outcome.as_ref().map(|o| o.imported).unwrap_or(0),
            duplicated: outcome.as_ref().map(|o| o.duplicated).unwrap_or(0),
            failed: outcome.as_ref().map(|o| o.failed).unwrap_or(0),
            last_error: outcome.and_then(|o| o.last_error),
        };
        if let Some(note) = pickup_note {
            result.last_error = Some(match result.last_error {
                Some(prev) => format!("{prev}；{note}"),
                None => note,
            });
        }
        tracing::info!(
            order = %order_id,
            product = p.product_id,
            quantity,
            imported = result.imported,
            "KiroShop 下单完成"
        );
        Ok(result)
    }

    /// 手动取号（面板按钮）：取号 + 增量入库
    pub async fn manual_pickup(&self, order_id: &str) -> Result<BuyOutcome> {
        if !self.ready() {
            return Self::not_ready();
        }
        let client = self.client();
        let (_, _) = client.pickup_order(order_id).await?;
        let outcome = self.poll_delivery_for_order(order_id).await;
        let detail = client.order(order_id).await?;
        self.refresh_tracked(&detail.order);
        Ok(BuyOutcome {
            order_id: order_id.to_string(),
            amount: detail.order.amount,
            quantity: detail.order.quantity_count(),
            status: detail.order.status.clone().unwrap_or_default(),
            status_text: detail.order.status_text.clone(),
            imported: outcome.as_ref().map(|o| o.imported).unwrap_or(0),
            duplicated: outcome.as_ref().map(|o| o.duplicated).unwrap_or(0),
            failed: outcome.as_ref().map(|o| o.failed).unwrap_or(0),
            last_error: outcome.and_then(|o| o.last_error),
        })
    }

    /// 对一张单做增量拿号并入库（手动「导入交付」/ 轮询共用）
    pub async fn poll_delivery_for_order(&self, order_id: &str) -> Option<PurchaseOutcome> {
        let tracked = match self.store.tracked_order(order_id).ok().flatten() {
            Some(t) => t,
            None => {
                // 未跟踪的单（网页下的历史单）：以 seq=0 全量拉，但不重复入库
                // 已导入过的（import_one_credential 层面去重，Duplicate 不算失败）
                self.track_unknown_order(order_id).await;
                match self.store.tracked_order(order_id).ok().flatten() {
                    Some(t) => t,
                    None => return None,
                }
            }
        };
        let client = self.client();
        let page = match client.delivery(order_id, tracked.last_seq, 200).await {
            Ok(p) => p,
            Err(e) => {
                let _ = self.store.set_order_error(order_id, Some(&e.to_string()));
                tracing::warn!(order = order_id, "KiroShop 增量拿号失败: {e}");
                return None;
            }
        };
        let contents: Vec<String> = page
            .data
            .iter()
            .map(|d| d.content.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();
        let new_max_seq = page
            .next_after_seq()
            .or(page.data.iter().map(|d| d.seq).max())
            .unwrap_or(tracked.last_seq);
        let delivered = page
            .meta
            .as_ref()
            .and_then(|m| m.get("delivered"))
            .and_then(Value::as_i64)
            .map(|v| v.clamp(0, u32::MAX as i64) as u32)
            .unwrap_or(tracked.delivered);

        if !contents.is_empty() {
            // 与下面的「增量拿号完成」成对出现；只有这条没有那条，说明入库中途被打断
            tracing::info!(
                order = order_id,
                after_seq = tracked.last_seq,
                fetched = contents.len(),
                "KiroShop 增量拿号开始入库"
            );
        }
        let outcome = if contents.is_empty() {
            None
        } else {
            Some(self.import_contents(order_id, &contents).await)
        };
        let (imported, duplicated, failed, last_error) = match &outcome {
            Some(o) => (o.imported, o.duplicated, o.failed, o.last_error.clone()),
            None => (0, 0, 0, None),
        };
        // 每一轮拉号都留痕：成功路径原先完全没日志，订单计数对不上时无从追查
        // （2608824e 发货 5、凭据 5、入库计数 4，就是因为查不到是哪一轮少计的）。
        tracing::info!(
            order = order_id,
            after_seq = tracked.last_seq,
            new_max_seq,
            fetched = contents.len(),
            imported,
            duplicated,
            failed,
            delivered,
            "KiroShop 增量拿号完成"
        );
        // 游标由 last_seq 承担；imported/failed 只是面板展示计数
        let _ = self.store.advance_delivery(order_id, new_max_seq, delivered, imported, failed);
        if let Some(err) = &last_error {
            let _ = self.store.set_order_error(order_id, Some(err));
            let _ = self.store.add_local_event(
                "import_failed",
                "error",
                &format!("订单 {order_id} 有 {} 条凭据入库失败：{err}", contents.len()),
                Some(order_id),
            );
        }
        outcome
    }

    /// 把交付内容入库（复用 vendor 的解析：ksk_ / Social / IdC JSON 全兼容）
    async fn import_contents(&self, order_id: &str, contents: &[String]) -> PurchaseOutcome {
        let cfg = self.config();
        let region = cfg.api_region.trim();
        let keys: Vec<PurchasedKey> = contents
            .iter()
            .map(|c| PurchasedKey {
                key: c.clone(),
                account: None,
                password: None,
                issuer_url: None,
                price: None,
                region: if region.is_empty() {
                    None
                } else {
                    Some(region.to_string())
                },
            })
            .collect();
        import_keys(
            &self.admin,
            keys,
            &format!("kiroshop:{order_id}"),
            cfg.groups.clone(),
            cfg.rpm_limit.max(1),
            if region.is_empty() {
                None
            } else {
                Some(region.to_string())
            },
            cfg.priority,
            cfg.credit_limit,
        )
        .await
    }

    /// 领取补号（领取即代表原订单质保结束）并入库
    pub async fn pickup_supplement(&self, order_id: &str, supplement_id: i64) -> Result<BuyOutcome> {
        if !self.ready() {
            return Self::not_ready();
        }
        let client = self.client();
        let resp = client.pickup_supplement(order_id, supplement_id).await?;
        // 响应结构文档未给：递归收集所有 content 字符串字段入库
        let mut contents = Vec::new();
        collect_contents(&resp, &mut contents);
        let outcome = if contents.is_empty() {
            PurchaseOutcome {
                purchased: 0,
                imported: 0,
                duplicated: 0,
                failed: 1,
                last_error: Some("补号响应里没有识别出凭据内容，请到订单详情查看".into()),
            }
        } else {
            self.import_contents(order_id, &contents).await
        };
        let _ = self.store.add_local_event(
            "supplement_picked",
            "info",
            &format!(
                "订单 {order_id} 补号 #{supplement_id} 已领取：入库 {}、重复 {}、失败 {}",
                outcome.imported, outcome.duplicated, outcome.failed
            ),
            Some(order_id),
        );
        Ok(BuyOutcome {
            order_id: order_id.to_string(),
            imported: outcome.imported,
            duplicated: outcome.duplicated,
            failed: outcome.failed,
            last_error: outcome.last_error,
            ..Default::default()
        })
    }

    // ============ 跟踪 ============

    fn upsert_tracked(&self, order: &Order, source: &str) {
        if order.product_id == 0 {
            return; // 余额充值单不是取号订单
        }
        let tracked = self
            .store
            .tracked_order(&order.id)
            .ok()
            .flatten();
        let t = TrackedOrder {
            order_id: order.id.clone(),
            product_id: order.product_id,
            product_name: order.product_name.clone(),
            quantity: order.quantity_count(),
            amount: order.amount,
            status: order.status.clone().unwrap_or_default(),
            status_text: order.status_text.clone().unwrap_or_default(),
            eta: None,
            lane: order.lane.clone().unwrap_or_default(),
            source: tracked
                .as_ref()
                .map(|t| t.source.clone())
                .unwrap_or_else(|| source.to_string()),
            client_ref: order.client_ref.clone(),
            created_at: order.created_at.clone(),
            ..tracked.unwrap_or_default()
        };
        if let Err(e) = self.store.upsert_order(&t) {
            tracing::warn!(order = %order.id, "KiroShop 落跟踪失败: {e}");
        }
    }

    async fn track_unknown_order(&self, order_id: &str) {
        let detail = match self.client().order(order_id).await {
            Ok(d) => d,
            Err(_) => return,
        };
        self.upsert_tracked(&detail.order, "manual");
    }

    fn refresh_tracked(&self, order: &Order) {
        self.upsert_tracked(order, "manual");
    }

    fn track_from_order(&self, order: &Order, source: &str) {
        self.upsert_tracked(order, source);
    }

    /// 启动播种：把卖家侧未终态的存量单（含别处下的）纳管为在途
    pub async fn seed_tracked_orders(&self) {
        if !self.ready() {
            return;
        }
        let Ok(orders) = self.client().all_orders().await else {
            tracing::warn!("KiroShop：播种在途单失败（拉订单列表出错）");
            return;
        };
        let known: std::collections::HashSet<String> = self
            .store
            .tracked_orders()
            .map(|ts| ts.into_iter().map(|t| t.order_id).collect())
            .unwrap_or_default();
        let mut seeded = 0;
        for o in &orders {
            if o.product_id == 0 || o.is_terminal() || known.contains(&o.id) {
                continue;
            }
            self.upsert_tracked(o, "migrated");
            seeded += 1;
        }
        if seeded > 0 {
            tracing::info!("KiroShop：播种 {seeded} 张存量在途单");
        }
    }

    // ============ 事件 ============

    /// 处理一条卖家事件（webhook 与轮询共用）。返回是否新事件。
    pub async fn handle_shop_event(&self, ev: &ShopEvent) -> Result<bool> {
        let order_id = ev.order.as_ref().map(|o| o.id.clone());
        // 先落库占坑（按事件 id 去重），失败要撤掉让重试/轮询再次处理
        let inserted = self
            .store
            .record_shop_event(ev.id, &ev.r#type, order_id.as_deref(), ev.created_at.as_deref())
            .unwrap_or(false);
        if !inserted {
            return Ok(false);
        }
        let result: Result<()> = match ev.r#type.as_str() {
            "order.created" | "order.paid" => {
                if let Some(o) = &ev.order {
                    self.upsert_tracked(o, "manual");
                }
                Ok(())
            }
            "order.delivery" | "order.completed" => {
                if let Some(o) = &ev.order {
                    self.upsert_tracked(o, "manual");
                }
                if let Some(oid) = &order_id {
                    self.poll_delivery_for_order(oid).await;
                }
                Ok(())
            }
            "order.refunded" => {
                if let Some(o) = &ev.order {
                    self.upsert_tracked(o, "manual");
                }
                let _ = self.store.add_local_event(
                    "order_refunded",
                    "warn",
                    &format!("订单 {}{} 已退款", order_id.clone().unwrap_or_default(),
                        ev.order.as_ref().and_then(|o| o.refund.as_ref())
                            .and_then(|r| r.get("partial"))
                            .and_then(Value::as_bool)
                            .filter(|p| *p)
                            .map(|_| "（部分退款，已发的号照常可用）").unwrap_or("")),
                    order_id.as_deref(),
                );
                Ok(())
            }
            "order.closed" | "order.timeout" => {
                if let Some(o) = &ev.order {
                    self.upsert_tracked(o, "manual");
                }
                let _ = self.store.add_local_event(
                    "order_closed",
                    "warn",
                    &format!("订单 {} 已关闭/超时（未付款自动关单）", order_id.clone().unwrap_or_default()),
                    order_id.as_deref(),
                );
                Ok(())
            }
            "order.supplement" => {
                let _ = self.store.add_local_event(
                    "supplement",
                    "info",
                    &format!(
                        "订单 {} 有管理员补发的号可领（领取即结束原单质保），到面板订单详情领取",
                        order_id.clone().unwrap_or_default()
                    ),
                    order_id.as_deref(),
                );
                Ok(())
            }
            "after_sale.replied" => {
                let _ = self.store.add_local_event(
                    "after_sale_replied",
                    "info",
                    &format!("订单 {} 的售后有客服回复，到面板查看", order_id.clone().unwrap_or_default()),
                    order_id.as_deref(),
                );
                Ok(())
            }
            "ping" => {
                let _ = self.store.add_local_event("ping", "info", "KiroShop webhook 测试 ping", None);
                Ok(())
            }
            _ => Ok(()),
        };
        if result.is_err() {
            // 撤掉占坑行，卖家重试或轮询会再次触达
            let _ = self.store.remove_shop_event(ev.id);
            return result.map(|_| true);
        }
        Ok(true)
    }

    /// 事件轮询一轮：追新 + 翻页走满。首次运行只播种游标不回放历史。
    pub async fn poll_events_once(&self) {
        if !self.ready() {
            return;
        }
        let client = self.client();
        let seeded_key = "events_seeded";
        let seeded = self
            .store
            .kv_get(seeded_key)
            .ok()
            .flatten()
            .is_some();
        let mut after = self.store.max_shop_event_id().unwrap_or(0);

        let mut applied = 0usize;
        for _ in 0..10 {
            let Ok((events, next, has_more)) = client.events(after, 100).await else {
                tracing::warn!("KiroShop：事件轮询失败");
                return;
            };
            for ev in &events {
                if !seeded {
                    // 首轮：只占坑（历史事件不回放导入，避免风暴），不处理
                    let _ = self.store.record_shop_event(
                        ev.id, &ev.r#type,
                        ev.order.as_ref().map(|o| o.id.as_str()),
                        ev.created_at.as_deref(),
                    );
                    continue;
                }
                match self.handle_shop_event(ev).await {
                    Ok(true) => applied += 1,
                    Ok(false) => {}
                    Err(e) => tracing::warn!(event = ev.id, "KiroShop 处理事件失败: {e}"),
                }
            }
            after = next.unwrap_or(after);
            if !has_more {
                break;
            }
        }
        if !seeded {
            let _ = self.store.kv_set(seeded_key, &Utc::now().timestamp().to_string());
            tracing::info!("KiroShop：事件游标已播种到 {after}（历史事件不回放）");
        } else if applied > 0 {
            tracing::debug!("KiroShop：本轮处理 {applied} 条新事件");
        }
    }

    /// 发货轮询一轮：刷在途单状态 + 增量收货 + 自动补货判据
    pub async fn poll_deliveries_once(&self) {
        if !self.ready() {
            return;
        }
        let tracked = match self.store.tracked_orders() {
            Ok(t) => t,
            Err(_) => return,
        };
        let active: Vec<&TrackedOrder> = tracked
            .iter()
            .filter(|t| !is_terminal_status(&t.status) && t.product_id != 0)
            .collect();
        if active.is_empty() {
            return;
        }
        let client = self.client();
        for t in &active {
            // 刷一遍订单状态（eta/状态文案常变），失败不阻塞收货
            if let Ok(detail) = client.order(&t.order_id).await {
                let _ = self.store.refresh_order(
                    &t.order_id,
                    detail.order.status.as_deref().unwrap_or(""),
                    detail.order.status_text.as_deref().unwrap_or(""),
                    detail.order.delivered_count(),
                    detail.eta.as_deref(),
                );
                if let Some(refund) = detail.order.refund.as_ref().filter(|r| !r.is_null()) {
                    let _ = self.store.add_local_event(
                        "order_refunded",
                        "warn",
                        &format!("订单 {} 出现退款标记：{refund}", t.order_id),
                        Some(&t.order_id),
                    );
                }
            }
            self.poll_delivery_for_order(&t.order_id).await;
        }
    }

    /// 账户刷新 + 余额告警状态机（带回滞，避免在阈值附近抖动）
    pub async fn refresh_account_once(&self) {
        if !self.ready() {
            return;
        }
        match self.client().account().await {
            Ok(acc) => {
                *self.last_account.write() = Some(acc.clone());
                self.set_last_error(None);
                if let (Some(threshold), Some(balance)) =
                    (self.config().balance_alert_threshold, acc.balance)
                {
                    let state = self.store.kv_get("balance_alert").ok().flatten();
                    let low = state.as_deref() != Some("low");
                    let high = balance >= threshold + 5.0;
                    if low && balance < threshold {
                        let _ = self.store.kv_set("balance_alert", "low");
                        let _ = self.store.add_local_event(
                            "balance_low",
                            "warn",
                            &format!("KiroShop 余额 ¥{balance:.2} 已低于告警线 ¥{threshold:.2}，请充值"),
                            None,
                        );
                    } else if !low && high {
                        let _ = self.store.kv_set("balance_alert", "ok");
                        let _ = self.store.add_local_event(
                            "balance_recovered",
                            "info",
                            &format!("KiroShop 余额回到 ¥{balance:.2}"),
                            None,
                        );
                    }
                }
            }
            Err(e) => {
                self.set_last_error(Some(format!("账户刷新失败: {e}")));
            }
        }
    }

    // ============ 自动补货 ============

    /// 池内可用的 KiroShop 号（来源 kiroshop:、未禁用、未过期）
    pub fn pool_count(&self) -> u32 {
        let all = self.admin.get_all_credentials();
        let now = Utc::now();
        all.credentials
            .iter()
            .filter(|c| {
                c.source_channel
                    .as_deref()
                    .is_some_and(|s| s.starts_with("kiroshop"))
                    && !c.disabled
                    && c.expires_at
                        .as_deref()
                        .and_then(|e| chrono::DateTime::parse_from_rfc3339(e).ok())
                        .map(|exp| exp.with_timezone(&Utc) > now)
                        .unwrap_or(true)
            })
            .count() as u32
    }

    /// 在途数量：未终态跟踪单的剩余待交付数
    pub fn inflight_count(&self) -> u32 {
        self.store
            .tracked_orders()
            .map(|ts| {
                ts.iter()
                    .filter(|t| !is_terminal_status(&t.status) && t.product_id != 0)
                    .map(|t| t.quantity.saturating_sub(t.delivered))
                    .sum()
            })
            .unwrap_or(0)
    }

    /// 自动补货一轮（由发货轮询搭车调用）
    pub async fn autobuy_tick(&self) {
        let cfg = self.config();
        let ab = &cfg.auto_buy;
        if !cfg.is_ready() || !ab.enabled || ab.pool_target == 0 {
            return;
        }
        if Utc::now().timestamp() < *self.autobuy_cooldown_until.read() {
            return;
        }
        let pool = self.pool_count();
        let inflight = self.inflight_count();
        if pool + inflight >= ab.pool_target {
            return;
        }
        let mut needed = ab.pool_target - pool - inflight;
        needed = needed.min(ab.max_per_order.max(1));

        let Ok(products) = self.client().products().await else {
            return;
        };
        // 候选：有足够现货、单价在上限内；取最便宜
        let candidates: Vec<&Product> = products
            .iter()
            .filter(|p| p.available() >= needed.min(1))
            .filter(|p| p.available() >= needed || ab.allow_backorder)
            .filter(|p| {
                p.default_option()
                    .and_then(|o| o.price)
                    .or(p.price)
                    .is_some_and(|price| ab.max_unit_price.is_none_or(|m| price <= m))
            })
            .collect();
        let Some(product) = candidates
            .iter()
            .min_by(|a, b| {
                let pa = a.default_option().and_then(|o| o.price).or(a.price);
                let pb = b.default_option().and_then(|o| o.price).or(b.price);
                pa.partial_cmp(&pb).unwrap_or(std::cmp::Ordering::Equal)
            })
        else {
            return;
        };
        let unit = product.default_option().and_then(|o| o.price).or(product.price);
        let Some(unit) = unit else { return };
        let quantity = needed.min(product.available().max(1)).max(1);

        // 每日预算：今日已扣 + 本单金额 不得超
        if let Some(budget) = ab.daily_budget {
            let spent = self
                .last_account()
                .and_then(|a| a.api_spent_today)
                .unwrap_or(0.0);
            if spent + unit * quantity as f64 > budget {
                let _ = self.store.add_local_event(
                    "autobuy_budget",
                    "warn",
                    &format!(
                        "自动补货停在预算线：今日已扣 ¥{spent:.2} + 本单 ¥{:.2} > 每日 ¥{budget:.2}",
                        unit * quantity as f64
                    ),
                    None,
                );
                *self.autobuy_cooldown_until.write() = Utc::now().timestamp() + 3600;
                return;
            }
        }

        tracing::info!(
            product = product.id,
            quantity,
            pool,
            inflight,
            "KiroShop 自动补货触发"
        );
        let outcome = self
            .buy(BuyParams {
                product_id: product.id,
                quantity,
                option_key: product.default_option().map(|o| o.key.clone()),
                coupon_code: None,
                max_amount: None,
                allow_backorder: ab.allow_backorder,
                client_ref: Some(format!("autobuy-{}", Utc::now().timestamp())),
                idempotency_key: None,
                source: "auto".into(),
            })
            .await;
        match outcome {
            Ok(o) => {
                let _ = self.store.add_local_event(
                    "autobuy",
                    "info",
                    &format!(
                        "自动补货：商品「{}」×{}，金额 ¥{:.2}，入库 {} 条",
                        product.display_name,
                        o.quantity,
                        o.amount.unwrap_or(0.0),
                        o.imported
                    ),
                    Some(&o.order_id),
                );
            }
            Err(e) => {
                // 失败冷却 10 分钟，防止每分钟空转打限速
                *self.autobuy_cooldown_until.write() = Utc::now().timestamp() + 600;
                let _ = self.store.add_local_event(
                    "autobuy_failed",
                    "error",
                    &format!("自动补货失败（冷却 10 分钟）：{e}"),
                    None,
                );
            }
        }
    }

    // ============ webhook 验签 ============

    /// 校验 `X-KiroShop-Signature: t=<unix 秒>,v1=<hex(HMAC-SHA256(secret, "<t>.<body>"))>`
    /// 时间窗 5 分钟；密钥未配置时只校验路径 token。
    pub fn verify_webhook_signature(&self, signature: &str, body: &[u8]) -> bool {
        let secret = self.config().webhook_secret;
        if secret.is_empty() {
            // 未配置密钥：只靠路径 token（提醒一次）
            tracing::debug!("KiroShop webhook 未配置签名密钥，仅路径 token 校验");
            return true;
        }
        let Some((t, v1)) = signature.split_once(',') else {
            return false;
        };
        let t = t.trim().strip_prefix("t=").unwrap_or("").to_string();
        let v1 = v1.trim().strip_prefix("v1=").unwrap_or("").to_string();
        let Ok(ts) = t.parse::<i64>() else {
            return false;
        };
        if (Utc::now().timestamp() - ts).abs() > 300 {
            return false;
        }
        let mut msg = t.into_bytes();
        msg.push(b'.');
        msg.extend_from_slice(body);
        let mac = hmac_sha256(secret.as_bytes(), &msg);
        let expected = hex::encode(mac);
        // 常量时间比较，防时序侧信道
        if expected.len() != v1.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in expected.bytes().zip(v1.bytes()) {
            diff |= a ^ b;
        }
        diff == 0
    }

    /// 入站 webhook 路径 token 校验（常量时间）
    pub fn webhook_token_matches(&self, token: &str) -> bool {
        use subtle::ConstantTimeEq;
        let expect = self.config().webhook_path_token;
        if expect.is_empty() {
            return false;
        }
        token.as_bytes().ct_eq(expect.as_bytes()).into()
    }

    /// 后台主循环
    pub async fn run(self: Arc<Self>) {
        // 启动播种：把卖家侧存量在途单纳管
        self.seed_tracked_orders().await;
        self.refresh_account_once().await;

        let (mut ev_tick, mut dl_tick, mut acc_tick) = (0u64, 0u64, 0u64);
        loop {
            let cfg = self.config();
            let ev_interval = cfg.events_poll_interval_secs.clamp(10, 3600);
            let dl_interval = cfg.delivery_poll_interval_secs.clamp(15, 3600);
            let enabled = cfg.enabled && !cfg.api_key.trim().is_empty();
            drop(cfg);

            if !enabled {
                // 未启用：低频待机，等配置改回来
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            ev_tick += 1;
            dl_tick += 1;
            if ev_tick >= ev_interval {
                ev_tick = 0;
                self.poll_events_once().await;
                // 账户刷新搭事件轮询的车：每 6 个事件周期（默认约 4.5 分钟）一轮
                acc_tick += 1;
                if acc_tick >= 6 {
                    acc_tick = 0;
                    self.refresh_account_once().await;
                }
            }
            if dl_tick >= dl_interval {
                dl_tick = 0;
                self.poll_deliveries_once().await;
                self.autobuy_tick().await;
            }
        }
    }
}

/// 终态判断（与 client::Order::is_terminal 同一套口径，供 store 行使用）
pub fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "picked_up" | "completed" | "supplemented" | "timeout" | "failed" | "refunded"
    )
}

/// 递归收集 JSON 里所有非空 `content` 字符串（补号响应结构未知，宽匹配）
fn collect_contents(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k == "content" {
                    if let Some(s) = val.as_str() {
                        let s = s.trim();
                        if !s.is_empty() {
                            out.push(s.to_string());
                        }
                    }
                } else {
                    collect_contents(val, out);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_contents(item, out);
            }
        }
        _ => {}
    }
}

/// HMAC-SHA256（标准 ipad/opad 构造）。项目已依赖 sha2，不再引 hmac crate。
fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let block = 64usize;
    let key = if key.len() > block {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    let mut ipad = vec![0x36u8; block];
    let mut opad = vec![0x5cu8; block];
    for (i, b) in key.iter().enumerate() {
        ipad[i] ^= b;
        opad[i] ^= b;
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner);
    outer.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiroshop::config::KiroshopConfig;

    fn service() -> Arc<KiroshopService> {
        // 走真实的 AdminService 构造（空凭据），池计数等路径不依赖外部状态
        let tm = std::sync::Arc::new(
            crate::kiro::token_manager::MultiTokenManager::new(
                crate::model::config::Config::default(),
                vec![],
                None,
                None,
                false,
            )
            .unwrap(),
        );
        let admin = std::sync::Arc::new(crate::admin::AdminService::new(tm, Vec::<String>::new()));
        let cfg: KiroshopConfig = serde_json::from_str(r#"{"apiKey":"ks_live_t"}"#).unwrap();
        KiroshopService::new(
            reqwest::Client::new(),
            cfg,
            KiroshopStore::open_in_memory().unwrap(),
            admin,
        )
    }

    /// 入库代理选取：空池返回 None（保持回落全局代理的原行为），
    /// 有池时只会选到池内可用的代理，且多次抽样能覆盖多个出口（不是固定取第一个）。
    #[tokio::test]
    async fn 入库随机代理_空池为none_有池只取池内可用() {
        let svc = service();
        assert_eq!(svc.admin.pick_random_proxy_url(), None, "空池应回落全局代理");

        let urls: Vec<String> = (1..=5)
            .map(|i| format!("socks5://u:p@10.0.0.{i}:1080"))
            .collect();
        let (added, errors) = svc
            .admin
            .batch_add_proxies(crate::admin::types::BatchAddProxyRequest { urls: urls.clone() });
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(added.len(), 5);

        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let picked = svc.admin.pick_random_proxy_url().expect("池非空应有结果");
            assert!(urls.contains(&picked), "必须取自代理池: {picked}");
            seen.insert(picked);
        }
        assert!(seen.len() > 1, "200 次抽样只命中一个出口，说明不是随机");
    }

    #[test]
    fn hmac与rfc4231向量一致() {
        // RFC 4231 Test Case 2: key="Jefe", data="what do ya want for nothing?"
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[tokio::test]
    async fn webhook签名校验_时间窗与篡改() {
        let s = service();
        let body = br#"{"id":1,"type":"ping"}"#;
        let t = Utc::now().timestamp();
        let msg = format!("{t}.{}", String::from_utf8_lossy(body));
        let sig = format!("t={t},v1={}", hex::encode(hmac_sha256(b"secret", msg.as_bytes())));

        // 未配置密钥时放行（仅靠路径 token）
        assert!(s.verify_webhook_signature(&sig, body));

        // 配置密钥后：正确签名过、错密钥不过、过期时间戳不过
        s.config.write().webhook_secret = "secret".into();
        assert!(s.verify_webhook_signature(&sig, body));
        let bad = format!("t={t},v1={}", hex::encode(hmac_sha256(b"other", msg.as_bytes())));
        assert!(!s.verify_webhook_signature(&bad, body));
        let old = t - 400;
        let old_msg = format!("{old}.{}", String::from_utf8_lossy(body));
        let old_sig = format!("t={old},v1={}", hex::encode(hmac_sha256(b"secret", old_msg.as_bytes())));
        assert!(!s.verify_webhook_signature(&old_sig, body));
        // 篡改 body
        assert!(!s.verify_webhook_signature(&sig, b"{}"));
    }

    #[tokio::test]
    async fn webhook路径token常量时间比对() {
        let s = service();
        let expect = s.config.read().webhook_path_token.clone();
        assert!(s.webhook_token_matches(&expect));
        assert!(!s.webhook_token_matches("wrong"));
    }

    #[test]
    fn 补号响应递归收集content() {
        let v: Value = serde_json::from_str(
            r#"{"data":{"items":[{"content":" {\"a\":1} "},{"content":""}],"note":{"content":"x"}}}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        collect_contents(&v, &mut out);
        assert_eq!(out, vec![r#"{"a":1}"#.to_string(), "x".to_string()]);
    }

    #[tokio::test]
    async fn 事件按id去重且失败撤坑重放() {
        let s = service();
        let ev = ShopEvent {
            id: 5,
            r#type: "ping".into(),
            ..Default::default()
        };
        assert!(s.handle_shop_event(&ev).await.unwrap());
        assert!(!s.handle_shop_event(&ev).await.unwrap(), "重复事件应跳过");
        assert_eq!(s.store.unacked_local_count().unwrap(), 1, "只留一条 ping 告警");
        // 撤坑后可重放（模拟处理失败后的重试路径）
        s.store.remove_shop_event(5).unwrap();
        assert!(s.handle_shop_event(&ev).await.unwrap());
    }

    #[test]
    fn 终态口径一致() {
        for st in ["picked_up", "completed", "supplemented", "timeout", "failed", "refunded"] {
            assert!(is_terminal_status(st));
        }
        for st in ["pending", "paid", "out_of_stock", ""] {
            assert!(!is_terminal_status(st));
        }
    }

    #[tokio::test]
    async fn 在途计数只算未终态剩余量() {
        let s = service();
        for (id, qty, delivered, status) in [
            ("a", 3u32, 1u32, "paid"),
            ("b", 2, 2, "picked_up"),
            ("c", 5, 0, "out_of_stock"),
        ] {
            s.store
                .upsert_order(&TrackedOrder {
                    order_id: id.into(),
                    product_id: 403,
                    quantity: qty,
                    delivered,
                    status: status.into(),
                    ..Default::default()
                })
                .unwrap();
            s.store.advance_delivery(id, 0, delivered, 0, 0).unwrap();
        }
        assert_eq!(s.inflight_count(), (3 - 1) + (5 - 0));
    }
}

