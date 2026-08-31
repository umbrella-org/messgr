//! A bounded, zeroizing, TTL-lazy-expiry cache of `Uuid`-keyed secrets
//! (DESIGN.md §7.6: "an in-process LRU cache holds unwrapped DEKs — bounded
//! size, TTL, zeroized on eviction"). Generic over what the `Uuid` keys
//! mean (`customer_dek::lifecycle` keys by `customer_id`; `tenant_pepper`
//! keys by `tenant_id`) — one implementation, not two near-identical ones.

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lru::LruCache;
use uuid::Uuid;
use zeroize::Zeroizing;

type CacheEntry = (Zeroizing<Vec<u8>>, Instant);

/// Sizing has no number in DESIGN.md ("bounded size, TTL" is stated without
/// values) — these are T-008's recommended starting points for the two real
/// callers (T-011's ingest path, T-013's dispatcher), not hardcoded here:
/// DEK cache `new(NonZeroUsize::new(100_000)..., Duration::from_secs(3600))`;
/// tenant-pepper cache `new(NonZeroUsize::new(8)..., Duration::from_secs(3600))`
/// (realistically one tenant per process — §9 decision 16 — with headroom).
/// Revisit against real load once either caller exists.
pub struct KeyCache {
    inner: Mutex<LruCache<Uuid, CacheEntry>>,
    ttl: Duration,
}

impl KeyCache {
    pub fn new(capacity: NonZeroUsize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(capacity)),
            ttl,
        }
    }

    /// A hit within `ttl` returns a clone of the cached plaintext (small —
    /// 32 bytes — and cheap to clone rather than hold a lock across the
    /// caller's use of it). A stale hit is evicted and treated as a miss.
    pub fn get(&self, key: Uuid) -> Option<Zeroizing<Vec<u8>>> {
        let mut inner = self.inner.lock().expect("KeyCache mutex poisoned");
        match inner.get(&key) {
            Some((plaintext, cached_at)) if cached_at.elapsed() < self.ttl => {
                Some(plaintext.clone())
            }
            Some(_) => {
                inner.pop(&key);
                None
            }
            None => None,
        }
    }

    pub fn put(&self, key: Uuid, plaintext: Zeroizing<Vec<u8>>) {
        let mut inner = self.inner.lock().expect("KeyCache mutex poisoned");
        inner.put(key, (plaintext, Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    #[test]
    fn evicts_the_least_recently_used_entry_past_capacity() {
        let cache =
            KeyCache::new(NonZeroUsize::new(2).unwrap(), Duration::from_secs(60));
        cache.put(key(1), Zeroizing::new(vec![1]));
        cache.put(key(2), Zeroizing::new(vec![2]));
        cache.put(key(3), Zeroizing::new(vec![3])); // evicts key(1), the LRU entry

        assert!(cache.get(key(1)).is_none());
        assert_eq!(*cache.get(key(2)).unwrap(), vec![2]);
        assert_eq!(*cache.get(key(3)).unwrap(), vec![3]);
    }

    #[test]
    fn a_stale_entry_past_ttl_is_treated_as_a_miss() {
        let cache =
            KeyCache::new(NonZeroUsize::new(4).unwrap(), Duration::from_millis(1));
        cache.put(key(1), Zeroizing::new(vec![1]));
        std::thread::sleep(Duration::from_millis(10));

        assert!(
            cache.get(key(1)).is_none(),
            "entry must expire past its TTL"
        );
    }

    #[test]
    fn a_fresh_entry_within_ttl_hits() {
        let cache =
            KeyCache::new(NonZeroUsize::new(4).unwrap(), Duration::from_secs(60));
        cache.put(key(1), Zeroizing::new(vec![9, 9]));

        assert_eq!(*cache.get(key(1)).unwrap(), vec![9, 9]);
    }
}
