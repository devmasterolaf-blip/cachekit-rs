//! Thread-safe cache wrappers.
//!
//! [`PolicyCache`] needs `&mut self` even for reads, because a read updates
//! eviction order and statistics. The wrappers here put the cache behind a
//! [`Mutex`] and offer the same operations through `&self`, returning clones
//! of the values so no lock is held once a call returns.
//!
//! * [`SyncCache`] guards one cache with one lock.
//! * [`ShardedCache`] splits the key space over several independently locked
//!   caches so that threads working on different keys rarely contend.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::cache::{Cache, PolicyCache};
use crate::error::{ConfigError, LoadError};
use crate::policy::{BoxedPolicy, Policy};
use crate::stats::Stats;

/// Locks `mutex`, recovering the guard if another thread panicked while
/// holding it. The only user code that runs under the lock is listeners,
/// weighers and loaders, and the cache keeps its own state consistent
/// around those calls, so the data behind a poisoned lock is still usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A cache shared between threads behind a single mutex.
///
/// ```
/// use std::sync::Arc;
/// use std::thread;
/// use cachekit::{LruCache, SyncCache};
///
/// let cache = Arc::new(SyncCache::new(LruCache::new(100)));
/// let workers: Vec<_> = (0..4)
///     .map(|id| {
///         let cache = Arc::clone(&cache);
///         thread::spawn(move || cache.put(id, id * 10))
///     })
///     .collect();
/// for worker in workers {
///     worker.join().unwrap();
/// }
/// assert_eq!(cache.get(&3), Some(30));
/// ```
pub struct SyncCache<K, V, P = BoxedPolicy> {
    inner: Mutex<PolicyCache<K, V, P>>,
}

impl<K, V, P> SyncCache<K, V, P>
where
    K: Hash + Eq + Clone,
    P: Policy,
{
    /// Wraps `cache` so it can be shared between threads.
    pub fn new(cache: PolicyCache<K, V, P>) -> Self {
        SyncCache {
            inner: Mutex::new(cache),
        }
    }

    /// Unwraps the cache.
    pub fn into_inner(self) -> PolicyCache<K, V, P> {
        self.inner
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs `f` with exclusive access to the underlying cache. Use this for
    /// operations the wrapper does not expose, or to do several operations
    /// under one lock acquisition.
    pub fn with<R>(&self, f: impl FnOnce(&mut PolicyCache<K, V, P>) -> R) -> R {
        f(&mut lock(&self.inner))
    }

    /// Looks up `key` and returns a clone of its value. See [`Cache::get`].
    pub fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        lock(&self.inner).get(key).cloned()
    }

    /// Looks up `key` without touching statistics or eviction order.
    pub fn peek(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        lock(&self.inner).peek(key).cloned()
    }

    /// Stores `value` under `key`. See [`Cache::put`].
    pub fn put(&self, key: K, value: V) -> Option<V> {
        lock(&self.inner).put(key, value)
    }

    /// Stores `value` with its own time to live. See
    /// [`PolicyCache::put_with_ttl`].
    pub fn put_with_ttl(&self, key: K, value: V, ttl: Duration) -> Option<V> {
        lock(&self.inner).put_with_ttl(key, value, ttl)
    }

    /// Removes `key` and returns its value.
    pub fn remove(&self, key: &K) -> Option<V> {
        lock(&self.inner).remove(key)
    }

    /// Returns `true` if `key` is present and not expired.
    pub fn contains_key(&self, key: &K) -> bool {
        lock(&self.inner).contains_key(key)
    }

    /// Returns the value for `key`, loading and storing it on a miss. The
    /// lock is held while the loader runs, so concurrent callers asking for
    /// the same key load it only once.
    pub fn get_or_insert_with<F>(&self, key: K, loader: F) -> V
    where
        V: Clone,
        F: FnOnce(&K) -> V,
    {
        lock(&self.inner).get_or_insert_with(key, loader)
    }

    /// Fallible variant of [`get_or_insert_with`](Self::get_or_insert_with).
    pub fn get_or_try_insert_with<F, E>(&self, key: K, loader: F) -> Result<V, LoadError<E>>
    where
        V: Clone,
        F: FnOnce(&K) -> Result<V, E>,
    {
        lock(&self.inner).get_or_try_insert_with(key, loader)
    }

    /// Number of entries currently stored.
    pub fn len(&self) -> usize {
        lock(&self.inner).len()
    }

    /// Returns `true` when the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Removes every entry.
    pub fn clear(&self) {
        lock(&self.inner).clear();
    }

    /// Removes every expired entry and returns how many were removed.
    pub fn purge_expired(&self) -> usize {
        lock(&self.inner).purge_expired()
    }

    /// A copy of the activity counters.
    pub fn stats(&self) -> Stats {
        lock(&self.inner).stats()
    }

    /// Sets every activity counter back to zero.
    pub fn reset_stats(&self) {
        lock(&self.inner).reset_stats();
    }
}

