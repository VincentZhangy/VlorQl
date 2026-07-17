//! Generic [`Cache`] trait and a no-op implementation.
//!
//! The [`Cache`] trait is the single abstraction used by all caching
//! layers in VlorQl.  A no-op implementation ([`NoopCache`]) is provided
//! so callers that do not need caching can use it without conditional
//! logic.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;

/// A generic key-value cache with thread-safe operations.
///
/// Implementations must be [`Send`] + [`Sync`] so they can be shared
/// across async tasks and stored in an `Arc`.
///
/// # Examples
///
/// ```
/// use vlorql_core::cache::{Cache, NoopCache};
///
/// let cache = NoopCache::<u64, String>::new();
/// assert!(cache.get(&42).is_none());
/// cache.insert(42, "hello".to_owned());
/// assert_eq!(cache.size(), 0); // NoopCache never stores anything
/// ```
pub trait Cache<K: Eq + Hash, V: Clone>: Send + Sync {
    /// Returns the cached value for `key`, or `None` on a miss.
    fn get(&self, key: &K) -> Option<V>;

    /// Inserts a value into the cache.
    ///
    /// Returns the previous value associated with the key, if any.
    fn insert(&self, key: K, value: V) -> Option<V>;

    /// Removes the entry for `key` from the cache.
    fn invalidate(&self, key: &K);

    /// Removes all entries from the cache.
    fn clear(&self);

    /// Returns the number of entries currently in the cache.
    fn size(&self) -> u64;
}

// ---------------------------------------------------------------------------
// NoopCache — a cache that never stores anything
// ---------------------------------------------------------------------------

/// A cache that discards every inserted value.
///
/// Useful as a default when no caching is desired; it avoids branching
/// on `Option<impl Cache>` throughout the codebase.
#[derive(Debug, Default)]
pub struct NoopCache<K, V> {
    _marker: std::marker::PhantomData<(K, V)>,
}

impl<K, V> NoopCache<K, V> {
    /// Creates a new no-op cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<K: Eq + Hash + Send + Sync, V: Clone + Send + Sync> Cache<K, V> for NoopCache<K, V> {
    fn get(&self, _key: &K) -> Option<V> {
        None
    }

    fn insert(&self, _key: K, _value: V) -> Option<V> {
        None
    }

    fn invalidate(&self, _key: &K) {}

    fn clear(&self) {}

    fn size(&self) -> u64 {
        0
    }
}

// ---------------------------------------------------------------------------
// MemoryCache — a simple in-memory HashMap-backed cache
// ---------------------------------------------------------------------------

/// A simple in-memory cache backed by a `HashMap`.
///
/// This is the default implementation for most use cases.  It is not
/// LRU or TTL-aware; entries live until explicitly invalidated or the
/// cache is dropped.
///
/// # Examples
///
/// ```
/// use vlorql_core::cache::{Cache, MemoryCache};
///
/// let cache = MemoryCache::new();
/// assert!(cache.get(&"key").is_none());
/// cache.insert("key", 42u64);
/// assert_eq!(cache.get(&"key"), Some(42));
/// assert_eq!(cache.size(), 1);
/// ```
#[derive(Debug)]
pub struct MemoryCache<K, V> {
    inner: Mutex<HashMap<K, V>>,
}

impl<K, V> MemoryCache<K, V> {
    /// Creates a new empty in-memory cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Creates an empty in-memory cache with the specified capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::with_capacity(capacity)),
        }
    }
}

impl<K, V> Default for MemoryCache<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash + Send + Sync, V: Clone + Send> Cache<K, V> for MemoryCache<K, V> {
    fn get(&self, key: &K) -> Option<V> {
        let map = self.inner.lock().expect("MemoryCache lock poisoned");
        map.get(key).cloned()
    }

    fn insert(&self, key: K, value: V) -> Option<V> {
        let mut map = self.inner.lock().expect("MemoryCache lock poisoned");
        map.insert(key, value)
    }

    fn invalidate(&self, key: &K) {
        let mut map = self.inner.lock().expect("MemoryCache lock poisoned");
        map.remove(key);
    }

    fn clear(&self) {
        let mut map = self.inner.lock().expect("MemoryCache lock poisoned");
        map.clear();
    }

    fn size(&self) -> u64 {
        let map = self.inner.lock().expect("MemoryCache lock poisoned");
        map.len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_cache_never_stores() {
        let cache = NoopCache::<u64, String>::new();
        assert_eq!(cache.size(), 0);
        assert!(cache.get(&1).is_none());
        cache.insert(1, "hello".to_owned());
        assert_eq!(cache.size(), 0);
        assert!(cache.get(&1).is_none());
    }

    #[test]
    fn memory_cache_insert_and_get() {
        let cache = MemoryCache::new();
        assert!(cache.get(&"a").is_none());
        cache.insert("a", 10);
        assert_eq!(cache.get(&"a"), Some(10));
        assert_eq!(cache.size(), 1);
    }

    #[test]
    fn memory_cache_overwrite() {
        let cache = MemoryCache::new();
        cache.insert("x", 1);
        let prev = cache.insert("x", 2);
        assert_eq!(prev, Some(1));
        assert_eq!(cache.get(&"x"), Some(2));
    }

    #[test]
    fn memory_cache_invalidate() {
        let cache = MemoryCache::new();
        cache.insert("k", "v".to_owned());
        assert_eq!(cache.size(), 1);
        cache.invalidate(&"k");
        assert!(cache.get(&"k").is_none());
        assert_eq!(cache.size(), 0);
    }

    #[test]
    fn memory_cache_clear() {
        let cache = MemoryCache::new();
        cache.insert("a", 1);
        cache.insert("b", 2);
        assert_eq!(cache.size(), 2);
        cache.clear();
        assert_eq!(cache.size(), 0);
    }

    #[test]
    fn memory_cache_with_capacity() {
        let cache = MemoryCache::<&str, u64>::with_capacity(100);
        assert_eq!(cache.size(), 0);
    }
}