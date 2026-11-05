//! Index-based doubly linked lists.
//!
//! The cache engine stores entries in a slab and refers to them by their slot
//! index. Eviction policies need to keep those slots in some order and move
//! them around in constant time, which is what the types in this module do:
//! instead of heap-allocated nodes and pointers, the `prev`/`next` links live
//! in two `Vec<usize>` indexed by slot.
//!
//! * [`Links`] holds the raw links and can back any number of disjoint lists.
//! * [`Ends`] is the head/tail/length of one list threaded through a `Links`.
//! * [`SlotList`] bundles one of each for the common single-list case.

/// Marks the absence of a neighbour (start or end of a list).
const NIL: usize = usize::MAX;
/// Stored in `prev` for a slot that is not a member of any list.
const UNLINKED: usize = usize::MAX - 1;

/// Head, tail and length of a single list threaded through a [`Links`] table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ends {
    head: usize,
    tail: usize,
    len: usize,
}

impl Ends {
    /// Creates the ends of an empty list.
    pub const fn new() -> Self {
        Ends {
            head: NIL,
            tail: NIL,
            len: 0,
        }
    }

    /// First slot of the list, if any.
    pub fn front(&self) -> Option<usize> {
        to_option(self.head)
    }

    /// Last slot of the list, if any.
    pub fn back(&self) -> Option<usize> {
        to_option(self.tail)
    }

    /// Number of slots in the list.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when the list has no slots.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for Ends {
    fn default() -> Self {
        Ends::new()
    }
}

/// Link storage for slots. One table can back several disjoint lists, each
/// described by its own [`Ends`]; a slot is a member of at most one of them.
#[derive(Debug, Clone, Default)]
pub struct Links {
    prev: Vec<usize>,
    next: Vec<usize>,
}

impl Links {
    /// Creates an empty table.
    pub fn new() -> Self {
        Links::default()
    }

    /// Creates an empty table with room for `capacity` slots.
    pub fn with_capacity(capacity: usize) -> Self {
        Links {
            prev: Vec::with_capacity(capacity),
            next: Vec::with_capacity(capacity),
        }
    }

    fn ensure(&mut self, slot: usize) {
        assert!(slot < UNLINKED, "slot index {slot} is out of range");
        if slot >= self.prev.len() {
            self.prev.resize(slot + 1, UNLINKED);
            self.next.resize(slot + 1, NIL);
        }
    }

    /// Returns `true` if `slot` is currently a member of some list.
    pub fn is_linked(&self, slot: usize) -> bool {
        self.prev.get(slot).is_some_and(|&p| p != UNLINKED)
    }

    /// Slot that follows `slot` in its list.
    pub fn next(&self, slot: usize) -> Option<usize> {
        if self.is_linked(slot) {
            to_option(self.next[slot])
        } else {
            None
        }
    }

    /// Slot that precedes `slot` in its list.
    pub fn prev(&self, slot: usize) -> Option<usize> {
        if self.is_linked(slot) {
            to_option(self.prev[slot])
        } else {
            None
        }
    }

    /// Appends `slot` to the list described by `ends`.
    ///
    /// # Panics
    ///
    /// Panics if the slot is already a member of a list.
    pub fn push_back(&mut self, ends: &mut Ends, slot: usize) {
        self.ensure(slot);
        assert!(!self.is_linked(slot), "slot {slot} is already linked");
        self.prev[slot] = ends.tail;
        self.next[slot] = NIL;
        if ends.tail == NIL {
            ends.head = slot;
        } else {
            self.next[ends.tail] = slot;
        }
        ends.tail = slot;
        ends.len += 1;
    }

    /// Prepends `slot` to the list described by `ends`.
    ///
    /// # Panics
    ///
    /// Panics if the slot is already a member of a list.
    pub fn push_front(&mut self, ends: &mut Ends, slot: usize) {
        self.ensure(slot);
        assert!(!self.is_linked(slot), "slot {slot} is already linked");
        self.prev[slot] = NIL;
        self.next[slot] = ends.head;
        if ends.head == NIL {
            ends.tail = slot;
        } else {
            self.prev[ends.head] = slot;
        }
        ends.head = slot;
        ends.len += 1;
    }

