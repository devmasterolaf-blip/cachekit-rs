//! Eviction policies.
//!
//! A policy does not own keys or values. The cache engine stores entries in
//! numbered slots and tells the policy what happens to each slot; the policy
//! keeps the slots in *eviction order* and answers one question: which slot
//! should go first. Slot indices are small, dense and reused after removal,
//! which lets policies keep their bookkeeping in plain vectors.

use std::fmt;
use std::str::FromStr;

use crate::error::ParsePolicyError;

mod fifo;
mod lfu;
mod lru;

pub use fifo::Fifo;
pub use lfu::Lfu;
pub use lru::Lru;

/// Bookkeeping needed to choose eviction victims.
///
/// The engine upholds these rules, and implementations may rely on them:
///
/// * `on_insert` is called once for a slot before any other call about it;
/// * `on_access`, `on_update` and `on_remove` are only called for slots that
///   were inserted and not yet removed;
/// * after `on_remove` the same index may be passed to `on_insert` again.
pub trait Policy {
    /// Short lowercase name of the policy, for example `"lru"`.
    fn name(&self) -> &'static str;

    /// A new entry was stored in `slot`.
    fn on_insert(&mut self, slot: usize);

    /// The entry in `slot` was read.
    fn on_access(&mut self, slot: usize);

    /// The value in `slot` was replaced by a newer one for the same key.
    fn on_update(&mut self, slot: usize);

    /// The entry in `slot` was removed, for whatever reason.
    fn on_remove(&mut self, slot: usize);

    /// The slot that should be evicted first, or `None` if nothing is tracked.
    fn coldest(&self) -> Option<usize>;

    /// The slot that follows `slot` in eviction order, i.e. the next victim
    /// if `slot` had to be kept. `None` when `slot` is last or unknown.
    fn next(&self, slot: usize) -> Option<usize>;

    /// Number of tracked slots.
    fn len(&self) -> usize;

    /// Returns `true` when no slot is tracked.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forgets every slot.
    fn clear(&mut self);
}

impl<P: Policy + ?Sized> Policy for Box<P> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn on_insert(&mut self, slot: usize) {
        (**self).on_insert(slot)
    }

    fn on_access(&mut self, slot: usize) {
        (**self).on_access(slot)
    }

    fn on_update(&mut self, slot: usize) {
        (**self).on_update(slot)
    }

    fn on_remove(&mut self, slot: usize) {
        (**self).on_remove(slot)
    }

    fn coldest(&self) -> Option<usize> {
        (**self).coldest()
    }

    fn next(&self, slot: usize) -> Option<usize> {
        (**self).next(slot)
    }

    fn len(&self) -> usize {
        (**self).len()
    }

    fn clear(&mut self) {
        (**self).clear()
    }
}

/// A policy behind a pointer, used when the policy type is only known at
/// run time.
pub type BoxedPolicy = Box<dyn Policy + Send>;

/// The eviction policies that ship with the crate, for choosing one at run
/// time (from configuration or a command line flag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyKind {
    /// Least recently used, see [`Lru`].
    Lru,
    /// Least frequently used, see [`Lfu`].
    Lfu,
    /// First in, first out, see [`Fifo`].
    Fifo,
}

impl PolicyKind {
    /// Every built-in policy, in a stable order.
    pub const ALL: [PolicyKind; 3] = [PolicyKind::Lru, PolicyKind::Lfu, PolicyKind::Fifo];

    /// Lowercase name, the same string [`Policy::name`] returns.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyKind::Lru => "lru",
            PolicyKind::Lfu => "lfu",
            PolicyKind::Fifo => "fifo",
        }
    }

    /// Creates a fresh, empty policy of this kind.
    pub fn build(self) -> BoxedPolicy {
        match self {
            PolicyKind::Lru => Box::new(Lru::new()),
            PolicyKind::Lfu => Box::new(Lfu::new()),
            PolicyKind::Fifo => Box::new(Fifo::new()),
        }
    }
}

impl fmt::Display for PolicyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PolicyKind {
    type Err = ParsePolicyError;

    /// Parses a policy name, ignoring ASCII case and surrounding whitespace.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let name = text.trim();
        PolicyKind::ALL
            .into_iter()
            .find(|kind| kind.as_str().eq_ignore_ascii_case(name))
            .ok_or_else(|| ParsePolicyError::new(name))
    }
}

/// Collects the slots tracked by `policy` in eviction order, coldest first.
pub fn eviction_order<P: Policy + ?Sized>(policy: &P) -> Vec<usize> {
    let mut order = Vec::with_capacity(policy.len());
    let mut cursor = policy.coldest();
    while let Some(slot) = cursor {
        order.push(slot);
        cursor = policy.next(slot);
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_their_names() {
        for kind in PolicyKind::ALL {
            assert_eq!(kind.as_str().parse::<PolicyKind>(), Ok(kind));
            assert_eq!(kind.build().name(), kind.as_str());
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn parsing_ignores_case_and_whitespace() {
        assert_eq!(" LRU ".parse::<PolicyKind>(), Ok(PolicyKind::Lru));
        assert_eq!("Fifo".parse::<PolicyKind>(), Ok(PolicyKind::Fifo));
    }

    #[test]
    fn unknown_names_are_rejected() {
        let err = "random".parse::<PolicyKind>().unwrap_err();
        assert_eq!(err.name(), "random");
    }
}
