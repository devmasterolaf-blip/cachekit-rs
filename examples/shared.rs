//! Sharing one cache between threads.
//!
//! Each worker asks the cache for the squares of a few numbers; the loader
//! only runs for numbers nobody has asked for yet.
//!
//! Run with `cargo run --example shared`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use cachekit::{CacheBuilder, PolicyKind, ShardedCache};

fn main() {
    let cache: Arc<ShardedCache<u64, u64>> = Arc::new(
        CacheBuilder::new()
            .policy(PolicyKind::Lfu)
            .max_entries(64)
            .build_sharded(4)
            .expect("the configuration is valid"),
    );
    let computed = Arc::new(AtomicUsize::new(0));

    let workers: Vec<_> = (0..4u64)
        .map(|worker| {
            let cache = Arc::clone(&cache);
            let computed = Arc::clone(&computed);
            thread::spawn(move || {
                let mut sum = 0;
                for step in 0..1000 {
                    let number = (step * (worker + 1)) % 32;
                    sum += cache.get_or_insert_with(number, |n| {
                        computed.fetch_add(1, Ordering::Relaxed);
                        n * n
                    });
                }
                sum
            })
        })
        .collect();

    for (worker, handle) in workers.into_iter().enumerate() {
        let sum = handle.join().expect("worker panicked");
        println!("worker {worker}: sum of squares = {sum}");
    }

    let stats = cache.stats();
    println!(
        "{} lookups, {} computed, hit rate {:.1}%",
        stats.requests(),
        computed.load(Ordering::Relaxed),
        stats.hit_rate() * 100.0
    );
    for (shard, stats) in cache.shard_stats().iter().enumerate() {
        println!("shard {shard}: {} hits, {} misses", stats.hits, stats.misses);
    }
}