    /// Removes `slot` from the list described by `ends`. Returns `false` and
    /// does nothing when the slot is not linked.
    ///
    /// The caller is responsible for passing the `Ends` of the list the slot
    /// actually belongs to.
    pub fn unlink(&mut self, ends: &mut Ends, slot: usize) -> bool {
        if !self.is_linked(slot) {
            return false;
        }
        let prev = self.prev[slot];
        let next = self.next[slot];
        if prev == NIL {
            ends.head = next;
        } else {
            self.next[prev] = next;
        }
        if next == NIL {
            ends.tail = prev;
        } else {
            self.prev[next] = prev;
        }
        self.prev[slot] = UNLINKED;
        self.next[slot] = NIL;
        ends.len -= 1;
        true
    }

    /// Forgets every link. All `Ends` threaded through this table must be
    /// reset by the caller as well.
    pub fn clear(&mut self) {
        self.prev.clear();
        self.next.clear();
    }
}

/// A single doubly linked list of slot indices with O(1) insertion, removal
/// and reordering.
///
/// ```
/// use cachekit::list::SlotList;
///
/// let mut list = SlotList::new();
/// list.push_back(3);
/// list.push_back(7);
/// list.push_back(1);
/// list.move_to_back(3);
/// assert_eq!(list.iter().collect::<Vec<_>>(), vec![7, 1, 3]);
/// ```
#[derive(Debug, Clone, Default)]
pub struct SlotList {
    links: Links,
    ends: Ends,
}

impl SlotList {
    /// Creates an empty list.
    pub fn new() -> Self {
        SlotList::default()
    }

    /// Creates an empty list with room for `capacity` slots.
    pub fn with_capacity(capacity: usize) -> Self {
        SlotList {
            links: Links::with_capacity(capacity),
            ends: Ends::new(),
        }
    }

    /// Number of slots in the list.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Returns `true` when the list is empty.
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Returns `true` if `slot` is in the list.
    pub fn contains(&self, slot: usize) -> bool {
        self.links.is_linked(slot)
    }

    /// First slot.
    pub fn front(&self) -> Option<usize> {
        self.ends.front()
    }

    /// Last slot.
    pub fn back(&self) -> Option<usize> {
        self.ends.back()
    }

    /// Slot after `slot`, or `None` at the end of the list.
    pub fn next(&self, slot: usize) -> Option<usize> {
        self.links.next(slot)
    }

    /// Slot before `slot`, or `None` at the start of the list.
    pub fn prev(&self, slot: usize) -> Option<usize> {
        self.links.prev(slot)
    }

    /// Appends `slot`. Panics if it is already in the list.
    pub fn push_back(&mut self, slot: usize) {
        self.links.push_back(&mut self.ends, slot);
    }

    /// Prepends `slot`. Panics if it is already in the list.
    pub fn push_front(&mut self, slot: usize) {
        self.links.push_front(&mut self.ends, slot);
    }

    /// Removes `slot`; returns whether it was present.
    pub fn remove(&mut self, slot: usize) -> bool {
        self.links.unlink(&mut self.ends, slot)
    }

    /// Removes and returns the first slot.
    pub fn pop_front(&mut self) -> Option<usize> {
        let slot = self.ends.front()?;
        self.links.unlink(&mut self.ends, slot);
        Some(slot)
    }

    /// Moves `slot` to the end of the list; returns whether it was present.
    pub fn move_to_back(&mut self, slot: usize) -> bool {
        if !self.links.unlink(&mut self.ends, slot) {
            return false;
        }
        self.links.push_back(&mut self.ends, slot);
        true
    }

    /// Removes every slot.
    pub fn clear(&mut self) {
        self.links.clear();
        self.ends = Ends::new();
    }

    /// Iterates over the slots from front to back.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            links: &self.links,
            cursor: self.ends.head,
            remaining: self.ends.len,
        }
    }
}

/// Front-to-back iterator over a [`SlotList`].
#[derive(Debug, Clone)]
pub struct Iter<'a> {
    links: &'a Links,
    cursor: usize,
    remaining: usize,
}

