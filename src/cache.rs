//! The cache engine and the [`Cache`] trait it implements.

use std::collections::HashMap;
use std::hash::Hash;
use std::mem;
use std::sync::Arc;
use std::time::Duration;

use crate::clock::{Clock, SystemClock};
use crate::error::LoadError;
use crate::event::{Listener, RemovalCause};
use crate::policy::{eviction_order, Fifo, Lfu, Lru, Policy};
use crate::stats::Stats;

/// Operations shared by every cache in this crate.
///
/// The trait is object safe, so `Box<dyn Cache<K, V>>` works when the
/// concrete cache type is chosen at run time.
pub trait Cache<K, V> {
    /// Looks up `key`, counting a hit or a miss and telling the eviction
    /// policy about the read. An entry whose time to live has passed is
    /// removed and reported as a miss.
    fn get(&mut self, key: &K) -> Option<&V>;

    /// Looks up `key` without touching statistics or eviction order. Expired
    /// entries are not returned, but they are not removed either.
    fn peek(&self, key: &K) -> Option<&V>;

    /// Stores `value` under `key` and returns the value it replaced, if any.
    /// Other entries are evicted as needed to stay within capacity.
    ///
    /// In a weight-bounded cache a value heavier than the whole cache is
    /// rejected: it is not stored, and the key ends up absent.
    fn put(&mut self, key: K, value: V) -> Option<V>;

    /// Removes `key` and returns its value.
    fn remove(&mut self, key: &K) -> Option<V>;

    /// Returns `true` if `key` is present. Like [`peek`](Cache::peek), this
    /// does not count as a read.
    fn contains_key(&self, key: &K) -> bool {
        self.peek(key).is_some()
    }

    /// Number of entries currently stored. Expired entries that have not
    /// been removed yet are included; they disappear when they are next
    /// looked up or when the cache is purged.
    fn len(&self) -> usize;

    /// Returns `true` when the cache holds no entries.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Removes every entry. Statistics are kept, and each dropped entry is
    /// counted as an explicit removal.
    fn clear(&mut self);

    /// A copy of the activity counters.
    fn stats(&self) -> Stats;

    /// Sets every activity counter back to zero.
    fn reset_stats(&mut self);
}

/// A key/value pair stored in a slot.
#[derive(Debug)]
struct Entry<K, V> {
    key: K,
    value: V,
    /// Weight assigned by the weigher when the value was written.
    weight: u64,
    /// Clock reading at which the entry stops being valid.
    expires_at: Option<Duration>,
}

/// Computes the weight of an entry for weight-bounded caches.
///
/// The weigher runs once per write. It should be cheap and must return the
/// same weight for the same value, since the cache keeps a running total.
pub type Weigher<K, V> = Arc<dyn Fn(&K, &V) -> u64 + Send + Sync>;

/// A bounded cache whose eviction order is decided by the policy `P`.
///
/// Entries live in a slab of slots; a hash map takes keys to slot indices and
/// the policy orders the same indices for eviction. All operations are O(1)
/// on average for the policies shipped with this crate.
///
/// ```
/// use cachekit::{Cache, LruCache};
///
/// let mut cache = LruCache::new(2);
/// cache.put("a", 1);
/// cache.put("b", 2);
/// cache.get(&"a");
/// cache.put("c", 3); // evicts "b", the least recently used entry
/// assert!(cache.contains_key(&"a"));
/// assert!(!cache.contains_key(&"b"));
/// ```
pub struct PolicyCache<K, V, P> {
    map: HashMap<K, usize>,
    slots: Vec<Option<Entry<K, V>>>,
    free: Vec<usize>,
    policy: P,
    max_entries: Option<usize>,
    max_weight: Option<u64>,
    weigher: Option<Weigher<K, V>>,
    total_weight: u64,
    default_ttl: Option<Duration>,
    clock: Arc<dyn Clock>,
    listeners: Vec<Listener<K, V>>,
    stats: Stats,
}

/// Settings handed from the builder to the engine. The builder validates
/// them; the engine trusts what it is given.
pub(crate) struct Settings<K, V> {
    pub(crate) max_entries: Option<usize>,
    pub(crate) max_weight: Option<u64>,
    pub(crate) weigher: Option<Weigher<K, V>>,
    pub(crate) ttl: Option<Duration>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) listeners: Vec<Listener<K, V>>,
}

impl<K, V> Settings<K, V> {
    /// Settings with no bounds, no expiry and the system clock.
    pub(crate) fn new() -> Self {
        Settings {
            max_entries: None,
            max_weight: None,
            weigher: None,
            ttl: None,
            clock: Arc::new(SystemClock::new()),
            listeners: Vec::new(),
        }
    }
}

/// A read-only view of one live entry, produced by
/// [`PolicyCache::entries`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryView<'a, K, V> {
    /// The entry's key.
    pub key: &'a K,
    /// The entry's value.
    pub value: &'a V,
    /// Weight recorded when the value was written.
    pub weight: u64,
    /// Time left before the entry expires, `None` if it never does.
    pub remaining_ttl: Option<Duration>,
}

/// Iterator over the live entries of a cache in eviction order, coldest
/// first. Created by [`PolicyCache::entries`].
pub struct Entries<'a, K, V, P> {
    cache: &'a PolicyCache<K, V, P>,
    cursor: Option<usize>,
    now: Duration,
}

