//! 自动提取模式的判定规则（纯函数，不碰 IO）
//!
//! 自动模式的授权链条是：收到 `all_keys_dead` → 在观察窗口内反复盘点本地凭据池，
//! 确认名下卖家 Key 确已全部失效 → 下一轮 `new_keys_available` 才允许自动扣费，
//! 且只提最小数量。任何一环给不出肯定结论，就退回手动。
//!
//! 上述链条依赖卖家持续推送 `all_keys_dead`，而实测**并非每家都推**（Drop 家只在
//! 最初推过一次，此后 60+ 次新货通知期间再没推过）。失效确认是一次性额度，用后即
//! 废，所以这类卖家在消费掉第一张额度后自动提取会永久死锁。故 [`decide_authorization`]
//! 补了一条兜底：拿不到可用额度时就地盘点本家凭据，无存活即放行。该兜底不消费额度、
//! 会反复成立，因此**必须**由全局池闸兜住上限，池闸未启用时不走这条路。
//!
//! 判定只读本地凭据状态（`disabled` / `disabled_reason` / `failure_count`），
//! 不打上游探活 —— 本地状态本就是真实请求失败累积出来的，够用且零成本。
//!
//! @author wangzhong

use std::time::Duration;

use super::store::ValidationStatus;

/// 观察窗口内的重查间隔
pub const VALIDATION_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// 观察窗口总长。卖家说"全部失效"的那一刻，本地往往还没探到 —— 本地的
/// `disabled` / `failure_count` 要靠真实请求打上去失败才累积，故需要留出观察时间。
pub const VALIDATION_WINDOW: Duration = Duration::from_secs(180);

/// 提取入库时写入的 `source_channel` 前缀（见 `service.rs` 的 `import_keys`）
pub const VENDOR_CHANNEL_PREFIX: &str = "vendor:";

/// 本地连续失败自动禁用阈值，与 `token_manager` 的 `MAX_FAILURES_PER_CREDENTIAL`
/// 对齐。达到该值的凭据必然已被置为 disabled，这里再判一次是防御性的。
const LOCAL_FAILURE_THRESHOLD: u32 = 3;

/// 判定所需的凭据状态切片。
///
/// 刻意不直接用 `CredentialEntrySnapshot` —— 判定规则只关心这四个字段，
/// 用自带类型可让规则脱离凭据池独立测试，上游快照加减字段也不波及此处。
#[derive(Debug, Clone, Default)]
pub struct VendorKeyState {
    pub source_channel: Option<String>,
    pub disabled: bool,
    pub disabled_reason: Option<String>,
    pub failure_count: u32,
}

/// 单张卖家 Key 的健康归类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyHealth {
    /// 还能用
    Alive,
    /// 确认失效（因失败被自动禁用）
    Dead,
    /// 说不清：被人工禁用或禁用未记原因。它不反映 Key 本身状态，不能单独当作
    /// 失效证据；但它同样不可用，故不阻塞「已无可用 Key」的结论（见 [`conclude`]）
    Ambiguous,
}

/// 本地凭据池中卖家 Key 的盘点结果
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VendorKeyCensus {
    pub total: usize,
    pub alive: usize,
    pub dead: usize,
    pub ambiguous: usize,
}

/// 归类单条凭据。
///
/// 人工禁用（`Manual`）单独归为 [`KeyHealth::Ambiguous`]：用户在面板上手动停用
/// 一张还能用的 Key 是常见操作，把它算作失效会让自动模式凭一个假信号去扣费。
fn classify(entry: &VendorKeyState) -> KeyHealth {
    if entry.disabled {
        return match entry.disabled_reason.as_deref() {
            Some("Manual") => KeyHealth::Ambiguous,
            // 其余禁用原因均由上游拒绝或刷新失败触发，是失效的直接证据
            Some(_) => KeyHealth::Dead,
            // 禁用但没记原因：无从判断，不当作证据
            None => KeyHealth::Ambiguous,
        };
    }
    if entry.failure_count >= LOCAL_FAILURE_THRESHOLD {
        return KeyHealth::Dead;
    }
    KeyHealth::Alive
}

