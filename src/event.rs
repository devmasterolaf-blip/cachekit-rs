//! Removal notifications.

use std::fmt;
use std::sync::Arc;

/// Why an entry left the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemovalCause {
    /// The caller removed it with `remove`, `pop_coldest` or `clear`.
    Explicit,
    /// The caller stored a new value under the same key.
    Replaced,
    /// The eviction policy removed it to make room.
    Evicted,
    /// Its time to live ran out.
    Expired,
    /// It was never stored because it was heavier than the whole cache.
    Rejected,
}

impl RemovalCause {
    /// Returns `true` when the cache itself decided to drop the entry
    /// (eviction, expiry or rejection) rather than the caller.
    pub fn was_automatic(self) -> bool {
        matches!(
            self,
            RemovalCause::Evicted | RemovalCause::Expired | RemovalCause::Rejected
        )
    }

    /// Lowercase name of the cause.
    pub fn as_str(self) -> &'static str {
        match self {
            RemovalCause::Explicit => "explicit",
            RemovalCause::Replaced => "replaced",
            RemovalCause::Evicted => "evicted",
            RemovalCause::Expired => "expired",
            RemovalCause::Rejected => "rejected",
        }
    }
}

impl fmt::Display for RemovalCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A callback told about every entry that leaves the cache.
///
/// Listeners run synchronously inside the cache operation that removed the
/// entry, after the cache's own state has been updated. They receive
/// borrowed data and cannot call back into the cache, so they should record
/// what they need (a counter, a channel send, a log line) and return.
pub type Listener<K, V> = Arc<dyn Fn(&K, &V, RemovalCause) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_causes_are_flagged() {
        assert!(RemovalCause::Evicted.was_automatic());
        assert!(RemovalCause::Expired.was_automatic());
        assert!(RemovalCause::Rejected.was_automatic());
        assert!(!RemovalCause::Explicit.was_automatic());
        assert!(!RemovalCause::Replaced.was_automatic());
    }

    #[test]
    fn causes_display_as_lowercase_names() {
        assert_eq!(RemovalCause::Evicted.to_string(), "evicted");
        assert_eq!(RemovalCause::Replaced.as_str(), "replaced");
    }
}
