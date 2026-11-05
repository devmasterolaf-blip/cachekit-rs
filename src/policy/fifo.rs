//! First-in-first-out eviction.

use super::Policy;
use crate::list::SlotList;

/// Evicts entries in the order their keys were first inserted.
///
/// Reads and updates do not change an entry's position: replacing the value
/// of an existing key keeps the key's original place in the queue.
#[derive(Debug, Clone, Default)]
pub struct Fifo {
    queue: SlotList,
}

impl Fifo {
    /// Creates an empty FIFO policy.
    pub fn new() -> Self {
        Fifo::default()
    }
}

impl Policy for Fifo {
    fn name(&self) -> &'static str {
        "fifo"
    }

    fn on_insert(&mut self, slot: usize) {
        self.queue.push_back(slot);
    }

    fn on_access(&mut self, _slot: usize) {}

    fn on_update(&mut self, _slot: usize) {}

    fn on_remove(&mut self, slot: usize) {
        self.queue.remove(slot);
    }

    fn coldest(&self) -> Option<usize> {
        self.queue.front()
    }

    fn next(&self, slot: usize) -> Option<usize> {
        self.queue.next(slot)
    }

    fn len(&self) -> usize {
        self.queue.len()
    }

    fn clear(&mut self) {
        self.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::eviction_order;

    #[test]
    fn reads_and_updates_do_not_reorder() {
        let mut fifo = Fifo::new();
        for slot in 0..3 {
            fifo.on_insert(slot);
        }
        fifo.on_access(0);
        fifo.on_update(0);
        assert_eq!(eviction_order(&fifo), vec![0, 1, 2]);
        assert_eq!(fifo.coldest(), Some(0));
    }

    #[test]
    fn removal_from_the_middle_keeps_order() {
        let mut fifo = Fifo::new();
        for slot in 0..4 {
            fifo.on_insert(slot);
        }
        fifo.on_remove(1);
        assert_eq!(eviction_order(&fifo), vec![0, 2, 3]);
        fifo.on_insert(1);
        assert_eq!(eviction_order(&fifo), vec![0, 2, 3, 1]);
        assert_eq!(fifo.len(), 4);
    }

    #[test]
    fn clear_empties_the_queue() {
        let mut fifo = Fifo::new();
        fifo.on_insert(0);
        fifo.clear();
        assert!(fifo.is_empty());
        assert_eq!(fifo.coldest(), None);
    }
}
