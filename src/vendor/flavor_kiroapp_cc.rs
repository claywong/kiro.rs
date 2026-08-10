//! kiroapp.cc 的协议实现：`/openapi/*` + `Authorization: Bearer km_xxx`
//!
//! 简化版协议，只有 4 个接口：库存、余额、提取、批量提取。
//! - 单次提取返回 `{key}`
//! - 批量提取返回 `{keys: [...], pointsCost?: number}`
//! - 库存返回 `{availableKeys, keyPrice}`
//! - 余额返回 `{balance}`
//! - 错误统一为 `{error: {type, message}, retryAfter?: number}`，由
//!   [`error_message`] 取出 message（同时兼容退化的 `{error: "文本"}`）
//!
//! 注意与 [`super::flavor_kiroapp`]（kiroapp**.io**，`/api/me/*`）不是同一家卖家：
//! 本家没有 webhook、没有积分流水、没有密钥列表、也没有阶梯定价。
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
            // 该卖家不分区
            zones: Vec::new(),
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
            // 一个 Key 都没捞到时不能拿 0 去除，否则单价成 inf/NaN，
            // 序列化进 JSON 会变成 null 或直接报错。
            unit_price: self
                .points_cost
                .filter(|_| purchased > 0)
                .map(|c| c / purchased as f64),
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
                    // 本家的区域是订单级的（zone），逐张不带区
                    region: None,
                })
                .collect(),
            replayed: false,
            zone: None,
        }
    }
}

/// Key 允许的字符集。用它切 token，把周围的空白、引号、标点剥掉。
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// 递归捞出所有 `ksk_` 前缀的字符串并去重（保持首次出现顺序）。
///
/// claim 的成功响应**已知有两种形态**：`{"key":".."}` / `{"keys":[..]}` 是文档
/// 形态，但实测也出现过响应体就是裸 Key 文本、不是合法 JSON 的情况。后者若按
/// JSON 硬解会失败，而此时钱已经扣了 —— 解析失败就等于把付过费的 Key 扔掉。
///
/// 因此严格解析失败时降级到本函数按前缀扫。每个字符串还会按 Key 字符集切 token
/// 后再匹配，这样 `your key: ksk_abc` 这类夹带说明文字的响应也能捞出干净的 Key。
pub fn extract_keys(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(value, &mut out);
    out
}

fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => {
            for token in s.split(|c: char| !is_key_char(c)) {
                if token.starts_with("ksk_") && token.len() > 4 && !out.iter().any(|k| k == token) {
                    out.push(token.to_string());
                }
            }
        }
        serde_json::Value::Array(items) => {
            for it in items {
                walk(it, out);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                walk(v, out);
            }
        }
        _ => {}
    }
}

