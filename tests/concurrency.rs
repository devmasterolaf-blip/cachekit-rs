//! The thread-safe wrappers used from several threads at once.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use cachekit::{
    Cache, CacheBuilder, LoadError, ManualClock, PolicyKind, ShardedCache, SyncCache,
};

const THREADS: usize = 4;

/// Runs `work(thread_index)` on `THREADS` threads that start together.
fn in_parallel<F>(work: F)
where
    F: Fn(usize) + Send + Sync + 'static,
{
    let work = Arc::new(work);
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|index| {
            let work = Arc::clone(&work);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                work(index);
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("worker thread panicked");
    }
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn wrappers_can_be_shared_between_threads() {
    assert_send_sync::<SyncCache<String, Vec<u8>>>();
    assert_send_sync::<ShardedCache<String, Vec<u8>>>();
}

#[test]
fn sync_cache_runs_a_loader_once_per_key() {
    let cache: Arc<SyncCache<u32, String>> =
        Arc::new(CacheBuilder::new().max_entries(8).build_sync().unwrap());
    let loads = Arc::new(AtomicUsize::new(0));

    let shared = Arc::clone(&cache);
    let counter = Arc::clone(&loads);
    in_parallel(move |_| {
        for _ in 0..200 {
            let value = shared.get_or_insert_with(7, |key| {
                counter.fetch_add(1, Ordering::SeqCst);
                format!("value-{key}")
            });
            assert_eq!(value, "value-7");
        }
    });

    assert_eq!(loads.load(Ordering::SeqCst), 1);
    let stats = cache.stats();
    assert_eq!(stats.loads, 1);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.hits, (THREADS * 200 - 1) as u64);
}

#[test]
fn sync_cache_stays_within_its_bound_under_contention() {
    let cache: Arc<SyncCache<usize, usize>> = Arc::new(
        CacheBuilder::new()
            .policy(PolicyKind::Lfu)
            .max_entries(16)
            .build_sync()
            .unwrap(),
    );

    let shared = Arc::clone(&cache);
    in_parallel(move |thread_index| {
        for step in 0..2000 {
            let key = (step * 7 + thread_index * 13) % 64;
            if step % 3 == 0 {
                shared.put(key, step);
            } else if step % 11 == 0 {
                shared.remove(&key);
            } else {
                shared.get(&key);
            }
            assert!(shared.len() <= 16);
        }
    });

    let stats = cache.stats();
    let gets_per_thread = (0..2000).filter(|step| step % 3 != 0 && step % 11 != 0).count();
    assert_eq!(stats.requests(), (THREADS * gets_per_thread) as u64);
    // Every entry that came in either is still there or left through one of
    // the counted exits.
    assert_eq!(
        stats.insertions,
        cache.len() as u64 + stats.evictions + stats.removals
    );
}

#[test]
fn sharded_cache_counts_every_write_exactly_once() {
    let cache: Arc<ShardedCache<String, usize>> = Arc::new(
        CacheBuilder::new()
            .max_entries(64)
            .build_sharded(4)
            .unwrap(),
    );
    assert_eq!(cache.shard_count(), 4);

    let shared = Arc::clone(&cache);
    in_parallel(move |thread_index| {
        for step in 0..500 {
            shared.put(format!("t{thread_index}-k{step}"), step);
        }
    });

    let stats = cache.stats();
    assert_eq!(stats.insertions, (THREADS * 500) as u64);
    assert_eq!(stats.updates, 0);
    assert!(cache.len() <= 64);
    assert_eq!(stats.insertions - stats.evictions, cache.len() as u64);

    let per_shard = cache.shard_stats();
    assert_eq!(per_shard.len(), 4);
    assert_eq!(per_shard.iter().copied().sum::<cachekit::Stats>(), stats);
    assert!(
        per_shard.iter().all(|shard| shard.insertions > 0),
        "2000 distinct keys should reach every shard: {per_shard:?}"
    );
}

