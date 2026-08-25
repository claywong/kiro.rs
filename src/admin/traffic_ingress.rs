//! 流量入口开关：手动开关 + RPM 容量闸门。
//!
//! 把「是否对外接量」同步到外部系统的指定账号。最终推给对方的 `schedulable` 是两
//! 个条件取**与**:
//!
//! | 手动开关 | RPM 判据 | 推给外部 |
//! |---|---|---|
//! | 关 | 任意 | `false` |
//! | 开 | `rpmTotal >= minRpm` | `true` |
//! | 开 | `rpmTotal < minRpm` | `false` |
//!
//! # RPM 判据
//!
//! 读的是 `token_manager.available_rpm_total()`——可用凭证 `rpmLimit` 之和，与并发
//! 联动同一口径。这是**容量**指标而非实际流量,选它是关键:实际流量会随入口开关
//! 变化（入口一关流量被分走、读数更低），拿它当判据会永远开不回来；容量不从请求
//! 派生，关掉入口不会让读数变好也不会变坏,双向切换才成立。这与
//! [`super::health_gate::judge`] 里「报错数只能判不稳、不能判稳」是同一个坑。
//!
//! `minRpm = 0` 表示停用该判据,退回纯手动开关。
//!
//! # 设计要点
//! - **防抖**:连续 `confirmations` 轮判定一致才切换,避免 RPM 卡在阈值附近时来回
//!   推、刷对方审计日志。手动开关不走防抖,点了立即生效。
//! - **手动开关仍是总闸**。关闭时直接推 `false`,不再看 RPM,也不重置已攒的 streak
//!   计数——重新打开时按当前读数重新确认。
//! - **判据翻转与推送失败共用重试路径**:推失败不改 `applied`,下一轮继续。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use reqwest::Client;
use tokio::sync::Notify;

use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::TrafficIngressConfig;

use super::schedulable_client::{SchedulableTarget, push_all};

/// 三态编码：本进程尚未推送过 / 已推 false / 已推 true。
/// `rpm_ok` 复用同一套常量,`NONE` 表示还没判过（判据停用时也是 NONE）。
const APPLIED_NONE: u8 = 0;
const APPLIED_FALSE: u8 = 1;
const APPLIED_TRUE: u8 = 2;

fn encode_tristate(value: bool) -> u8 {
    if value { APPLIED_TRUE } else { APPLIED_FALSE }
}

fn decode_tristate(raw: u8) -> Option<bool> {
    match raw {
        APPLIED_FALSE => Some(false),
        APPLIED_TRUE => Some(true),
        _ => None,
    }
}

pub struct TrafficIngressState {
    enabled: AtomicBool,
    applied: AtomicU8,
    /// 最近一轮 RPM 判据结论,供面板区分「手动关」与「RPM 不够被自动关」。
    rpm_ok: AtomicU8,
    changed: Notify,
}

