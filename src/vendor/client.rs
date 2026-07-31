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

    /// Drop 的报价查询：`GET /api/v1/reservation?quantity=N`。
    ///
    /// 一次给齐库存、限购、单价与余额，故 [`Self::stock`] 与 [`Self::profile`]
    /// 都走它，不必分两次请求。
    async fn drop_quote(&self, quantity: u32) -> Result<drop_flavor::QuoteResponse, VendorApiError> {
        self.get_with(
            drop_flavor::PATH_RESERVATION,
            &[("quantity", quantity.max(1).to_string())],
        )
        .await
    }

    /// 同 [`Self::drop_quote`]，但把「库存不足」这一种 400 当成「可提取 0 个」。
    ///
    /// 报价接口的 `quantity` 会参与校验，超过库存直接 400 —— 而查库存时我们恰恰
    /// 还不知道上限。若把这种 400 原样上抛，卖家一卖空面板就只显示一条报错，
    /// 看不出「就是没货」这个正常状态。
    ///
    /// 只对库存类文案降级，其余 400（如订单号格式错）仍照常报错。
    async fn drop_quote_lenient(&self) -> Result<drop_flavor::QuoteResponse, VendorApiError> {
        match self.drop_quote(drop_flavor::PROBE_QUANTITY).await {
            Ok(q) => Ok(q),
            Err(e) if e.status == Some(400) && mentions_out_of_stock(&e.message) => {
                tracing::debug!("Drop 报价返回库存不足，按 0 库存处理: {}", e.message);
                Ok(drop_flavor::QuoteResponse {
                    stock: 0,
                    ..Default::default()
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Drop 的下单：`POST /api/v1/reservation`，202 时按 `order_id` 轮询。
    ///
    /// **不传 `max_total_cny`**。文档建议传它做涨价保护，但同一订单号重试时参数
    /// 必须一致（否则 409），而重试时重新报出的价格不保证还是原值 —— 传了会把
    /// 一次可幂等重放的重试变成永久失败。涨价风险由自动模式的数量上限兜着。
    ///
    /// 202 表示钱已扣、Key 待对账，此时**只能拿 order_id 轮询，绝不能换单号重下**。
    /// 轮询用尽仍未出货也返回 Ok：钱确实扣了，返回已知信息让人工按 order_id 核对，
    /// 报错会让人误以为没花钱。
    async fn purchase_drop(
        &self,
        quantity: u32,
        client_order_id: &str,
    ) -> Result<drop_flavor::OrderResponse, VendorApiError> {
        let body = serde_json::json!({
            "quantity": quantity,
            "client_order_id": client_order_id,
        });
        let mut order: drop_flavor::OrderResponse =
            self.post_json(drop_flavor::PATH_RESERVATION, &body).await?;

        // 回显的订单号与我们发出的不一致，说明这单大概不是我们这一单（卖家侧串号，
        // 或我们把别人的重放当成了自己的）。只告警不阻断 —— 钱可能已经扣了，
        // 报错会让人误以为没花钱；Key 仍照常入库，凭 order_id 可人工追。
        if let Some(echoed) = order.client_order_id.as_deref()
            && echoed.trim() != client_order_id.trim()
        {
            tracing::warn!(
                sent = %client_order_id,
                echoed = %echoed,
                "Drop 回显的 client_order_id 与发出的不一致，请人工核对该单"
            );
        }

        if !order.is_pending() || order.has_keys() {
            return Ok(order);
        }

        let Some(order_id) = order.order_id.clone().filter(|s| !s.trim().is_empty()) else {
            // pending 却没给 order_id：无从轮询，也无从人工核对，只能上抛
            return Err(VendorApiError {
                status: Some(202),
                message: "卖家返回待对账订单但未给出 order_id，无法查询结果，请在卖家侧核对是否已扣费"
                    .to_string(),
            });
        };
        let path = format!("{}{}", drop_flavor::PATH_ORDER_PREFIX, order_id.trim());

        for attempt in 1..=drop_flavor::ORDER_POLL_MAX_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_secs(
                drop_flavor::ORDER_POLL_INTERVAL_SECS,
            ))
            .await;
            match self.get::<drop_flavor::OrderResponse>(&path).await {
                Ok(latest) => {
                    let done = !latest.is_pending() || latest.has_keys();
                    order = latest;
                    if done {
                        return Ok(order);
                    }
                }
                // 查询失败不终止：钱已扣，值得把剩下的机会用完
                Err(e) => tracing::warn!(
                    order_id = %order_id,
                    attempt,
                    "Drop 订单查询失败，稍后重试: {}",
                    e
                ),
            }
        }

        tracing::warn!(
            order_id = %order_id,
            "Drop 订单轮询用尽仍未出货，可能已扣费，请人工按 order_id 核对"
        );
        Ok(order)
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
    pub async fn purchase(
        &self,
        count: u32,
        client_order_id: &str,
        batch_order_id: Option<&str>,
    ) -> Result<PurchaseResult, VendorApiError> {
        let mut body = serde_json::json!({
            "count": count,
            "client_order_id": client_order_id,
        });
        match self.flavor {
            VendorFlavor::Legacy => {
                let r: legacy::PurchaseResponse =
                    self.post_json(legacy::PATH_PURCHASE, &body).await?;
                Ok(r.into())
            }
            VendorFlavor::Kiroapp => {
                // 只有该 flavor 支持按批次定向拉取
                if let Some(batch) = batch_order_id.filter(|s| !s.trim().is_empty()) {
                    body["order_id"] = serde_json::json!(batch.trim());
                }
                let r: kiroapp::PurchaseResponse =
                    self.post_json(kiroapp::PATH_PURCHASE, &body).await?;
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
            VendorFlavor::Drop => {
                // 本家的数量参数叫 quantity，且下单可能异步（202 → 轮询）
                let order = self.purchase_drop(count, client_order_id).await?;
                Ok(order.into())
            }
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
            VendorFlavor::Drop => Ok(self.drop_quote_lenient().await?.into()),
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
            // 本家没有独立档案接口，余额与限购都在报价里
            VendorFlavor::Drop => Ok(self.drop_quote_lenient().await?.into()),
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
        self.get(legacy::PATH_STATUS).await
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

/// 错误文案是否在说「没货 / 货不够」。
///
/// 用于 Drop 的报价降级（见 [`VendorClient::drop_quote_lenient`]）：卖家没给
/// 机器可读的错误码区分「参数错」与「没货」，只能认文案。中英都列上，卖家
/// 换语言不至于立刻失效；认不出就照常报错，不会把真错误吞掉。
fn mentions_out_of_stock(message: &str) -> bool {
    const NEEDLES: [&str; 6] = [
        "库存",
        "缺货",
        "售罄",
        "out of stock",
        "insufficient stock",
        "no stock",
    ];
    let lower = message.to_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n))
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
        // 有 webhook 远程管理
        assert!(caps.webhook_manage);
        // 只能按 order_id 查单条，没有列表
        assert!(!caps.purchase_orders);
        assert!(!caps.redeem);
        assert!(!caps.ledger);
        assert!(!caps.system_status);
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
        assert!(client.system_status().await.is_err());
        assert!(client.gen_logs().await.is_err());
        assert!(client.ledger(None, None, None).await.is_err());
        assert!(client.my_keys(false, None, None).await.is_err());

        // 订单列表无接口但不报错，返回空分页（面板展示「暂无」而非一条错误）
        let paged = client.purchase_orders(None, None).await.unwrap();
        assert!(paged.items.is_empty());
        assert_eq!(paged.total, Some(0));
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
    fn 识别库存不足的错误文案() {
        // 中文（卖家当前的文案）
        assert!(mentions_out_of_stock("库存不足：需要 1 个，当前可售 0 个"));
        assert!(mentions_out_of_stock("商品已售罄"));
        assert!(mentions_out_of_stock("缺货"));
        // 英文，大小写不敏感
        assert!(mentions_out_of_stock("Out Of Stock"));
        assert!(mentions_out_of_stock("insufficient stock for this request"));

        // 其余 400 不能被当成没货吞掉
        assert!(!mentions_out_of_stock("数量或订单号格式不正确"));
        assert!(!mentions_out_of_stock("API Key 无效"));
        assert!(!mentions_out_of_stock("余额不足"));
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
