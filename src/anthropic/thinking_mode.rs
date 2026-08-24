//! Anthropic adaptive thinking 模型判定（本地特性）。
//!
//! 官方规则（`code.claude.com/docs/en/model-config`）：**Fable 5、Sonnet 5、
//! Opus 4.7 及更新的模型一律使用 adaptive reasoning，固定思考预算模式不适用**。
//! 只有 Opus 4.6 / Sonnet 4.6 还能退回 `budget_tokens` 模式。对更新的模型发
//! `thinking.budget_tokens` 会被 Anthropic 官方端点以 400 拒绝，报错提示
//! 「Use "thinking.type.adaptive" and "output_config.effort"」。
//!
//! 本项目上游是 Kiro / AWS Q 而非 Anthropic 官方端点，`budget_tokens` 不进 wire
//! （只由 `converter` 推导 effort），所以不会真的触发 400；此处判定的实际作用是让
//! `generate_thinking_prefix` 对这些模型走 `<thinking_effort>` 分支而不是语义已废弃的
//! `<max_thinking_length>` 分支。
//!
//! 独立成文件而非塞进 `handlers.rs` / `converter.rs`：模型协议兼容是上游迟早也会碰的
//! 领域（见 CLAUDE.md 第一条），隔离在这里让将来上游实现后可整体让位，而不必在
//! 上游大文件里拆解交织的改动。
//!
//! @author wangzhong

/// 按 backend_id 判断该模型是否只支持 adaptive thinking。
///
/// 传入的应是 [`crate::anthropic::converter::map_model`] 映射后的 backend_id
/// （形如 `claude-opus-4.7`），而非客户端原始别名——原始名形态太多
/// （`4-7` / `4.7` / `-thinking` 后缀 / 自定义别名），在其上做字符串匹配不可靠。
///
/// 判定规则刻意贴着**枚举**，不做跨族的版本外推：
///
/// - Claude 5 代全系（主版本 >= 5）→ true。整代都只有 adaptive。
/// - `opus` 主版本 4 且次版本 >= 6 → true。这里 4.6 与 4.7+ 的理由不同但结论一致：
///   - 4.7+ 是官方 adaptive-only（发 `budget_tokens` 会 400）；
///   - **4.6 是 Kiro 侧约束**——上游只在 adaptive 下接受 `output_config`，普通
///     `enabled` 会 400，故 `native_reasoning_requested` 对它硬要求 adaptive
///     （见 `converter.rs` 同名函数与 `test_enabled_thinking_does_not_emit_output_config_for_opus_4_6`）。
///     若这里把 4.6 判成 enabled，它的 effort 字段会彻底不下发，是功能回退。
/// - `fable` / `mythos` → true。这两族自 5 起才存在。
/// - 其余一律 false，典型：
///   - **Sonnet 4.6** 在 `enabled` 下就能带 `output_config`（见 converter 的
///     `enabled_thinking_emits_output_config_for_sonnet_4_6`），无须转 adaptive；
///   - 4.5 及更早只支持固定预算。
///
/// 无法解析的形态按 false 处理：宁可多注一个无用标签，也不要把本该带预算的模型
/// 误判成 adaptive。
pub fn backend_requires_adaptive_thinking(backend_id: &str) -> bool {
    let id = backend_id.trim().to_ascii_lowercase();
    let Some(body) = id.strip_prefix("claude-") else {
        return false;
    };

    // fable / mythos 自 5 起才存在，无固定预算模式。
    for family in ["fable", "mythos"] {
        if body.starts_with(family) {
            return true;
        }
    }

    for family in ["opus", "sonnet"] {
        if let Some(rest) = body.strip_prefix(family) {
            let (major, minor) = parse_version(rest.trim_start_matches(['-', '.']));
            let Some(major) = major else {
                return false;
            };
            // Claude 5 代全系只有 adaptive，与族无关。
            if major >= 5 {
                return true;
            }
            // 门槛只对 opus 成立；sonnet 4.x 一律走固定预算（4.6 在 enabled 下即可
            // 带 output_config）。opus 取 >= 4.6 而非 4.7，见函数文档。
            return family == "opus" && major == 4 && minor.is_some_and(|m| m >= 6);
        }
    }

    false
}