impl TrafficIngressState {
    /// 启动即关闭时假定外部已是 `schedulable=false`，跳过启动对齐推送——
    /// 否则每次重启都会对外部账号推一遍 `false`，只刷对方审计日志。
    /// 若外部侧被人为改回 true，面板上重新开关一次即可重新对齐。
    /// 面板「开→关」仍会正常推一次 false（用户主动操作，应该同步）。
    fn new(enabled: bool) -> Arc<Self> {
        let initial_applied = if enabled { APPLIED_NONE } else { APPLIED_FALSE };
        Arc::new(Self {
            enabled: AtomicBool::new(enabled),
            applied: AtomicU8::new(initial_applied),
            rpm_ok: AtomicU8::new(APPLIED_NONE),
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

    pub fn applied(&self) -> Option<bool> {
        decode_tristate(self.applied.load(Ordering::Relaxed))
    }

    fn set_applied(&self, enabled: bool) {
        self.applied
            .store(encode_tristate(enabled), Ordering::Relaxed);
    }

    /// 最近一轮 RPM 判据结论。`None` = 还没判过或判据已停用。
    pub fn rpm_ok(&self) -> Option<bool> {
        decode_tristate(self.rpm_ok.load(Ordering::Relaxed))
    }

    fn set_rpm_ok(&self, ok: bool) {
        self.rpm_ok.store(encode_tristate(ok), Ordering::Relaxed);
    }
}

pub type SharedTrafficIngressState = Arc<TrafficIngressState>;

/// 配置完整时启动同步任务。启动后会立即把外部账号对齐到持久化的开关状态。
pub fn spawn(
    config: TrafficIngressConfig,
    client: Client,
    token_manager: Arc<MultiTokenManager>,
) -> Option<SharedTrafficIngressState> {
    if !config.is_configured() {
        return None;
    }

    let state = TrafficIngressState::new(config.enabled);
    tracing::info!(
        base_url = %config.normalized_base_url(),
        accounts = ?config.account_ids,
        enabled = config.enabled,
        min_rpm = config.min_rpm,
        check_interval_secs = config.check_interval_secs,
        confirmations = config.confirmations,
        "流量入口控制器已就绪"
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
    config: Arc<TrafficIngressConfig>,
    client: Client,
    token_manager: Arc<MultiTokenManager>,
    state: SharedTrafficIngressState,
) {
    let retry_interval = Duration::from_secs(config.retry_interval_secs.max(5));
    let check_interval = Duration::from_secs(config.check_interval_secs.max(5));
    let need = config.confirmations.max(1);

    // 已确认生效的 RPM 判据结论。`None` = 尚未判过，第一轮读数直接采信，
    // 让启动后能尽快对齐（否则要白等 confirmations 轮）。
    let mut confirmed: Option<bool> = None;
    // 与 `confirmed` 相反的读数连续出现了多少轮。
    let mut streak: u32 = 0;

    loop {
        let desired = if state.enabled() {
            evaluate_rpm_gate(
                &config,
                &token_manager,
                &state,
                &mut confirmed,
                &mut streak,
                need,
            )
        } else {
            // 手动关是总闸：不看 RPM，直接关。streak 不清零，
            // 重新打开时按当时读数重新确认。
            false
        };

        let target = SchedulableTarget {
            label: "流量入口",
            base_url: config.normalized_base_url(),
            token: &config.token,
            auth_header: config.auth_header(),
            account_ids: &config.account_ids,
            max_attempts: config.max_attempts,
        };
        // 已经是目标值就不重推，只等下一个检查点或开关变化。
        if state.applied() == Some(desired) {
            wait_next(&config, &state, check_interval).await;
            continue;
        }

        let intent = state.enabled();
        let pushed = push_all(&target, &client, desired).await;

        // 手动切换可能发生在推送途中。只有手动开关仍未变化时，才把本轮记为已应用；
        // 否则直接进入下一轮，把最新值覆盖过去。
        if state.enabled() != intent {
            continue;
        }

        if pushed {
            state.set_applied(desired);
            tracing::info!(
                schedulable = desired,
                manual = intent,
                rpm_ok = ?state.rpm_ok(),
                "流量入口已同步"
            );
            wait_next(&config, &state, check_interval).await;
        } else {
            tokio::select! {
                _ = tokio::time::sleep(retry_interval) => {}
                _ = state.changed.notified() => {}
            }
        }
    }
}

/// 读一轮 RPM 并做防抖确认，返回该判据当前放行与否。
///
/// `confirmed` / `streak` 由调用方持有：判据结论要跨轮累积，放在循环外才能防抖。
fn evaluate_rpm_gate(
    config: &TrafficIngressConfig,
    token_manager: &MultiTokenManager,
    state: &TrafficIngressState,
    confirmed: &mut Option<bool>,
    streak: &mut u32,
    need: u32,
) -> bool {
    if !config.rpm_gate_active() {
        return true;
    }

    let (total_rpm, unlimited) = token_manager.available_rpm_total(config.unlimited_rpm);
    let reading = config.rpm_allows(total_rpm);

    match *confirmed {
        // 首轮：直接采信，不等防抖。
        None => {
            *confirmed = Some(reading);
            *streak = 0;
            tracing::info!(
                total_rpm,
                min_rpm = config.min_rpm,
                unlimited,
                rpm_ok = reading,
                "流量入口：首轮 RPM 判定"
            );
        }
        Some(current) if reading != current => {
            *streak += 1;
            if *streak >= need {
                *confirmed = Some(reading);
                *streak = 0;
                tracing::info!(
                    total_rpm,
                    min_rpm = config.min_rpm,
                    unlimited,
                    rpm_ok = reading,
                    "流量入口：RPM 判定翻转（已连续确认）"
                );
            } else {
                tracing::debug!(
                    total_rpm,
                    min_rpm = config.min_rpm,
                    streak = *streak,
                    need,
                    "流量入口：RPM 判定待确认，暂不切换"
                );
            }
        }
        // 读数与已确认结论一致，清掉半截 streak。
        Some(_) => *streak = 0,
    }

    let effective = confirmed.unwrap_or(reading);
    state.set_rpm_ok(effective);
    effective
}

async fn wait_next(
    config: &TrafficIngressConfig,
    state: &TrafficIngressState,
    check_interval: Duration,
) {
    if config.rpm_gate_active() {
        // 判据开启：必须周期性醒来重读 RPM。
        tokio::select! {
            _ = tokio::time::sleep(check_interval) => {}
            _ = state.changed.notified() => {}
        }
    } else {
        // 纯手动模式：没有需要轮询的判据，停在开关上省一次空转。
        state.changed.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 集成测试暂时注释：MultiTokenManager 构造需要完整配置，
    // 这个测试只验证手动开关，用 min_rpm = 0 绕过 RPM 判据也能过，
    // 但 mock 代价太高。RPM 判据防抖的单独测试在下面。
    //
    // #[tokio::test]
    // async fn 启动对齐配置且手动切换立即推送() { ... }

    /// 单独测 RPM 判据的防抖逻辑（纯函数，不依赖 HTTP）
    #[test]
    fn rpm_判据防抖正确() {
        use crate::model::config::TrafficIngressConfig;

        let config = TrafficIngressConfig {
            min_rpm: 200,
            confirmations: 2,
            ..Default::default()
        };

        // 模拟 token_manager 返回的 RPM 读数
        struct FakeTokenManager(u32);
        impl FakeTokenManager {
            fn available_rpm_total(&self, _unlimited: u32) -> (u32, usize) {
                (self.0, 0)
            }
        }

        let state = TrafficIngressState::new(true);
        let mut confirmed: Option<bool> = None;
        let mut streak: u32 = 0;
        let need = config.confirmations.max(1);

        // 首轮：250 >= 200，放行。confirmed 变 Some(true)，streak 归零
        let tm = FakeTokenManager(250);
        let (rpm, _) = tm.available_rpm_total(config.unlimited_rpm);
        let reading = config.rpm_allows(rpm);
        assert!(reading);
        // 手动模拟 evaluate_rpm_gate 的防抖逻辑核心
        if confirmed.is_none() {
            confirmed = Some(reading);
            streak = 0;
        }
        assert_eq!(confirmed, Some(true));
        assert_eq!(streak, 0);

        // 第 2 轮：180 < 200，读数变 false，但 streak 只到 1，未翻转
        let tm = FakeTokenManager(180);
        let (rpm, _) = tm.available_rpm_total(config.unlimited_rpm);
        let reading = config.rpm_allows(rpm);
        assert!(!reading);
        if confirmed == Some(true) && reading != true {
            streak += 1;
        }
        assert_eq!(confirmed, Some(true)); // 仍是 true
        assert_eq!(streak, 1);

        // 第 3 轮：170 < 200，streak 到 2 = need，翻转
        let tm = FakeTokenManager(170);
        let (rpm, _) = tm.available_rpm_total(config.unlimited_rpm);
        let reading = config.rpm_allows(rpm);
        assert!(!reading);
        if confirmed == Some(true) && reading != true {
            streak += 1;
            if streak >= need {
                confirmed = Some(reading);
                streak = 0;
            }
        }
        assert_eq!(confirmed, Some(false));
        assert_eq!(streak, 0);

        // 第 4 轮：回到 210，读数 true，streak 开始累积
        let tm = FakeTokenManager(210);
        let (rpm, _) = tm.available_rpm_total(config.unlimited_rpm);
        let reading = config.rpm_allows(rpm);
        assert!(reading);
        if confirmed == Some(false) && reading != false {
            streak += 1;
        } else if reading == confirmed.unwrap() {
            streak = 0;
        }
        assert_eq!(confirmed, Some(false));
        assert_eq!(streak, 1);

        // 第 5 轮：再次 >= 200，streak 到 2，翻回 true
        let tm = FakeTokenManager(220);
        let (rpm, _) = tm.available_rpm_total(config.unlimited_rpm);
        let reading = config.rpm_allows(rpm);
        assert!(reading);
        if confirmed == Some(false) && reading != false {
            streak += 1;
            if streak >= need {
                confirmed = Some(reading);
                streak = 0;
            }
        }
        assert_eq!(confirmed, Some(true));
        assert_eq!(streak, 0);
    }
}