/// 从 kiroapp.cc 的错误体里取人类可读信息。
///
/// 兼容两种形状：文档形态 `{"error":{"message":".."}}`，以及退化的
/// `{"error":"文本"}` —— 对方将来改简单形式时不必回来改代码。
pub fn error_message(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let err = v.get("error")?;
    if let Some(s) = err.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    let msg = err.get("message")?.as_str()?.trim();
    (!msg.is_empty()).then(|| msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用真实返回做样本
    #[test]
    fn 解析库存_真实样本() {
        let r: StockResponse = serde_json::from_str(r#"{"availableKeys":0,"keyPrice":50}"#).unwrap();
        let s: StockInfo = r.into();
        assert_eq!(s.available, 0);
        assert_eq!(s.price_min, Some(50.0));
        // 无阶梯定价，min = max
        assert_eq!(s.price_max, Some(50.0));
        // 库存接口不带余额，需单独查
        assert_eq!(s.balance, None);
    }

    #[test]
    fn 解析库存_容忍缺字段() {
        let r: StockResponse = serde_json::from_str("{}").unwrap();
        assert_eq!(r.available_keys, 0);
        assert_eq!(r.key_price, None);
    }

    #[test]
    fn 解析余额_真实样本() {
        let b: BalanceResponse = serde_json::from_str(r#"{"balance":0}"#).unwrap();
        assert_eq!(b.balance, Some(0.0));
    }

    #[test]
    fn 解析单次提取() {
        let r: ClaimSingleResponse = serde_json::from_str(r#"{"key":"ksk_abc"}"#).unwrap();
        assert_eq!(r.key, "ksk_abc");
    }

    #[test]
    fn 解析批量提取_自产不扣费() {
        let r: ClaimBatchResponse =
            serde_json::from_str(r#"{"keys":["ksk_a","ksk_b"],"pointsCost":0}"#).unwrap();
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.points_cost, Some(0.0));
    }

    /// 用真实的库存不足返回做样本：错误体是嵌套的，不能按 `{"error":"文本"}` 解析
    #[test]
    fn 解析嵌套错误体_真实样本() {
        let raw = r#"{"error":{"message":"库存不足：需要 1 个，当前可售 0 个","type":"out_of_stock"}}"#;
        assert_eq!(
            error_message(raw).as_deref(),
            Some("库存不足：需要 1 个，当前可售 0 个")
        );
    }

    #[test]
    fn 解析扁平错误体() {
        assert_eq!(error_message(r#"{"error":"余额不足"}"#).as_deref(), Some("余额不足"));
    }

    #[test]
    fn 无法识别的错误体返回none() {
        assert!(error_message("upstream boom").is_none());
        assert!(error_message(r#"{"ok":true}"#).is_none());
        // 空白 message 视为无效
        assert!(error_message(r#"{"error":{"message":"  "}}"#).is_none());
    }

    #[test]
    fn 捞key_单个字段() {
        let v = serde_json::json!({"key": "ksk_abc123"});
        assert_eq!(extract_keys(&v), vec!["ksk_abc123"]);
    }

    #[test]
    fn 捞key_数组与嵌套对象() {
        let v = serde_json::json!({
            "order": {"id": 7, "items": [{"apiKey": "ksk_a"}, {"apiKey": "ksk_b"}]}
        });
        assert_eq!(extract_keys(&v), vec!["ksk_a", "ksk_b"]);
    }

    #[test]
    fn 捞key_去重且忽略非key串() {
        let v = serde_json::json!({
            "a": "ksk_dup", "b": "ksk_dup", "c": "usr-not-a-key", "d": 123, "e": null
        });
        assert_eq!(extract_keys(&v), vec!["ksk_dup"]);
    }

    /// 形态二：响应体就是裸 Key 文本，不是合法 JSON。
    /// 确认降级路径不会因为解析失败把已扣费的 Key 丢掉。
    #[test]
    fn 捞key_裸文本响应() {
        let body = "ksk_bare123";
        let v = serde_json::from_str::<serde_json::Value>(body)
            .unwrap_or_else(|_| serde_json::Value::String(body.to_string()));
        assert_eq!(extract_keys(&v), vec!["ksk_bare123"]);
    }

    /// 裸文本但带说明文字/换行，按 token 切分后仍能捞出干净的 Key
    #[test]
    fn 捞key_文本夹带说明文字() {
        let body = "your key: ksk_abc123\n请妥善保存";
        let v = serde_json::from_str::<serde_json::Value>(body)
            .unwrap_or_else(|_| serde_json::Value::String(body.to_string()));
        assert_eq!(extract_keys(&v), vec!["ksk_abc123"]);
    }

    #[test]
    fn 捞key_顶层json字符串() {
        let v: serde_json::Value = serde_json::from_str(r#""ksk_quoted""#).unwrap();
        assert_eq!(extract_keys(&v), vec!["ksk_quoted"]);
    }

    /// 只有前缀没有实体的串不算 Key，避免把 `ksk_` 占位符当成结果
    #[test]
    fn 捞key_忽略空前缀() {
        assert!(extract_keys(&serde_json::json!({"key": "ksk_"})).is_empty());
    }

    #[test]
    fn 捞key_两侧空白会被裁掉() {
        let v = serde_json::json!({"key": "  ksk_pad  "});
        assert_eq!(extract_keys(&v), vec!["ksk_pad"]);
    }

    #[test]
    fn 捞不到key时返回空() {
        assert!(extract_keys(&serde_json::json!({"ok": true})).is_empty());
    }

    #[test]
    fn 零个key时不产生inf单价() {
        let r = ClaimResult {
            keys: vec![],
            points_cost: Some(50.0),
        };
        let pr = r.into_purchase_result("order-1".to_string(), 1);
        assert_eq!(pr.purchased, 0);
        assert!(pr.unit_price.is_none(), "0 个 Key 不能算出单价");
        // 总扣费仍要保留，人工核对时需要看到钱确实扣了
        assert_eq!(pr.total_debit, Some(50.0));
    }
}