impl<'a, K, V, P> Iterator for Entries<'a, K, V, P>
where
    K: Hash + Eq + Clone,
    P: Policy,
{
    type Item = EntryView<'a, K, V>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(slot) = self.cursor {
            self.cursor = self.cache.policy.next(slot);
            let entry = self.cache.entry(slot);
            let remaining_ttl = match entry.expires_at {
                None => None,
                Some(deadline) => match deadline.checked_sub(self.now) {
                    Some(left) if !left.is_zero() => Some(left),
                    _ => continue,
                },
            };
            return Some(EntryView {
                key: &entry.key,
                value: &entry.value,
                weight: entry.weight,
                remaining_ttl,
            });
        }
        None
    }
}

/// A cache that evicts the least recently used entry.
pub type LruCache<K, V> = PolicyCache<K, V, Lru>;

/// A cache whose built-in policy was chosen at run time, as produced by
/// [`CacheBuilder::build`](crate::CacheBuilder::build).
pub type DynCache<K, V> = PolicyCache<K, V, Box<dyn Policy + Send>>;

/// A cache that evicts the least frequently used entry.
pub type LfuCache<K, V> = PolicyCache<K, V, Lfu>;

/// A cache that evicts entries in insertion order.
pub type FifoCache<K, V> = PolicyCache<K, V, Fifo>;

