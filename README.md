# cachekit-rs

An in-memory caching toolkit for Rust, built on the standard library only.

`cachekit` gives you a bounded cache with a choice of eviction policy (LRU,
LFU or FIFO), optional expiry, weighted capacity, on-demand loading, removal
listeners and statistics, plus thread-safe wrappers and a small command line
tool that replays recorded workloads so you can compare policies before
picking one.


This is a young project (version 0.1.0). It is not published on crates.io,
it has not been benchmarked against other cache crates, and the API may
still change.

## Features

- **Three eviction policies** behind one engine, each O(1) per read and write:
  LRU and FIFO on an index-based linked list, LFU on frequency buckets.
- **Two kinds of bound**: a maximum number of entries, a maximum total
  weight measured by a callback you supply, or both.
- **Time to live** per cache or per entry, read through a `Clock` trait so
  tests can move time by hand.
- **Loading**: `get_or_insert_with` and the fallible
  `get_or_try_insert_with`, which returns a typed `LoadError`.
- **Removal listeners** that are told why an entry left (replaced, evicted,
  expired, removed, rejected).
- **Statistics**: hits, misses, hit rate, insertions, updates, evictions,
  expirations and more.
- **Thread safety**: `SyncCache` (one mutex) and `ShardedCache` (one mutex
  per shard).
- **A validating builder** with typed `ConfigError`s.
- **Snapshots**: save the contents to a line-oriented text format and load
  them back.
- **A `cachekit` binary** that replays a trace of get/put operations against
  a policy and prints the statistics.

No `unsafe` code and no dependencies.

## Building

You need a Rust toolchain with Cargo. Development and testing happen on
Rust 1.93; the manifest declares 1.74 as the minimum, but older toolchains
are not tested.

```sh
git clone <repository-url> cachekit-rs
cd cachekit-rs
cargo build --release      # library and the target/release/cachekit binary
```

To use the library from another project, add it as a path or git
dependency (the package is called `cachekit`):

```toml
[dependencies]
cachekit = { path = "../cachekit-rs" }
```

## Using the library

### Quick start

```rust
use cachekit::{Cache, LruCache};

let mut cache = LruCache::new(2);
cache.put("a", 1);
cache.put("b", 2);
assert_eq!(cache.get(&"a"), Some(&1));

cache.put("c", 3); // "b" was used least recently, so it is evicted
assert_eq!(cache.get(&"b"), None);

let stats = cache.stats();
assert_eq!((stats.hits, stats.misses, stats.evictions), (1, 1, 1));
```

`LfuCache` and `FifoCache` work the same way. The `Cache` trait holds the
operations they share (`get`, `peek`, `put`, `remove`, `contains_key`,
`len`, `clear`, `stats`).

### Configuring with the builder

```rust
use std::time::Duration;
use cachekit::{Cache, CacheBuilder, ConfigError, PolicyKind};

let mut pages = CacheBuilder::new()
    .policy(PolicyKind::Lfu)
    .max_entries(10_000)
    .max_weight(8 * 1024 * 1024)
    .weigher(|_path: &String, body: &Vec<u8>| body.len() as u64)
    .ttl(Duration::from_secs(300))
    .listener(|path: &String, _body: &Vec<u8>, cause| eprintln!("dropped {path}: {cause}"))
    .build()?;

pages.put("/index.html".to_string(), b"<html>...</html>".to_vec());
pages.put_with_ttl("/status".to_string(), b"ok".to_vec(), Duration::from_secs(5));
assert_eq!(pages.weight(), 18);

// Mistakes are reported before a cache exists.
let unbounded = CacheBuilder::<String, String>::new().build();
assert_eq!(unbounded.err(), Some(ConfigError::Unbounded));
```

A value heavier than `max_weight` can never fit; it is rejected, counted in
`stats().rejections` and reported to listeners.

### Expiry you can test

```rust
use std::time::Duration;
use cachekit::{Cache, CacheBuilder, ManualClock};

let clock = ManualClock::new();
let mut sessions = CacheBuilder::new()
    .max_entries(100)
    .ttl(Duration::from_secs(60))
    .clock(clock.clone())
    .build()?;

sessions.put("alice", "token-1");
clock.advance(Duration::from_secs(59));
assert!(sessions.contains_key(&"alice"));
clock.advance(Duration::from_secs(1));
assert!(!sessions.contains_key(&"alice"));
```

Without `.clock(...)` the cache uses the system's monotonic clock.

### Loading missing values

```rust
use cachekit::{Cache, LruCache};

let mut parsed: LruCache<String, i64> = LruCache::new(100);

// Infallible: the closure runs only when the key is missing.
let length = parsed.get_or_insert_with("hello".to_string(), |text| text.len() as i64);
assert_eq!(length, 5);

// Fallible: an error is handed back and nothing is cached.
let load = |text: &String| text.parse::<i64>();
assert_eq!(parsed.get_or_try_insert_with("42".to_string(), load), Ok(42));
let failure = parsed
    .get_or_try_insert_with("forty-two".to_string(), load)
    .unwrap_err();
println!("{failure}"); // cache loader failed: invalid digit found in string
assert!(!parsed.contains_key(&"forty-two".to_string()));
```

### Sharing between threads

```rust
use std::sync::Arc;
use std::thread;
use cachekit::{CacheBuilder, ShardedCache};

let cache: Arc<ShardedCache<u64, String>> =
    Arc::new(CacheBuilder::new().max_entries(1024).build_sharded(8)?);

let workers: Vec<_> = (0..4)
    .map(|_| {
        let cache = Arc::clone(&cache);
        thread::spawn(move || {
            for id in 0..100 {
                let name = cache.get_or_insert_with(id, |id| format!("user-{id}"));
                assert_eq!(name, format!("user-{id}"));
            }
        })
    })
    .collect();
for worker in workers {
    worker.join().unwrap();
}
assert_eq!(cache.stats().loads, 100); // each id was loaded once
```

