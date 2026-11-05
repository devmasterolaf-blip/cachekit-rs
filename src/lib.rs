//! `cachekit` is an in-memory caching toolkit built on the standard library.
//!
//! The crate is organised as small modules that are combined by the cache
//! engine: eviction policies, a clock abstraction for expiry, statistics and
//! so on. Modules are added to this file as they are implemented.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod builder;
pub mod cache;
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
