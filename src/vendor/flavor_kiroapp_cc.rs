//! kiroapp.cc 的协议实现：`/openapi/*` + `Authorization: Bearer km_xxx`
//!
//! 简化版协议，只有 4 个接口：库存、余额、提取、批量提取。
//! - 单次提取返回 `{key}`
//! - 批量提取返回 `{keys: [...], pointsCost?: number}`
//! - 库存返回 `{availableKeys, keyPrice}`
//! - 余额返回 `{balance}`
//! - 错误统一为 `{error: {type, message}, retryAfter?: number}`
//!
//! @author wangzhong

use serde::Deserialize;

use super::protocol::{PurchaseResult, PurchasedKey, StockInfo};

pub const PATH_STOCK: &str = "/openapi/stock";
pub const PATH_BALANCE: &str = "/openapi/balance";
pub const PATH_CLAIM: &str = "/openapi/claim";

/// `GET /openapi/stock` 响应
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StockResponse {
    #[serde(default)]
    pub available_keys: u32,
    #[serde(default)]
    pub key_price: Option<f64>,
}

impl From<StockResponse> for StockInfo {
    fn from(r: StockResponse) -> Self {
        Self {
            available: r.available_keys,
            price_min: r.key_price,
            price_max: r.key_price, // 无阶梯定价，min = max
            balance: None,           // 库存接口不带余额，需单独查
        }
    }
}

/// `GET /openapi/balance` 响应
#[derive(Debug, Clone, Deserialize)]
pub struct BalanceResponse {
    #[serde(default)]
    pub balance: Option<f64>,
}

/// `POST /openapi/claim` 单次提取响应（无 count 参数）
#[derive(Debug, Clone, Deserialize)]
pub struct ClaimSingleResponse {
    #[serde(default)]
    pub key: String,
}

/// `POST /openapi/claim` 批量提取响应（带 {"count": N}）
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimBatchResponse {
    #[serde(default)]
    pub keys: Vec<String>,
    /// 实际扣费。自己产出的 Key 不扣积分，此时为 0
    #[serde(default)]
    pub points_cost: Option<f64>,
}

/// 统一的提取结果，单次和批量都转成这个
pub struct ClaimResult {
    pub keys: Vec<String>,
    pub points_cost: Option<f64>,
}

/// 错误响应 `{error: {type, message}, retryAfter?: number}`
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    pub error: ErrorDetail,
    #[serde(default)]
    pub retry_after: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorDetail {
    #[serde(default, rename = "type")]
    pub error_type: String,
    #[serde(default)]
    pub message: String,
}

impl ClaimResult {
    /// 转换为通用的 PurchaseResult
    pub fn into_purchase_result(
        self,
        order_id: String,
        requested: u32,
    ) -> PurchaseResult {
        let purchased = self.keys.len() as u32;
        PurchaseResult {
            purchased,
            requested: Some(requested),
            remaining: None, // kiroapp.cc 不返回剩余余额
            unit_price: self.points_cost.map(|c| c / purchased as f64),
            total_debit: self.points_cost,
            order_id: Some(order_id),
            keys: self
                .keys
                .into_iter()
                .map(|k| PurchasedKey {
                    key: k,
                    account: None,
                    password: None,
                    issuer_url: None,
                    price: None,
                })
                .collect(),
            replayed: false,
        }
    }
}
