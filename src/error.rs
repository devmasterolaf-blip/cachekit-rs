//! Error types shared across the crate.

use std::error::Error;
use std::fmt;

/// A cache configuration that cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// No capacity bound was configured, so the cache could grow forever.
    Unbounded,
    /// `max_entries` was set to zero.
    ZeroCapacity,
    /// `max_weight` was set to zero.
    ZeroMaxWeight,
    /// `max_weight` was set but no weigher was given to measure entries.
    MissingWeigher,
    /// A weigher was given but there is no `max_weight` for it to enforce.
    UnusedWeigher,
    /// The time to live was zero, which would expire entries immediately.
    ZeroTtl,
    /// A sharded cache was requested with zero shards.
    ZeroShards,
    /// There are more shards than capacity to share between them, which
    /// would leave some shards unable to hold anything.
    TooManyShards {
        /// Number of shards requested.
        shards: usize,
        /// The capacity bound (entries or weight) that is too small.
        capacity: u64,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Unbounded => {
                write!(
                    f,
                    "the cache has no capacity bound; set max_entries or max_weight"
                )
            }
            ConfigError::ZeroCapacity => {
                write!(f, "max_entries must be at least 1")
            }
            ConfigError::ZeroMaxWeight => {
                write!(f, "max_weight must be at least 1")
            }
            ConfigError::MissingWeigher => {
                write!(f, "max_weight needs a weigher to measure entries")
            }
            ConfigError::UnusedWeigher => {
                write!(f, "a weigher was configured without max_weight")
            }
            ConfigError::ZeroTtl => {
                write!(f, "the time to live must be longer than zero")
            }
            ConfigError::ZeroShards => {
                write!(f, "a sharded cache needs at least one shard")
            }
            ConfigError::TooManyShards { shards, capacity } => {
                write!(
                    f,
                    "{shards} shards cannot share a capacity of {capacity}; \
                     use fewer shards or a larger capacity"
                )
            }
        }
    }
}

impl Error for ConfigError {}

/// A policy name that is not one of the built-in policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePolicyError {
    name: String,
}

impl ParsePolicyError {
    pub(crate) fn new(name: &str) -> Self {
        ParsePolicyError {
            name: name.to_string(),
        }
    }

    /// The text that could not be parsed.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for ParsePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown eviction policy '{}' (expected lru, lfu or fifo)",
            self.name
        )
    }
}

impl Error for ParsePolicyError {}

/// A loader passed to
/// [`get_or_try_insert_with`](crate::PolicyCache::get_or_try_insert_with)
/// failed, so no value was cached.
///
/// The loader's own error is kept and can be inspected with
/// [`get_ref`](Self::get_ref), taken out with
/// [`into_inner`](Self::into_inner), or reached through
/// [`Error::source`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError<E> {
    source: E,
}

impl<E> LoadError<E> {
    pub(crate) fn new(source: E) -> Self {
        LoadError { source }
    }

    /// The error returned by the loader.
    pub fn get_ref(&self) -> &E {
        &self.source
    }

    /// Unwraps the error returned by the loader.
    pub fn into_inner(self) -> E {
        self.source
    }
}

impl<E: fmt::Display> fmt::Display for LoadError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cache loader failed: {}", self.source)
    }
}

impl<E: Error + 'static> Error for LoadError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_error_wraps_the_loader_error() {
        let err = LoadError::new(ConfigError::ZeroTtl);
        assert_eq!(err.get_ref(), &ConfigError::ZeroTtl);
        assert!(err.to_string().starts_with("cache loader failed: "));
        assert!(err.source().is_some());
        assert_eq!(err.into_inner(), ConfigError::ZeroTtl);
    }

    #[test]
    fn config_errors_have_actionable_messages() {
        assert!(ConfigError::Unbounded.to_string().contains("max_entries"));
        assert!(ConfigError::ZeroCapacity.to_string().contains("at least 1"));
        let shards = ConfigError::TooManyShards {
            shards: 8,
            capacity: 4,
        };
        assert_eq!(
            shards.to_string(),
            "8 shards cannot share a capacity of 4; use fewer shards or a larger capacity"
        );
    }

    #[test]
    fn parse_policy_error_names_the_input() {
        let err = ParsePolicyError::new("arc");
        assert_eq!(err.name(), "arc");
        assert!(err.to_string().contains("'arc'"));
    }
}
