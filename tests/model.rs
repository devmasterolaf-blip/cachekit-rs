//! Checks the cache engine against a deliberately naive reference model.
//!
//! The model keeps its entries in a plain vector and finds eviction victims
//! by scanning, so it is slow but easy to believe. Both are driven with the
//! same pseudo-random operations and must agree after every step on the
//! results, the size, the weight, the eviction order and, at the end, the
//! statistics.

use std::time::Duration;

use cachekit::{Cache, CacheBuilder, DynCache, ManualClock, PolicyKind, Stats};

const KEYS: u64 = 12;

/// Weight of a value in the weighted configurations: 1 to 7.
fn weight_of(value: u64) -> u64 {
    value % 7 + 1
}

/// A small xorshift generator, so runs are reproducible without any
/// dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[derive(Debug, Clone, Copy)]
struct Config {
    policy: PolicyKind,
    max_entries: Option<usize>,
    max_weight: Option<u64>,
    ttl_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct ModelEntry {
    key: u64,
    value: u64,
    weight: u64,
    expires_at: Option<u64>,
    /// Number of uses, for LFU.
    uses: u64,
    /// When the entry reached its current use count, for LFU ties.
    counted_at: u64,
    /// When the entry was last read or written, for LRU.
    used_at: u64,
    /// When the entry was inserted, for FIFO.
    inserted_at: u64,
}

struct Model {
    config: Config,
    entries: Vec<ModelEntry>,
    now_ms: u64,
    step: u64,
    stats: Stats,
}

impl Model {
    fn new(config: Config) -> Self {
        Model {
            config,
            entries: Vec::new(),
            now_ms: 0,
            step: 0,
            stats: Stats::new(),
        }
    }

    fn weigh(&self, value: u64) -> u64 {
        if self.config.max_weight.is_some() {
            weight_of(value)
        } else {
            1
        }
    }

    fn total_weight(&self) -> u64 {
        self.entries.iter().map(|entry| entry.weight).sum()
    }

    fn is_expired(&self, entry: &ModelEntry) -> bool {
        entry.expires_at.is_some_and(|at| self.now_ms >= at)
    }

    /// Sort key that puts the next eviction victim first.
    fn coldness(&self, entry: &ModelEntry) -> (u64, u64) {
        match self.config.policy {
            PolicyKind::Lru => (entry.used_at, 0),
            PolicyKind::Fifo => (entry.inserted_at, 0),
            PolicyKind::Lfu => (entry.uses, entry.counted_at),
        }
    }

    fn touch(&mut self, index: usize) {
        self.step += 1;
        let entry = &mut self.entries[index];
        entry.uses += 1;
        entry.counted_at = self.step;
        entry.used_at = self.step;
    }

    /// Index of the live entry for `key`; an expired one is dropped.
    fn live_index(&mut self, key: u64) -> Option<usize> {
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        if self.is_expired(&self.entries[index]) {
            self.entries.remove(index);
            self.stats.expirations += 1;
            None
        } else {
            Some(index)
        }
    }

    fn over_limits(&self, extra_entries: usize, extra_weight: u64) -> bool {
        let too_many = self
            .config
            .max_entries
            .is_some_and(|max| self.entries.len() + extra_entries > max);
        let too_heavy = self
            .config
            .max_weight
            .is_some_and(|max| self.total_weight() + extra_weight > max);
        too_many || too_heavy
    }

    /// Removes the coldest entry other than `keep` to make room. Returns
    /// `false` when there is nothing left to remove.
    fn evict_coldest(&mut self, keep: Option<u64>) -> bool {
        let victim = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| Some(entry.key) != keep)
            .min_by_key(|(_, entry)| self.coldness(entry))
            .map(|(index, _)| index);
        let Some(index) = victim else {
            return false;
        };
        let entry = self.entries.remove(index);
        if self.is_expired(&entry) {
            self.stats.expirations += 1;
        } else {
            self.stats.evictions += 1;
        }
        true
    }

