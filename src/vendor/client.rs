//! 卖家（Key 供应商）出站 API 客户端
//!
//! 按 [`VendorFlavor`] 分发到各家的路径与鉴权形态，对上统一返回
//! [`super::protocol`] 里的中立结构 —— service 层不需要知道对接的是哪一家。
//!
//! 各家 DTO 与字段映射见 [`super::flavor_legacy`] / [`super::flavor_kiroapp`]。
//! 不支持的能力直接返回 [`VendorApiError::unsupported`]，不发请求。
//!
//! @author wangzhong

use anyhow::Context;
use serde::Deserialize;

use crate::http_client::{self, ProxyConfig};
use crate::model::config::{TlsBackend, VendorConfig};

use super::flavor_drop as drop_flavor;
use super::flavor_kiroapp as kiroapp;
use super::flavor_kiroapp_cc as kiroapp_cc;
use super::flavor_legacy as legacy;
use super::protocol::{
    EarliestKeyInfo, LedgerEntry, OrderInfo, Paged, ProfileInfo, PurchaseResult, RedeemResult,
    StockInfo, VendorApiError, VendorCapabilities, VendorFlavor, VendorKeyInfo, truncate,
};

/// 出站请求超时（秒）。提取 Key 需要卖家侧现场生成，给足时间。
const REQUEST_TIMEOUT_SECS: u64 = 120;

/// 卖家返回的错误体：`{"error":"错误说明"}`
#[derive(Debug, Deserialize)]
struct VendorError {
    #[serde(default)]
    error: Option<String>,
}

/// 卖家 API 客户端。复用全局代理与 TLS 后端配置。
pub struct VendorClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    flavor: VendorFlavor,
}

impl VendorClient {
    /// 按配置构建客户端。`base_url` / `api_key` 为空时返回 Err。
    pub fn new(
        vendor: &VendorConfig,
        proxy: Option<&ProxyConfig>,
        tls_backend: TlsBackend,
    ) -> anyhow::Result<Self> {
        if !vendor.outbound_enabled() {
            anyhow::bail!("卖家配置不完整（baseUrl / apiKey 为空）");
        }
        let http = http_client::build_client(proxy, REQUEST_TIMEOUT_SECS, tls_backend)
            .context("构建卖家 API 客户端失败")?;
        Ok(Self {
            http,
            base_url: vendor.normalized_base_url().to_string(),
            api_key: vendor.api_key.trim().to_string(),
            flavor: vendor.flavor,
        })
    }

    pub fn capabilities(&self) -> VendorCapabilities {
        self.flavor.capabilities()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// 按 flavor 附加鉴权头。两家形态不同，集中在此一处。
    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.flavor {
            // Drop 与首家同用 X-API-Key（都是 usr-xxx 形态的 Key）
            VendorFlavor::Legacy | VendorFlavor::Drop => req.header("X-API-Key", &self.api_key),
            VendorFlavor::Kiroapp | VendorFlavor::KiroappCc => req.bearer_auth(&self.api_key),
        }
    }