/// 解析 `4.7` / `5` / `4-7` 等形态为 (主版本, 次版本)。次版本缺失时为 None。
fn parse_version(version: &str) -> (Option<u32>, Option<u32>) {
    let mut parts = version.split(['.', '-']);
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let minor = parts.next().and_then(|p| p.parse::<u32>().ok());
    (major, minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_only_models_are_detected() {
        for id in [
            "claude-opus-4.7",
            "claude-opus-4.8",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-mythos-5",
        ] {
            assert!(
                backend_requires_adaptive_thinking(id),
                "{id} 应判定为仅支持 adaptive"
            );
        }
    }

    #[test]
    fn fixed_budget_models_are_excluded() {
        // Sonnet 4.6 在 enabled 下即可带 output_config，无须转 adaptive；
        // 4.5 及更早只支持固定预算。
        for id in [
            "claude-sonnet-4.6",
            "claude-opus-4.5",
            "claude-sonnet-4.5",
            "claude-haiku-4.5",
        ] {
            assert!(
                !backend_requires_adaptive_thinking(id),
                "{id} 不应判定为仅支持 adaptive"
            );
        }
    }

    /// Opus 4.6 必须判为 adaptive——这是 Kiro 侧约束，不是官方 400 名单的一部分。
    ///
    /// 上游 `native_reasoning_requested` 对 opus 4.6 硬要求 adaptive，普通 enabled 下
    /// `output_config` 根本不下发。判成 enabled 会静默丢掉它的 effort 字段。
    #[test]
    fn opus_4_6_requires_adaptive_due_to_kiro_constraint() {
        assert!(
            backend_requires_adaptive_thinking("claude-opus-4.6"),
            "opus 4.6 只在 adaptive 下接受 output_config，必须判为 adaptive"
        );
    }

    #[test]
    fn non_claude_models_are_excluded() {
        // GPT / 其他厂商模型走各自的 reasoning 字段，不参与 Anthropic thinking 模式判定。
        for id in ["gpt-5.6-sol", "gpt-5.6-luna", "deepseek-3.2", "", "claude-"] {
            assert!(
                !backend_requires_adaptive_thinking(id),
                "{id:?} 不应判定为仅支持 adaptive"
            );
        }
    }

    /// 4.x 的版本门槛只对 opus 成立，不可外推到 sonnet。
    ///
    /// 同为 4.6：opus 必须 adaptive（Kiro 侧只在 adaptive 下接受 output_config），
    /// sonnet 则在 enabled 下就能带 output_config，两者结论相反。
    #[test]
    fn version_threshold_does_not_generalize_across_families() {
        assert!(
            backend_requires_adaptive_thinking("claude-opus-4.6"),
            "opus 4.6 应判为 adaptive"
        );
        assert!(
            !backend_requires_adaptive_thinking("claude-sonnet-4.6"),
            "同为 4.6，sonnet 不应跟着 opus 判为 adaptive"
        );
        // 但 5 代全系与族无关，一律 adaptive。
        assert!(backend_requires_adaptive_thinking("claude-sonnet-5"));
        assert!(backend_requires_adaptive_thinking("claude-opus-5"));
    }

    #[test]
    fn version_parsing_is_lenient_about_separators() {
        // backend_id 正常是点号，但容忍连字符形态，避免上游改规范化格式时静默失效。
        assert!(backend_requires_adaptive_thinking("claude-opus-4-7"));
        assert!(backend_requires_adaptive_thinking("claude-opus-4-6"));
        assert!(!backend_requires_adaptive_thinking("claude-opus-4-5"));
        // 大小写与空白容错。
        assert!(backend_requires_adaptive_thinking("  Claude-Opus-5  "));
    }
}