`build_sync()` gives a `SyncCache` with a single lock instead. Both wrappers
return clones of the values, so wrap large values in an `Arc`.

### Saving and restoring

```rust
use cachekit::{snapshot, Cache, LruCache};

let mut cache: LruCache<String, u32> = LruCache::new(100);
cache.put("visits".to_string(), 3);

let mut saved = Vec::new(); // any `Write`, for example a `File`
snapshot::save(&cache, &mut saved)?;
assert_eq!(saved, b"cachekit-snapshot v1\nvisits\t3\t-\n");

let mut restored: LruCache<String, u32> = LruCache::new(100);
snapshot::load(&mut restored, saved.as_slice())?; // any `BufRead`
assert_eq!(restored.peek(&"visits".to_string()), Some(&3));
```

Keys and values are written with `Display` and read with `FromStr`. Each
line holds a key, a value and the remaining time to live in milliseconds
(or `-`), separated by tabs.

The snippets above use `?` and therefore belong in a function that returns
a `Result`. Two complete programs live in `examples/`:

```sh
cargo run --example quickstart
cargo run --example shared
```

## Using the command line tool

A trace is a text file with one operation per line:

```text
# comment
put <key> <value...>   store a value (the rest of the line)
get <key>              look a key up
del <key>              remove a key
tick <milliseconds>    advance the simulated clock
purge                  drop every expired entry now
clear                  empty the cache
```

`replay` runs a trace against one policy:

```console
$ cargo run -- replay examples/traces/session.trace --capacity 2 --ttl-ms 1000
trace:       examples/traces/session.trace
policy:      lru
capacity:    2 entries
ttl:         1000 ms
operations:  11
simulated:   1100 ms
entries:     1
requests:    5
hits:        3
misses:      2
hit rate:    60.00%
insertions:  3
updates:     0
evictions:   1
expirations: 1
removals:    0
rejections:  0
loads:       0
load errors: 0
```

`compare` runs the same trace against all three policies:

```console
$ cargo run -- compare examples/traces/session.trace --capacity 2
trace:       examples/traces/session.trace
capacity:    2 entries
operations:  11

policy       hits     misses  hit rate  evictions expirations
lru             4          1    80.00%          1           0
lfu             4          1    80.00%          1           0
fifo            3          2    60.00%          1           0
```

Options:

| Option | Meaning |
|--------|---------|
| `--policy <lru\|lfu\|fifo>` | Eviction policy for `replay` (default `lru`) |
| `--capacity <n>` | Keep at most `n` entries |
| `--max-weight <n>` | Keep at most `n` bytes of keys plus values |
| `--ttl-ms <n>` | Expire entries `n` simulated milliseconds after a write |
| `--load <file>` | Fill the cache from a snapshot before a `replay` |
| `--save <file>` | Write the final contents of a `replay` to a snapshot |

At least one of `--capacity` and `--max-weight` is required. Use `-` as the
trace to read it from standard input. Time in a replay is simulated: only
`tick` lines move it. The tool exits with status 2 for a wrong command line
and 1 for files it cannot read or parse.

## Behaviour worth knowing

- Expired entries are removed lazily: when they are looked up, when they
  are the next eviction victim, or when you call `purge_expired()`. Until
  then they still count towards `len()`, `weight()` and the bounds, so a
  full cache can evict a live entry while an expired one is still stored.
- Reading an entry does not extend its time to live; only writing does.
- LFU counts every read and write and never ages the counts.
- A `ShardedCache` splits `max_entries` and `max_weight` evenly between its
  shards. One shard can evict, or reject a heavy value, while the cache as
  a whole still has room.
- `SyncCache` and `ShardedCache` hold their lock while a loader runs. That
  gives one load per key, but slow loaders block other callers of the same
  lock, and a loader must not call back into the same cache.
- Lookups take `&K`; there is no borrowed-key lookup such as `get("text")`
  on a `String`-keyed cache yet.
- Snapshots do not store LFU use counts (every restored entry starts at
  one use) and round remaining TTLs up to whole milliseconds.
- Trace keys cannot contain whitespace, and the whole trace is read into
  memory before it is replayed.

## Project layout

```text
src/
  lib.rs         crate overview and re-exports
  cache.rs       the Cache trait and the PolicyCache engine
  policy/        the Policy trait and the LRU, LFU and FIFO orderings
  list.rs        index-based doubly linked lists shared by the policies
  builder.rs     CacheBuilder and its validation
  clock.rs       Clock, SystemClock and ManualClock
  stats.rs       the Stats counters
  event.rs       RemovalCause and the listener type
  error.rs       ConfigError, LoadError, ParsePolicyError
  sync.rs        SyncCache and ShardedCache
  snapshot.rs    the snapshot text format
  trace.rs       trace parsing and replay
  cli.rs         the command line tool as a testable function
  main.rs        the cachekit binary
tests/
  model.rs       randomised comparison with a naive reference model
  concurrency.rs the thread-safe wrappers under contention
  snapshot.rs    snapshots through real files
  cli.rs         the real binary, end to end
examples/        runnable examples and a sample trace
```

## Running the tests

```sh
cargo test                   # unit, integration and documentation tests
cargo test --test model      # one integration test file
cargo test --lib cache::     # unit tests of one module
```

The tests are deterministic: expiry uses a manual clock, the randomised
model test uses fixed seeds, and files are written under Cargo's own
`target/tmp` directory.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Changes are listed in
[CHANGELOG.md](CHANGELOG.md).

## License

MIT, see [LICENSE](LICENSE).