impl<K, V, P> PolicyCache<K, V, P>
where
    K: Hash + Eq + Clone,
    P: Policy,
{
    /// Creates a cache holding at most `max_entries` entries, using the
    /// policy's default state.
    ///
    /// # Panics
    ///
    /// Panics if `max_entries` is zero.
    pub fn new(max_entries: usize) -> Self
    where
        P: Default,
    {
        Self::with_policy(P::default(), max_entries)
    }

    /// Creates a cache holding at most `max_entries` entries, evicting with
    /// the given policy instance. Entries never expire; use
    /// [`CacheBuilder`](crate::CacheBuilder) for expiry and other options.
    ///
    /// # Panics
    ///
    /// Panics if `max_entries` is zero.
    pub fn with_policy(policy: P, max_entries: usize) -> Self {
        assert!(max_entries > 0, "a cache needs room for at least one entry");
        let mut settings = Settings::new();
        settings.max_entries = Some(max_entries);
        Self::from_settings(policy, settings)
    }

    pub(crate) fn from_settings(policy: P, settings: Settings<K, V>) -> Self {
        PolicyCache {
            map: HashMap::new(),
            slots: Vec::new(),
            free: Vec::new(),
            policy,
            max_entries: settings.max_entries,
            max_weight: settings.max_weight,
            weigher: settings.weigher,
            total_weight: 0,
            default_ttl: settings.ttl,
            clock: settings.clock,
            listeners: settings.listeners,
            stats: Stats::new(),
        }
    }

    /// Maximum number of entries the cache will hold, if bounded by count.
    pub fn max_entries(&self) -> Option<usize> {
        self.max_entries
    }

    /// Maximum total weight the cache will hold, if bounded by weight.
    pub fn max_weight(&self) -> Option<u64> {
        self.max_weight
    }

    /// Sum of the weights of all stored entries. Without a weigher every
    /// entry weighs 1.
    pub fn weight(&self) -> u64 {
        self.total_weight
    }

    /// Weight recorded for `key` when it was last written.
    pub fn weight_of(&self, key: &K) -> Option<u64> {
        let slot = *self.map.get(key)?;
        if self.is_expired(slot) {
            return None;
        }
        Some(self.entry(slot).weight)
    }

    /// Iterates over the live entries in eviction order, coldest first,
    /// with their weight and remaining time to live. Expired entries are
    /// skipped. Iterating does not count as reading the entries.
    pub fn entries(&self) -> Entries<'_, K, V, P> {
        Entries {
            cache: self,
            cursor: self.policy.coldest(),
            now: self.clock.now(),
        }
    }

    /// Iterates over the live key/value pairs in eviction order, coldest
    /// first.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> + '_ {
        self.entries().map(|entry| (entry.key, entry.value))
    }

    /// Iterates over the live keys in eviction order, coldest first.
    pub fn keys(&self) -> impl Iterator<Item = &K> + '_ {
        self.entries().map(|entry| entry.key)
    }

    /// Registers a callback that is told about every entry leaving the
    /// cache, together with the [`RemovalCause`]. Listeners are called in
    /// the order they were added.
    pub fn add_listener<F>(&mut self, listener: F)
    where
        F: Fn(&K, &V, RemovalCause) + Send + Sync + 'static,
    {
        self.listeners.push(Arc::new(listener));
    }

    /// Name of the eviction policy, for example `"lru"`.
    pub fn policy_name(&self) -> &'static str {
        self.policy.name()
    }

    /// Time to live applied by [`put`](Cache::put), if any.
    pub fn default_ttl(&self) -> Option<Duration> {
        self.default_ttl
    }

    /// Stores `value` under `key` with its own time to live, overriding the
    /// cache default for this entry. Returns the live value it replaced.
    ///
    /// A zero `ttl` means the value is never valid: nothing is stored and the
    /// call behaves like [`remove`](Cache::remove).
    pub fn put_with_ttl(&mut self, key: K, value: V, ttl: Duration) -> Option<V> {
        if ttl.is_zero() {
            return self.remove(&key);
        }
        self.store(key, value, Some(ttl))
    }

    /// Returns the value for `key`, computing and storing it with `loader`
    /// if it is missing or expired.
    ///
    /// The lookup counts as a hit or a miss like [`get`](Cache::get). The
    /// loaded value is returned even when the cache declines to keep it
    /// because it is heavier than the whole cache.
    pub fn get_or_insert_with<F>(&mut self, key: K, loader: F) -> V
    where
        V: Clone,
        F: FnOnce(&K) -> V,
    {
        if let Some(value) = self.get(&key) {
            return value.clone();
        }
        let value = loader(&key);
        self.stats.loads += 1;
        self.put(key, value.clone());
        value
    }

    /// Like [`get_or_insert_with`](Self::get_or_insert_with) for loaders
    /// that can fail. A failed load stores nothing and is reported as a
    /// [`LoadError`] wrapping the loader's error, so the next lookup runs
    /// the loader again.
    pub fn get_or_try_insert_with<F, E>(&mut self, key: K, loader: F) -> Result<V, LoadError<E>>
    where
        V: Clone,
        F: FnOnce(&K) -> Result<V, E>,
    {
        if let Some(value) = self.get(&key) {
            return Ok(value.clone());
        }
        match loader(&key) {
            Ok(value) => {
                self.stats.loads += 1;
                self.put(key, value.clone());
                Ok(value)
            }
            Err(error) => {
                self.stats.load_failures += 1;
                Err(LoadError::new(error))
            }
        }
    }

    /// Time left before `key` expires. `None` if the key is absent, already
    /// expired, or has no expiry.
    pub fn remaining_ttl(&self, key: &K) -> Option<Duration> {
        let slot = *self.map.get(key)?;
        let deadline = self.entry(slot).expires_at?;
        deadline
            .checked_sub(self.clock.now())
            .filter(|left| !left.is_zero())
    }

    /// Removes every expired entry and returns how many were removed.
    ///
    /// Expired entries are otherwise only removed when they are looked up, so
    /// a cache that is written more than it is read should call this from
    /// time to time. The call scans all entries.
    pub fn purge_expired(&mut self) -> usize {
        let now = self.clock.now();
        let expired: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(slot, entry)| match entry {
                Some(entry) if entry.expires_at.is_some_and(|at| now >= at) => Some(slot),
                _ => None,
            })
            .collect();
        for &slot in &expired {
            self.expire(slot);
        }
        expired.len()
    }

    /// The live entry the policy would evict next.
    pub fn peek_coldest(&self) -> Option<(&K, &V)> {
        let mut cursor = self.policy.coldest();
        while let Some(slot) = cursor {
            if !self.is_expired(slot) {
                let entry = self.entry(slot);
                return Some((&entry.key, &entry.value));
            }
            cursor = self.policy.next(slot);
        }
        None
    }

    /// Removes and returns the live entry the policy would evict next. The
    /// removal is counted as an explicit removal, not as an eviction. Expired
    /// entries met on the way are dropped.
    pub fn pop_coldest(&mut self) -> Option<(K, V)> {
        loop {
            let slot = self.policy.coldest()?;
            if self.is_expired(slot) {
                self.expire(slot);
                continue;
            }
            let entry = self.detach(slot);
            self.stats.removals += 1;
            self.notify(&entry.key, &entry.value, RemovalCause::Explicit);
            return Some((entry.key, entry.value));
        }
    }

    /// Clock reading at which a value written now with `ttl` expires. A
    /// deadline too far away to represent means the entry never expires.
    fn deadline(&self, ttl: Option<Duration>) -> Option<Duration> {
        ttl.and_then(|ttl| self.clock.now().checked_add(ttl))
    }

    fn is_expired(&self, slot: usize) -> bool {
        self.entry(slot)
            .expires_at
            .is_some_and(|at| self.clock.now() >= at)
    }

    fn notify(&self, key: &K, value: &V, cause: RemovalCause) {
        for listener in &self.listeners {
            listener(key, value, cause);
        }
    }

    /// Removes the expired entry in `slot`.
    fn expire(&mut self, slot: usize) {
        let entry = self.detach(slot);
        self.stats.expirations += 1;
        self.notify(&entry.key, &entry.value, RemovalCause::Expired);
    }

    /// Slot holding a live entry for `key`. An expired entry found under the
    /// key is removed on the spot.
    fn live_slot(&mut self, key: &K) -> Option<usize> {
        let slot = *self.map.get(key)?;
        if self.is_expired(slot) {
            self.expire(slot);
            None
        } else {
            Some(slot)
        }
    }

    fn weigh(&self, key: &K, value: &V) -> u64 {
        match &self.weigher {
            Some(weigher) => weigher(key, value),
            None => 1,
        }
    }

    /// Returns `true` if the cache would exceed a bound after growing by the
    /// given number of entries and weight.
    fn exceeds_limits(&self, extra_entries: usize, extra_weight: u64) -> bool {
        let too_many = self
            .max_entries
            .is_some_and(|max| self.map.len() + extra_entries > max);
        let too_heavy = self
            .max_weight
            .is_some_and(|max| self.total_weight.saturating_add(extra_weight) > max);
        too_many || too_heavy
    }

    fn store(&mut self, key: K, value: V, ttl: Option<Duration>) -> Option<V> {
        let weight = self.weigh(&key, &value);
        if self.max_weight.is_some_and(|max| weight > max) {
            // The value can never fit, even in an empty cache. Whatever was
            // stored under the key is stale now, so it goes as well.
            self.stats.rejections += 1;
            self.notify(&key, &value, RemovalCause::Rejected);
            let slot = self.live_slot(&key)?;
            let stale = self.detach(slot);
            self.notify(&stale.key, &stale.value, RemovalCause::Replaced);
            return Some(stale.value);
        }
        let expires_at = self.deadline(ttl);
        if let Some(slot) = self.live_slot(&key) {
            let entry = self.entry_mut(slot);
            entry.expires_at = expires_at;
            let old_weight = mem::replace(&mut entry.weight, weight);
            let old = mem::replace(&mut entry.value, value);
            self.total_weight = (self.total_weight - old_weight).saturating_add(weight);
            self.policy.on_update(slot);
            self.stats.updates += 1;
            self.notify(&key, &old, RemovalCause::Replaced);
            self.evict_others_until_fit(slot);
            return Some(old);
        }
        while self.exceeds_limits(1, weight) {
            let Some(victim) = self.policy.coldest() else {
                break;
            };
            self.evict(victim);
        }
        self.attach(Entry {
            key,
            value,
            weight,
            expires_at,
        });
        self.stats.insertions += 1;
        None
    }

    /// After the entry in `keep` grew, evicts other entries in eviction order
    /// until the cache fits its bounds again. `keep` itself always fits
    /// because oversized values are rejected before they are stored.
    fn evict_others_until_fit(&mut self, keep: usize) {
        while self.exceeds_limits(0, 0) {
            let victim = match self.policy.coldest() {
                Some(slot) if slot == keep => self.policy.next(slot),
                other => other,
            };
            let Some(victim) = victim else {
                break;
            };
            self.evict(victim);
        }
    }

    fn entry(&self, slot: usize) -> &Entry<K, V> {
        self.slots[slot]
            .as_ref()
            .expect("slot referenced by the index must be occupied")
    }

    fn entry_mut(&mut self, slot: usize) -> &mut Entry<K, V> {
        self.slots[slot]
            .as_mut()
            .expect("slot referenced by the index must be occupied")
    }

    /// Stores `entry` in a free slot and returns the slot index.
    fn attach(&mut self, entry: Entry<K, V>) -> usize {
        let key = entry.key.clone();
        let slot = match self.free.pop() {
            Some(slot) => {
                self.slots[slot] = Some(entry);
                slot
            }
            None => {
                self.slots.push(Some(entry));
                self.slots.len() - 1
            }
        };
        self.map.insert(key, slot);
        self.policy.on_insert(slot);
        self.total_weight = self.total_weight.saturating_add(self.entry(slot).weight);
        slot
    }

    /// Takes the entry out of `slot`, releasing the slot for reuse.
    fn detach(&mut self, slot: usize) -> Entry<K, V> {
        let entry = self.slots[slot]
            .take()
            .expect("slot referenced by the index must be occupied");
        self.map.remove(&entry.key);
        self.policy.on_remove(slot);
        self.free.push(slot);
        self.total_weight -= entry.weight;
        entry
    }

    /// Makes room by removing the entry in `slot`. If that entry has already
    /// expired the removal is counted as an expiration rather than an
    /// eviction.
    fn evict(&mut self, slot: usize) {
        if self.is_expired(slot) {
            self.expire(slot);
        } else {
            let entry = self.detach(slot);
            self.stats.evictions += 1;
            self.notify(&entry.key, &entry.value, RemovalCause::Evicted);
        }
    }
}

