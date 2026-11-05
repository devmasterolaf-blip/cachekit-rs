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
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Unbounded => {
                write!(f, "the cache has no capacity bound; set max_entries")
            }
            ConfigError::ZeroCapacity => {
                write!(f, "max_entries must be at least 1")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_errors_have_actionable_messages() {
        assert!(ConfigError::Unbounded.to_string().contains("max_entries"));
        assert!(ConfigError::ZeroCapacity.to_string().contains("at least 1"));
    }

    #[test]
    fn parse_policy_error_names_the_input() {
        let err = ParsePolicyError::new("arc");
        assert_eq!(err.name(), "arc");
        assert!(err.to_string().contains("'arc'"));
    }
}