impl Iterator for Iter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.cursor == NIL {
            return None;
        }
        let slot = self.cursor;
        self.cursor = self.links.next[slot];
        self.remaining -= 1;
        Some(slot)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl<'a> IntoIterator for &'a SlotList {
    type Item = usize;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

fn to_option(index: usize) -> Option<usize> {
    if index == NIL {
        None
    } else {
        Some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(list: &SlotList) -> Vec<usize> {
        list.iter().collect()
    }

    #[test]
    fn push_back_keeps_insertion_order() {
        let mut list = SlotList::new();
        for slot in [4, 0, 9] {
            list.push_back(slot);
        }
        assert_eq!(collect(&list), vec![4, 0, 9]);
        assert_eq!(list.front(), Some(4));
        assert_eq!(list.back(), Some(9));
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn push_front_prepends() {
        let mut list = SlotList::new();
        list.push_front(1);
        list.push_front(2);
        list.push_back(3);
        assert_eq!(collect(&list), vec![2, 1, 3]);
    }

    #[test]
    fn remove_handles_head_middle_and_tail() {
        let mut list = SlotList::new();
        for slot in 0..5 {
            list.push_back(slot);
        }
        assert!(list.remove(2));
        assert_eq!(collect(&list), vec![0, 1, 3, 4]);
        assert!(list.remove(0));
        assert_eq!(list.front(), Some(1));
        assert!(list.remove(4));
        assert_eq!(list.back(), Some(3));
        assert_eq!(collect(&list), vec![1, 3]);
        assert!(!list.remove(4), "removing twice is a no-op");
        assert!(!list.remove(100), "unknown slots are ignored");
    }

    #[test]
    fn removed_slot_can_be_reused() {
        let mut list = SlotList::new();
        list.push_back(0);
        list.push_back(1);
        list.remove(0);
        assert!(!list.contains(0));
        list.push_back(0);
        assert_eq!(collect(&list), vec![1, 0]);
    }

    #[test]
    fn move_to_back_reorders() {
        let mut list = SlotList::new();
        for slot in 0..3 {
            list.push_back(slot);
        }
        assert!(list.move_to_back(0));
        assert_eq!(collect(&list), vec![1, 2, 0]);
        assert!(list.move_to_back(0), "moving the tail keeps it in place");
        assert_eq!(collect(&list), vec![1, 2, 0]);
        assert!(!list.move_to_back(7));
    }

    #[test]
    fn pop_front_drains_in_order() {
        let mut list = SlotList::new();
        list.push_back(5);
        list.push_back(6);
        assert_eq!(list.pop_front(), Some(5));
        assert_eq!(list.pop_front(), Some(6));
        assert_eq!(list.pop_front(), None);
        assert!(list.is_empty());
        assert_eq!(list.front(), None);
        assert_eq!(list.back(), None);
    }

    #[test]
    fn neighbours_are_reported() {
        let mut list = SlotList::new();
        for slot in [10, 20, 30] {
            list.push_back(slot);
        }
        assert_eq!(list.next(10), Some(20));
        assert_eq!(list.next(30), None);
        assert_eq!(list.prev(20), Some(10));
        assert_eq!(list.prev(10), None);
        assert_eq!(list.next(15), None, "unlinked slots have no neighbours");
    }

    #[test]
    fn clear_resets_everything() {
        let mut list = SlotList::new();
        list.push_back(1);
        list.push_back(2);
        list.clear();
        assert!(list.is_empty());
        assert!(!list.contains(1));
        list.push_back(2);
        assert_eq!(collect(&list), vec![2]);
    }

    #[test]
    #[should_panic(expected = "already linked")]
    fn double_insert_panics() {
        let mut list = SlotList::new();
        list.push_back(1);
        list.push_back(1);
    }

    #[test]
    fn two_lists_share_one_link_table() {
        let mut links = Links::new();
        let mut a = Ends::new();
        let mut b = Ends::new();
        links.push_back(&mut a, 0);
        links.push_back(&mut b, 1);
        links.push_back(&mut a, 2);
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 1);
        assert_eq!(links.next(0), Some(2));
        assert_eq!(links.next(1), None);
        assert!(links.unlink(&mut a, 0));
        links.push_back(&mut b, 0);
        assert_eq!(a.front(), Some(2));
        assert_eq!(b.back(), Some(0));
        assert_eq!(links.prev(0), Some(1));
    }

    #[test]
    fn iterator_reports_exact_length() {
        let mut list = SlotList::new();
        for slot in 0..4 {
            list.push_back(slot);
        }
        let mut iter = list.iter();
        assert_eq!(iter.len(), 4);
        iter.next();
        assert_eq!(iter.len(), 3);
    }
}
