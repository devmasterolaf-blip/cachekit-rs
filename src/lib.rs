//! `cachekit` is an in-memory caching toolkit built on the standard library.
//!
//! One engine, [`PolicyCache`], stores the entries; an eviction [`Policy`]
//! (LRU, LFU or FIFO) decides which entry goes when the cache is full. The
//! engine can bound the cache by entry count, by total weight or both,
//! expire entries after a time to live, load missing values on demand,
//! report removals to listeners and count what happened in [`Stats`].
//!
//! # Quick start
//!
//! ```
//! use cachekit::{Cache, LruCache};
//!
//! let mut cache = LruCache::new(2);
//! cache.put("a", 1);
//! cache.put("b", 2);
//! assert_eq!(cache.get(&"a"), Some(&1));
//!
//! cache.put("c", 3); // "b" is the least recently used entry and is evicted
//! assert_eq!(cache.get(&"b"), None);
//! assert_eq!(cache.stats().evictions, 1);
//! ```
//!
//! # Configuring a cache
//!
//! [`CacheBuilder`] validates the settings and reports mistakes as a
//! [`ConfigError`]. Expiry reads time through the [`Clock`] trait, so tests
//! can drive it with a [`ManualClock`]:
//!
//! ```
//! use std::time::Duration;
//! use cachekit::{Cache, CacheBuilder, ManualClock, PolicyKind};
//!
//! let clock = ManualClock::new();
//! let mut cache = CacheBuilder::new()
//!     .policy(PolicyKind::Lfu)
//!     .max_weight(1024)
//!     .weigher(|_key: &String, value: &Vec<u8>| value.len() as u64)
//!     .ttl(Duration::from_secs(30))
//!     .clock(clock.clone())
//!     .build()?;
//!
//! cache.put("page".to_string(), vec![0; 100]);
//! assert_eq!(cache.weight(), 100);
//!
//! clock.advance(Duration::from_secs(30));
//! assert_eq!(cache.get(&"page".to_string()), None);
//! assert_eq!(cache.stats().expirations, 1);
//! # Ok::<(), cachekit::ConfigError>(())
//! ```
//!
//! # Where to look
//!
//! | Need | Item |
//! |------|------|
//! | A cache with a fixed policy | [`LruCache`], [`LfuCache`], [`FifoCache`] |
//! | A policy chosen at run time | [`CacheBuilder::build`], [`DynCache`] |
//! | Code generic over caches | the [`Cache`] trait |
//! | Loading missing values | [`PolicyCache::get_or_insert_with`], [`PolicyCache::get_or_try_insert_with`] |
//! | Sharing between threads | [`SyncCache`], [`ShardedCache`] |
//! | Reacting to removals | [`CacheBuilder::listener`], [`RemovalCause`] |
//! | Saving and restoring contents | the [`snapshot`] module |
//! | Replaying recorded workloads | the [`trace`] module and the `cachekit` binary ([`cli`]) |
//!
//! # Things to know
//!
//! * Expired entries are removed lazily: when they are looked up, when they
//!   are the next eviction victim, or by [`PolicyCache::purge_expired`].
//!   Until then they still count towards [`Cache::len`] and the bounds.
//! * A [`ShardedCache`] splits its capacity between the shards, so a single
//!   shard can evict while others still have room.
//! * [`SyncCache`] and [`ShardedCache`] hold their lock while a loader
//!   runs. That gives one load per key, but a loader must not call back
//!   into the same cache.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod builder;
pub mod cache;
pub mod cli;
pub mod clock;
pub mod error;
pub mod event;
pub mod list;
pub mod policy;
pub mod snapshot;
pub mod stats;
pub mod sync;
pub mod trace;

pub use builder::CacheBuilder;
pub use cache::{
    Cache, DynCache, EntryView, FifoCache, LfuCache, LruCache, PolicyCache, Weigher,
};
pub use clock::{Clock, ManualClock, SystemClock};
pub use error::{ConfigError, LoadError, ParsePolicyError};
pub use event::{Listener, RemovalCause};
pub use policy::{BoxedPolicy, Policy, PolicyKind};
pub use snapshot::SnapshotError;
pub use stats::Stats;
pub use sync::{ShardedCache, SyncCache};