impl<K, V, P> Cache<K, V> for PolicyCache<K, V, P>
where
    K: Hash + Eq + Clone,
    P: Policy,
{
    fn get(&mut self, key: &K) -> Option<&V> {
        let Some(slot) = self.live_slot(key) else {
            self.stats.misses += 1;
            return None;
        };
        self.stats.hits += 1;
        self.policy.on_access(slot);
        Some(&self.entry(slot).value)
    }

    fn peek(&self, key: &K) -> Option<&V> {
        let slot = *self.map.get(key)?;
        if self.is_expired(slot) {
            return None;
        }
        Some(&self.entry(slot).value)
    }

    fn put(&mut self, key: K, value: V) -> Option<V> {
        self.store(key, value, self.default_ttl)
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let slot = self.live_slot(key)?;
        let entry = self.detach(slot);
        self.stats.removals += 1;
        self.notify(&entry.key, &entry.value, RemovalCause::Explicit);
        Some(entry.value)
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn clear(&mut self) {
        if !self.listeners.is_empty() {
            for slot in eviction_order(&self.policy) {
                let entry = self.entry(slot);
                self.notify(&entry.key, &entry.value, RemovalCause::Explicit);
            }
        }
        self.stats.removals += self.map.len() as u64;
        self.map.clear();
        self.slots.clear();
        self.free.clear();
        self.policy.clear();
        self.total_weight = 0;
    }

    fn stats(&self) -> Stats {
        self.stats
    }

    fn reset_stats(&mut self) {
        self.stats = Stats::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::CacheBuilder;
    use crate::clock::ManualClock;
    use std::fmt::Debug;
    use std::sync::Mutex;

    const SECOND: Duration = Duration::from_secs(1);

    /// An LRU cache with a ten second default TTL driven by a manual clock.
    fn expiring_cache(max_entries: usize) -> (LruCache<&'static str, i32>, ManualClock) {
        let clock = ManualClock::new();
        let cache = CacheBuilder::new()
            .max_entries(max_entries)
            .ttl(10 * SECOND)
            .clock(clock.clone())
            .build_with(Lru::new())
            .unwrap();
        (cache, clock)
    }

    /// A cache bounded by the total length of its string values.
    fn weighted_cache<P: Policy>(policy: P, max_weight: u64) -> PolicyCache<&'static str, String, P> {
        CacheBuilder::new()
            .max_weight(max_weight)
            .weigher(|_key: &&'static str, value: &String| value.len() as u64)
            .build_with(policy)
            .unwrap()
    }

    type Events = Arc<Mutex<Vec<(String, RemovalCause)>>>;

    /// Records `key=value` and the cause for every removal notification.
    fn recorder<K: Debug, V: Debug>() -> (Events, impl Fn(&K, &V, RemovalCause) + Send + Sync) {
        let events = Events::default();
        let sink = Arc::clone(&events);
        let listener = move |key: &K, value: &V, cause: RemovalCause| {
            sink.lock()
                .unwrap()
                .push((format!("{key:?}={value:?}"), cause));
        };
        (events, listener)
    }

    fn drain(events: &Events) -> Vec<(String, RemovalCause)> {
        mem::take(&mut *events.lock().unwrap())
    }

    fn text(len: usize) -> String {
        "x".repeat(len)
    }

    #[test]
    fn put_then_get_returns_value() {
        let mut cache = LruCache::new(2);
        assert_eq!(cache.put("a", 1), None);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"missing"), None);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn put_replaces_and_returns_old_value() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        assert_eq!(cache.put("a", 2), Some(1));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.peek(&"a"), Some(&2));
    }

    #[test]
    fn lru_evicts_least_recently_used() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.get(&"a");
        cache.put("c", 3);
        assert!(cache.contains_key(&"a"));
        assert!(!cache.contains_key(&"b"));
        assert!(cache.contains_key(&"c"));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn peek_does_not_refresh_recency_or_stats() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        assert_eq!(cache.peek(&"a"), Some(&1));
        cache.put("c", 3);
        assert!(!cache.contains_key(&"a"), "peek must not protect the entry");
        assert_eq!(cache.stats().requests(), 0);
    }

    #[test]
    fn remove_frees_room_and_slot_is_reused() {
        let mut cache = LruCache::new(2);
        cache.put(1, "one");
        cache.put(2, "two");
        assert_eq!(cache.remove(&1), Some("one"));
        assert_eq!(cache.remove(&1), None);
        cache.put(3, "three");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.slots.len(), 2, "freed slot should be reused");
        assert_eq!(cache.stats().evictions, 0);
    }

    #[test]
    fn stats_track_every_kind_of_operation() {
        let mut cache = LruCache::new(1);
        cache.put("a", 1); // insertion
        cache.put("a", 2); // update
        cache.get(&"a"); // hit
        cache.get(&"b"); // miss
        cache.put("b", 3); // insertion + eviction
        cache.remove(&"b"); // removal
        let stats = cache.stats();
        assert_eq!(stats.insertions, 2);
        assert_eq!(stats.updates, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.evictions, 1);
        assert_eq!(stats.removals, 1);
        cache.reset_stats();
        assert_eq!(cache.stats(), Stats::new());
    }

    #[test]
    fn clear_keeps_stats_and_allows_reuse() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.get(&"a");
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().removals, 1);
        cache.put("b", 2);
        assert_eq!(cache.get(&"b"), Some(&2));
    }

    #[test]
    fn coldest_entry_can_be_inspected_and_popped() {
        let mut cache = LruCache::new(3);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.get(&"a");
        assert_eq!(cache.peek_coldest(), Some((&"b", &2)));
        assert_eq!(cache.pop_coldest(), Some(("b", 2)));
        assert_eq!(cache.pop_coldest(), Some(("a", 1)));
        assert_eq!(cache.pop_coldest(), None);
    }

    #[test]
    fn fifo_ignores_reads_when_evicting() {
        let mut cache = FifoCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.get(&"a");
        cache.put("a", 10);
        cache.put("c", 3);
        assert!(!cache.contains_key(&"a"), "oldest key goes first");
        assert!(cache.contains_key(&"b"));
        assert_eq!(cache.policy_name(), "fifo");
    }

    #[test]
    fn lfu_keeps_frequently_read_entries() {
        let mut cache = LfuCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.get(&"a");
        cache.get(&"a");
        cache.get(&"b");
        cache.put("c", 3);
        assert!(!cache.contains_key(&"b"), "b has fewer uses than a");
        cache.put("d", 4);
        assert!(!cache.contains_key(&"c"), "the newcomer has the fewest uses");
        assert!(cache.contains_key(&"a"));
        assert!(cache.contains_key(&"d"));
    }

    #[test]
    fn entry_expires_exactly_at_its_deadline() {
        let (mut cache, clock) = expiring_cache(4);
        cache.put("a", 1);
        clock.advance(9 * SECOND);
        assert_eq!(cache.get(&"a"), Some(&1));
        clock.advance(SECOND);
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.len(), 0, "a failed lookup removes the expired entry");
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.expirations), (1, 1, 1));
    }

    #[test]
    fn reads_do_not_extend_life_but_writes_do() {
        let (mut cache, clock) = expiring_cache(4);
        cache.put("read", 1);
        cache.put("written", 1);
        clock.advance(6 * SECOND);
        cache.get(&"read");
        assert_eq!(cache.put("written", 2), Some(1));
        clock.advance(6 * SECOND);
        assert_eq!(cache.get(&"read"), None);
        assert_eq!(cache.get(&"written"), Some(&2));
    }

    #[test]
    fn peek_hides_expired_entries_without_removing_them() {
        let (mut cache, clock) = expiring_cache(4);
        cache.put("a", 1);
        clock.advance(10 * SECOND);
        assert_eq!(cache.peek(&"a"), None);
        assert!(!cache.contains_key(&"a"));
        assert_eq!(cache.len(), 1, "peek cannot remove, so the entry lingers");
        assert_eq!(cache.stats().expirations, 0);
    }

    #[test]
    fn per_entry_ttl_overrides_the_default() {
        let (mut cache, clock) = expiring_cache(4);
        cache.put("default", 1);
        cache.put_with_ttl("short", 2, 2 * SECOND);
        cache.put_with_ttl("long", 3, 60 * SECOND);
        assert_eq!(cache.remaining_ttl(&"short"), Some(2 * SECOND));
        clock.advance(2 * SECOND);
        assert_eq!(cache.remaining_ttl(&"short"), None);
        assert_eq!(cache.remaining_ttl(&"default"), Some(8 * SECOND));
        clock.advance(20 * SECOND);
        assert!(!cache.contains_key(&"default"));
        assert!(cache.contains_key(&"long"));
        assert_eq!(cache.remaining_ttl(&"missing"), None);
    }

    #[test]
    fn entries_without_ttl_have_no_remaining_ttl() {
        let mut cache = LruCache::new(1);
        cache.put("a", 1);
        assert_eq!(cache.default_ttl(), None);
        assert_eq!(cache.remaining_ttl(&"a"), None);
    }

    #[test]
    fn zero_ttl_put_acts_as_remove() {
        let (mut cache, _clock) = expiring_cache(4);
        cache.put("a", 1);
        assert_eq!(cache.put_with_ttl("a", 2, Duration::ZERO), Some(1));
        assert!(cache.is_empty());
        assert_eq!(cache.put_with_ttl("b", 2, Duration::ZERO), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn purge_removes_only_expired_entries() {
        let (mut cache, clock) = expiring_cache(8);
        cache.put("old1", 1);
        cache.put("old2", 2);
        clock.advance(5 * SECOND);
        cache.put("young", 3);
        clock.advance(5 * SECOND);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.purge_expired(), 2);
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key(&"young"));
        assert_eq!(cache.stats().expirations, 2);
        assert_eq!(cache.purge_expired(), 0);
    }

    #[test]
    fn writing_over_an_expired_key_is_an_insertion() {
        let (mut cache, clock) = expiring_cache(4);
        cache.put("a", 1);
        clock.advance(10 * SECOND);
        assert_eq!(cache.put("a", 2), None, "the old value is already gone");
        let stats = cache.stats();
        assert_eq!((stats.insertions, stats.updates, stats.expirations), (2, 0, 1));
        assert_eq!(cache.get(&"a"), Some(&2));
    }

    #[test]
    fn removing_an_expired_key_returns_nothing() {
        let (mut cache, clock) = expiring_cache(4);
        cache.put("a", 1);
        clock.advance(10 * SECOND);
        assert_eq!(cache.remove(&"a"), None);
        assert_eq!(cache.stats().removals, 0);
        assert_eq!(cache.stats().expirations, 1);
    }

    #[test]
    fn expired_victim_counts_as_expiration_not_eviction() {
        let (mut cache, clock) = expiring_cache(1);
        cache.put("a", 1);
        clock.advance(10 * SECOND);
        cache.put("b", 2);
        let stats = cache.stats();
        assert_eq!((stats.evictions, stats.expirations), (0, 1));
    }

    #[test]
    fn coldest_accessors_skip_expired_entries() {
        let (mut cache, clock) = expiring_cache(4);
        cache.put_with_ttl("dying", 1, SECOND);
        cache.put("alive", 2);
        clock.advance(SECOND);
        assert_eq!(cache.peek_coldest(), Some((&"alive", &2)));
        assert_eq!(cache.pop_coldest(), Some(("alive", 2)));
        assert!(cache.is_empty());
        assert_eq!(cache.pop_coldest(), None);
    }

    #[test]
    fn weight_bound_evicts_until_the_new_entry_fits() {
        let mut cache = weighted_cache(Lru::new(), 10);
        cache.put("a", text(4));
        cache.put("b", text(4));
        assert_eq!(cache.weight(), 8);
        cache.put("c", text(4));
        assert!(!cache.contains_key(&"a"));
        assert_eq!(cache.weight(), 8);
        assert_eq!(cache.weight_of(&"c"), Some(4));
        assert_eq!(cache.stats().evictions, 1);
    }

    #[test]
    fn one_heavy_entry_can_evict_several_light_ones() {
        let mut cache = weighted_cache(Lru::new(), 10);
        for key in ["a", "b", "c", "d", "e"] {
            cache.put(key, text(2));
        }
        cache.put("big", text(7));
        assert_eq!(cache.len(), 2);
        assert!(cache.contains_key(&"e"));
        assert!(cache.contains_key(&"big"));
        assert_eq!(cache.weight(), 9);
        assert_eq!(cache.stats().evictions, 4);
    }

    #[test]
    fn entry_weighing_exactly_the_bound_is_accepted() {
        let mut cache = weighted_cache(Lru::new(), 10);
        cache.put("a", text(3));
        cache.put("full", text(10));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.weight(), 10);
        assert_eq!(cache.stats().rejections, 0);
    }

    #[test]
    fn oversized_value_is_rejected_and_stale_value_dropped() {
        let mut cache = weighted_cache(Lru::new(), 10);
        cache.put("a", text(3));
        cache.put("b", text(3));
        assert_eq!(cache.put("c", text(11)), None);
        assert!(!cache.contains_key(&"c"));
        assert_eq!(cache.len(), 2, "a rejected write must not evict anything");
        assert_eq!(cache.put("a", text(11)), Some(text(3)));
        assert!(!cache.contains_key(&"a"));
        assert_eq!(cache.weight(), 3);
        let stats = cache.stats();
        assert_eq!((stats.rejections, stats.evictions, stats.insertions), (2, 0, 2));
    }

    #[test]
    fn growing_update_keeps_the_updated_entry_under_fifo() {
        let mut cache = weighted_cache(Fifo::new(), 10);
        cache.put("first", text(2));
        cache.put("second", text(4));
        cache.put("third", text(4));
        // "first" is the next FIFO victim, but it is the entry being written.
        assert_eq!(cache.put("first", text(6)), Some(text(2)));
        assert!(cache.contains_key(&"first"));
        assert!(!cache.contains_key(&"second"));
        assert!(cache.contains_key(&"third"));
        assert_eq!(cache.weight(), 10);
    }

    #[test]
    fn growing_update_keeps_the_updated_entry_under_lfu() {
        let mut cache = weighted_cache(Lfu::new(), 10);
        cache.put("cold", text(2));
        cache.put("hot", text(8));
        for _ in 0..5 {
            cache.get(&"hot");
        }
        cache.put("cold", text(9));
        assert!(cache.contains_key(&"cold"));
        assert!(!cache.contains_key(&"hot"));
        assert_eq!(cache.weight(), 9);
    }

    #[test]
    fn shrinking_update_releases_weight() {
        let mut cache = weighted_cache(Lru::new(), 10);
        cache.put("a", text(8));
        cache.put("a", text(1));
        assert_eq!(cache.weight(), 1);
        cache.put("b", text(9));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.stats().evictions, 0);
    }

    #[test]
    fn zero_weight_entries_never_force_weight_eviction() {
        let mut cache = weighted_cache(Lru::new(), 5);
        cache.put("full", text(5));
        for key in ["e1", "e2", "e3"] {
            cache.put(key, String::new());
        }
        assert_eq!(cache.len(), 4);
        assert_eq!(cache.weight(), 5);
    }

    #[test]
    fn entry_and_weight_bounds_apply_together() {
        let mut cache: LruCache<&str, String> = CacheBuilder::new()
            .max_entries(2)
            .max_weight(10)
            .weigher(|_: &&str, value: &String| value.len() as u64)
            .build_with(Lru::new())
            .unwrap();
        cache.put("a", text(1));
        cache.put("b", text(1));
        cache.put("c", text(1));
        assert_eq!(cache.len(), 2, "entry bound");
        cache.put("d", text(10));
        assert_eq!(cache.len(), 1, "weight bound");
        assert!(cache.contains_key(&"d"));
    }

    #[test]
    fn removal_and_clear_release_weight() {
        let mut cache = weighted_cache(Lru::new(), 10);
        cache.put("a", text(4));
        cache.put("b", text(5));
        cache.remove(&"a");
        assert_eq!(cache.weight(), 5);
        cache.clear();
        assert_eq!(cache.weight(), 0);
        cache.put("c", text(10));
        assert_eq!(cache.weight(), 10);
    }

    #[test]
    fn unweighted_cache_counts_one_per_entry() {
        let mut cache = LruCache::new(3);
        cache.put(1, 1);
        cache.put(2, 2);
        assert_eq!(cache.weight(), 2);
        assert_eq!(cache.weight_of(&1), Some(1));
        assert_eq!(cache.weight_of(&9), None);
        assert_eq!(cache.max_weight(), None);
    }

    #[test]
    fn listener_sees_eviction_replacement_and_explicit_removal() {
        let mut cache = LruCache::new(2);
        let (events, listener) = recorder();
        cache.add_listener(listener);
        cache.put("a", 1);
        cache.put("b", 2);
        assert!(drain(&events).is_empty(), "plain inserts remove nothing");
        cache.put("a", 10);
        cache.put("c", 3);
        cache.remove(&"a");
        cache.pop_coldest();
        assert_eq!(
            drain(&events),
            vec![
                ("\"a\"=1".to_string(), RemovalCause::Replaced),
                ("\"b\"=2".to_string(), RemovalCause::Evicted),
                ("\"a\"=10".to_string(), RemovalCause::Explicit),
                ("\"c\"=3".to_string(), RemovalCause::Explicit),
            ]
        );
    }

    #[test]
    fn listener_sees_expiry_from_lookup_and_purge() {
        let (mut cache, clock) = expiring_cache(4);
        let (events, listener) = recorder();
        cache.add_listener(listener);
        cache.put("a", 1);
        cache.put("b", 2);
        clock.advance(10 * SECOND);
        cache.get(&"a");
        cache.purge_expired();
        assert_eq!(
            drain(&events),
            vec![
                ("\"a\"=1".to_string(), RemovalCause::Expired),
                ("\"b\"=2".to_string(), RemovalCause::Expired),
            ]
        );
    }

    #[test]
    fn listener_sees_rejection_and_the_stale_value() {
        let mut cache = weighted_cache(Lru::new(), 4);
        let (events, listener) = recorder();
        cache.add_listener(listener);
        cache.put("a", text(1));
        cache.put("a", text(5));
        assert_eq!(
            drain(&events),
            vec![
                ("\"a\"=\"xxxxx\"".to_string(), RemovalCause::Rejected),
                ("\"a\"=\"x\"".to_string(), RemovalCause::Replaced),
            ]
        );
    }

    #[test]
    fn clear_notifies_in_eviction_order() {
        let mut cache = LruCache::new(3);
        let (events, listener) = recorder();
        cache.add_listener(listener);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.get(&"a");
        cache.clear();
        let keys: Vec<String> = drain(&events).into_iter().map(|(entry, _)| entry).collect();
        assert_eq!(keys, vec!["\"b\"=2", "\"a\"=1"]);
    }

    #[test]
    fn several_listeners_run_in_registration_order() {
        let mut cache = LruCache::new(1);
        let order = Arc::new(Mutex::new(Vec::new()));
        for id in 0..3 {
            let order = Arc::clone(&order);
            cache.add_listener(move |_: &u8, _: &u8, _| order.lock().unwrap().push(id));
        }
        cache.put(1, 1);
        cache.put(2, 2);
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn loader_runs_only_on_a_miss() {
        let mut cache = LruCache::new(2);
        let mut calls = 0;
        for _ in 0..3 {
            let value = cache.get_or_insert_with("k", |key| {
                calls += 1;
                key.len() * 10
            });
            assert_eq!(value, 10);
        }
        assert_eq!(calls, 1);
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.loads), (2, 1, 1));
        assert_eq!(stats.insertions, 1);
    }

    #[test]
    fn loader_runs_again_after_expiry() {
        let (mut cache, clock) = expiring_cache(2);
        assert_eq!(cache.get_or_insert_with("k", |_| 1), 1);
        clock.advance(10 * SECOND);
        assert_eq!(cache.get_or_insert_with("k", |_| 2), 2);
        assert_eq!(cache.stats().loads, 2);
    }

    #[test]
    fn failed_load_caches_nothing_and_keeps_the_error() {
        let mut cache: LruCache<&str, i32> = LruCache::new(2);
        let result = cache.get_or_try_insert_with("k", |_| "nope".parse::<i32>());
        let error = result.unwrap_err();
        assert!(error.to_string().starts_with("cache loader failed: "));
        assert!(!cache.contains_key(&"k"));
        assert_eq!(cache.stats().load_failures, 1);

        let retry = cache.get_or_try_insert_with("k", |_| "7".parse::<i32>());
        assert_eq!(retry, Ok(7));
        assert_eq!(cache.peek(&"k"), Some(&7));
        let stats = cache.stats();
        assert_eq!((stats.loads, stats.load_failures, stats.misses), (1, 1, 2));
    }

    #[test]
    fn oversized_loaded_value_is_returned_but_not_cached() {
        let mut cache = weighted_cache(Lru::new(), 4);
        let value = cache.get_or_insert_with("k", |_| text(9));
        assert_eq!(value, text(9));
        assert!(cache.is_empty());
        assert_eq!(cache.stats().rejections, 1);
    }

    #[test]
    fn iteration_follows_eviction_order() {
        let mut lru = LruCache::new(3);
        let mut lfu = LfuCache::new(3);
        let mut fifo = FifoCache::new(3);
        for key in ["a", "b", "c"] {
            lru.put(key, 0);
            lfu.put(key, 0);
            fifo.put(key, 0);
        }
        for key in ["a", "a", "c"] {
            lru.get(&key);
            lfu.get(&key);
            fifo.get(&key);
        }
        assert_eq!(lru.keys().copied().collect::<Vec<_>>(), ["b", "a", "c"]);
        assert_eq!(lfu.keys().copied().collect::<Vec<_>>(), ["b", "c", "a"]);
        assert_eq!(fifo.keys().copied().collect::<Vec<_>>(), ["a", "b", "c"]);
    }

    #[test]
    fn iteration_skips_expired_entries_and_reports_remaining_ttl() {
        let (mut cache, clock) = expiring_cache(4);
        cache.put_with_ttl("short", 1, 2 * SECOND);
        cache.put("default", 2);
        clock.advance(3 * SECOND);
        let entries: Vec<_> = cache.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, &"default");
        assert_eq!(entries[0].remaining_ttl, Some(7 * SECOND));
        assert_eq!(entries[0].weight, 1);
        assert_eq!(cache.len(), 2, "iteration does not remove anything");
    }

    #[test]
    fn iteration_does_not_touch_stats_or_order() {
        let mut cache = LruCache::new(2);
        cache.put("a", 1);
        cache.put("b", 2);
        let pairs: Vec<_> = cache.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(pairs, [("a", 1), ("b", 2)]);
        assert_eq!(cache.stats().requests(), 0);
        cache.put("c", 3);
        assert!(!cache.contains_key(&"a"));
    }

    #[test]
    #[should_panic(expected = "at least one entry")]
    fn zero_capacity_is_rejected() {
        let _ = LruCache::<u8, u8>::new(0);
    }

    #[test]
    fn works_as_trait_object() {
        let mut cache: Box<dyn Cache<String, u32>> = Box::new(LruCache::new(1));
        cache.put("k".to_string(), 7);
        assert_eq!(cache.get(&"k".to_string()), Some(&7));
        assert_eq!(cache.len(), 1);
    }
}