/// 从 `source_channel` 解出供应商 id。
///
/// 两种历史格式都要认：
/// - 多供应商（当前）：`vendor:{vendor_id}:{order_id}`
/// - 单供应商（存量）：`vendor:{order_id}` —— 归属默认供应商
///
/// 非卖家渠道返回 None。存量格式的 order_id 是 32 位十六进制，而供应商 id
/// 由用户自取，故靠「是否还有第二个冒号」区分，不靠内容猜。
pub fn vendor_id_of(source_channel: &str) -> Option<&str> {
    let rest = source_channel.strip_prefix(VENDOR_CHANNEL_PREFIX)?;
    match rest.split_once(':') {
        // vendor:{vendor_id}:{order_id}
        Some((vid, _)) if !vid.is_empty() => Some(vid),
        // vendor:{order_id} —— 单供应商时期写入的，归默认那一家
        _ => Some(crate::model::config::DEFAULT_VENDOR_ID),
    }
}

/// 盘点凭据池里属于**指定供应商**的 Key。
///
/// 两层筛选：
/// 1. `source_channel` 带 `vendor:` 前缀 —— 自建或其他渠道导入的凭据
///    与卖家轮换无关，不该影响补货判断。
/// 2. 前缀里的供应商 id 等于 `vendor_id` —— 多供应商下，A 家推来「全部失效」时
///    若把 B 家健康的 Key 也算进来，就永远得不出「已无可用 Key」的结论，
///    A 家的自动补货会被 B 家挡死。
pub fn census(entries: &[VendorKeyState], vendor_id: &str) -> VendorKeyCensus {
    let mut c = VendorKeyCensus::default();
    for entry in entries.iter().filter(|e| {
        e.source_channel
            .as_deref()
            .and_then(vendor_id_of)
            .is_some_and(|vid| vid == vendor_id)
    }) {
        c.total += 1;
        match classify(entry) {
            KeyHealth::Alive => c.alive += 1,
            KeyHealth::Dead => c.dead += 1,
            KeyHealth::Ambiguous => c.ambiguous += 1,
        }
    }
    c
}

/// 盘点池里**所有卖家来源**的存活 Key，不分供应商。
///
/// 与 [`census`] 的分工：`census` 回答「本家的 Key 是不是都死了」，只看本家；
/// 本函数回答「池子整体还够不够用」，看所有 `vendor:` 来源。
///
/// 多家同时收到「全部失效」时，各家的 `census` 都会得出 `alive == 0`（按设计
/// 互不可见），于是三家各提一份。全局池闸靠本函数补上那个缺失的视图。
///
/// 仍然排除非 `vendor:` 来源：自建或其他渠道导入的凭据与卖家补货无关，
/// 把它们算进来会让池闸凭无关凭据挡掉真正需要的补货。
///
/// 只数 [`KeyHealth::Alive`]：`Ambiguous`（人工禁用 / 禁用未记原因）当下不可用，
/// 记进「够用」会让池子实际枯竭时仍不补货。
pub fn pool_alive(entries: &[VendorKeyState]) -> u32 {
    entries
        .iter()
        .filter(|e| {
            e.source_channel
                .as_deref()
                .and_then(vendor_id_of)
                .is_some()
        })
        .filter(|e| classify(e) == KeyHealth::Alive)
        .count() as u32
}

