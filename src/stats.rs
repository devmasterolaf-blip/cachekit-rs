//! Cache activity counters.

use std::fmt;
use std::ops::{Add, AddAssign};

/// Counters describing what a cache has done since it was created or since
/// the counters were last reset.
///
/// `Stats` is a plain value: caches hand out copies, and copies from several
/// caches (for example the shards of a sharded cache) can be added together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Lookups that found a live entry.
    pub hits: u64,
    /// Lookups that found nothing.
    pub misses: u64,
    /// Writes that stored a new key.
    pub insertions: u64,
    /// Writes that replaced the value of an existing key.
    pub updates: u64,
    /// Entries removed by the eviction policy to make room.
    pub evictions: u64,
    /// Entries removed because their time to live ran out.
    pub expirations: u64,
    /// Entries removed explicitly by the caller.
    pub removals: u64,
    /// Writes refused because the value was heavier than the whole cache.
    pub rejections: u64,
    /// Loader calls that produced a value.
    pub loads: u64,
    /// Loader calls that returned an error.
    pub load_failures: u64,
}

impl Stats {
    /// Creates a zeroed set of counters.
    pub const fn new() -> Self {
        Stats {
            hits: 0,
            misses: 0,
            insertions: 0,
            updates: 0,
            evictions: 0,
            expirations: 0,
            removals: 0,
            rejections: 0,
            loads: 0,
            load_failures: 0,
        }
    }

    /// Total number of lookups, hits plus misses.
    pub fn requests(&self) -> u64 {
        self.hits + self.misses
    }

    /// Fraction of lookups that were hits, between 0 and 1. A cache that has
    /// not been queried yet reports 0.
    pub fn hit_rate(&self) -> f64 {
        let requests = self.requests();
        if requests == 0 {
            0.0
        } else {
            self.hits as f64 / requests as f64
        }
    }

    /// Fraction of lookups that were misses, between 0 and 1. A cache that
    /// has not been queried yet reports 0.
    pub fn miss_rate(&self) -> f64 {
        let requests = self.requests();
        if requests == 0 {
            0.0
        } else {
            self.misses as f64 / requests as f64
        }
    }
}

impl Add for Stats {
    type Output = Stats;

    fn add(mut self, other: Stats) -> Stats {
        self += other;
        self
    }
}

impl AddAssign for Stats {
    fn add_assign(&mut self, other: Stats) {
        self.hits += other.hits;
        self.misses += other.misses;
        self.insertions += other.insertions;
        self.updates += other.updates;
        self.evictions += other.evictions;
        self.expirations += other.expirations;
        self.removals += other.removals;
        self.rejections += other.rejections;
        self.loads += other.loads;
        self.load_failures += other.load_failures;
    }
}

impl std::iter::Sum for Stats {
    fn sum<I: Iterator<Item = Stats>>(iter: I) -> Stats {
        iter.fold(Stats::new(), Add::add)
    }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "requests:    {}", self.requests())?;
        writeln!(f, "hits:        {}", self.hits)?;
        writeln!(f, "misses:      {}", self.misses)?;
        writeln!(f, "hit rate:    {:.2}%", self.hit_rate() * 100.0)?;
        writeln!(f, "insertions:  {}", self.insertions)?;
        writeln!(f, "updates:     {}", self.updates)?;
        writeln!(f, "evictions:   {}", self.evictions)?;
        writeln!(f, "expirations: {}", self.expirations)?;
        writeln!(f, "removals:    {}", self.removals)?;
        writeln!(f, "rejections:  {}", self.rejections)?;
        writeln!(f, "loads:       {}", self.loads)?;
        write!(f, "load errors: {}", self.load_failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_are_zero_without_requests() {
        let stats = Stats::new();
        assert_eq!(stats.requests(), 0);
        assert_eq!(stats.hit_rate(), 0.0);
        assert_eq!(stats.miss_rate(), 0.0);
    }

    #[test]
    fn rates_follow_hits_and_misses() {
        let stats = Stats {
            hits: 3,
            misses: 1,
            ..Stats::new()
        };
        assert_eq!(stats.requests(), 4);
        assert!((stats.hit_rate() - 0.75).abs() < 1e-12);
        assert!((stats.miss_rate() - 0.25).abs() < 1e-12);
    }

    #[test]
    fn stats_add_field_by_field() {
        let a = Stats {
            hits: 1,
            misses: 2,
            insertions: 3,
            updates: 4,
            evictions: 5,
            expirations: 7,
            removals: 6,
            rejections: 8,
            loads: 9,
            load_failures: 10,
        };
        let total: Stats = [a, a, a].into_iter().sum();
        assert_eq!(total.hits, 3);
        assert_eq!(total.misses, 6);
        assert_eq!(total.insertions, 9);
        assert_eq!(total.updates, 12);
        assert_eq!(total.evictions, 15);
        assert_eq!(total.expirations, 21);
        assert_eq!(total.removals, 18);
        assert_eq!(total.rejections, 24);
        assert_eq!(total.loads, 27);
        assert_eq!(total.load_failures, 30);
    }

    #[test]
    fn display_lists_every_counter() {
        let stats = Stats {
            hits: 1,
            misses: 1,
            ..Stats::new()
        };
        let text = stats.to_string();
        assert!(text.contains("hit rate:    50.00%"));
        assert!(text.contains("evictions:   0"));
        assert_eq!(text.lines().count(), 12);
    }
}
