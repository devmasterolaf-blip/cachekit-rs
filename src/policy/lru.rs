//! Least-recently-used eviction.

use super::Policy;
use crate::list::SlotList;

/// Evicts the entry that was read or written longest ago.
///
/// Slots are kept in a single recency list: every read or update moves the
/// slot to the back, and the front of the list is the next victim.
#[derive(Debug, Clone, Default)]
pub struct Lru {
    recency: SlotList,
}

impl Lru {
    /// Creates an empty LRU policy.
    pub fn new() -> Self {
        Lru::default()
    }
}

impl Policy for Lru {
    fn name(&self) -> &'static str {
        "lru"
    }

    fn on_insert(&mut self, slot: usize) {
        self.recency.push_back(slot);
    }

    fn on_access(&mut self, slot: usize) {
        self.recency.move_to_back(slot);
    }

    fn on_update(&mut self, slot: usize) {
        self.recency.move_to_back(slot);
    }

    fn on_remove(&mut self, slot: usize) {
        self.recency.remove(slot);
    }

    fn coldest(&self) -> Option<usize> {
        self.recency.front()
    }

    fn next(&self, slot: usize) -> Option<usize> {
        self.recency.next(slot)
    }

    fn len(&self) -> usize {
        self.recency.len()
    }

    fn clear(&mut self) {
        self.recency.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::eviction_order;

    #[test]
    fn victim_is_least_recently_inserted_without_reads() {
        let mut lru = Lru::new();
        for slot in 0..3 {
            lru.on_insert(slot);
        }
        assert_eq!(lru.coldest(), Some(0));
        assert_eq!(eviction_order(&lru), vec![0, 1, 2]);
    }

    #[test]
    fn access_and_update_refresh_recency() {
        let mut lru = Lru::new();
        for slot in 0..3 {
            lru.on_insert(slot);
        }
        lru.on_access(0);
        assert_eq!(eviction_order(&lru), vec![1, 2, 0]);
        lru.on_update(1);
        assert_eq!(eviction_order(&lru), vec![2, 0, 1]);
    }

    #[test]
    fn remove_and_reinsert_slot() {
        let mut lru = Lru::new();
        lru.on_insert(0);
        lru.on_insert(1);
        lru.on_remove(0);
        assert_eq!(lru.len(), 1);
        lru.on_insert(0);
        assert_eq!(eviction_order(&lru), vec![1, 0]);
    }

    #[test]
    fn clear_empties_the_policy() {
        let mut lru = Lru::new();
        lru.on_insert(0);
        lru.clear();
        assert!(lru.is_empty());
        assert_eq!(lru.coldest(), None);
    }

    #[test]
    fn boxed_policy_delegates() {
        let mut boxed: Box<dyn Policy> = Box::new(Lru::new());
        boxed.on_insert(4);
        boxed.on_insert(2);
        boxed.on_access(4);
        assert_eq!(boxed.name(), "lru");
        assert_eq!(eviction_order(&boxed), vec![2, 4]);
    }
}
