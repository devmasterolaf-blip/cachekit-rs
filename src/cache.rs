//! The cache engine and the [`Cache`] trait it implements.

use std::collections::HashMap;
use std::hash::Hash;
use std::mem;

use crate::policy::{Lru, Policy};
use crate::stats::Stats;

/// Operations shared by every cache in this crate.
///
/// The trait is object safe, so `Box<dyn Cache<K, V>>` works when the
/// concrete cache type is chosen at run time.
pub trait Cache<K, V> {
    /// Looks up `key`, counting a hit or a miss and telling the eviction
    /// policy about the read.
    fn get(&mut self, key: &K) -> Option<&V>;

    /// Looks up `key` without touching statistics or eviction order.
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

    /// Number of entries currently stored.
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
    max_entries: usize,
    stats: Stats,
}

/// A cache that evicts the least recently used entry.
pub type LruCache<K, V> = PolicyCache<K, V, Lru>;

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
    /// the given policy instance.
    ///
    /// # Panics
    ///
    /// Panics if `max_entries` is zero.
    pub fn with_policy(policy: P, max_entries: usize) -> Self {
        assert!(max_entries > 0, "a cache needs room for at least one entry");
        PolicyCache {
            map: HashMap::new(),
            slots: Vec::new(),
            free: Vec::new(),
            policy,
            max_entries,
            stats: Stats::new(),
        }
    }

    /// Maximum number of entries the cache will hold.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Name of the eviction policy, for example `"lru"`.
    pub fn policy_name(&self) -> &'static str {
        self.policy.name()
    }

    /// The entry the policy would evict next.
    pub fn peek_coldest(&self) -> Option<(&K, &V)> {
        let slot = self.policy.coldest()?;
        let entry = self.entry(slot);
        Some((&entry.key, &entry.value))
    }

    /// Removes and returns the entry the policy would evict next. The removal
    /// is counted as an explicit removal, not as an eviction.
    pub fn pop_coldest(&mut self) -> Option<(K, V)> {
        let slot = self.policy.coldest()?;
        let entry = self.detach(slot);
        self.stats.removals += 1;
        Some((entry.key, entry.value))
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

    /// Evicts the coldest entry. Returns `false` if there was nothing left to
    /// evict.
    fn evict_coldest(&mut self) -> bool {
        match self.policy.coldest() {
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
        let slot = match self.map.get(key) {
            Some(&slot) => slot,
            None => {
                self.stats.misses += 1;
                return None;
            }
        };
        self.stats.hits += 1;
        self.policy.on_access(slot);
        Some(&self.entry(slot).value)
    }

    fn peek(&self, key: &K) -> Option<&V> {
        let slot = *self.map.get(key)?;
        Some(&self.entry(slot).value)
    }

    fn put(&mut self, key: K, value: V) -> Option<V> {
        if let Some(&slot) = self.map.get(&key) {
            let old = mem::replace(&mut self.entry_mut(slot).value, value);
            self.policy.on_update(slot);
            self.stats.updates += 1;
            return Some(old);
        }
        while self.map.len() >= self.max_entries {
            if !self.evict_coldest() {
                break;
            }
        }
        self.attach(Entry { key, value });
        self.stats.insertions += 1;
        None
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let slot = *self.map.get(key)?;
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
