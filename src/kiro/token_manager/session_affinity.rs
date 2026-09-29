//! 会话亲和：session_id → 凭据 绑定表
//!
//! 上游 prompt cache 大概率按账号隔离：同一会话的多轮请求若被调度到不同账号，
//! 上游需要全价重读整段上下文。这里记录每个会话最近一次成功发出请求的凭据，
//! 后续请求优先复用它（忽略 RPM 限制，其它可用性检查照常，见 `acquire_context_impl`）。
//!
//! - 键是 `metadata.user_id` 里的 session_id（与发往上游的 conversationId 同源）。
//! - 滑动 TTL：每次命中 / 改绑都刷新 `last_used`。
//! - 只存内存，重启即丢；丢失只影响下一轮能否命中缓存。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// 一次选号的亲和结果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AffinityOutcome {
    /// 未启用亲和或请求没有 session
    #[default]
    None,
    /// 新会话首次绑定
    Bind,
    /// 复用已绑定的凭据
    Hit,
    /// 原绑定凭据不可用，改绑到新凭据
    Rebind,
}

impl AffinityOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bind => "bind",
            Self::Hit => "hit",
            Self::Rebind => "rebind",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

struct Binding {
    credential_id: u64,
    last_used: Instant,
}

pub(crate) struct SessionAffinity {
    bindings: Mutex<HashMap<String, Binding>>,
    ttl: Duration,
    max_entries: usize,
    /// 统计活跃负载的窗口：`last_used` 在窗口内的绑定才算占用该凭据
    load_window: Duration,
    /// 各结果的窗口计数（汇总日志后清零）
    counters: [AtomicU64; 4],
}

