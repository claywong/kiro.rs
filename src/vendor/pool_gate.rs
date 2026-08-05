//! 全局提取限制（跨供应商共享）
//!
//! 为什么需要这一层：各家的失效判定按设计互不可见（见 [`super::auto::census`]
//! 的注释 —— A 家推「全部失效」时若把 B 家健康的 Key 算进来，A 的补货会被 B
//! 挡死）。代价是多家 Key 同期失效时，三家各自都得出「池子空了」的结论，于是
//! 各提一份。本模块补上那个缺失的全局视图。
//!
//! 两件事必须一起做，只做前者等于没做：
//!
//! 1. **阈值**：池中存活的卖家 Key 达到 `target` 就不再自动补货。
//! 2. **串行化**：自动提取是 `tokio::spawn` 并发触发的（见
//!    [`super::service::VendorService::spawn_auto_purchase`]）。若只加阈值判断，
//!    三家会同时读到「池里 0 个存活」然后同时下单 —— 竞态下闸门形同虚设。
//!    故盘点与下单必须在同一把锁内完成，后来者重新盘点时才能看到前者刚导入的 Key。
//!
//! 锁跨出站请求持有，因此配超时：拿不到锁就记跳过，不无限等 —— 一家卡在网络上
//! 不该拖住其余几家。
//!
//! @author wangzhong

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, MutexGuard};

/// 等锁上限。超时即放弃本轮（记跳过，不消费失效确认额度）。
///
/// 取 60s 的依据：锁的持有区间是「一次库存查询 + 一次下单 + 逐条导入」，
/// 正常在秒级；60s 已属异常，此时让后来者退出比排队更好 —— 失效确认额度还在，
/// 下一个事件仍可触发。
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);

/// 跨供应商共享的提取闸门
pub struct PoolGate {
    /// 运行时阈值。0 = 不启用。面板改后立即生效，故用原子量而非读 config 快照。
    target: AtomicU32,
    /// 逐渠道模式：判据换成「本家有没有存活」，全局阈值不再参与。
    /// 见 [`Config::auto_purchase_per_channel`](crate::model::config::Config::auto_purchase_per_channel)。
    per_channel: AtomicBool,
    /// 提取串行锁。守护「盘点 → 下单 → 导入」这段临界区。
    lock: Mutex<()>,
}

impl PoolGate {
    /// 仅全局阈值模式。生产路径走 [`Self::with_mode`]，本构造留给测试。
    #[cfg(test)]
    pub fn new(target: u32) -> Arc<Self> {
        Self::with_mode(target, false)
    }

    pub fn with_mode(target: u32, per_channel: bool) -> Arc<Self> {
        Arc::new(Self {
            target: AtomicU32::new(target),
            per_channel: AtomicBool::new(per_channel),
            lock: Mutex::new(()),
        })
    }

    /// 当前阈值，0 表示不启用
    pub fn target(&self) -> u32 {
        self.target.load(Ordering::Relaxed)
    }

    pub fn set_target(&self, target: u32) {
        self.target.store(target, Ordering::Relaxed);
    }

    pub fn per_channel(&self) -> bool {
        self.per_channel.load(Ordering::Relaxed)
    }

    pub fn set_per_channel(&self, on: bool) {
        self.per_channel.store(on, Ordering::Relaxed);
    }

    /// 是否启用了全局阈值闸
    pub fn enabled(&self) -> bool {
        self.target() > 0
    }

    /// 是否放行「就地盘点」兜底路径，以及是否需要串行化。
    ///
    /// 两种模式各自提供了刹车，都算有刹车：全局阈值靠池量上限，逐渠道靠本家
    /// 盘点（买到即 `alive == 1`，下一条推送被 `StillAlive` 拒）。两者皆关时
    /// 兜底无上限，必须维持原行为（只认卖家额度）。
    pub fn gating_active(&self) -> bool {
        self.enabled() || self.per_channel()
    }

