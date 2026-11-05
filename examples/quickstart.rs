//! A tour of the single-threaded cache: bounds, expiry, loading, removal
//! notifications and statistics.
//!
//! Run with `cargo run --example quickstart`.

use std::time::Duration;

use cachekit::{Cache, CacheBuilder, ManualClock, PolicyKind};

fn main() {
    // A manual clock makes the expiry below deterministic; leave `.clock()`
    // out to use the system's monotonic clock.
    let clock = ManualClock::new();

    let mut cache = CacheBuilder::new()
        .policy(PolicyKind::Lru)
        .max_entries(3)
        .ttl(Duration::from_secs(60))
        .clock(clock.clone())
        .listener(|key: &String, value: &String, cause| {
            println!("  removed {key} = {value} ({cause})");
        })
        .build()
        .expect("the configuration is valid");

    println!("filling the cache:");
    for (key, value) in [("a", "apple"), ("b", "banana"), ("c", "cherry")] {
        cache.put(key.to_string(), value.to_string());
    }

    // Reading "a" makes "b" the least recently used entry, so the next
    // insertion evicts "b".
    println!("reading a, then inserting d:");
    cache.get(&"a".to_string());
    cache.put("d".to_string(), "date".to_string());

    println!("loading a missing key:");
    let loaded = cache.get_or_insert_with("e".to_string(), |key| format!("loaded for {key}"));
    println!("  got '{loaded}'");

    println!("a minute later:");
    clock.advance(Duration::from_secs(60));
    let purged = cache.purge_expired();
    println!("  purged {purged} expired entries, {} left", cache.len());

    println!("statistics:\n{}", cache.stats());
}
