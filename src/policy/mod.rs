//! Eviction policies.
//!
//! A policy does not own keys or values. The cache engine stores entries in
//! numbered slots and tells the policy what happens to each slot; the policy
//! keeps the slots in *eviction order* and answers one question: which slot
//! should go first. Slot indices are small, dense and reused after removal,
//! which lets policies keep their bookkeeping in plain vectors.

mod fifo;
mod lru;

pub use fifo::Fifo;
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