    /// 取提取锁。`Err` 表示等待超时，调用方应记跳过而非继续下单。
    pub async fn acquire(&self) -> Result<MutexGuard<'_, ()>, String> {
        tokio::time::timeout(LOCK_TIMEOUT, self.lock.lock())
            .await
            .map_err(|_| {
                format!(
                    "等待全局提取锁超时（{}s），另一家的提取仍在进行",
                    LOCK_TIMEOUT.as_secs()
                )
            })
    }

    /// 按阈值判断当前池量是否已够用。`Err(原因)` 表示本轮不该补货。
    ///
    /// 未启用（阈值 0）时一律放行，保持升级前后行为一致。
    ///
    /// 逐渠道模式下**一律放行**：该模式的判据是「本家有没有存活」，已由调用方
    /// 的 `census` 判完。此处若还按池量拦，`target=1` 会把第二家挡死 —— 那正是
    /// 本模式要解掉的约束，两个判据同时生效等于开关无效。
    pub fn check(&self, pool_alive: u32) -> Result<(), String> {
        if self.per_channel() {
            return Ok(());
        }
        let target = self.target();
        if target == 0 {
            return Ok(());
        }
        if pool_alive >= target {
            return Err(format!(
                "池中已有 {pool_alive} 个可用卖家 Key（全局限制 {target}），本轮不补货"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 阈值为零时不拦截() {
        let g = PoolGate::new(0);
        assert!(!g.enabled());
        // 池里再多也放行 —— 未启用就该完全等价于改动前的行为
        assert!(g.check(99).is_ok());
    }

    #[test]
    fn 达到阈值即拦截() {
        let g = PoolGate::new(3);
        assert!(g.check(3).is_err(), "等于阈值就算够用");
        assert!(g.check(4).is_err());
    }

    #[test]
    fn 低于阈值放行() {
        let g = PoolGate::new(3);
        assert!(g.check(0).is_ok());
        assert!(g.check(2).is_ok());
    }

    #[test]
    fn 阈值可运行时修改() {
        let g = PoolGate::new(0);
        g.set_target(1);
        assert!(g.enabled());
        assert!(g.check(1).is_err());
        g.set_target(0);
        assert!(g.check(1).is_ok(), "改回 0 应重新放行");
    }

    /// 拦截文案要带上两个数字，面板上要凭它判断是不是配错了阈值
    #[test]
    fn 拦截原因含池量与阈值() {
        let msg = PoolGate::new(2).check(5).unwrap_err();
        assert!(msg.contains('5'), "要说明当前池量: {msg}");
        assert!(msg.contains('2'), "要说明阈值: {msg}");
    }

    #[test]
    fn 逐渠道模式一律放行() {
        let g = PoolGate::with_mode(1, true);
        assert!(g.per_channel());
        assert!(g.gating_active(), "per_channel 也算有刹车");
        // 池量超阈值也放行 —— per_channel 开启时阈值不参与判断
        assert!(g.check(99).is_ok());
    }

    #[test]
    fn 两种刹车皆无时兜底不放行() {
        let g = PoolGate::with_mode(0, false);
        assert!(!g.gating_active(), "两种刹车皆无");
    }

    /// 固定 `service.rs` 里的实际用法：`let _gate = if enabled { Some(acquire) } else { None }`。
    ///
    /// 这个测试盯的是一个易错点 —— 若写成 `let _ = ...`（裸下划线）守卫会当场
    /// 释放，锁形同虚设且不会有任何编译告警。整个池闸的正确性都压在这个绑定上。
    #[tokio::test]
    async fn 守卫按可选值持有时仍然生效() {
        let g = PoolGate::new(1);
        let _gate = if g.enabled() {
            Some(g.acquire().await.expect("首次应拿到"))
        } else {
            None
        };
        let blocked = tokio::time::timeout(Duration::from_millis(50), g.acquire()).await;
        assert!(blocked.is_err(), "Option 包裹的守卫必须仍然持锁");
    }

    /// 未启用时不取锁，故并发的两家都能进 —— 这正是我们想要的：
    /// 关闭池闸就该完全等价于改动前的行为，不付串行化代价。
    #[tokio::test]
    async fn 未启用时不串行化() {
        let g = PoolGate::new(0);
        let _gate = if g.enabled() {
            Some(g.acquire().await.unwrap())
        } else {
            None
        };
        assert!(_gate.is_none(), "阈值为 0 时不该取锁");
        let second = tokio::time::timeout(Duration::from_millis(50), g.acquire()).await;
        assert!(second.is_ok(), "没人持锁，第二家应立刻通过");
    }

    #[tokio::test]
    async fn 锁串行化两次提取() {
        let g = PoolGate::new(1);
        let first = g.acquire().await.expect("首次应立刻拿到");
        // 持锁期间第二次尝试拿不到（用短超时代替等满 60s）
        let blocked = tokio::time::timeout(Duration::from_millis(50), g.acquire()).await;
        assert!(blocked.is_err(), "持锁期间不该放第二家进临界区");
        drop(first);
        assert!(g.acquire().await.is_ok(), "释放后应可再取");
    }
}
