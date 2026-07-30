//! 次级卖家 kiroapp 出站 API 客户端
//!
//! 只覆盖三个端点，能力远少于主卖家（[`super::client`]）：
//! - `GET  /openapi/stock`   → `{"availableKeys":N,"keyPrice":P}`
//! - `GET  /openapi/balance` → `{"balance":B}`
//! - `POST /openapi/claim`   → 提取一个 Key（无数量参数、无订单号）
//!
//! 与主卖家的三处关键差异：
//! 1. 认证头是 `Authorization: Bearer`，不是 `X-API-Key`；
//! 2. 错误体是嵌套的 `{"error":{"message":..,"type":..}}`，不是 `{"error":"文本"}`；
//! 3. claim 无幂等键，超时重试可能重复扣费，故上层不做自动重试。
//!
//! @author wangzhong

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::http_client::{self, ProxyConfig};
use crate::model::config::{KiroappConfig, TlsBackend};

/// 出站请求超时（秒）。claim 需对方现场分配，给足时间。
const REQUEST_TIMEOUT_SECS: u64 = 120;

/// `GET /openapi/stock` 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroappStock {
    /// 当前可售 Key 数
    #[serde(default)]
    pub available_keys: Option<u32>,
    /// 单个 Key 的价格
    #[serde(default)]
    pub key_price: Option<f64>,
}

/// `GET /openapi/balance` 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroappBalance {
    #[serde(default)]
    pub balance: Option<f64>,
}

/// 出站调用失败，携带 HTTP 状态码便于上层原样透出
#[derive(Debug)]
pub struct KiroappApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for KiroappApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(code) => write!(f, "kiroapp 接口返回 {}: {}", code, self.message),
            None => write!(f, "kiroapp 接口调用失败: {}", self.message),
        }
    }
}

impl std::error::Error for KiroappApiError {}

/// kiroapp 客户端。复用全局代理与 TLS 后端配置。
pub struct KiroappClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl KiroappClient {
    /// 按配置构建客户端。`base_url` / `api_key` 为空时返回 Err。
    pub fn new(
        cfg: &KiroappConfig,
        proxy: Option<&ProxyConfig>,
        tls_backend: TlsBackend,
    ) -> anyhow::Result<Self> {
        if !cfg.enabled() {
            anyhow::bail!("kiroapp 配置不完整（baseUrl / apiKey 为空）");
        }
        let http = http_client::build_client(proxy, REQUEST_TIMEOUT_SECS, tls_backend)
            .context("构建 kiroapp API 客户端失败")?;
        Ok(Self {
            http,
            base_url: cfg.normalized_base_url().to_string(),
            api_key: cfg.api_key.trim().to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, KiroappApiError> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| KiroappApiError {
                status: None,
                message: e.to_string(),
            })?;
        let (status, body) = read_body(resp).await?;
        parse_json(status, &body)
    }

    /// `GET /openapi/stock` —— 可售数量与单价
    pub async fn stock(&self) -> Result<KiroappStock, KiroappApiError> {
        self.get("/openapi/stock").await
    }

    /// `GET /openapi/balance` —— 账号余额
    pub async fn balance(&self) -> Result<KiroappBalance, KiroappApiError> {
        self.get("/openapi/balance").await
    }

    /// `POST /openapi/claim` —— 提取一个 Key。
    ///
    /// 对方无幂等键，**调用方不得自动重试**：超时时无法区分「没扣费」与
    /// 「扣了费但响应丢了」，重发会二次扣费。
    ///
    /// 返回体结构未公开（下单成功才见得到），故用 [`extract_keys`] 递归捞
    /// `ksk_` 串，而不是绑定某个固定字段名。同时把原始响应回传，便于面板在
    /// 捞不到 Key 时展示原文供人工核对。
    ///
    /// 已知两种可能形态，都必须兜住：
    /// - JSON（Key 是某个字段的值，字段名未知，可能嵌在对象/数组里）；
    /// - **裸文本**（响应体就是 `ksk_xxx` 本身，不是合法 JSON）。
    ///
    /// 后者若按 JSON 硬解会失败，而此时钱已经扣了 —— 解析失败就等于把付过费
    /// 的 Key 扔掉，所以 2xx 一律降级成纯文本再扫，绝不因为不是 JSON 就报错。
    pub async fn claim(&self) -> Result<ClaimOutcome, KiroappApiError> {
        let resp = self
            .http
            .post(self.url("/openapi/claim"))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| KiroappApiError {
                status: None,
                message: e.to_string(),
            })?;
        let (status, body) = read_body(resp).await?;
        // 非 2xx 统一走错误解析（库存不足 / 余额不足都在这里）
        if !(200..300).contains(&status) {
            return Err(KiroappApiError {
                status: Some(status),
                message: error_message(&body).unwrap_or_else(|| truncate(&body, 300)),
            });
        }
        // 2xx：能当 JSON 就当 JSON，不能就整体视作一个字符串
        let value = serde_json::from_str::<serde_json::Value>(&body)
            .unwrap_or_else(|_| serde_json::Value::String(body.clone()));
        Ok(ClaimOutcome {
            keys: extract_keys(&value),
            raw: value,
        })
    }
}

/// claim 的解析结果
#[derive(Debug, Clone)]
pub struct ClaimOutcome {
    /// 从响应里捞出的 `ksk_` Key（去重后）
    pub keys: Vec<String>,
    /// 原始响应，捞不到 Key 时回传给面板供人工核对
    pub raw: serde_json::Value,
}