    /// 统一处理响应：非 2xx 时解析错误体并带上状态码。
    ///
    /// 错误体形状按 flavor 分：多数家是扁平的 `{"error":"文本"}`，Drop 是嵌套的
    /// `{"error":{"code":..,"message":..,"request_id":..}}`。后者若按扁平解会整段
    /// JSON 连 request_id 一起塞进面板。
    async fn parse<T: for<'de> Deserialize<'de>>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, VendorApiError> {
        let status = resp.status();
        let body = resp.text().await.map_err(|e| VendorApiError {
            status: Some(status.as_u16()),
            message: format!("读取响应体失败: {e}"),
        })?;

        if !status.is_success() {
            let nested = match self.flavor {
                VendorFlavor::Drop => drop_flavor::error_message(&body),
                _ => None,
            };
            let message = nested
                .or_else(|| {
                    serde_json::from_str::<VendorError>(&body)
                        .ok()
                        .and_then(|e| e.error)
                })
                .unwrap_or_else(|| truncate(&body, 300));
            return Err(VendorApiError {
                status: Some(status.as_u16()),
                message,
            });
        }

        serde_json::from_str::<T>(&body).map_err(|e| VendorApiError {
            status: Some(status.as_u16()),
            message: format!("解析响应失败: {e}；原文片段: {}", truncate(&body, 200)),
        })
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, VendorApiError> {
        let resp = self
            .auth(self.http.get(self.url(path)))
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;
        self.parse(resp).await
    }

    /// 带查询参数的 GET（分页接口用）
    async fn get_with<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, VendorApiError> {
        let resp = self
            .auth(self.http.get(self.url(path)).query(query))
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;
        self.parse(resp).await
    }

    async fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, VendorApiError> {
        let resp = self
            .auth(self.http.post(self.url(path)).json(body))
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;
        self.parse(resp).await
    }

    /// kiroapp.cc 的 `POST /openapi/claim`，**成功响应宽松解析**。
    ///
    /// 与其余接口刻意不同：2xx 时先按文档形态严格解析（`{"key":..}` /
    /// `{"keys":[..]}`），失败则降级到 [`kiroapp_cc::extract_keys`] 按 `ksk_` 前缀扫，
    /// 连裸文本响应也能捞出来。原因是这个接口**无幂等键**，一旦返回 2xx 钱就已经
    /// 扣了 —— 若因为响应结构不认识就报错，等于把付过费的 Key 直接扔掉。
    ///
    /// 非 2xx 仍走严格路径，并按 kiroapp.cc 的嵌套错误体 `{"error":{"message":..}}`
    /// 取信息（[`Self::parse`] 里的 `VendorError` 只认扁平的 `{"error":"文本"}`）。
    async fn claim_kiroapp_cc(
        &self,
        body: &serde_json::Value,
        count: u32,
    ) -> Result<kiroapp_cc::ClaimResult, VendorApiError> {
        let resp = self
            .auth(self.http.post(self.url(kiroapp_cc::PATH_CLAIM)).json(body))
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| VendorApiError {
            status: Some(status.as_u16()),
            message: format!("读取响应体失败: {e}"),
        })?;

        if !status.is_success() {
            return Err(VendorApiError {
                status: Some(status.as_u16()),
                message: kiroapp_cc::error_message(&text)
                    .unwrap_or_else(|| truncate(&text, 300)),
            });
        }

        // 先按文档形态严格解析
        if count == 1 {
            let single = serde_json::from_str::<kiroapp_cc::ClaimSingleResponse>(&text)
                .ok()
                .map(|r| r.key.trim().to_string())
                .filter(|k| !k.is_empty());
            if let Some(key) = single {
                return Ok(kiroapp_cc::ClaimResult {
                    keys: vec![key],
                    points_cost: None,
                });
            }
        } else {
            let batch = serde_json::from_str::<kiroapp_cc::ClaimBatchResponse>(&text)
                .ok()
                .filter(|r| !r.keys.is_empty());
            if let Some(r) = batch {
                return Ok(kiroapp_cc::ClaimResult {
                    keys: r.keys,
                    points_cost: r.points_cost,
                });
            }
        }

        // 降级：能当 JSON 就当 JSON，不能就整体视作一个字符串再按前缀扫。
        // 捞不到也返回 Ok(空) 而不是 Err —— 上层据此提示人工核对是否已扣费，
        // 报错会让人误以为没花钱。
        let value = serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|_| serde_json::Value::String(text.clone()));
        let keys = kiroapp_cc::extract_keys(&value);
        if keys.is_empty() {
            tracing::warn!(
                "kiroapp.cc claim 返回中未识别出 ksk_ Key，可能已扣费: {}",
                truncate(&text, 300)
            );
        } else {
            tracing::warn!(
                "kiroapp.cc claim 响应结构不符合文档，已按前缀降级捞出 {} 个 Key",
                keys.len()
            );
        }
        let points_cost = serde_json::from_str::<kiroapp_cc::ClaimBatchResponse>(&text)
            .ok()
            .and_then(|r| r.points_cost);
        Ok(kiroapp_cc::ClaimResult { keys, points_cost })
    }

    /// Drop 的下单：`POST /api/my/purchase`，**成功响应宽松解析**。
    ///
    /// 参数与首家一致（`count` + `client_order_id`），但错误与成功两侧都要特化：
    ///
    /// - **2xx 时严格解析失败要降级**，与 [`Self::claim_kiroapp_cc`] 同一道理：
    ///   拿到 2xx 就说明钱已经扣了，若因为响应结构不认识就报错，等于把付过费的
    ///   Key 直接扔掉。本家已知会把金额字符串化（这正是 `flavor_drop` 存在的
    ///   理由），`purchased` 或 `keys` 哪天跟着变形完全合理。DTO 本身已用
    ///   `Countish` / `KeyEntry` 吃下常见变形，这里再兜一层「结构完全不认识」
    ///   的情况：按 `ksk_` 前缀扫，连裸文本响应也能捞出来。
    /// - **非 2xx 时按状态码补语义**：本家 404 意为库存不足而非路径错，卖家返
    ///   空体时面板只剩裸状态码，方向会被带偏。
    async fn purchase_drop(
        &self,
        body: &serde_json::Value,
        client_order_id: &str,
    ) -> Result<PurchaseResult, VendorApiError> {
        let resp = self
            .auth(
                self.http
                    .post(self.url(drop_flavor::PATH_PURCHASE))
                    .json(body),
            )
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| VendorApiError {
            status: Some(status.as_u16()),
            message: format!("读取响应体失败: {e}"),
        })?;

        if !status.is_success() {
            let code = status.as_u16();
            // 卖家给了可读 message 就用它；给空体 / HTML 时才退到状态码语义
            let message = drop_flavor::error_message(&text)
                .or_else(|| {
                    let t = text.trim();
                    (!t.is_empty() && serde_json::from_str::<serde_json::Value>(t).is_ok())
                        .then(|| truncate(t, 300))
                })
                .or_else(|| drop_flavor::status_hint(code).map(str::to_string))
                .unwrap_or_else(|| truncate(&text, 300));
            return Err(VendorApiError {
                status: Some(code),
                message,
            });
        }

        // 先按文档形态解析（DTO 已容忍字符串化的 purchased 与裸字符串 keys）
        if let Ok(r) = serde_json::from_str::<drop_flavor::PurchaseResponse>(&text) {
            let result: PurchaseResult = r.into();
            if !result.keys.is_empty() {
                return Ok(result);
            }
            // 解析成功但一个 Key 都没有：可能是卖家真的出货 0 个（合法），
            // 也可能是 keys 字段改了名。降级扫一遍，捞到就用捞到的。
            let scanned = scan_keys(&text);
            if scanned.is_empty() {
                return Ok(result);
            }
            tracing::warn!(
                order_id = %client_order_id,
                found = scanned.len(),
                "Drop 下单响应的 keys 字段为空但正文里有 ksk_ Key，已按前缀降级捞出"
            );
            return Ok(PurchaseResult {
                purchased: result.purchased.max(scanned.len() as u32),
                keys: scanned,
                ..result
            });
        }

        // 结构完全不认识。钱已经扣了，捞不到也返回 Ok(空) 而不是 Err ——
        // 上层据此提示人工按订单号核对，报错会让人误以为没花钱。
        let scanned = scan_keys(&text);
        if scanned.is_empty() {
            tracing::warn!(
                order_id = %client_order_id,
                "Drop 下单返回 2xx 但结构无法识别且未捞到 ksk_ Key，可能已扣费: {}",
                truncate(&text, 300)
            );
        } else {
            tracing::warn!(
                order_id = %client_order_id,
                found = scanned.len(),
                "Drop 下单响应结构不符合文档，已按前缀降级捞出 Key"
            );
        }
        Ok(PurchaseResult {
            purchased: scanned.len() as u32,
            order_id: Some(client_order_id.to_string()),
            keys: scanned,
            ..Default::default()
        })
    }

    // ============ 提取（消费侧）============

    /// 下单提取 Key。
    ///
    /// `client_order_id` 必须是 32 位十六进制串，并且**同一订单号必须始终配同一个
    /// `count`**：两家卖家都对「相同订单号 + 相同 count」幂等重放，改 count 会返回
    /// 409。因此调用方需持久化首次决定的 count，重试时原样复用。
    ///
    /// `batch_order_id` 仅 `batch_scoped_purchase` 能力可用时有意义（kiroapp 的
    /// 开号批次 id），传入后只拉取该批次产出的 Key。
    ///
    /// `zone` 仅 `zoned_purchase` 能力可用时有意义。**幂等键与 zone 也要配死**：
    /// 同一订单号重试时必须传同一个 zone，换区等于换了笔单。
    pub async fn purchase(
        &self,
        count: u32,
        client_order_id: &str,
        batch_order_id: Option<&str>,
        zone: Option<&str>,
    ) -> Result<PurchaseResult, VendorApiError> {
        let mut body = purchase_body(count, client_order_id);
        match self.flavor {
            VendorFlavor::Legacy => {
                // 不带 zone 时卖家只从它自己的默认区（us）取货，且不跨区补 ——
                // 该区缺货就直接返回缺货。故有分区能力时必须显式指定。
                if let Some(z) = zone.filter(|s| !s.trim().is_empty()) {
                    body["zone"] = serde_json::json!(z.trim());
                }
                let r: legacy::PurchaseResponse =
                    self.post_json(legacy::PATH_PURCHASE, &body).await?;
                Ok(r.into())
            }
            VendorFlavor::Kiroapp => {
                // 只有该 flavor 支持按批次定向拉取
                if let Some(batch) = batch_order_id.filter(|s| !s.trim().is_empty()) {
                    body["order_id"] = serde_json::json!(batch.trim());
                }
                // 区域字段名是 region（不是 zone）。文档：us / eu，也接受 us-east-1 / eu-central-1
                if let Some(z) = zone.filter(|s| !s.trim().is_empty()) {
                    body["region"] = serde_json::json!(z.trim());
                }
                let mut r: kiroapp::PurchaseResponse =
                    self.post_json(kiroapp::PATH_PURCHASE, &body).await?;
                // 响应不回显 region，手动补上以便前端展示实际成交区域
                r.region = zone.map(|s| s.to_string());
                Ok(r.into())
            }
            VendorFlavor::KiroappCc => {
                // kiroapp.cc: count=1 时无参数（单次提取），count>1 时传 {"count": N}
                let body = if count == 1 {
                    serde_json::json!({})
                } else {
                    serde_json::json!({ "count": count })
                };
                let result = self.claim_kiroapp_cc(&body, count).await?;
                Ok(result.into_purchase_result(client_order_id.to_string(), count))
            }
            VendorFlavor::Drop => self.purchase_drop(&body, client_order_id).await,
        }
    }

    /// 库存与报价
    pub async fn stock(&self) -> Result<StockInfo, VendorApiError> {
        match self.flavor {
            VendorFlavor::Legacy => {
                let r: legacy::StockResponse = self.get(legacy::PATH_STOCK).await?;
                Ok(r.into())
            }
            VendorFlavor::Kiroapp => {
                let r: kiroapp::StockResponse = self.get(kiroapp::PATH_STOCK).await?;
                Ok(r.into())
            }
            VendorFlavor::KiroappCc => {
                let r: kiroapp_cc::StockResponse = self.get(kiroapp_cc::PATH_STOCK).await?;
                Ok(r.into())
            }
            VendorFlavor::Drop => {
                // /api/me/stock 一次给出库存 + 单价 + 余额，优先走它。
                // 该端点曾经不存在（旧版文档时期 404），故失败时退回 /api/status 的
                // keys_stock —— 那里只有数量，但至少库存卡片不会空着。
                match self
                    .get::<drop_flavor::StockResponse>(drop_flavor::PATH_STOCK)
                    .await
                {
                    Ok(r) => Ok(r.into()),
                    Err(e) => {
                        tracing::warn!(
                            "Drop {} 不可用（{}），退回 {} 取库存（无单价与余额）",
                            drop_flavor::PATH_STOCK,
                            e,
                            drop_flavor::PATH_STATUS
                        );
                        let r: drop_flavor::StatusResponse =
                            self.get(drop_flavor::PATH_STATUS).await?;
                        Ok(r.into())
                    }
                }
            }
        }
    }

    /// 账户档案（余额 / 限购 / webhook 配置）
    pub async fn profile(&self) -> Result<ProfileInfo, VendorApiError> {
        match self.flavor {
            VendorFlavor::Legacy => {
                let r: legacy::ProfileResponse = self.get(legacy::PATH_PROFILE).await?;
                Ok(r.into())
            }
            VendorFlavor::Kiroapp => {
                let r: kiroapp::ProfileResponse = self.get(kiroapp::PATH_PROFILE).await?;
                Ok(r.into())
            }
            VendorFlavor::KiroappCc => {
                // kiroapp.cc 只有余额接口，无完整档案，构造简化的 ProfileInfo
                let r: kiroapp_cc::BalanceResponse = self.get(kiroapp_cc::PATH_BALANCE).await?;
                Ok(ProfileInfo {
                    email: None,
                    name: None,
                    created_at: None,
                    balance: r.balance,
                    quota: r.balance,
                    used_quota: None,
                    max_purchase: None,
                    min_purchase: None,
                    webhook_url: None,
                })
            }
            VendorFlavor::Drop => {
                // 与首家同路径，但金额是字符串，故用本家自己的 DTO
                let r: drop_flavor::ProfileResponse = self.get(drop_flavor::PATH_PROFILE).await?;
                Ok(r.into())
            }
        }
    }

    /// 历史提取订单，用于跟本地事件对账
    pub async fn purchase_orders(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Paged<OrderInfo>, VendorApiError> {
        match self.flavor {
            VendorFlavor::Legacy => {
                // 该卖家返回裸数组、无分页，固定最近 50 条
                let orders: Vec<legacy::PurchaseOrder> = self.get(legacy::PATH_ORDERS).await?;
                Ok(legacy::orders_to_paged(orders))
            }
            VendorFlavor::Kiroapp => {
                let env: kiroapp::Envelope<kiroapp::KiroappOrder> = self
                    .get_with(kiroapp::PATH_ORDERS, &paging(page, page_size))
                    .await?;
                Ok(env.map_into())
            }
            // 这两家都只能按 id 查单条，没有列表接口，返回空分页
            VendorFlavor::KiroappCc | VendorFlavor::Drop => Ok(Paged {
                items: vec![],
                total: Some(0),
                page: Some(page.unwrap_or(1)),
                page_size: Some(page_size.unwrap_or(50)),
                pages: Some(0),
            }),
        }
    }

    /// 兑换码充值。两家均对「同账号 + 同码」幂等，超时重试原样重发即可。
    pub async fn redeem(&self, code: &str) -> Result<RedeemResult, VendorApiError> {
        let body = serde_json::json!({ "code": code });
        match self.flavor {
            VendorFlavor::Legacy => {
                let r: legacy::RedeemResponse = self.post_json(legacy::PATH_REDEEM, &body).await?;
                Ok(r.into())
            }
            VendorFlavor::Kiroapp => {
                let r: kiroapp::RedeemResponse =
                    self.post_json(kiroapp::PATH_REDEEM, &body).await?;
                Ok(r.into())
            }
            VendorFlavor::KiroappCc | VendorFlavor::Drop => {
                Err(VendorApiError::unsupported("兑换码充值"))
            }
        }
    }

    // ============ 各家独有能力 ============

    /// 卖家系统状态：存活 / 失效 / 存货 Key 数。仅 `system_status` 能力。
    pub async fn system_status(&self) -> Result<legacy::VendorSystemStatus, VendorApiError> {
        if !self.capabilities().system_status {
            return Err(VendorApiError::unsupported("系统状态查询"));
        }
        // 首家与 Drop 的路径恰好同名，但分开取：Drop 的 stock() 走
        // drop_flavor::PATH_STATUS，若只改那一处、这里继续用 legacy 的常量，
        // 同一端点就有了两个真相来源，且没有测试会失败。同 webhook_path()。
        let path = match self.flavor {
            VendorFlavor::Drop => drop_flavor::PATH_STATUS,
            _ => legacy::PATH_STATUS,
        };
        self.get(path).await
    }

    /// 卖家近期开号批次与平均间隔，用于估算下一批新 Key 大概什么时候到。
    /// 仅 `gen_logs` 能力。
    pub async fn gen_logs(&self) -> Result<legacy::GenLogsResponse, VendorApiError> {
        if !self.capabilities().gen_logs {
            return Err(VendorApiError::unsupported("开号记录查询"));
        }
        self.get(legacy::PATH_GEN_LOGS).await
    }

    /// 更新卖家侧保存的 webhook URL。仅 `webhook_manage` 能力。
    pub async fn set_webhook_url(&self, webhook_url: &str) -> Result<(), VendorApiError> {
        if !self.capabilities().webhook_manage {
            return Err(VendorApiError::unsupported(
                "通过 API 配置 webhook 地址（请在卖家网页的设置里填）",
            ));
        }
        let resp = self
            .auth(
                self.http
                    .put(self.url(self.webhook_path()))
                    .json(&serde_json::json!({ "webhook_url": webhook_url })),
            )
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;
        self.parse::<serde_json::Value>(resp).await.map(|_| ())
    }

    /// 让卖家往已保存的 URL 推一条测试消息。仅 `webhook_manage` 能力。
    pub async fn test_webhook(&self) -> Result<serde_json::Value, VendorApiError> {
        if !self.capabilities().webhook_manage {
            return Err(VendorApiError::unsupported("由 API 触发 webhook 测试推送"));
        }
        self.post_json(self.webhook_test_path(), &serde_json::json!({}))
            .await
    }

    /// webhook 地址读写路径。首家与 Drop 恰好同名，但分开取以免日后一家改路径
    /// 时误改另一家。
    fn webhook_path(&self) -> &'static str {
        match self.flavor {
            VendorFlavor::Drop => drop_flavor::PATH_WEBHOOK,
            _ => legacy::PATH_WEBHOOK,
        }
    }

    fn webhook_test_path(&self) -> &'static str {
        match self.flavor {
            VendorFlavor::Drop => drop_flavor::PATH_WEBHOOK_TEST,
            _ => legacy::PATH_WEBHOOK_TEST,
        }
    }

    /// 积分流水。仅 `ledger` 能力。
    pub async fn ledger(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
        entry_type: Option<&str>,
    ) -> Result<Paged<LedgerEntry>, VendorApiError> {
        if !self.capabilities().ledger {
            return Err(VendorApiError::unsupported("积分流水查询"));
        }
        let mut query = paging(page, page_size);
        if let Some(t) = entry_type.filter(|s| !s.trim().is_empty()) {
            query.push(("type", t.trim().to_string()));
        }
        let env: kiroapp::Envelope<kiroapp::KiroappLedgerEntry> =
            self.get_with(kiroapp::PATH_LEDGER, &query).await?;
        Ok(env.map_into())
    }

    /// 名下密钥列表。`history` 为 true 时含已失效的。仅 `my_keys` 能力。
    ///
    /// 卖家的库存接口不给任何时间字段，本接口的 `created_at`（开号时刻）是
    /// 判断 Key 新鲜度的唯一来源。
    pub async fn my_keys(
        &self,
        history: bool,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Paged<VendorKeyInfo>, VendorApiError> {
        if !self.capabilities().my_keys {
            return Err(VendorApiError::unsupported("名下密钥列表查询"));
        }
        let mut query = paging(page, page_size);
        if history {
            query.push(("history", "1".to_string()));
        }
        let env: kiroapp::Envelope<kiroapp::KiroappMyKey> =
            self.get_with(kiroapp::PATH_KEYS, &query).await?;
        Ok(env.map_into())
    }

    /// 最早密钥时间与总数，估算账龄用。仅 `earliest_key` 能力。
    pub async fn earliest_key(&self) -> Result<EarliestKeyInfo, VendorApiError> {
        if !self.capabilities().earliest_key {
            return Err(VendorApiError::unsupported("最早密钥时间查询"));
        }
        let r: kiroapp::CreatedAtResponse = self.get(kiroapp::PATH_KEYS_CREATED_AT).await?;
        Ok(r.into())
    }
}