impl SessionAffinity {
    pub(crate) fn new(ttl_secs: u64, max_entries: usize, load_window_secs: u64) -> Self {
        Self {
            bindings: Mutex::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_secs.max(1)),
            max_entries: max_entries.max(1),
            load_window: Duration::from_secs(load_window_secs.max(1)),
            counters: [const { AtomicU64::new(0) }; 4],
        }
    }

    /// 查询会话当前绑定的凭据（已过期视为未绑定）。只读，不刷新 TTL。
    pub(crate) fn lookup(&self, session: &str, now: Instant) -> Option<u64> {
        self.bindings
            .lock()
            .get(session)
            .filter(|b| now.duration_since(b.last_used) < self.ttl)
            .map(|b| b.credential_id)
    }

    /// 记录会话本次实际使用的凭据，刷新 TTL，返回亲和结果。
    pub(crate) fn record_use(&self, session: &str, credential_id: u64, now: Instant) -> AffinityOutcome {
        let mut bindings = self.bindings.lock();
        let outcome = match bindings.get(session) {
            Some(b) if now.duration_since(b.last_used) < self.ttl => {
                if b.credential_id == credential_id {
                    AffinityOutcome::Hit
                } else {
                    AffinityOutcome::Rebind
                }
            }
            _ => AffinityOutcome::Bind,
        };
        if !bindings.contains_key(session) && bindings.len() >= self.max_entries {
            Self::evict_oldest(&mut bindings);
        }
        bindings.insert(
            session.to_string(),
            Binding {
                credential_id,
                last_used: now,
            },
        );
        drop(bindings);
        self.counters[outcome.index()].fetch_add(1, Ordering::Relaxed);
        outcome
    }

    /// 各凭据的活跃绑定数：`last_used` 在负载窗口内（且未过期）的会话才计入。
    pub(crate) fn active_load(&self, now: Instant) -> HashMap<u64, usize> {
        let window = self.load_window.min(self.ttl);
        let mut load = HashMap::new();
        for b in self.bindings.lock().values() {
            if now.duration_since(b.last_used) < window {
                *load.entry(b.credential_id).or_insert(0) += 1;
            }
        }
        load
    }

    /// 统计一次无 session（或亲和关闭）的请求
    pub(crate) fn count_none(&self) {
        self.counters[AffinityOutcome::None.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// 容量满时淘汰最久未用的一条（只在满容量时发生，线性扫描可接受）
    fn evict_oldest(bindings: &mut HashMap<String, Binding>) {
        if let Some(oldest) = bindings
            .iter()
            .min_by_key(|(_, b)| b.last_used)
            .map(|(k, _)| k.clone())
        {
            bindings.remove(&oldest);
        }
    }

    /// 清理过期绑定
    pub(crate) fn evict_expired(&self, now: Instant) {
        let ttl = self.ttl;
        self.bindings
            .lock()
            .retain(|_, b| now.duration_since(b.last_used) < ttl);
    }

    pub(crate) fn len(&self) -> usize {
        self.bindings.lock().len()
    }

    /// 取出并清零窗口计数：[none, bind, hit, rebind]
    pub(crate) fn take_counters(&self) -> [u64; 4] {
        std::array::from_fn(|i| self.counters[i].swap(0, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_use_classifies_bind_hit_rebind() {
        let aff = SessionAffinity::new(3600, 10, 300);
        let now = Instant::now();
        assert_eq!(aff.lookup("s1", now), None);
        assert_eq!(aff.record_use("s1", 7, now), AffinityOutcome::Bind);
        assert_eq!(aff.lookup("s1", now), Some(7));
        assert_eq!(aff.record_use("s1", 7, now), AffinityOutcome::Hit);
        assert_eq!(aff.record_use("s1", 9, now), AffinityOutcome::Rebind);
        assert_eq!(aff.lookup("s1", now), Some(9));
        assert_eq!(aff.take_counters(), [0, 1, 1, 1]);
        assert_eq!(aff.take_counters(), [0, 0, 0, 0]);
    }

    #[test]
    fn expired_binding_counts_as_unbound() {
        let aff = SessionAffinity::new(60, 10, 300);
        let t0 = Instant::now();
        aff.record_use("s1", 7, t0);
        let later = t0 + Duration::from_secs(61);
        assert_eq!(aff.lookup("s1", later), None);
        assert_eq!(aff.record_use("s1", 8, later), AffinityOutcome::Bind);
        aff.evict_expired(later + Duration::from_secs(61));
        assert_eq!(aff.len(), 0);
    }

    #[test]
    fn capacity_evicts_least_recently_used() {
        let aff = SessionAffinity::new(3600, 2, 300);
        let t0 = Instant::now();
        aff.record_use("a", 1, t0);
        aff.record_use("b", 2, t0 + Duration::from_secs(1));
        aff.record_use("a", 1, t0 + Duration::from_secs(2)); // a 变成最近使用
        aff.record_use("c", 3, t0 + Duration::from_secs(3)); // 淘汰 b
        let now = t0 + Duration::from_secs(4);
        assert_eq!(aff.len(), 2);
        assert_eq!(aff.lookup("a", now), Some(1));
        assert_eq!(aff.lookup("b", now), None);
        assert_eq!(aff.lookup("c", now), Some(3));
    }

    #[test]
    fn active_load_counts_only_recent_bindings() {
        let aff = SessionAffinity::new(3600, 10, 300);
        let t0 = Instant::now();
        aff.record_use("old", 1, t0);
        aff.record_use("a", 1, t0 + Duration::from_secs(200));
        aff.record_use("b", 2, t0 + Duration::from_secs(250));
        aff.record_use("c", 2, t0 + Duration::from_secs(260));
        let load = aff.active_load(t0 + Duration::from_secs(310));
        // "old" 已超出 5 分钟负载窗口（绑定本身仍在 TTL 内）
        assert_eq!(load.get(&1), Some(&1));
        assert_eq!(load.get(&2), Some(&2));
        assert_eq!(aff.lookup("old", t0 + Duration::from_secs(310)), Some(1));
    }
}
