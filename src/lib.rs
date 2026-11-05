//! `cachekit` is an in-memory caching toolkit built on the standard library.
//!
//! The crate is organised as small modules that are combined by the cache
//! engine: eviction policies, a clock abstraction for expiry, statistics and
//! so on. Modules are added to this file as they are implemented.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cache;
pub mod list;
pub mod policy;
pub mod stats;

pub use cache::{Cache, FifoCache, LfuCache, LruCache, PolicyCache};
pub use stats::Stats;