    fn get(&mut self, key: u64) -> Option<u64> {
        match self.live_index(key) {
            Some(index) => {
                self.stats.hits += 1;
                self.touch(index);
                Some(self.entries[index].value)
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }

    fn put(&mut self, key: u64, value: u64) -> Option<u64> {
        let weight = self.weigh(value);
        if self.config.max_weight.is_some_and(|max| weight > max) {
            self.stats.rejections += 1;
            let index = self.live_index(key)?;
            return Some(self.entries.remove(index).value);
        }
        let expires_at = self.config.ttl_ms.map(|ttl| self.now_ms + ttl);
        if let Some(index) = self.live_index(key) {
            let entry = &mut self.entries[index];
            let old = std::mem::replace(&mut entry.value, value);
            entry.weight = weight;
            entry.expires_at = expires_at;
            self.touch(index);
            self.stats.updates += 1;
            while self.over_limits(0, 0) && self.evict_coldest(Some(key)) {}
            return Some(old);
        }
        while self.over_limits(1, weight) && self.evict_coldest(None) {}
        self.step += 1;
        self.entries.push(ModelEntry {
            key,
            value,
            weight,
            expires_at,
            uses: 1,
            counted_at: self.step,
            used_at: self.step,
            inserted_at: self.step,
        });
        self.stats.insertions += 1;
        None
    }

    fn remove(&mut self, key: u64) -> Option<u64> {
        let index = self.live_index(key)?;
        self.stats.removals += 1;
        Some(self.entries.remove(index).value)
    }

    fn purge(&mut self) -> usize {
        let before = self.entries.len();
        let now = self.now_ms;
        self.entries
            .retain(|entry| !entry.expires_at.is_some_and(|at| now >= at));
        let purged = before - self.entries.len();
        self.stats.expirations += purged as u64;
        purged
    }

    /// Keys of the live entries, next eviction victim first.
    fn live_keys(&self) -> Vec<u64> {
        let mut live: Vec<&ModelEntry> = self
            .entries
            .iter()
            .filter(|entry| !self.is_expired(entry))
            .collect();
        live.sort_by_key(|entry| self.coldness(entry));
        live.iter().map(|entry| entry.key).collect()
    }
}

fn build(config: Config, clock: &ManualClock) -> DynCache<u64, u64> {
    let mut builder = CacheBuilder::new()
        .policy(config.policy)
        .clock(clock.clone());
    if let Some(max_entries) = config.max_entries {
        builder = builder.max_entries(max_entries);
    }
    if let Some(max_weight) = config.max_weight {
        builder = builder
            .max_weight(max_weight)
            .weigher(|_key: &u64, value: &u64| weight_of(*value));
    }
    if let Some(ttl_ms) = config.ttl_ms {
        builder = builder.ttl(Duration::from_millis(ttl_ms));
    }
    builder.build().unwrap()
}

fn check(config: Config, seed: u64, steps: usize) {
    let clock = ManualClock::new();
    let mut cache = build(config, &clock);
    let mut model = Model::new(config);
    let mut rng = Rng(seed);

    for step in 0..steps {
        let key = rng.below(KEYS);
        let context = format!("{config:?}, seed {seed}, step {step}");
        match rng.below(100) {
            0..=44 => {
                assert_eq!(cache.get(&key).copied(), model.get(key), "get {key}: {context}");
            }
            45..=84 => {
                let value = rng.below(1000);
                assert_eq!(
                    cache.put(key, value),
                    model.put(key, value),
                    "put {key} {value}: {context}"
                );
            }
            85..=91 => {
                assert_eq!(cache.remove(&key), model.remove(key), "remove {key}: {context}");
            }
            92..=97 => {
                let by = rng.below(40);
                clock.advance(Duration::from_millis(by));
                model.now_ms += by;
            }
            _ => {
                assert_eq!(cache.purge_expired(), model.purge(), "purge: {context}");
            }
        }

        assert_eq!(cache.len(), model.entries.len(), "len: {context}");
        assert_eq!(cache.weight(), model.total_weight(), "weight: {context}");
        let keys: Vec<u64> = cache.keys().copied().collect();
        assert_eq!(keys, model.live_keys(), "eviction order: {context}");
        if let Some(max_entries) = config.max_entries {
            assert!(cache.len() <= max_entries, "entry bound: {context}");
        }
        if let Some(max_weight) = config.max_weight {
            assert!(cache.weight() <= max_weight, "weight bound: {context}");
        }
    }
    assert_eq!(cache.stats(), model.stats, "stats: {config:?}, seed {seed}");
}

fn check_all_policies(max_entries: Option<usize>, max_weight: Option<u64>, ttl_ms: Option<u64>) {
    for policy in PolicyKind::ALL {
        let config = Config {
            policy,
            max_entries,
            max_weight,
            ttl_ms,
        };
        for seed in [1, 0x9E37_79B9_7F4A_7C15, 0xDEAD_BEEF_CAFE] {
            check(config, seed, 3000);
        }
    }
}

#[test]
fn entry_bound_matches_the_model() {
    check_all_policies(Some(5), None, None);
}

#[test]
fn capacity_of_one_matches_the_model() {
    check_all_policies(Some(1), None, None);
}

#[test]
fn weight_bound_matches_the_model() {
    check_all_policies(None, Some(12), None);
}

#[test]
fn weight_bound_with_rejections_matches_the_model() {
    // Values weighing 6 or 7 can never fit and must be rejected.
    check_all_policies(None, Some(5), None);
}

#[test]
fn both_bounds_match_the_model() {
    check_all_policies(Some(4), Some(14), None);
}

#[test]
fn entry_bound_with_ttl_matches_the_model() {
    check_all_policies(Some(5), None, Some(60));
}

#[test]
fn both_bounds_with_ttl_match_the_model() {
    check_all_policies(Some(6), Some(16), Some(45));
}

#[test]
fn roomy_cache_with_ttl_matches_the_model() {
    // Nothing is ever evicted for space, so only expiry removes entries.
    check_all_policies(Some(KEYS as usize), None, Some(25));
}
