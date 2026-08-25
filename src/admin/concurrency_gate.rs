//! 并发联动看门狗。
//!
//! 把本地「有效凭证的 RPM 总量」按固定除数换算成外部账号的并发上限并推过去：
//!
//! ```text
//! 并发 = 有效凭证 rpmLimit 之和 / divisor（默认 6），再夹到 [min, max]
//! ```
//!
//! 「有效」沿用 `MultiTokenManager::available_count` 的口径（未禁用且未被限流），
//! 保证面板上看到的可用数与这里的求和分母是同一批凭证。
//!
//! # 与另两个联动的关系
//! 健康联动、流量入口推的都是 `schedulable`（要不要接量），这里推的是 `concurrency`
//! （能接多少）。同一个外部账号被两边分别写这两个字段互不干扰。
//!
//! # 设计要点
//! - **目标值不变则不推**。并发是个数值而非开关，重复推同值只会刷对方审计日志。
//!   仍保留 `reaffirmIntervalSecs` 定期重推，纠正对方后台被手动改动造成的漂移
//!   （与健康联动同样的理由：只在变化时推，会漏掉「对方被改了而本地不知道」）。
//! - **手动值也受 [min, max] 夹取**。手动是「跳过换算」，不是「跳过安全边界」。
//! - **不限速凭证按 `unlimitedRpm` 折算**。本地 `rpmLimit=0` 表示不限速，直接求和
//!   会算成 0 反而低估容量。折算值是估值，命中时日志会带上计数。
//! - **关闭联动不回滚**。外部账号保留最后推上去的值，由人工决定要不要改回。这点与
//!   流量入口不同：那边关闭意味着「停止接量」有明确目标态，而并发没有天然的"关闭值"。
//! - 全流程失败只 warn，绝不影响主服务。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use reqwest::Client;
use tokio::sync::Notify;

use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::ConcurrencyGateConfig;

use super::concurrency_client::{ConcurrencyTarget, push_all};

/// `applied` 的哨兵值：尚未成功推送过任何值。真实并发永远 >= 1，不会与之撞车。
const APPLIED_NONE: u32 = u32::MAX;

pub struct ConcurrencyGateState {
    enabled: AtomicBool,
    /// 当前生效的除数，可运行时改。
    divisor: AtomicU32,
    /// 手动并发值；`APPLIED_NONE` 表示未设置，走自动换算。
    manual: AtomicU32,
    /// 最近一次成功推上去的并发值。
    applied: AtomicU32,
    changed: Notify,
}

impl ConcurrencyGateState {
    fn new(config: &ConcurrencyGateConfig) -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(config.enabled),
            divisor: AtomicU32::new(config.effective_divisor()),
            manual: AtomicU32::new(config.manual_concurrency.unwrap_or(APPLIED_NONE)),
            applied: AtomicU32::new(APPLIED_NONE),
            changed: Notify::new(),
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::Relaxed) != enabled {
            self.changed.notify_one();
        }
    }

    pub fn divisor(&self) -> u32 {
        self.divisor.load(Ordering::Relaxed)
    }

    /// 除数为 0 会导致除零，一律拒绝；调用方负责校验后再传入。
    pub fn set_divisor(&self, divisor: u32) {
        if divisor > 0 && self.divisor.swap(divisor, Ordering::Relaxed) != divisor {
            self.changed.notify_one();
        }
    }

    pub fn manual_concurrency(&self) -> Option<u32> {
        match self.manual.load(Ordering::Relaxed) {
            APPLIED_NONE => None,
            value => Some(value),
        }
    }

    /// `None` 表示清除手动值、回到自动换算。
    pub fn set_manual_concurrency(&self, manual: Option<u32>) {
        let encoded = manual.unwrap_or(APPLIED_NONE);
        if self.manual.swap(encoded, Ordering::Relaxed) != encoded {
            self.changed.notify_one();
        }
    }

    pub fn applied(&self) -> Option<u32> {
        match self.applied.load(Ordering::Relaxed) {
            APPLIED_NONE => None,
            value => Some(value),
        }
    }

}

pub type SharedConcurrencyGateState = Arc<ConcurrencyGateState>;

/// 配置完整时启动看门狗。返回的状态句柄供 admin 接口读写。
pub fn spawn(
    config: ConcurrencyGateConfig,
    client: Client,
    token_manager: Arc<MultiTokenManager>,
) -> Option<SharedConcurrencyGateState> {
    if !config.is_configured() {
        return None;
    }

    let state = ConcurrencyGateState::new(&config);
    tracing::info!(
        base_url = %config.normalized_base_url(),
        accounts = ?config.account_ids,
        enabled = config.enabled,
        divisor = config.effective_divisor(),
        manual = ?config.manual_concurrency,
        "并发联动控制器已就绪"
    );
    tokio::spawn(run(
        Arc::new(config),
        client,
        token_manager,
        Arc::clone(&state),
    ));
    Some(state)
}

