//! The cache engine and the [`Cache`] trait it implements.

use std::collections::HashMap;
use std::hash::Hash;
use std::mem;
use std::sync::Arc;
use std::time::Duration;

use crate::clock::{Clock, SystemClock};
use crate::policy::{Fifo, Lfu, Lru, Policy};
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

    /// Removes every entry. Statistics are kept.
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
    /// Clock reading at which the entry stops being valid.
    expires_at: Option<Duration>,
}

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
    default_ttl: Option<Duration>,
    clock: Arc<dyn Clock>,
    stats: Stats,
}

/// Settings handed from the builder to the engine. The builder validates
/// them; the engine trusts what it is given.
pub(crate) struct Settings {
    pub(crate) max_entries: Option<usize>,
    pub(crate) ttl: Option<Duration>,
    pub(crate) clock: Arc<dyn Clock>,
}

impl Settings {
    /// Settings with no bounds, no expiry and the system clock.
    pub(crate) fn new() -> Self {
        Settings {
            max_entries: None,
            ttl: None,
            clock: Arc::new(SystemClock::new()),
        }
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

    pub(crate) fn from_settings(policy: P, settings: Settings) -> Self {
        PolicyCache {
            map: HashMap::new(),
            slots: Vec::new(),
            free: Vec::new(),
            policy,
            max_entries: settings.max_entries,
            default_ttl: settings.ttl,
            clock: settings.clock,
            stats: Stats::new(),
        }
    }

    /// Maximum number of entries the cache will hold, if bounded by count.
    pub fn max_entries(&self) -> Option<usize> {
        self.max_entries
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

    /// Removes the expired entry in `slot`.
    fn expire(&mut self, slot: usize) {
        self.detach(slot);
        self.stats.expirations += 1;
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

    fn store(&mut self, key: K, value: V, ttl: Option<Duration>) -> Option<V> {
        let expires_at = self.deadline(ttl);
        if let Some(slot) = self.live_slot(&key) {
            let entry = self.entry_mut(slot);
            entry.expires_at = expires_at;
            let old = mem::replace(&mut entry.value, value);
            self.policy.on_update(slot);
            self.stats.updates += 1;
            return Some(old);
        }
        while self.max_entries.is_some_and(|max| self.map.len() >= max) {
            if !self.evict_coldest() {
                break;
            }
        }
        self.attach(Entry {
            key,
            value,
            expires_at,
        });
        self.stats.insertions += 1;
        None
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
        entry
    }

    /// Makes room by removing the coldest entry. If that entry has already
    /// expired the removal is counted as an expiration rather than an
    /// eviction. Returns `false` if there was nothing left to remove.
    fn evict_coldest(&mut self) -> bool {
        match self.policy.coldest() {
            Some(slot) if self.is_expired(slot) => {
                self.expire(slot);
                true
            }
            Some(slot) => {
                self.detach(slot);
                self.stats.evictions += 1;
                true
            }
            None => false,
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
        Some(entry.value)
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn clear(&mut self) {
        self.map.clear();
        self.slots.clear();
        self.free.clear();
        self.policy.clear();
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
