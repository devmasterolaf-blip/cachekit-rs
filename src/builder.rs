//! Validated cache construction.

use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use crate::cache::{DynCache, PolicyCache, Settings, Weigher};
use crate::clock::Clock;
use crate::error::ConfigError;
use crate::event::{Listener, RemovalCause};
use crate::policy::{Policy, PolicyKind};

/// Collects cache settings and checks them before building a cache.
///
/// ```
/// use cachekit::{Cache, CacheBuilder, PolicyKind};
///
/// let mut cache = CacheBuilder::new()
///     .policy(PolicyKind::Lfu)
///     .max_entries(100)
///     .build()
///     .expect("valid configuration");
/// cache.put("answer", 42);
/// assert_eq!(cache.get(&"answer"), Some(&42));
/// ```
pub struct CacheBuilder<K, V> {
    policy: PolicyKind,
    max_entries: Option<usize>,
    max_weight: Option<u64>,
    weigher: Option<Weigher<K, V>>,
    ttl: Option<Duration>,
    clock: Option<Arc<dyn Clock>>,
    listeners: Vec<Listener<K, V>>,
}

impl<K, V> Default for CacheBuilder<K, V> {
    fn default() -> Self {
        CacheBuilder {
            policy: PolicyKind::Lru,
            max_entries: None,
            max_weight: None,
            weigher: None,
            ttl: None,
            clock: None,
            listeners: Vec::new(),
        }
    }
}

