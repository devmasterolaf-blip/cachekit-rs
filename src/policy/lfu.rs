//! Least-frequently-used eviction with O(1) frequency buckets.

use super::Policy;
use crate::list::{Ends, Links};

const NONE: usize = usize::MAX;

/// All slots that currently share one use count.
#[derive(Debug, Clone)]
struct Bucket {
    freq: u64,
    members: Ends,
    prev: usize,
    next: usize,
}

/// Evicts the entry with the fewest uses; ties go to the entry that reached
/// that use count first.
///
/// A use is an insert, a read or an update. Slots with the same use count
/// are grouped in a bucket, and the buckets form a list sorted by count.
/// Bumping a slot moves it to the neighbouring bucket (creating it if
/// needed), so every operation is O(1) regardless of how many distinct
/// counts exist.
#[derive(Debug, Clone)]
pub struct Lfu {
    /// Links between slots inside their bucket.
    links: Links,
    /// Bucket index for each slot, `NONE` when the slot is not tracked.
    bucket_of: Vec<usize>,
    /// Bucket storage; unused indices are listed in `free_buckets`.
    buckets: Vec<Bucket>,
    free_buckets: Vec<usize>,
    /// Bucket with the smallest use count.
    lowest: usize,
    len: usize,
}

impl Default for Lfu {
    fn default() -> Self {
        Lfu {
            links: Links::new(),
            bucket_of: Vec::new(),
            buckets: Vec::new(),
            free_buckets: Vec::new(),
            lowest: NONE,
            len: 0,
        }
    }
}

impl Lfu {
    /// Creates an empty LFU policy.
    pub fn new() -> Self {
        Lfu::default()
    }

    /// Use count recorded for `slot`, or `None` if the slot is not tracked.
    pub fn frequency(&self, slot: usize) -> Option<u64> {
        let bucket = self.bucket_index(slot)?;
        Some(self.buckets[bucket].freq)
    }

    /// Number of distinct use counts currently present.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len() - self.free_buckets.len()
    }

    fn bucket_index(&self, slot: usize) -> Option<usize> {
        match self.bucket_of.get(slot) {
            Some(&bucket) if bucket != NONE => Some(bucket),
            _ => None,
        }
    }

    /// Allocates a bucket for `freq` and links it between `prev` and `next`.
    fn new_bucket(&mut self, freq: u64, prev: usize, next: usize) -> usize {
        let bucket = Bucket {
            freq,
            members: Ends::new(),
            prev,
            next,
        };
        let index = match self.free_buckets.pop() {
            Some(index) => {
                self.buckets[index] = bucket;
                index
            }
            None => {
                self.buckets.push(bucket);
                self.buckets.len() - 1
            }
        };
        if prev == NONE {
            self.lowest = index;
        } else {
            self.buckets[prev].next = index;
        }
        if next != NONE {
            self.buckets[next].prev = index;
        }
        index
    }

    /// Unlinks an empty bucket from the bucket list and recycles it.
    fn release_bucket(&mut self, index: usize) {
        let Bucket { prev, next, .. } = self.buckets[index];
        if prev == NONE {
            self.lowest = next;
        } else {
            self.buckets[prev].next = next;
        }
        if next != NONE {
            self.buckets[next].prev = prev;
        }
        self.free_buckets.push(index);
    }

    /// Records one more use of `slot`.
    fn bump(&mut self, slot: usize) {
        let Some(current) = self.bucket_index(slot) else {
            return;
        };
        let freq = self.buckets[current].freq;
        let Some(target_freq) = freq.checked_add(1) else {
            return;
        };
        let next = self.buckets[current].next;
        let target = if next != NONE && self.buckets[next].freq == target_freq {
            next
        } else {
            self.new_bucket(target_freq, current, next)
        };
        self.links.unlink(&mut self.buckets[current].members, slot);
        self.links.push_back(&mut self.buckets[target].members, slot);
        self.bucket_of[slot] = target;
        if self.buckets[current].members.is_empty() {
            self.release_bucket(current);
        }
    }
}