/// 下单请求体。抽成纯函数是为了能直接断言参数名 ——
/// 没有 HTTP mock 库时，这是唯一能锁住「发的是 `count` 而不是 `quantity`」的办法。
///
/// 三家（Legacy / Kiroapp / Drop）的主名都是 `count` + `client_order_id`。
/// Drop 家文档注明「也接受 quantity」，但我们发主名。
fn purchase_body(count: u32, client_order_id: &str) -> serde_json::Value {
    serde_json::json!({
        "count": count,
        "client_order_id": client_order_id,
    })
}

/// 从任意响应正文里按 `ksk_` 前缀捞 Key，转成中立结构。
///
/// 复用 [`kiroapp_cc::extract_keys`]（它递归遍历 JSON、按 Key 字符集切 token、
/// 去重）。不是合法 JSON 时整体当一个字符串再扫，故裸文本响应也能捞出。
fn scan_keys(text: &str) -> Vec<crate::vendor::protocol::PurchasedKey> {
    let value = serde_json::from_str::<serde_json::Value>(text)
        .unwrap_or_else(|_| serde_json::Value::String(text.to_string()));
    kiroapp_cc::extract_keys(&value)
        .into_iter()
        .map(|k| crate::vendor::protocol::PurchasedKey {
            key: k,
            account: None,
            password: None,
            issuer_url: None,
            price: None,
        })
        .collect()
}

