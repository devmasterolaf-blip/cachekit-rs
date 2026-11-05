//! Validated cache construction.

use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use crate::cache::{DynCache, PolicyCache, Settings};
use crate::clock::Clock;
use crate::error::ConfigError;
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
    ttl: Option<Duration>,
    clock: Option<Arc<dyn Clock>>,
    marker: PhantomData<fn(K, V)>,
}

impl<K, V> Default for CacheBuilder<K, V> {
    fn default() -> Self {
        CacheBuilder {
            policy: PolicyKind::Lru,
            max_entries: None,
            ttl: None,
            clock: None,
            marker: PhantomData,
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

    /// Checks the configuration without building anything.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self.max_entries {
            None => return Err(ConfigError::Unbounded),
            Some(0) => return Err(ConfigError::ZeroCapacity),
            Some(_) => {}
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

    fn into_settings(self) -> Settings {
        let mut settings = Settings::new();
        settings.max_entries = self.max_entries;
        settings.ttl = self.ttl;
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