/// 由盘点结果得出确认结论。
///
/// `window_expired` 为 false 时（观察窗口内），只有"确认全部失效"是终态，
/// 其余一律 [`ValidationStatus::Pending`] 继续观察 —— 卖家刚说失效时本地通常
/// 还没探到，此刻的"仍然健康"不是结论。
///
/// 待定（人工禁用 / 禁用未记原因）不阻塞结论：这类 Key 同样处于不可用状态，
/// 拿不到它"是否真失效"的证据不代表它能用。因此判据只看有无存活的 Key —— 一旦
/// `alive == 0` 即刻确认，不必等窗口走完：观察窗口的意义是等本地失败计数追上现实，
/// 而"已无可用 Key"这个结论不依赖失效证据，再等也不会更成立。
pub fn conclude(c: VendorKeyCensus, window_expired: bool) -> (ValidationStatus, String) {
    // 池里没有卖家 Key：没有任何存活的东西，补货的前提天然成立
    if c.total == 0 {
        return (
            ValidationStatus::ConfirmedDead,
            "本地没有来自卖家的凭据，无需确认".to_string(),
        );
    }

    if c.alive == 0 {
        let detail = if c.ambiguous == 0 {
            format!("{} 张卖家 Key 均已失效", c.dead)
        } else {
            format!(
                "已无可用卖家 Key：失效 {} / 人工禁用或无禁用原因 {}（共 {}）",
                c.dead, c.ambiguous, c.total
            )
        };
        return (ValidationStatus::ConfirmedDead, detail);
    }

    // 仍有存活的 Key：窗口内继续观察，等本地状态追上卖家的说法
    if !window_expired {
        return (
            ValidationStatus::Pending,
            format!(
                "观察中：存活 {} / 失效 {} / 待定 {}（共 {}）",
                c.alive, c.dead, c.ambiguous, c.total
            ),
        );
    }

    (
        ValidationStatus::StillAlive,
        format!(
            "观察窗口结束仍有 {} 张卖家 Key 健康（共 {}），未触发自动提取",
            c.alive, c.total
        ),
    )
}

/// 卖家失效确认在本地的落库状态，供 [`decide_authorization`] 判定。
///
/// 刻意不直接收 `VendorEventRecord`：判定只需要这三样，收整行会把纯函数
/// 绑到存储结构上，加个无关字段就得改这里。
pub struct DeadEventVerdict {
    /// 确认结论。None 表示尚未写入结论（观察还没跑过）。
    pub status: Option<ValidationStatus>,
    /// 额度是否已被此前的自动提取取用
    pub used: bool,
    /// 结论详情，用于拼错误原因
    pub detail: Option<String>,
}

/// 授权判定结果
#[derive(Debug, PartialEq, Eq)]
pub enum AuthDecision {
    /// 用卖家的失效额度。一次性，调用方**必须**消费掉。
    DeadEvent,
    /// 就地盘点通过。不消费任何额度，上限全靠池闸。
    LocalCensus { detail: String },
    /// 无授权，本轮不提取
    Denied { reason: String },
}

/// 定下自动提取的授权来源。
///
/// 优先认卖家推来的失效额度；拿不到可用额度时，改用就地盘点的结论兜底 ——
/// 有的卖家并不按文档持续推 `all_keys_dead`（实测 Drop 家只在最初推过一次，
/// 此后 60+ 次新货通知期间再没推过），只认卖家事件会让这些家的自动提取在
/// 消费掉第一张额度后**永久死锁**：额度作废了，而补发额度的事件永远不来。
///
/// `pool_gate_enabled` 是兜底路径的**前置条件**，这是有意的联锁：就地盘点不
/// 消费额度，只要本家无存活 Key 就会反复成立，唯一的上限是池闸。若池闸没开还
/// 放行，等于每条新货通知都下一单且没有任何刹车。故池闸未启用时维持原行为 ——
/// 只认卖家事件，宁可不自动提取，也不留一条无上限的扣费路径。
pub fn decide_authorization(
    dead: Option<&DeadEventVerdict>,
    pool_gate_enabled: bool,
    census: VendorKeyCensus,
) -> AuthDecision {
    // 卖家给了可用额度就直接用。其余情形都记下原因转入兜底 ——
    // 兜底判据是当下盘点出来的，比这个落库结论更新，由它给最终答案。
    let vendor_verdict = match dead {
        None => "尚未收到「全部失效」事件".to_string(),
        Some(d) => match d.status {
            Some(ValidationStatus::ConfirmedDead) if !d.used => return AuthDecision::DeadEvent,
            Some(ValidationStatus::ConfirmedDead) => {
                "上一次失效确认已用于此前的自动提取".to_string()
            }
            Some(ValidationStatus::Pending) => "失效确认仍在观察中".to_string(),
            Some(ValidationStatus::StillAlive) => d
                .detail
                .clone()
                .unwrap_or_else(|| "本地仍有健康的卖家 Key".to_string()),
            Some(ValidationStatus::Inconclusive) | None => d
                .detail
                .clone()
                .unwrap_or_else(|| "旧 Key 是否失效无法确认".to_string()),
        },
    };

    if !pool_gate_enabled {
        return AuthDecision::Denied {
            reason: format!(
                "{vendor_verdict}，且未启用全局提取限制（autoPurchasePoolTarget=0），\
                 不做就地盘点 —— 该兜底不消费额度，需池闸兜住上限"
            ),
        };
    }

    // window_expired 取 true：这里要的是当下的终局结论，不是观察窗口里的中间态。
    // 传 false 会把「仍有存活」记成待定，而此刻没有后续轮次会来复查它。
    let (status, detail) = conclude(census, true);
    if status == ValidationStatus::ConfirmedDead {
        return AuthDecision::LocalCensus { detail };
    }
    AuthDecision::Denied {
        reason: format!("{vendor_verdict}；就地盘点结论: {detail}"),
    }
}

