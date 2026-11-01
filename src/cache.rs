//! Tiny TTL cache: `RwLock<HashMap<String, CacheEntry>>` holding
//! pre-serialized JSON response bytes.
//!
//! Design notes (see README "Performance design"):
//! - Cache hits serve bytes with zero re-serialization — the hot path is a
//!   hash lookup, an expiry check, and a `memcpy`.
//! - Entries are keyed per route + normalized query params.
//! - TTLs mirror OSIRIS's own conventions: 45-60s for fast-moving feeds,
//!   longer for slow ones (TLE sets, news with rate limits).
//! - Expired entries are evicted opportunistically on insert once the map
//!   exceeds a bound, so memory stays flat without a background task.

use std::{
    collections::HashMap,
    sync::RwLock,
    time::{Duration, Instant},
};

struct CacheEntry {
    body: Vec<u8>,
    expires_at: Instant,
}

/// Bounded in-memory TTL cache of pre-serialized response bodies.
#[derive(Default)]
pub struct TtlCache {
    inner: RwLock<HashMap<String, CacheEntry>>,
}

/// Hard bound on entries; beyond this, expired entries are swept on insert.
const MAX_ENTRIES: usize = 2048;

impl TtlCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached body if present and unexpired.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let inner = self.inner.read().ok()?;
        let entry = inner.get(key)?;
        if Instant::now() < entry.expires_at {
            Some(entry.body.clone())
        } else {
            None
        }
    }

    /// Store a body for `ttl`.
    pub fn put(&self, key: String, body: Vec<u8>, ttl: Duration) {
        let Ok(mut inner) = self.inner.write() else {
            return;
        };
        if inner.len() >= MAX_ENTRIES {
            let now = Instant::now();
            inner.retain(|_, e| now < e.expires_at);
        }
        inner.insert(
            key,
            CacheEntry {
                body,
                expires_at: Instant::now() + ttl,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_returns_body() {
        let c = TtlCache::new();
        c.put("k".into(), b"{}".to_vec(), Duration::from_secs(60));
        assert_eq!(c.get("k"), Some(b"{}".to_vec()));
    }

    #[test]
    fn miss_on_unknown_key() {
        let c = TtlCache::new();
        assert_eq!(c.get("nope"), None);
    }

    #[test]
    fn expired_entry_is_a_miss() {
        let c = TtlCache::new();
        c.put("k".into(), b"{}".to_vec(), Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(c.get("k"), None);
    }
}