#[test]
fn sharded_cache_routes_a_key_to_the_same_shard_every_time() {
    let cache: ShardedCache<String, u32> = CacheBuilder::new()
        .max_entries(40)
        .build_sharded(5)
        .unwrap();
    for number in 0..40u32 {
        let key = format!("key-{number}");
        let shard = cache.shard_index(&key);
        assert!(shard < 5);
        assert_eq!(cache.shard_index(&key), shard);
        cache.put(key.clone(), number);
        assert!(cache.with_shard(&key, |inner| inner.contains_key(&key)));
    }
    // Capacity is split per shard, so an unlucky distribution may evict a
    // few entries even though the total bound was not reached.
    assert!(cache.len() <= 40);
    let survivors = (0..40u32)
        .filter(|number| cache.peek(&format!("key-{number}")) == Some(*number))
        .count();
    assert_eq!(survivors, cache.len());
}

#[test]
fn listeners_see_every_eviction_from_every_thread() {
    let evicted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&evicted);
    let cache: Arc<ShardedCache<usize, usize>> = Arc::new(
        CacheBuilder::new()
            .policy(PolicyKind::Fifo)
            .max_entries(8)
            .listener(move |_key: &usize, _value: &usize, cause| {
                if cause.was_automatic() {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            })
            .build_sharded(2)
            .unwrap(),
    );

    let shared = Arc::clone(&cache);
    in_parallel(move |thread_index| {
        for step in 0..300 {
            shared.put(thread_index * 1000 + step, step);
        }
    });

    let stats = cache.stats();
    assert_eq!(stats.evictions as usize, THREADS * 300 - cache.len());
    assert_eq!(evicted.load(Ordering::SeqCst), stats.evictions as usize);
}

#[test]
fn shards_share_the_clock_for_expiry() {
    let clock = ManualClock::new();
    let cache: ShardedCache<u32, u32> = CacheBuilder::new()
        .max_entries(32)
        .ttl(Duration::from_secs(5))
        .clock(clock.clone())
        .build_sharded(4)
        .unwrap();
    for key in 0..16 {
        cache.put(key, key);
    }
    cache.put_with_ttl(100, 100, Duration::from_secs(60));
    assert_eq!(cache.len(), 17);

    clock.advance(Duration::from_secs(5));
    assert!(!cache.contains_key(&3));
    assert!(cache.contains_key(&100));
    assert_eq!(cache.purge_expired(), 16);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.stats().expirations, 16);
}

#[test]
fn failed_loads_cache_nothing_in_any_wrapper() {
    let sync: SyncCache<u32, u32> = CacheBuilder::new().max_entries(4).build_sync().unwrap();
    let sharded: ShardedCache<u32, u32> = CacheBuilder::new()
        .max_entries(4)
        .build_sharded(2)
        .unwrap();

    let error: LoadError<String> = sync
        .get_or_try_insert_with(1, |key| Err(format!("no backend for {key}")))
        .unwrap_err();
    assert_eq!(error.to_string(), "cache loader failed: no backend for 1");
    assert!(sync.is_empty());
    assert_eq!(sync.stats().load_failures, 1);

    let error = sharded
        .get_or_try_insert_with(1, |_| Err::<u32, _>("down"))
        .unwrap_err();
    assert_eq!(error.into_inner(), "down");
    assert!(sharded.is_empty());
    assert_eq!(sharded.get_or_try_insert_with(1, |_| Ok::<_, String>(9)), Ok(9));
    assert_eq!(sharded.get(&1), Some(9));
    assert_eq!(sharded.stats().load_failures, 1);
    assert_eq!(sharded.stats().loads, 1);
}

#[test]
fn a_panicking_loader_does_not_break_the_cache_for_other_threads() {
    let cache: Arc<SyncCache<u32, u32>> =
        Arc::new(CacheBuilder::new().max_entries(4).build_sync().unwrap());
    cache.put(1, 10);

    let shared = Arc::clone(&cache);
    let result = thread::spawn(move || {
        shared.get_or_insert_with(2, |_| panic!("loader exploded"));
    })
    .join();
    assert!(result.is_err());

    assert_eq!(cache.get(&1), Some(10));
    assert_eq!(cache.get(&2), None);
    assert_eq!(cache.get_or_insert_with(2, |_| 20), 20);
    assert_eq!(cache.len(), 2);
}