/// 读出状态码与响应体文本
async fn read_body(resp: reqwest::Response) -> Result<(u16, String), KiroappApiError> {
    let status = resp.status().as_u16();
    let body = resp.text().await.map_err(|e| KiroappApiError {
        status: Some(status),
        message: format!("读取响应体失败: {e}"),
    })?;
    Ok((status, body))
}

/// 统一解析：非 2xx 时按 kiroapp 的嵌套错误体取 message
fn parse_json<T: for<'de> Deserialize<'de>>(
    status: u16,
    body: &str,
) -> Result<T, KiroappApiError> {
    if !(200..300).contains(&status) {
        return Err(KiroappApiError {
            status: Some(status),
            message: error_message(body).unwrap_or_else(|| truncate(body, 300)),
        });
    }
    serde_json::from_str::<T>(body).map_err(|e| KiroappApiError {
        status: Some(status),
        message: format!("解析响应失败: {e}；原文片段: {}", truncate(body, 200)),
    })
}

/// 从错误体里取人类可读信息。
///
/// 兼容两种形状：kiroapp 的 `{"error":{"message":".."}}`，以及退化的
/// `{"error":"文本"}` —— 对方将来改简单形式时不必回来改代码。
fn error_message(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let err = v.get("error")?;
    if let Some(s) = err.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    let msg = err.get("message")?.as_str()?.trim();
    (!msg.is_empty()).then(|| msg.to_string())
}

/// 递归捞出所有 `ksk_` 前缀的字符串并去重（保持首次出现顺序）。
///
/// claim 的成功响应结构未公开，可能是 `{"key":".."}`、`{"keys":[..]}`、嵌在
/// 订单对象里，或者干脆是裸文本。按前缀捞比猜字段名稳，且 Key 格式本身有强特征。
///
/// 每个字符串还会按 Key 字符集切 token 后再匹配，这样 `your key: ksk_abc`
/// 这类夹带说明文字的响应也能捞出干净的 Key。
pub fn extract_keys(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(value, &mut out);
    out
}

/// Key 允许的字符集。用它切 token，把周围的空白、引号、标点剥掉。
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => {
            for token in s.split(|c: char| !is_key_char(c)) {
                if token.starts_with("ksk_")
                    && token.len() > 4
                    && !out.iter().any(|k| k == token)
                {
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
            for (_, v) in map {
                walk(v, out);
            }
        }
        _ => {}
    }
}

/// 按字符边界截断，避免把多字节 UTF-8 切坏
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(base: &str, key: &str) -> KiroappConfig {
        KiroappConfig {
            base_url: base.to_string(),
            api_key: key.to_string(),
            default_groups: vec![],
            default_rpm_limit: 10,
        }
    }

    #[test]
    fn base_url_去掉末尾斜杠() {
        assert_eq!(cfg("https://kiroapp.cc//", "k").normalized_base_url(), "https://kiroapp.cc");
    }

    #[test]
    fn 启用判定() {
        assert!(cfg("https://kiroapp.cc", "sk-x").enabled());
        assert!(!cfg("", "sk-x").enabled());
        assert!(!cfg("https://kiroapp.cc", "  ").enabled());
    }

    /// 用真实返回做样本
    #[test]
    fn 解析库存_真实样本() {
        let s: KiroappStock = parse_json(200, r#"{"availableKeys":0,"keyPrice":50}"#).unwrap();
        assert_eq!(s.available_keys, Some(0));
        assert_eq!(s.key_price, Some(50.0));
    }

    #[test]
    fn 解析余额_真实样本() {
        let b: KiroappBalance = parse_json(200, r#"{"balance":0}"#).unwrap();
        assert_eq!(b.balance, Some(0.0));
    }

    #[test]
    fn 解析库存_容忍缺字段() {
        let s: KiroappStock = parse_json(200, r#"{}"#).unwrap();
        assert_eq!(s.available_keys, None);
        assert_eq!(s.key_price, None);
    }

    /// 用真实的库存不足返回做样本：错误体是嵌套的，不能按 `{"error":"文本"}` 解析
    #[test]
    fn 解析嵌套错误体_真实样本() {
        let raw = r#"{"error":{"message":"库存不足：需要 1 个，当前可售 0 个","type":"out_of_stock"}}"#;
        let e = parse_json::<KiroappStock>(400, raw).unwrap_err();
        assert_eq!(e.status, Some(400));
        assert_eq!(e.message, "库存不足：需要 1 个，当前可售 0 个");
    }

    #[test]
    fn 解析扁平错误体() {
        let e = parse_json::<KiroappStock>(403, r#"{"error":"余额不足"}"#).unwrap_err();
        assert_eq!(e.message, "余额不足");
    }

    #[test]
    fn 无法识别的错误体退回原文片段() {
        let e = parse_json::<KiroappStock>(500, "upstream boom").unwrap_err();
        assert_eq!(e.message, "upstream boom");
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
    /// 模拟 claim 里的降级路径，确认不会因为解析失败把已扣费的 Key 丢掉。
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

    /// 形态一：Key 是 JSON 字段的值，字段名未知
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
    fn truncate_不切坏多字节字符() {
        assert_eq!(truncate("中文测试", 2), "中文…");
    }

    #[test]
    fn 客户端拒绝不完整配置() {
        assert!(KiroappClient::new(&cfg("", ""), None, TlsBackend::Rustls).is_err());
    }
}