impl<K, V> CacheBuilder<K, V>
where
    K: Hash + Eq + Clone,
{
    /// Starts a configuration with the LRU policy and no capacity bound. A
    /// bound must be set before building.
    pub fn new() -> Self {
        CacheBuilder::default()
    }

    /// Chooses the built-in eviction policy used by [`build`](Self::build).
    pub fn policy(mut self, policy: PolicyKind) -> Self {
        self.policy = policy;
        self
    }

    /// Limits the number of entries.
    pub fn max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = Some(max_entries);
        self
    }

    /// Limits the total weight of all entries, as measured by the
    /// [`weigher`](Self::weigher). Can be combined with
    /// [`max_entries`](Self::max_entries); both bounds are then enforced.
    pub fn max_weight(mut self, max_weight: u64) -> Self {
        self.max_weight = Some(max_weight);
        self
    }

    /// Sets the function that measures an entry's weight, for example the
    /// byte length of the value.
    pub fn weigher<F>(mut self, weigher: F) -> Self
    where
        F: Fn(&K, &V) -> u64 + Send + Sync + 'static,
    {
        self.weigher = Some(Arc::new(weigher));
        self
    }

    /// Expires entries this long after they were last written. Individual
    /// entries can override it with
    /// [`put_with_ttl`](PolicyCache::put_with_ttl).
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Uses `clock` to measure time instead of the system clock. Mostly
    /// useful with [`ManualClock`](crate::ManualClock) in tests.
    pub fn clock(mut self, clock: impl Clock + 'static) -> Self {
        self.clock = Some(Arc::new(clock));
        self
    }

    /// Adds a callback that is told about every entry leaving the cache.
    /// May be called several times; listeners run in the order added.
    pub fn listener<F>(mut self, listener: F) -> Self
    where
        F: Fn(&K, &V, RemovalCause) + Send + Sync + 'static,
    {
        self.listeners.push(Arc::new(listener));
        self
    }

    /// Checks the configuration without building anything.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_entries.is_none() && self.max_weight.is_none() {
            return Err(ConfigError::Unbounded);
        }
        if self.max_entries == Some(0) {
            return Err(ConfigError::ZeroCapacity);
        }
        if self.max_weight == Some(0) {
            return Err(ConfigError::ZeroMaxWeight);
        }
        match (self.max_weight, &self.weigher) {
            (Some(_), None) => return Err(ConfigError::MissingWeigher),
            (None, Some(_)) => return Err(ConfigError::UnusedWeigher),
            _ => {}
        }
        if self.ttl.is_some_and(|ttl| ttl.is_zero()) {
            return Err(ConfigError::ZeroTtl);
        }
        Ok(())
    }

    /// Builds a cache using the policy selected with
    /// [`policy`](Self::policy).
    pub fn build(self) -> Result<DynCache<K, V>, ConfigError> {
        let policy = self.policy.build();
        self.build_with(policy)
    }

    /// Builds a cache around a specific policy instance, ignoring the policy
    /// kind stored in the builder. Use this for statically typed caches or
    /// for policies implemented outside this crate.
    pub fn build_with<P: Policy>(self, policy: P) -> Result<PolicyCache<K, V, P>, ConfigError> {
        self.validate()?;
        Ok(PolicyCache::from_settings(policy, self.into_settings()))
    }

    fn into_settings(self) -> Settings<K, V> {
        let mut settings = Settings::new();
        settings.max_entries = self.max_entries;
        settings.max_weight = self.max_weight;
        settings.weigher = self.weigher;
        settings.ttl = self.ttl;
        settings.listeners = self.listeners;
        if let Some(clock) = self.clock {
            settings.clock = clock;
        }
        settings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::clock::ManualClock;
    use crate::policy::Fifo;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn builds_each_built_in_policy() {
        for kind in PolicyKind::ALL {
            let cache = CacheBuilder::<u32, u32>::new()
                .policy(kind)
                .max_entries(4)
                .build()
                .unwrap();
            assert_eq!(cache.policy_name(), kind.as_str());
            assert_eq!(cache.max_entries(), Some(4));
        }
    }

    #[test]
    fn missing_bound_is_rejected() {
        let result = CacheBuilder::<u32, u32>::new().build();
        assert_eq!(result.err(), Some(ConfigError::Unbounded));
    }

    #[test]
    fn zero_entries_is_rejected() {
        let result = CacheBuilder::<u32, u32>::new().max_entries(0).build();
        assert_eq!(result.err(), Some(ConfigError::ZeroCapacity));
    }

    #[test]
    fn weight_settings_must_come_in_pairs() {
        let no_weigher = CacheBuilder::<u32, String>::new().max_weight(10).build();
        assert_eq!(no_weigher.err(), Some(ConfigError::MissingWeigher));

        let no_bound = CacheBuilder::<u32, String>::new()
            .max_entries(10)
            .weigher(|_, v| v.len() as u64)
            .build();
        assert_eq!(no_bound.err(), Some(ConfigError::UnusedWeigher));

        let zero = CacheBuilder::<u32, String>::new()
            .max_weight(0)
            .weigher(|_, v| v.len() as u64)
            .build();
        assert_eq!(zero.err(), Some(ConfigError::ZeroMaxWeight));
    }

    #[test]
    fn weight_bound_alone_is_enough() {
        let cache = CacheBuilder::<u32, String>::new()
            .max_weight(64)
            .weigher(|_, v| v.len() as u64)
            .build()
            .unwrap();
        assert_eq!(cache.max_entries(), None);
        assert_eq!(cache.max_weight(), Some(64));
    }

    #[test]
    fn zero_ttl_is_rejected() {
        let result = CacheBuilder::<u32, u32>::new()
            .max_entries(1)
            .ttl(Duration::ZERO)
            .build();
        assert_eq!(result.err(), Some(ConfigError::ZeroTtl));
    }

    #[test]
    fn ttl_and_clock_reach_the_cache() {
        let clock = ManualClock::new();
        let mut cache = CacheBuilder::new()
            .max_entries(2)
            .ttl(Duration::from_secs(10))
            .clock(clock.clone())
            .build()
            .unwrap();
        assert_eq!(cache.default_ttl(), Some(Duration::from_secs(10)));
        cache.put("k", 1);
        clock.advance(Duration::from_secs(10));
        assert_eq!(cache.get(&"k"), None);
    }

    #[test]
    fn listeners_from_the_builder_are_installed() {
        let evicted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&evicted);
        let mut cache = CacheBuilder::new()
            .max_entries(1)
            .listener(move |_: &u32, _: &u32, cause| {
                if cause == RemovalCause::Evicted {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            })
            .build()
            .unwrap();
        cache.put(1, 1);
        cache.put(2, 2);
        cache.put(3, 3);
        assert_eq!(evicted.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn build_with_uses_the_given_policy() {
        let mut cache = CacheBuilder::new()
            .policy(PolicyKind::Lru)
            .max_entries(1)
            .build_with(Fifo::new())
            .unwrap();
        cache.put(1, 1);
        assert_eq!(cache.policy_name(), "fifo");
    }
}
