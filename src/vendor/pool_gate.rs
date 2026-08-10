//! 全局提取限制（跨供应商共享）
//!
//! 为什么需要这一层：各家的失效判定按设计互不可见（见 [`super::auto::census`]
//! 的注释 —— A 家推「全部失效」时若把 B 家健康的 Key 算进来，A 的补货会被 B
//! 挡死）。代价是多家 Key 同期失效时，三家各自都得出「池子空了」的结论，于是
//! 各提一份。本模块补上那个缺失的全局视图。
//!
//! 本模块另外持有**自动提取总闸**（[`PoolGate::auto_enabled`]）。它与阈值无关，
//! 放这里只因为二者都是跨供应商的量，而本结构体正是各家共享的那一个 `Arc`。
//! 总闸对所有家一律生效，阈值只管没开逐渠道的家 —— 故判断分成两个方法。
//!
//! 阈值这一侧要做的两件事必须一起做，只做前者等于没做：
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
    /// 自动提取总闸。false = 全局关闭。与 `target` 同放在这里是因为两者都是
    /// 跨供应商的量，而本结构体就是各家共享的那一个 `Arc` —— 另造一层没有意义。
    auto_enabled: AtomicBool,
    /// 提取串行锁。守护「盘点 → 下单 → 导入」这段临界区。
    lock: Mutex<()>,
}

impl PoolGate {
    /// 只给测试用的简写：总闸取缺省的开启态，与 `Config::auto_purchase_enabled`
    /// 的默认值一致。生产路径一律走 [`Self::with_auto_enabled`] 显式传入两个初值 ——
    /// 总闸是个容易被忘掉的全局状态，不该有一条「悄悄用了默认值」的构造路径。
    #[cfg(test)]
    fn new(target: u32) -> Arc<Self> {
        Self::with_auto_enabled(target, true)
    }

    pub fn with_auto_enabled(target: u32, auto_enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            target: AtomicU32::new(target),
            auto_enabled: AtomicBool::new(auto_enabled),
            lock: Mutex::new(()),
        })
    }

    /// 自动提取总闸是否开着。false 表示所有家都不该自动下单。
    pub fn auto_enabled(&self) -> bool {
        self.auto_enabled.load(Ordering::Relaxed)
    }

    pub fn set_auto_enabled(&self, enabled: bool) {
        self.auto_enabled.store(enabled, Ordering::Relaxed);
    }

    /// 当前阈值，0 表示不启用
    pub fn target(&self) -> u32 {
        self.target.load(Ordering::Relaxed)
    }

    pub fn set_target(&self, target: u32) {
        self.target.store(target, Ordering::Relaxed);
    }

    /// 是否启用了全局阈值闸
    pub fn enabled(&self) -> bool {
        self.target() > 0
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

    /// 总闸判断。`Err(原因)` 表示全局已关闭自动提取，本轮任何家都不该下单。
    ///
    /// 与 [`Self::check`] 分成两个方法而不是合并：总闸与阈值的适用范围不同 ——
    /// 阈值只管没开逐渠道的家，总闸对所有家一律生效，包括开了逐渠道的。
    pub fn check_auto_enabled(&self) -> Result<(), String> {
        if self.auto_enabled() {
            return Ok(());
        }
        Err("已全局关闭自动提取（总闸），本轮不补货".to_string())
    }

    /// 按阈值判断当前池量是否已够用。`Err(原因)` 表示本轮不该补货。
    ///
    /// 未启用（阈值 0）时一律放行，保持升级前后行为一致。
    ///
    /// 开了逐渠道的家**不调用本方法**（判据换成本家盘点，见
    /// [`VendorConfig::auto_purchase_per_channel`](crate::model::config::VendorConfig::auto_purchase_per_channel)）。
    /// 但它们买来的号仍会计入别家的 `pool_alive` —— 这是刻意的不对称：开着的家
    /// 只看自己，关着的家看总量且总量含开着的家。
    pub fn check(&self, pool_alive: u32) -> Result<(), String> {
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
    fn 总闸缺省开启() {
        // 与 Config::auto_purchase_enabled 的默认值一致 —— 存量配置没有这个键，
        // 反过来会让升级后自动提取集体静默停摆
        assert!(PoolGate::new(0).auto_enabled());
        assert!(PoolGate::new(0).check_auto_enabled().is_ok());
    }

    #[test]
    fn 总闸关闭时拦截() {
        let g = PoolGate::with_auto_enabled(0, false);
        assert!(!g.auto_enabled());
        assert!(g.check_auto_enabled().is_err());
    }

    #[test]
    fn 总闸可运行时切换() {
        let g = PoolGate::new(0);
        g.set_auto_enabled(false);
        assert!(g.check_auto_enabled().is_err());
        g.set_auto_enabled(true);
        assert!(g.check_auto_enabled().is_ok(), "重开应立即放行");
    }

    /// 总闸与阈值互不影响：阈值为 0（不启用）时总闸仍能拦，
    /// 总闸开着时阈值也照旧判 —— 两者适用范围不同，不该互相覆盖。
    #[test]
    fn 总闸与阈值互相独立() {
        let g = PoolGate::with_auto_enabled(0, false);
        assert!(g.check_auto_enabled().is_err(), "阈值未启用不影响总闸");
        assert!(g.check(99).is_ok(), "总闸关闭不改变阈值自身的判定");

        let g2 = PoolGate::with_auto_enabled(2, true);
        assert!(g2.check_auto_enabled().is_ok());
        assert!(g2.check(5).is_err(), "总闸开着时阈值照旧生效");
    }

    /// 面板与事件行都要凭这句话判断「为什么没补货」，得点明是总闸而非阈值
    #[test]
    fn 总闸拦截原因点明是总闸() {
        let msg = PoolGate::with_auto_enabled(0, false)
            .check_auto_enabled()
            .unwrap_err();
        assert!(msg.contains("总闸"), "要能与阈值拦截区分开: {msg}");
    }

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