impl Policy for Lfu {
    fn name(&self) -> &'static str {
        "lfu"
    }

    fn on_insert(&mut self, slot: usize) {
        if slot >= self.bucket_of.len() {
            self.bucket_of.resize(slot + 1, NONE);
        }
        assert!(
            self.bucket_of[slot] == NONE,
            "slot {slot} is already tracked"
        );
        let bucket = if self.lowest != NONE && self.buckets[self.lowest].freq == 1 {
            self.lowest
        } else {
            self.new_bucket(1, NONE, self.lowest)
        };
        self.links.push_back(&mut self.buckets[bucket].members, slot);
        self.bucket_of[slot] = bucket;
        self.len += 1;
    }

    fn on_access(&mut self, slot: usize) {
        self.bump(slot);
    }

    fn on_update(&mut self, slot: usize) {
        self.bump(slot);
    }

    fn on_remove(&mut self, slot: usize) {
        let Some(bucket) = self.bucket_index(slot) else {
            return;
        };
        self.links.unlink(&mut self.buckets[bucket].members, slot);
        self.bucket_of[slot] = NONE;
        self.len -= 1;
        if self.buckets[bucket].members.is_empty() {
            self.release_bucket(bucket);
        }
    }

    fn coldest(&self) -> Option<usize> {
        if self.lowest == NONE {
            None
        } else {
            self.buckets[self.lowest].members.front()
        }
    }

    fn next(&self, slot: usize) -> Option<usize> {
        let bucket = self.bucket_index(slot)?;
        if let Some(next) = self.links.next(slot) {
            return Some(next);
        }
        let next_bucket = self.buckets[bucket].next;
        if next_bucket == NONE {
            None
        } else {
            self.buckets[next_bucket].members.front()
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn clear(&mut self) {
        *self = Lfu::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::eviction_order;

    #[test]
    fn new_slots_start_with_one_use() {
        let mut lfu = Lfu::new();
        lfu.on_insert(0);
        lfu.on_insert(1);
        assert_eq!(lfu.frequency(0), Some(1));
        assert_eq!(lfu.frequency(5), None);
        assert_eq!(lfu.bucket_count(), 1);
        assert_eq!(eviction_order(&lfu), vec![0, 1]);
    }

    #[test]
    fn least_used_slot_is_coldest() {
        let mut lfu = Lfu::new();
        for slot in 0..3 {
            lfu.on_insert(slot);
        }
        lfu.on_access(0);
        lfu.on_access(0);
        lfu.on_access(2);
        assert_eq!(lfu.coldest(), Some(1));
        assert_eq!(eviction_order(&lfu), vec![1, 2, 0]);
        assert_eq!(lfu.frequency(0), Some(3));
        assert_eq!(lfu.bucket_count(), 3);
    }

    #[test]
    fn ties_are_broken_by_arrival_in_the_bucket() {
        let mut lfu = Lfu::new();
        for slot in 0..3 {
            lfu.on_insert(slot);
        }
        // All three reach two uses; 2 gets there first, then 0, then 1.
        lfu.on_access(2);
        lfu.on_access(0);
        lfu.on_update(1);
        assert_eq!(eviction_order(&lfu), vec![2, 0, 1]);
        assert_eq!(lfu.bucket_count(), 1);
    }

    #[test]
    fn empty_buckets_are_recycled() {
        let mut lfu = Lfu::new();
        lfu.on_insert(0);
        for _ in 0..10 {
            lfu.on_access(0);
        }
        assert_eq!(lfu.frequency(0), Some(11));
        assert_eq!(lfu.bucket_count(), 1);
        assert!(lfu.buckets.len() <= 2, "buckets must be reused, not leaked");
    }

    #[test]
    fn insert_creates_a_new_lowest_bucket_when_needed() {
        let mut lfu = Lfu::new();
        lfu.on_insert(0);
        lfu.on_access(0);
        lfu.on_insert(1);
        assert_eq!(lfu.coldest(), Some(1));
        assert_eq!(eviction_order(&lfu), vec![1, 0]);
    }

    #[test]
    fn bump_joins_an_existing_next_bucket() {
        let mut lfu = Lfu::new();
        lfu.on_insert(0);
        lfu.on_insert(1);
        lfu.on_access(0);
        lfu.on_access(1);
        assert_eq!(lfu.bucket_count(), 1);
        assert_eq!(eviction_order(&lfu), vec![0, 1]);
    }

    #[test]
    fn removal_drops_empty_bucket_and_keeps_chain_intact() {
        let mut lfu = Lfu::new();
        for slot in 0..3 {
            lfu.on_insert(slot);
        }
        lfu.on_access(1);
        lfu.on_access(2);
        lfu.on_access(2);
        // frequencies: 0 -> 1, 1 -> 2, 2 -> 3
        lfu.on_remove(1);
        assert_eq!(lfu.bucket_count(), 2);
        assert_eq!(eviction_order(&lfu), vec![0, 2]);
        lfu.on_remove(0);
        assert_eq!(lfu.coldest(), Some(2));
        lfu.on_remove(2);
        assert!(lfu.is_empty());
        assert_eq!(lfu.coldest(), None);
        assert_eq!(lfu.bucket_count(), 0);
    }

    #[test]
    fn reinserted_slot_starts_over() {
        let mut lfu = Lfu::new();
        lfu.on_insert(0);
        lfu.on_access(0);
        lfu.on_remove(0);
        lfu.on_insert(0);
        assert_eq!(lfu.frequency(0), Some(1));
        assert_eq!(lfu.len(), 1);
    }

    #[test]
    fn clear_resets_all_state() {
        let mut lfu = Lfu::new();
        lfu.on_insert(0);
        lfu.on_access(0);
        lfu.clear();
        assert!(lfu.is_empty());
        assert_eq!(lfu.frequency(0), None);
        lfu.on_insert(0);
        assert_eq!(lfu.frequency(0), Some(1));
    }

    #[test]
    fn untracked_slots_are_ignored() {
        let mut lfu = Lfu::new();
        lfu.on_access(3);
        lfu.on_remove(3);
        assert!(lfu.is_empty());
        assert_eq!(lfu.next(3), None);
    }
}