/// 自动模式的提取数量：三者取最小。
///
/// 数量一旦提交就与订单号永久绑定、无法改小，自动模式没有人工复核，
/// 因此宁可少提 —— 少提还能再手动补，多提是永久的。
///
/// `new_keys` 为 None 时按「卖家上限」参与取小，而不是按 0。有的卖家（Drop 家的
/// `batch.completed`）只说「新一批已上架」不说几张，按 0 算会让自动模式永远提不出
/// 东西；而缺这个数并不意味着没货 —— 真实上限由**刚查到的** `stock_max` 与配置
/// 上限共同兜着，两者都不受本函数影响，所以这里放宽不会导致超量提取。
pub fn decide_count(new_keys: Option<u32>, stock_max: u32, configured_max: u32) -> u32 {
    new_keys.unwrap_or(stock_max).min(stock_max).min(configured_max)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 存量测试用例都写的 `vendor:abc` 形式（单供应商时期格式），归默认那一家
    const DEFAULT: &str = crate::model::config::DEFAULT_VENDOR_ID;

    fn entry(
        channel: Option<&str>,
        disabled: bool,
        reason: Option<&str>,
        failures: u32,
    ) -> VendorKeyState {
        VendorKeyState {
            source_channel: channel.map(String::from),
            disabled,
            disabled_reason: reason.map(String::from),
            failure_count: failures,
        }
    }

    // ============ 授权判定 ============

    /// 空池的盘点结果 —— 对应「本地没有该家的凭据」，conclude 直接给 ConfirmedDead
    fn 空池() -> VendorKeyCensus {
        census(&[], DEFAULT)
    }

    /// 有一张健康 Key 的盘点结果
    fn 有存活() -> VendorKeyCensus {
        census(&[entry(Some("vendor:abc"), false, None, 0)], DEFAULT)
    }

    fn verdict(status: Option<ValidationStatus>, used: bool) -> DeadEventVerdict {
        DeadEventVerdict {
            status,
            used,
            detail: None,
        }
    }

    #[test]
    fn 授权_卖家额度可用时优先用它() {
        let v = verdict(Some(ValidationStatus::ConfirmedDead), false);
        // 即便池闸没开、即便本地仍有存活 Key，卖家的未消费额度依然作数 ——
        // 这是原有行为，不能因为加了兜底而改变
        assert_eq!(
            decide_authorization(Some(&v), false, 有存活()),
            AuthDecision::DeadEvent
        );
        assert_eq!(
            decide_authorization(Some(&v), true, 有存活()),
            AuthDecision::DeadEvent
        );
    }

    /// 这是 Drop 家的实际处境：额度用光了，而补发额度的事件永远不来
    #[test]
    fn 授权_额度已消费时就地盘点接手() {
        let v = verdict(Some(ValidationStatus::ConfirmedDead), true);
        match decide_authorization(Some(&v), true, 空池()) {
            AuthDecision::LocalCensus { detail } => {
                assert!(detail.contains("没有来自卖家的凭据"), "detail={detail}");
            }
            other => panic!("本地无存活 Key 时该由就地盘点授权，实际: {other:?}"),
        }
    }

    /// 从未推过失效事件的卖家，同样该能走兜底
    #[test]
    fn 授权_从无失效事件时就地盘点接手() {
        match decide_authorization(None, true, 空池()) {
            AuthDecision::LocalCensus { .. } => {}
            other => panic!("无事件也该能兜底授权，实际: {other:?}"),
        }
    }

    /// 联锁：兜底不消费额度，池闸没开就不放行
    #[test]
    fn 授权_池闸未启用时不走兜底() {
        let v = verdict(Some(ValidationStatus::ConfirmedDead), true);
        match decide_authorization(Some(&v), false, 空池()) {
            AuthDecision::Denied { reason } => {
                assert!(reason.contains("autoPurchasePoolTarget"), "reason={reason}");
                assert!(reason.contains("已用于此前"), "应带上卖家侧原因: {reason}");
            }
            other => panic!("池闸未启用时必须拒绝，否则是一条无上限的扣费路径: {other:?}"),
        }
        // 从无事件 + 池闸未开，同样拒绝
        match decide_authorization(None, false, 空池()) {
            AuthDecision::Denied { .. } => {}
            other => panic!("池闸未启用时必须拒绝，实际: {other:?}"),
        }
    }

    /// 本地还有健康 Key 时不该补货 —— 兜底判据必须能拒绝，不能只会放行
    #[test]
    fn 授权_本地仍有存活时拒绝() {
        let v = verdict(Some(ValidationStatus::ConfirmedDead), true);
        match decide_authorization(Some(&v), true, 有存活()) {
            AuthDecision::Denied { reason } => {
                assert!(reason.contains("就地盘点结论"), "reason={reason}");
            }
            other => panic!("仍有存活 Key 时必须拒绝，实际: {other:?}"),
        }
    }

    /// 观察窗口内（Pending）拿不到额度，但兜底给的是当下的终局结论：
    /// 此刻已无存活就直接放行，不必等窗口走完 —— 那个窗口的结论也只会是同一个。
    #[test]
    fn 授权_观察中但已无存活则兜底放行() {
        let v = verdict(Some(ValidationStatus::Pending), false);
        match decide_authorization(Some(&v), true, 空池()) {
            AuthDecision::LocalCensus { .. } => {}
            other => panic!("已无存活时该放行，实际: {other:?}"),
        }
        // 观察中且仍有存活 → 拒绝，且原因里保留「观察中」这个上下文
        match decide_authorization(Some(&v), true, 有存活()) {
            AuthDecision::Denied { reason } => {
                assert!(reason.contains("观察中"), "reason={reason}");
            }
            other => panic!("仍有存活时该拒绝，实际: {other:?}"),
        }
    }

    /// 结论为 StillAlive / 未写入结论时，卖家侧原因要透出来，便于面板显示
    #[test]
    fn 授权_拒绝原因带上卖家侧结论() {
        let v = DeadEventVerdict {
            status: Some(ValidationStatus::StillAlive),
            used: false,
            detail: Some("窗口结束仍有 2 张健康".to_string()),
        };
        match decide_authorization(Some(&v), true, 有存活()) {
            AuthDecision::Denied { reason } => {
                assert!(reason.contains("窗口结束仍有 2 张健康"), "reason={reason}");
            }
            other => panic!("实际: {other:?}"),
        }
    }

    #[test]
    fn 只统计卖家来源的凭据() {
        let entries = vec![
            entry(Some("vendor:abc"), true, Some("TooManyFailures"), 3),
            entry(Some("手动导入"), false, None, 0),
            entry(None, false, None, 0),
        ];
        let c = census(&entries, DEFAULT);
        assert_eq!(c.total, 1);
        assert_eq!(c.dead, 1);
        assert_eq!(c.alive, 0);
    }

    #[test]
    fn 人工禁用归为待定而非失效() {
        let entries = vec![entry(Some("vendor:abc"), true, Some("Manual"), 0)];
        let c = census(&entries, DEFAULT);
        assert_eq!(c.ambiguous, 1);
        assert_eq!(c.dead, 0);
        // 待定 Key 同样不可用，不阻塞确认，且无需等窗口走完
        let (status, detail) = conclude(c, false);
        assert_eq!(status, ValidationStatus::ConfirmedDead);
        assert!(detail.contains("已无可用卖家 Key"));
        assert_eq!(conclude(c, true).0, ValidationStatus::ConfirmedDead);
    }

    #[test]
    fn 禁用但无原因窗口结束后也确认() {
        let entries = vec![
            entry(Some("vendor:a"), true, None, 0),
            entry(Some("vendor:b"), true, Some("Manual"), 0),
            entry(Some("vendor:c"), true, Some("TooManyFailures"), 3),
        ];
        let c = census(&entries, DEFAULT);
        assert_eq!(c.ambiguous, 2);
        assert_eq!(c.dead, 1);
        assert_eq!(c.alive, 0);
        // 首次盘点即确认，不必等 3 分钟窗口
        assert_eq!(conclude(c, false).0, ValidationStatus::ConfirmedDead);
        assert_eq!(conclude(c, true).0, ValidationStatus::ConfirmedDead);
    }

    #[test]
    fn 有存活时待定不影响仍然健康的结论() {
        let entries = vec![
            entry(Some("vendor:a"), true, Some("Manual"), 0),
            entry(Some("vendor:b"), false, None, 0),
        ];
        let c = census(&entries, DEFAULT);
        assert_eq!(conclude(c, true).0, ValidationStatus::StillAlive);
    }

    #[test]
    fn 全部失效才确认() {
        let entries = vec![
            entry(Some("vendor:a"), true, Some("TooManyFailures"), 3),
            entry(Some("vendor:b"), true, Some("InvalidRefreshToken"), 0),
        ];
        let (status, detail) = conclude(census(&entries, DEFAULT), false);
        assert_eq!(status, ValidationStatus::ConfirmedDead);
        assert!(detail.contains('2'));
    }

    #[test]
    fn 窗口内仍有健康的记为观察中() {
        let entries = vec![
            entry(Some("vendor:a"), true, Some("TooManyFailures"), 3),
            entry(Some("vendor:b"), false, None, 0),
        ];
        let c = census(&entries, DEFAULT);
        assert_eq!(conclude(c, false).0, ValidationStatus::Pending);
        // 窗口结束才落定为「仍然健康」
        assert_eq!(conclude(c, true).0, ValidationStatus::StillAlive);
    }

    #[test]
    fn 未达阈值的失败仍算健康() {
        let entries = vec![entry(Some("vendor:a"), false, None, 2)];
        assert_eq!(census(&entries, DEFAULT).alive, 1);
        let entries = vec![entry(Some("vendor:a"), false, None, 3)];
        assert_eq!(census(&entries, DEFAULT).dead, 1);
    }

    #[test]
    fn 池里没有卖家key时直接确认() {
        let (status, _) = conclude(census(&[], DEFAULT), false);
        assert_eq!(status, ValidationStatus::ConfirmedDead);
    }

    #[test]
    fn 提取数量取三者最小() {
        assert_eq!(decide_count(Some(10), 5, 1), 1);
        assert_eq!(decide_count(Some(10), 0, 3), 0);
        assert_eq!(decide_count(Some(2), 5, 3), 2);
    }

    /// 事件不带数量时按卖家上限参与取小，不按 0 —— 否则 Drop 家的
    /// `batch.completed`（只说上架、不说几张）会让自动模式永远提不出东西。
    #[test]
    fn 事件缺数量时按卖家上限取小() {
        // 卖家还有 5 个，配置上限 3 → 提 3
        assert_eq!(decide_count(None, 5, 3), 3);
        // 配置上限比库存大 → 收敛到库存
        assert_eq!(decide_count(None, 2, 10), 2);
        // 真没货时仍是 0，放宽不会凭空造出提取
        assert_eq!(decide_count(None, 0, 10), 0);
    }

    // ============ 多供应商归属 ============

    #[test]
    fn 解析来源渠道的供应商归属() {
        // 多供应商格式
        assert_eq!(vendor_id_of("vendor:kiroapp:abc123"), Some("kiroapp"));
        // 单供应商存量格式 → 默认那一家
        assert_eq!(vendor_id_of("vendor:abc123"), Some(DEFAULT));
        // 非卖家渠道
        assert_eq!(vendor_id_of("手动导入"), None);
        assert_eq!(vendor_id_of(""), None);
    }

    /// 多供应商下最关键的一条：A 家的盘点不能把 B 家的 Key 算进来，
    /// 否则 B 家健康时 A 家永远得不出「已无可用 Key」，自动补货被挡死。
    #[test]
    fn 盘点只算本家的key() {
        let entries = vec![
            // A 家两张全失效
            entry(Some("vendor:a:o1"), true, Some("TooManyFailures"), 3),
            entry(Some("vendor:a:o2"), true, Some("InvalidRefreshToken"), 0),
            // B 家一张健康
            entry(Some("vendor:b:o3"), false, None, 0),
        ];

        let a = census(&entries, "a");
        assert_eq!(a.total, 2);
        assert_eq!(a.dead, 2);
        assert_eq!(a.alive, 0);
        // A 家可以确认失效，不受 B 家健康 Key 影响
        assert_eq!(conclude(a, false).0, ValidationStatus::ConfirmedDead);

        let b = census(&entries, "b");
        assert_eq!(b.total, 1);
        assert_eq!(b.alive, 1);
        assert_eq!(conclude(b, true).0, ValidationStatus::StillAlive);
    }

    #[test]
    fn 存量渠道归属默认供应商() {
        let entries = vec![
            // 单供应商时期写入的，无供应商段
            entry(Some("vendor:oldorder"), false, None, 0),
            entry(Some("vendor:kiroapp:new"), false, None, 0),
        ];
        assert_eq!(census(&entries, DEFAULT).total, 1);
        assert_eq!(census(&entries, "kiroapp").total, 1);
    }

    #[test]
    fn 未配置的供应商盘点为空() {
        let entries = vec![entry(Some("vendor:a:o1"), false, None, 0)];
        let c = census(&entries, "不存在的家");
        assert_eq!(c.total, 0);
        // 池里没有本家的 Key → 补货前提天然成立
        assert_eq!(conclude(c, false).0, ValidationStatus::ConfirmedDead);
    }

    /// 池闸的核心用途：三家各自 census 都是 0，但池子整体不空
    #[test]
    fn 全局盘点跨供应商累计() {
        let entries = vec![
            entry(Some("vendor:a:o1"), false, None, 0),
            entry(Some("vendor:b:o2"), false, None, 0),
            entry(Some("vendor:c:o3"), false, None, 0),
        ];
        // 每家自己看都只有 1 张，且都活着
        assert_eq!(census(&entries, "a").alive, 1);
        assert_eq!(pool_alive(&entries), 3, "池闸要看到三家的总量");
    }

    #[test]
    fn 全局盘点排除非卖家来源() {
        let entries = vec![
            entry(Some("vendor:a:o1"), false, None, 0),
            entry(Some("手动导入"), false, None, 0),
            entry(None, false, None, 0),
        ];
        assert_eq!(pool_alive(&entries), 1, "自建渠道的 Key 不能算进池闸");
    }

    /// 待定态（人工禁用 / 禁用未记原因）当下不可用，不能记进「够用」
    #[test]
    fn 全局盘点只数存活() {
        let entries = vec![
            entry(Some("vendor:a:o1"), false, None, 0),
            entry(Some("vendor:a:o2"), true, Some("TooManyFailures"), 3),
            entry(Some("vendor:b:o3"), true, Some("Manual"), 0),
            entry(Some("vendor:b:o4"), true, None, 0),
            entry(Some("vendor:c:o5"), false, None, LOCAL_FAILURE_THRESHOLD),
        ];
        assert_eq!(pool_alive(&entries), 1, "失效与待定都不算存活");
    }

    #[test]
    fn 空池全局盘点为零() {
        assert_eq!(pool_alive(&[]), 0);
    }
}
