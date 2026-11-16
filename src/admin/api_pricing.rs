//! Anthropic 标准 API 价格表（美元 / 百万 Token），用于估算“按官方 API 计费”的金额。
//!
//! - 缓存写入按 5 分钟 TTL 计价：输入价 × 1.25（无法得知客户端实际 TTL）
//! - 缓存读取各模型倍率不同，逐个列出
//! - 未收录的模型（非 Claude 或新模型）返回 `None`，由调用方标记为“未计价”
//!
//! 新模型上线时在 [`price_for`] 里补一行。

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}

impl ModelPrice {
    const fn new(input: f64, output: f64, cache_read: f64) -> Self {
        Self {
            input,
            output,
            cache_read,
        }
    }

    pub fn cache_write(&self) -> f64 {
        self.input * 1.25
    }
}

/// Claude Haiku 5.5 的提示（含缓存）超过该 Token 数时整单按高档计价
const HAIKU_5_5_LONG_PROMPT: u64 = 100_000;

/// 把请求里的模型名归一成官方 ID：小写、点号转连字符，去掉 `-thinking`、`[1m]`、
/// `-YYYYMMDD` 日期后缀和 `-latest`。
pub fn normalize_model(model: &str) -> String {
    let mut s = model.trim().to_ascii_lowercase().replace('.', "-");
    for suffix in ["[1m]", "-thinking", "-latest"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped.to_string();
        }
    }
    if let Some(idx) = s.rfind('-') {
        let tail = &s[idx + 1..];
        if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
            s.truncate(idx);
        }
    }
    s
}

/// 查价。`prompt_tokens` 为本次请求的提示总量（普通输入 + 缓存写入 + 缓存读取），
/// 仅用于 Haiku 5.5 的分档。
pub fn price_for(model: &str, prompt_tokens: u64) -> Option<ModelPrice> {
    let p = match normalize_model(model).as_str() {
        "claude-fable-5-2" | "claude-mythos-5-2" => ModelPrice::new(8.0, 40.0, 0.20),
        "claude-fable-5-1" | "claude-mythos-5-1" => ModelPrice::new(10.0, 50.0, 0.25),
        "claude-fable-5" | "claude-mythos-5" => ModelPrice::new(10.0, 50.0, 1.00),
        "claude-opus-5-5" => ModelPrice::new(4.0, 20.0, 0.20),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6"
        | "claude-opus-4-5" => ModelPrice::new(5.0, 25.0, 0.50),
        "claude-opus-4-1" | "claude-opus-4" => ModelPrice::new(15.0, 75.0, 1.50),
        "claude-sonnet-5-5" => ModelPrice::new(2.0, 10.0, 0.10),
        "claude-sonnet-5" => ModelPrice::new(2.0, 10.0, 0.20),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" | "claude-sonnet-4" => {
            ModelPrice::new(3.0, 15.0, 0.30)
        }
        "claude-haiku-5-5" => {
            if prompt_tokens > HAIKU_5_5_LONG_PROMPT {
                ModelPrice::new(0.50, 2.50, 0.05)
            } else {
                ModelPrice::new(0.10, 0.50, 0.01)
            }
        }
        "claude-haiku-4-5" => ModelPrice::new(1.0, 5.0, 0.10),
        _ => return None,
    };
    Some(p)
}

/// 按标准 API 价格计算单次请求金额（美元）；模型未收录时返回 `None`。
pub fn standard_cost_usd(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
) -> Option<f64> {
    let prompt = input_tokens + cache_creation_tokens + cache_read_tokens;
    let p = price_for(model, prompt)?;
    let micro = input_tokens as f64 * p.input
        + output_tokens as f64 * p.output
        + cache_creation_tokens as f64 * p.cache_write()
        + cache_read_tokens as f64 * p.cache_read;
    Some(micro / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_request_model_names() {
        assert_eq!(normalize_model("claude-opus-5-5-thinking"), "claude-opus-5-5");
        assert_eq!(normalize_model("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
        assert_eq!(normalize_model("Claude-Sonnet-4.6[1m]"), "claude-sonnet-4-6");
    }

    #[test]
    fn unknown_models_are_unpriced() {
        assert!(standard_cost_usd("kimi-k3", 1, 1, 0, 0).is_none());
        assert!(standard_cost_usd("claude-opus-4-9", 1, 1, 0, 0).is_none());
    }

    #[test]
    fn computes_cost_with_cache_rates() {
        // Sonnet 5.5：1M 输入 $2 + 1M 输出 $10 + 1M 写入 $2.5 + 1M 读取 $0.1
        let c = standard_cost_usd("claude-sonnet-5-5", 1_000_000, 1_000_000, 1_000_000, 1_000_000)
            .unwrap();
        assert!((c - 14.6).abs() < 1e-9);
    }

    #[test]
    fn haiku_5_5_switches_tier_on_long_prompt() {
        let short = standard_cost_usd("claude-haiku-5-5", 100_000, 0, 0, 0).unwrap();
        let long = standard_cost_usd("claude-haiku-5-5", 100_001, 0, 0, 0).unwrap();
        assert!((short - 0.01).abs() < 1e-9);
        assert!((long - 100_001.0 * 0.5 / 1e6).abs() < 1e-9);
    }
}
