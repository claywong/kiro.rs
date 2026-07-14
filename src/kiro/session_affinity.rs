//! Process-local session-to-credential affinity cache.

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct SessionAffinityContext {
    pub client_key_id: u64,
    pub session_id: String,
}

impl SessionAffinityContext {
    pub fn new(client_key_id: u64, session_id: impl Into<String>) -> Self {
        Self {
            client_key_id,
            session_id: session_id.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SessionAffinityKey(String);

impl SessionAffinityKey {
    pub(crate) fn new(
        context: &SessionAffinityContext,
        group: Option<&str>,
        model: Option<&str>,
    ) -> Self {
        let mut hasher = Sha256::new();
        for part in [
            context.client_key_id.to_string(),
            group.unwrap_or_default().to_string(),
            model.unwrap_or_default().to_ascii_lowercase(),
            context.session_id.clone(),
        ] {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        Self(hex::encode(hasher.finalize()))
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    credential_id: u64,
    last_accessed: Instant,
}

#[derive(Debug)]
struct CacheState {
    entries: HashMap<SessionAffinityKey, Entry>,
}

/// Bounded, sliding-TTL cache. Session identifiers are hashed before becoming keys.
#[derive(Debug)]
pub(crate) struct SessionAffinityCache {
    ttl: Duration,
    max_entries: usize,
    state: Mutex<CacheState>,
}

impl SessionAffinityCache {
    pub(crate) fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries,
            state: Mutex::new(CacheState {
                entries: HashMap::new(),
            }),
        }
    }

    pub(crate) fn get(&self, key: &SessionAffinityKey) -> Option<u64> {
        self.get_at(key, Instant::now())
    }

    fn get_at(&self, key: &SessionAffinityKey, now: Instant) -> Option<u64> {
        let mut state = self.state.lock();
        let entry = state.entries.get_mut(key)?;
        if now.saturating_duration_since(entry.last_accessed) >= self.ttl {
            state.entries.remove(key);
            return None;
        }
        entry.last_accessed = now;
        Some(entry.credential_id)
    }

    pub(crate) fn insert(&self, key: SessionAffinityKey, credential_id: u64) {
        self.insert_at(key, credential_id, Instant::now());
    }

    fn insert_at(&self, key: SessionAffinityKey, credential_id: u64, now: Instant) {
        if self.max_entries == 0 {
            return;
        }
        let mut state = self.state.lock();
        state
            .entries
            .retain(|_, entry| now.saturating_duration_since(entry.last_accessed) < self.ttl);
        if !state.entries.contains_key(&key)
            && state.entries.len() >= self.max_entries
            && let Some(oldest) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_accessed)
                .map(|(key, _)| key.clone())
        {
            state.entries.remove(&oldest);
        }
        state.entries.insert(
            key,
            Entry {
                credential_id,
                last_accessed: now,
            },
        );
    }

    pub(crate) fn remove(&self, key: &SessionAffinityKey) {
        self.state.lock().entries.remove(key);
    }

    pub(crate) fn invalidate_credential(&self, credential_id: u64) {
        self.state
            .lock()
            .entries
            .retain(|_, entry| entry.credential_id != credential_id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state.lock().entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(session: &str) -> SessionAffinityKey {
        SessionAffinityKey::new(
            &SessionAffinityContext::new(1, session),
            Some("g"),
            Some("m"),
        )
    }

    #[test]
    fn sliding_ttl_refreshes_and_then_expires() {
        let cache = SessionAffinityCache::new(Duration::from_secs(10), 10);
        let start = Instant::now();
        let key = key("session");
        cache.insert_at(key.clone(), 7, start);
        assert_eq!(cache.get_at(&key, start + Duration::from_secs(9)), Some(7));
        assert_eq!(cache.get_at(&key, start + Duration::from_secs(18)), Some(7));
        assert_eq!(cache.get_at(&key, start + Duration::from_secs(29)), None);
    }

    #[test]
    fn evicts_oldest_entry_at_capacity() {
        let cache = SessionAffinityCache::new(Duration::from_secs(60), 2);
        let start = Instant::now();
        let first = key("first");
        let second = key("second");
        let third = key("third");
        cache.insert_at(first.clone(), 1, start);
        cache.insert_at(second.clone(), 2, start + Duration::from_secs(1));
        cache.insert_at(third.clone(), 3, start + Duration::from_secs(2));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get_at(&first, start + Duration::from_secs(3)), None);
        assert_eq!(
            cache.get_at(&second, start + Duration::from_secs(3)),
            Some(2)
        );
        assert_eq!(
            cache.get_at(&third, start + Duration::from_secs(3)),
            Some(3)
        );
    }

    #[test]
    fn key_isolated_by_client_group_and_model() {
        let context = SessionAffinityContext::new(1, "same-session");
        let base = SessionAffinityKey::new(&context, Some("a"), Some("sonnet"));
        assert_ne!(
            base,
            SessionAffinityKey::new(
                &SessionAffinityContext::new(2, "same-session"),
                Some("a"),
                Some("sonnet")
            )
        );
        assert_ne!(
            base,
            SessionAffinityKey::new(&context, Some("b"), Some("sonnet"))
        );
        assert_ne!(
            base,
            SessionAffinityKey::new(&context, Some("a"), Some("opus"))
        );
    }
}
