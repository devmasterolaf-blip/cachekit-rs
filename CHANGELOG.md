# Changelog

Notable changes to cachekit-rs. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
uses [Semantic Versioning](https://semver.org/). While the version is
below 1.0 the API may change between minor versions.

## [Unreleased]

Everything below is the initial development of version 0.1.0, which has not
been tagged or published yet.

### Added

- `PolicyCache`, a bounded cache engine with pluggable eviction, and the
  `Cache` trait for the operations every cache shares.
- LRU, LFU and FIFO policies (`LruCache`, `LfuCache`, `FifoCache`), plus
  `PolicyKind` and `DynCache` for choosing a policy at run time.
- Bounds by entry count and by total weight with a caller-supplied
  weigher; values heavier than the whole cache are rejected.
- Time to live per cache and per entry (`put_with_ttl`), lazy expiry,
  `purge_expired` and `remaining_ttl`.
- `Clock` with `SystemClock` and a `ManualClock` for deterministic tests.
- `get_or_insert_with` and `get_or_try_insert_with` with the typed
  `LoadError`.
- Removal listeners with a `RemovalCause`.
- `Stats` counters with hit and miss rates.
- `CacheBuilder` with validation and `ConfigError`.
- `SyncCache` and `ShardedCache` for use from several threads.
- Inspection helpers: `entries`, `iter`, `keys`, `peek_coldest`,
  `pop_coldest`, `weight`, `weight_of`.
- The `snapshot` module: save and load contents in a tab-separated text
  format, with `SnapshotError` reporting line numbers.
- The `trace` module: parse and replay traces of `get`, `put`, `del`,
  `tick`, `purge` and `clear` operations.
- The `cachekit` binary with the `replay` and `compare` commands and the
  `--policy`, `--capacity`, `--max-weight`, `--ttl-ms`, `--load` and
  `--save` options.
- Examples (`quickstart`, `shared`) and a sample trace.

### Fixed

- The binary no longer panics on arguments that are not valid UTF-8; it
  reports the argument and exits with status 2.
- `cachekit replay --help` and `cachekit compare --help` print the usage
  text instead of failing with "unknown option".

### Known limitations

- Expired entries count towards `len()` and the bounds until they are
  looked up, chosen as eviction victim or purged.
- A sharded cache enforces its bounds per shard, not globally.
- LFU use counts are never aged and are not stored in snapshots.
- Lookups need a `&K`; borrowed forms of the key are not accepted.