/// A cache split into independently locked shards.
///
/// Each key belongs to exactly one shard, chosen by its hash, and each shard
/// is a complete cache with its own capacity, eviction order and statistics.
/// Two consequences follow:
///
/// * the capacity bound is per shard, so the cache can start evicting from a
///   busy shard before the total reaches the configured limit;
/// * eviction order is only meaningful within a shard.
///
/// Build one with [`CacheBuilder::build_sharded`](crate::CacheBuilder::build_sharded),
/// which divides the configured capacity between the shards.
pub struct ShardedCache<K, V, P = BoxedPolicy> {
    shards: Box<[Mutex<PolicyCache<K, V, P>>]>,
}

impl<K, V, P> ShardedCache<K, V, P>
where
    K: Hash + Eq + Clone,
    P: Policy,
{
    /// Combines already built caches into a sharded cache, one shard each.
    /// Fails with [`ConfigError::ZeroShards`] if `shards` is empty.
    pub fn from_shards(shards: Vec<PolicyCache<K, V, P>>) -> Result<Self, ConfigError> {
        if shards.is_empty() {
            return Err(ConfigError::ZeroShards);
        }
        Ok(ShardedCache {
            shards: shards.into_iter().map(Mutex::new).collect(),
        })
    }

    /// Number of shards.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Index of the shard responsible for `key`. The mapping is stable for
    /// the lifetime of the process and across runs of the same build.
    pub fn shard_index(&self, key: &K) -> usize {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        (hasher.finish() % self.shards.len() as u64) as usize
    }

    fn shard(&self, key: &K) -> MutexGuard<'_, PolicyCache<K, V, P>> {
        lock(&self.shards[self.shard_index(key)])
    }

    /// Runs `f` with exclusive access to the shard that owns `key`.
    pub fn with_shard<R>(&self, key: &K, f: impl FnOnce(&mut PolicyCache<K, V, P>) -> R) -> R {
        f(&mut self.shard(key))
    }

    /// Looks up `key` and returns a clone of its value. See [`Cache::get`].
    pub fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        self.shard(key).get(key).cloned()
    }

    /// Looks up `key` without touching statistics or eviction order.
    pub fn peek(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        self.shard(key).peek(key).cloned()
    }

    /// Stores `value` under `key`. See [`Cache::put`].
    pub fn put(&self, key: K, value: V) -> Option<V> {
        self.shard(&key).put(key, value)
    }

    /// Stores `value` with its own time to live. See
    /// [`PolicyCache::put_with_ttl`].
    pub fn put_with_ttl(&self, key: K, value: V, ttl: Duration) -> Option<V> {
        self.shard(&key).put_with_ttl(key, value, ttl)
    }

    /// Removes `key` and returns its value.
    pub fn remove(&self, key: &K) -> Option<V> {
        self.shard(key).remove(key)
    }

    /// Returns `true` if `key` is present and not expired.
    pub fn contains_key(&self, key: &K) -> bool {
        self.shard(key).contains_key(key)
    }

    /// Returns the value for `key`, loading and storing it on a miss. Only
    /// the shard that owns the key is locked while the loader runs.
    pub fn get_or_insert_with<F>(&self, key: K, loader: F) -> V
    where
        V: Clone,
        F: FnOnce(&K) -> V,
    {
        self.shard(&key).get_or_insert_with(key, loader)
    }

    /// Fallible variant of [`get_or_insert_with`](Self::get_or_insert_with).
    pub fn get_or_try_insert_with<F, E>(&self, key: K, loader: F) -> Result<V, LoadError<E>>
    where
        V: Clone,
        F: FnOnce(&K) -> Result<V, E>,
    {
        self.shard(&key).get_or_try_insert_with(key, loader)
    }

    /// Total number of entries across all shards. Shards are locked one at
    /// a time, so the result is a snapshot only if no other thread writes
    /// meanwhile.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|shard| lock(shard).len()).sum()
    }

    /// Returns `true` when no shard holds an entry.
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|shard| lock(shard).is_empty())
    }

    /// Removes every entry from every shard.
    pub fn clear(&self) {
        for shard in self.shards.iter() {
            lock(shard).clear();
        }
    }

    /// Removes every expired entry and returns how many were removed.
    pub fn purge_expired(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| lock(shard).purge_expired())
            .sum()
    }

    /// Activity counters summed over all shards.
    pub fn stats(&self) -> Stats {
        self.shard_stats().into_iter().sum()
    }

    /// Activity counters of each shard, indexed by shard.
    pub fn shard_stats(&self) -> Vec<Stats> {
        self.shards.iter().map(|shard| lock(shard).stats()).collect()
    }

    /// Sets every activity counter of every shard back to zero.
    pub fn reset_stats(&self) {
        for shard in self.shards.iter() {
            lock(shard).reset_stats();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::CacheBuilder;
    use crate::cache::LruCache;
    use crate::clock::ManualClock;
    use std::sync::Arc;
    use std::thread;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn wrappers_are_send_and_sync() {
        assert_send_sync::<SyncCache<String, String>>();
        assert_send_sync::<ShardedCache<String, String>>();
        assert_send_sync::<SyncCache<u64, Vec<u8>, crate::policy::Lfu>>();
    }

    #[test]
    fn sync_cache_mirrors_the_single_threaded_api() {
        let cache = SyncCache::new(LruCache::new(2));
        assert_eq!(cache.put("a", 1), None);
        assert_eq!(cache.put("a", 2), Some(1));
        assert_eq!(cache.get(&"a"), Some(2));
        assert_eq!(cache.peek(&"a"), Some(2));
        assert!(cache.contains_key(&"a"));
        assert_eq!(cache.remove(&"a"), Some(2));
        assert!(cache.is_empty());
        assert_eq!(cache.get_or_insert_with("b", |_| 5), 5);
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert_eq!(cache.len(), 0);
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses), (1, 1));
        cache.reset_stats();
        assert_eq!(cache.stats(), Stats::new());
    }

    #[test]
    fn with_gives_access_to_the_full_api() {
        let cache = SyncCache::new(LruCache::new(3));
        cache.put("a", 1);
        cache.put("b", 2);
        let keys: Vec<&str> = cache.with(|inner| inner.keys().copied().collect());
        assert_eq!(keys, ["a", "b"]);
        let inner = cache.into_inner();
        assert_eq!(inner.len(), 2);
    }

    #[test]
    fn concurrent_writers_never_exceed_capacity() {
        let cache = Arc::new(SyncCache::new(LruCache::new(50)));
        let workers: Vec<_> = (0..4u32)
            .map(|worker| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for i in 0..500u32 {
                        cache.put(worker * 1000 + i, i);
                        cache.get(&(worker * 1000 + i / 2));
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let stats = cache.stats();
        assert_eq!(cache.len(), 50);
        assert_eq!(stats.insertions, 2000);
        assert_eq!(stats.evictions, 1950);
        assert_eq!(stats.requests(), 2000);
    }

    #[test]
    fn loader_runs_once_for_concurrent_callers() {
        let cache = Arc::new(SyncCache::new(LruCache::new(4)));
        let calls = Arc::new(Mutex::new(0u32));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let calls = Arc::clone(&calls);
                thread::spawn(move || {
                    cache.get_or_insert_with("config", |_| {
                        *calls.lock().unwrap() += 1;
                        42
                    })
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), 42);
        }
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn poisoned_lock_is_recovered() {
        let cache = Arc::new(SyncCache::new(LruCache::new(2)));
        cache.put("a", 1);
        let clone = Arc::clone(&cache);
        let result = thread::spawn(move || {
            clone.get_or_insert_with("b", |_| -> i32 { panic!("loader blew up") })
        })
        .join();
        assert!(result.is_err());
        assert_eq!(cache.get(&"a"), Some(1));
        assert!(!cache.contains_key(&"b"));
    }

    #[test]
    fn from_shards_requires_at_least_one_shard() {
        let result = ShardedCache::<u8, u8, crate::policy::Lru>::from_shards(Vec::new());
        assert_eq!(result.err(), Some(ConfigError::ZeroShards));
    }

    #[test]
    fn keys_are_routed_to_a_stable_shard() {
        let cache = CacheBuilder::<u32, u32>::new()
            .max_entries(64)
            .build_sharded(4)
            .unwrap();
        assert_eq!(cache.shard_count(), 4);
        for key in 0..64 {
            cache.put(key, key);
        }
        for key in 0..64 {
            let index = cache.shard_index(&key);
            assert!(index < 4);
            assert_eq!(index, cache.shard_index(&key));
            let present = cache.with_shard(&key, |shard| shard.peek(&key).copied());
            assert_eq!(present, cache.peek(&key));
        }
    }

    #[test]
    fn sharded_stats_are_summed_over_shards() {
        let cache = CacheBuilder::<u32, u32>::new()
            .max_entries(400)
            .build_sharded(4)
            .unwrap();
        for key in 0..100 {
            cache.put(key, key);
        }
        for key in 0..150 {
            cache.get(&key);
        }
        let total = cache.stats();
        assert_eq!(total.insertions, 100);
        assert_eq!(total.hits, 100);
        assert_eq!(total.misses, 50);
        assert_eq!(cache.len(), 100);
        let per_shard = cache.shard_stats();
        assert_eq!(per_shard.len(), 4);
        assert!(per_shard.iter().all(|stats| stats.insertions > 0));
        cache.reset_stats();
        assert_eq!(cache.stats(), Stats::new());
    }

    #[test]
    fn sharded_cache_supports_ttl_removal_and_loading() {
        let clock = ManualClock::new();
        let cache = CacheBuilder::<String, usize>::new()
            .max_entries(16)
            .ttl(Duration::from_secs(5))
            .clock(clock.clone())
            .build_sharded(2)
            .unwrap();
        let value = cache.get_or_insert_with("alpha".to_string(), |key| key.len());
        assert_eq!(value, 5);
        cache.put_with_ttl("beta".to_string(), 1, Duration::from_secs(60));
        assert_eq!(cache.remove(&"missing".to_string()), None);
        clock.advance(Duration::from_secs(5));
        assert_eq!(cache.purge_expired(), 1);
        assert!(cache.contains_key(&"beta".to_string()));
        assert!(!cache.is_empty());
        cache.clear();
        assert!(cache.is_empty());
        let failed: Result<usize, _> =
            cache.get_or_try_insert_with("gamma".to_string(), |_| Err("offline"));
        assert_eq!(failed.unwrap_err().into_inner(), "offline");
    }

    #[test]
    fn sharded_cache_handles_concurrent_mixed_work() {
        let cache = Arc::new(
            CacheBuilder::<u32, u32>::new()
                .max_entries(64)
                .build_sharded(8)
                .unwrap(),
        );
        let workers: Vec<_> = (0..4u32)
            .map(|worker| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for i in 0..1000u32 {
                        let key = (worker * 31 + i) % 200;
                        if cache.get(&key).is_none() {
                            cache.put(key, i);
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let stats = cache.stats();
        assert_eq!(stats.requests(), 4000);
        assert!(cache.len() <= 64);
        assert!(stats.evictions > 0);
    }
}