/// 构造分页查询参数。`page_size` 超过卖家上限时收敛，避免白拿一个 400。
fn paging(page: Option<u32>, page_size: Option<u32>) -> Vec<(&'static str, String)> {
    let mut q = Vec::new();
    if let Some(p) = page {
        q.push(("page", p.max(1).to_string()));
    }
    if let Some(size) = page_size {
        q.push((
            "page_size",
            size.clamp(1, kiroapp::MAX_PAGE_SIZE).to_string(),
        ));
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(base: &str, key: &str, token: &str) -> VendorConfig {
        VendorConfig {
            id: "default".to_string(),
            name: String::new(),
            flavor: VendorFlavor::Legacy,
            base_url: base.to_string(),
            api_key: key.to_string(),
            webhook_path_token: token.to_string(),
            default_groups: vec![],
            default_rpm_limit: 300,
            default_api_region: String::new(),
            default_auth_region: String::new(),
            auto_purchase: false,
            auto_purchase_max_count: 1,
            auto_purchase_schedule: vec![],
            auto_purchase_per_channel: false,
        }
    }

    #[test]
    fn base_url_去掉末尾斜杠() {
        let c = cfg("https://v.example.com///", "usr-x", "t");
        assert_eq!(c.normalized_base_url(), "https://v.example.com");
    }

    #[test]
    fn 启用判定() {
        assert!(cfg("https://v", "usr-x", "t").inbound_enabled());
        // 缺 token：出站可用，入站不可用
        let no_token = cfg("https://v", "usr-x", "");
        assert!(no_token.outbound_enabled());
        assert!(!no_token.inbound_enabled());
        // 缺 key：两者都不可用
        assert!(!cfg("https://v", "  ", "t").outbound_enabled());
        assert!(!cfg("", "usr-x", "t").outbound_enabled());
    }

    #[test]
    fn 客户端拒绝不完整配置() {
        let c = cfg("", "", "");
        assert!(VendorClient::new(&c, None, TlsBackend::Rustls).is_err());
    }

    #[test]
    fn 客户端记住风味与能力() {
        let mut c = cfg("https://v", "km_x", "t");
        c.flavor = VendorFlavor::Kiroapp;
        let client = VendorClient::new(&c, None, TlsBackend::Rustls).unwrap();
        assert!(client.capabilities().ledger);
        assert!(!client.capabilities().gen_logs);
    }

    #[tokio::test]
    async fn 不支持的能力不发请求直接报错() {
        // base_url 指向黑洞地址：若真发了请求会超时而非立刻返回，
        // 能立刻拿到 unsupported 就证明短路生效
        let mut c = cfg("http://127.0.0.1:1", "km_x", "t");
        c.flavor = VendorFlavor::Kiroapp;
        let client = VendorClient::new(&c, None, TlsBackend::Rustls).unwrap();

        let e = client.gen_logs().await.unwrap_err();
        assert!(e.status.is_none());
        assert!(e.message.contains("开号记录"), "实际: {}", e.message);

        let e = client.system_status().await.unwrap_err();
        assert!(e.message.contains("系统状态"));

        let e = client.set_webhook_url("https://x").await.unwrap_err();
        assert!(e.message.contains("webhook"), "实际: {}", e.message);

        // 反向：legacy 不支持 kiroapp 的独有能力
        let legacy_client = VendorClient::new(&cfg("http://127.0.0.1:1", "usr-x", "t"), None, TlsBackend::Rustls)
            .unwrap();
        assert!(legacy_client.ledger(None, None, None).await.is_err());
        assert!(legacy_client.my_keys(false, None, None).await.is_err());
        assert!(legacy_client.earliest_key().await.is_err());
    }

    #[test]
    fn drop_家的能力集与鉴权() {
        let mut c = cfg("https://drop.kiro.ss", "usr-x", "t");
        c.flavor = VendorFlavor::Drop;
        let client = VendorClient::new(&c, None, TlsBackend::Rustls).unwrap();
        let caps = client.capabilities();
        // /api/status 既是系统状态也是库存来源
        assert!(caps.system_status);
        assert!(caps.webhook_manage);
        // 以下四项在本家实测均 404
        assert!(!caps.purchase_orders);
        assert!(!caps.redeem);
        assert!(!caps.gen_logs);
        assert!(!caps.ledger);
        assert!(!caps.tiered_pricing);
    }

    #[tokio::test]
    async fn drop_不支持的能力不发请求() {
        // 黑洞地址：真发了请求会超时，立刻返回就证明短路生效
        let mut c = cfg("http://127.0.0.1:1", "usr-x", "t");
        c.flavor = VendorFlavor::Drop;
        let client = VendorClient::new(&c, None, TlsBackend::Rustls).unwrap();

        let e = client.redeem("code").await.unwrap_err();
        assert!(e.message.contains("兑换码"), "实际: {}", e.message);
        assert!(client.gen_logs().await.is_err());
        assert!(client.ledger(None, None, None).await.is_err());
        assert!(client.my_keys(false, None, None).await.is_err());

        // 订单列表无接口但不报错，返回空分页（面板展示「暂无」而非一条错误）
        let paged = client.purchase_orders(None, None).await.unwrap();
        assert!(paged.items.is_empty());
        assert_eq!(paged.total, Some(0));
    }

    /// 锁住下单参数名。文档主名是 `count`，改成 `quantity` 此前不会有任何测试失败。
    #[test]
    fn 下单请求体用count而非quantity() {
        let body = purchase_body(2, "0123456789abcdef0123456789abcdef");
        assert_eq!(body["count"], 2, "参数名必须是 count（文档主名）");
        assert!(body.get("quantity").is_none(), "不该发 quantity");
        assert_eq!(
            body["client_order_id"],
            "0123456789abcdef0123456789abcdef",
            "订单号字段名必须是 client_order_id"
        );
        // 幂等保护字段不发：报价接口已撤，拿不到当前单价，填不出合理上限
        assert!(body.get("max_total_cny").is_none());
    }

    /// Drop 与首家的 /api/status 今天同路径，system_status() 与 stock() 必须
    /// 指向同一个端点。哪天分叉了这条会失败，提醒去改另一处。
    #[test]
    fn drop_的status路径与首家一致() {
        assert_eq!(
            legacy::PATH_STATUS,
            crate::vendor::flavor_drop::PATH_STATUS,
            "两家的 status 路径若分叉，system_status() 与 stock() 会指向不同端点"
        );
    }

    #[test]
    fn drop_家用webhook自己的路径() {
        let mut c = cfg("https://drop.kiro.ss", "usr-x", "t");
        c.flavor = VendorFlavor::Drop;
        let client = VendorClient::new(&c, None, TlsBackend::Rustls).unwrap();
        assert_eq!(client.webhook_path(), crate::vendor::flavor_drop::PATH_WEBHOOK);
        assert_eq!(
            client.webhook_test_path(),
            crate::vendor::flavor_drop::PATH_WEBHOOK_TEST
        );

        let legacy_client =
            VendorClient::new(&cfg("https://v", "usr-x", "t"), None, TlsBackend::Rustls).unwrap();
        assert_eq!(legacy_client.webhook_path(), legacy::PATH_WEBHOOK);
    }

    #[test]
    fn 分页参数收敛到卖家上限() {
        let q = paging(Some(0), Some(9999));
        assert_eq!(q[0], ("page", "1".to_string()), "页码最小为 1");
        assert_eq!(
            q[1],
            ("page_size", kiroapp::MAX_PAGE_SIZE.to_string()),
            "每页条数收敛到上限，避免白拿 400"
        );

        assert!(paging(None, None).is_empty(), "都不传则不加参数");
    }
}