async fn run(
    config: Arc<ConcurrencyGateConfig>,
    client: Client,
    token_manager: Arc<MultiTokenManager>,
    state: SharedConcurrencyGateState,
) {
    let check_interval = Duration::from_secs(config.check_interval_secs.max(5));
    let reaffirm_interval = Duration::from_secs(config.reaffirm_interval_secs.max(check_interval.as_secs()));
    let mut last_push = tokio::time::Instant::now();

    loop {
        if !state.enabled() {
            // 关闭时不推也不回滚，只等开关变化。
            state.changed.notified().await;
            continue;
        }

        let (total_rpm, unlimited) = token_manager.available_rpm_total(config.unlimited_rpm);
        let desired = resolve(&config, &state, total_rpm);
        let applied = state.applied();
        let due_for_reaffirm = last_push.elapsed() >= reaffirm_interval;

        if applied == Some(desired) && !due_for_reaffirm {
            wait_next(&state, check_interval).await;
            continue;
        }

        if unlimited > 0 {
            tracing::warn!(
                unlimited,
                unlimited_rpm = config.unlimited_rpm,
                total_rpm,
                "并发联动：有效凭证含不限速项，总量按折算值估算"
            );
        }

        let target = ConcurrencyTarget {
            label: "并发联动",
            base_url: config.normalized_base_url(),
            token: &config.token,
            auth_header: config.auth_header(),
            account_ids: &config.account_ids,
            max_attempts: config.max_attempts,
        };

        if push_all(&target, &client, desired).await {
            state.applied.store(desired, Ordering::Relaxed);
            last_push = tokio::time::Instant::now();
            tracing::info!(
                total_rpm,
                divisor = state.divisor(),
                manual = ?state.manual_concurrency(),
                concurrency = desired,
                from = ?applied,
                "并发联动已同步"
            );
        }

        wait_next(&state, check_interval).await;
    }
}

/// 换算当前该推的并发值。除数与手动值取运行时状态（可被 admin 接口改），
/// 上下限与不限速折算取配置。
fn resolve(config: &ConcurrencyGateConfig, state: &ConcurrencyGateState, total_rpm: u32) -> u32 {
    let effective = ConcurrencyGateConfig {
        divisor: state.divisor(),
        manual_concurrency: state.manual_concurrency(),
        ..config.clone()
    };
    effective.resolve_concurrency(total_rpm)
}

async fn wait_next(state: &ConcurrencyGateState, interval: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(interval) => {}
        _ = state.changed.notified() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ConcurrencyGateConfig {
        ConcurrencyGateConfig {
            divisor: 6,
            min_concurrency: 1,
            max_concurrency: 200,
            ..Default::default()
        }
    }

    #[test]
    fn 按除数向下取整() {
        let c = config();
        assert_eq!(c.resolve_concurrency(66), 11);
        // 68/6 = 11.33 → 11，向下取整不放大容量。
        assert_eq!(c.resolve_concurrency(68), 11);
        assert_eq!(c.resolve_concurrency(72), 12);
    }

    #[test]
    fn 算出零时夹到下限避免外部账号停摆() {
        let c = config();
        assert_eq!(c.resolve_concurrency(0), 1);
        assert_eq!(c.resolve_concurrency(5), 1);
    }

    #[test]
    fn 手动值跳过换算但仍受上下限约束() {
        let c = ConcurrencyGateConfig {
            manual_concurrency: Some(50),
            ..config()
        };
        // 与 total_rpm 无关。
        assert_eq!(c.resolve_concurrency(66), 50);
        assert_eq!(c.resolve_concurrency(0), 50);

        let capped = ConcurrencyGateConfig {
            manual_concurrency: Some(9999),
            ..config()
        };
        assert_eq!(capped.resolve_concurrency(66), 200);
    }

    /// 部署用的配置块必须能被反序列化出预期值。字段名写错（camelCase 拼错）在
    /// 运行时只会静默回落默认值——开关看着是关的，除数悄悄变回 6，很难察觉。
    #[test]
    fn 部署配置块能正确反序列化() {
        let json = r#"{
            "enabled": true,
            "baseUrl": "https://4code.us",
            "token": "t",
            "authHeader": "X-API-Key",
            "accountIds": [158],
            "divisor": 8,
            "unlimitedRpm": 300,
            "minConcurrency": 1,
            "maxConcurrency": 200,
            "checkIntervalSecs": 60,
            "reaffirmIntervalSecs": 300,
            "maxAttempts": 3
        }"#;
        let c: ConcurrencyGateConfig = serde_json::from_str(json).unwrap();
        assert!(c.enabled);
        assert!(c.is_configured());
        assert_eq!(c.account_ids, vec![158]);
        assert_eq!(c.divisor, 8);
        assert_eq!(c.unlimited_rpm, 300);
        assert_eq!(c.max_concurrency, 200);
        assert_eq!(c.auth_header(), "X-API-Key");
        assert_eq!(c.manual_concurrency, None, "未写该键应为自动换算");
        // 66 / 8 = 8.25 → 8
        assert_eq!(c.resolve_concurrency(66), 8);
    }

    /// 缺 token 或缺账号都不算配好，看门狗不该启动。
    #[test]
    fn 配置不全时不视为已配置() {
        let no_token = ConcurrencyGateConfig {
            account_ids: vec![158],
            ..config()
        };
        assert!(!no_token.is_configured(), "缺 token");

        let no_accounts = ConcurrencyGateConfig {
            token: "t".into(),
            ..config()
        };
        assert!(!no_accounts.is_configured(), "缺 accountIds");

        let ok = ConcurrencyGateConfig {
            token: "t".into(),
            account_ids: vec![158],
            ..config()
        };
        assert!(ok.is_configured());
    }

    #[test]
    fn 除数为零回落默认值不panic() {
        let c = ConcurrencyGateConfig {
            divisor: 0,
            ..config()
        };
        assert_eq!(c.effective_divisor(), 6);
        assert_eq!(c.resolve_concurrency(66), 11);
    }

    #[test]
    fn 上限小于下限时以下限为准() {
        let c = ConcurrencyGateConfig {
            min_concurrency: 10,
            max_concurrency: 5,
            ..config()
        };
        assert_eq!(c.resolve_concurrency(6), 10);
    }
}
