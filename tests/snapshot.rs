//! Snapshots written to and read from real files.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cachekit::snapshot::{self, SnapshotError};
use cachekit::{
    Cache, CacheBuilder, DynCache, LruCache, ManualClock, PolicyKind, RemovalCause, SyncCache,
};

/// A fresh path under Cargo's per-target scratch directory.
fn scratch_file(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("snapshot-tests");
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn cache_of(policy: PolicyKind, max_entries: usize) -> DynCache<String, String> {
    CacheBuilder::new()
        .policy(policy)
        .max_entries(max_entries)
        .build()
        .unwrap()
}

fn keys(cache: &DynCache<String, String>) -> Vec<String> {
    cache.keys().cloned().collect()
}

#[test]
fn every_policy_round_trips_its_contents_through_a_file() {
    for policy in PolicyKind::ALL {
        let mut cache = cache_of(policy, 8);
        for number in 0..6 {
            cache.put(format!("key-{number}"), format!("value {number}"));
        }
        cache.get(&"key-0".to_string());
        cache.get(&"key-3".to_string());
        cache.get(&"key-3".to_string());

        let path = scratch_file(&format!("round-trip-{policy}.snap"));
        let written = snapshot::save(&cache, BufWriter::new(File::create(&path).unwrap()));
        assert_eq!(written.unwrap(), 6);

        let mut restored = cache_of(policy, 8);
        let read = snapshot::load(&mut restored, BufReader::new(File::open(&path).unwrap()));
        assert_eq!(read.unwrap(), 6, "{policy}");
        assert_eq!(restored.len(), 6);
        for number in 0..6 {
            assert_eq!(
                restored.peek(&format!("key-{number}")),
                Some(&format!("value {number}")),
                "{policy}"
            );
        }
        // The file lists entries coldest first, and a restored cache evicts
        // in file order whatever the policy.
        assert_eq!(keys(&restored), keys(&cache), "{policy}");
    }
}

#[test]
fn lfu_use_counts_start_over_after_a_restore() {
    let mut cache = cache_of(PolicyKind::Lfu, 2);
    cache.put("popular".to_string(), "1".to_string());
    for _ in 0..5 {
        cache.get(&"popular".to_string());
    }
    cache.put("fresh".to_string(), "2".to_string());
    assert_eq!(keys(&cache), ["fresh", "popular"]);

    let mut buffer = Vec::new();
    snapshot::save(&cache, &mut buffer).unwrap();
    let mut restored = cache_of(PolicyKind::Lfu, 2);
    snapshot::load(&mut restored, buffer.as_slice()).unwrap();
    assert_eq!(keys(&restored), ["fresh", "popular"]);

    // In the original, "popular" has six uses and survives two reads of
    // "fresh". In the restored cache both start at one use, so the same
    // reads make "popular" the next victim.
    for target in [&mut cache, &mut restored] {
        target.get(&"fresh".to_string());
        target.get(&"fresh".to_string());
    }
    assert_eq!(keys(&cache), ["fresh", "popular"]);
    assert_eq!(keys(&restored), ["popular", "fresh"]);
}

#[test]
fn weights_are_recomputed_by_the_receiving_cache() {
    let build = |max_weight| {
        CacheBuilder::new()
            .policy(PolicyKind::Lru)
            .max_weight(max_weight)
            .weigher(|_key: &String, value: &String| value.len() as u64)
            .build()
            .unwrap()
    };
    let mut cache: DynCache<String, String> = build(100);
    cache.put("a".to_string(), "x".repeat(30));
    cache.put("b".to_string(), "y".repeat(30));
    cache.put("c".to_string(), "z".repeat(30));
    assert_eq!(cache.weight(), 90);

    let mut buffer = Vec::new();
    snapshot::save(&cache, &mut buffer).unwrap();

    let mut smaller: DynCache<String, String> = build(60);
    assert_eq!(snapshot::load(&mut smaller, buffer.as_slice()).unwrap(), 3);
    assert_eq!(smaller.weight(), 60);
    assert_eq!(keys(&smaller), ["b", "c"]);
    assert_eq!(smaller.stats().evictions, 1);

    let mut tiny: DynCache<String, String> = build(20);
    assert_eq!(snapshot::load(&mut tiny, buffer.as_slice()).unwrap(), 3);
    assert!(tiny.is_empty());
    assert_eq!(tiny.stats().rejections, 3);
}

#[test]
fn loading_merges_into_existing_contents_and_notifies_listeners() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let mut cache: DynCache<String, u32> = CacheBuilder::new()
        .policy(PolicyKind::Fifo)
        .max_entries(3)
        .listener(move |key: &String, value: &u32, cause| {
            sink.lock().unwrap().push((key.clone(), *value, cause));
        })
        .build()
        .unwrap();
    cache.put("old".to_string(), 1);
    cache.put("shared".to_string(), 2);

    let text = "cachekit-snapshot v1\nshared\t20\t-\nnew-1\t30\t-\nnew-2\t40\t-\n";
    assert_eq!(snapshot::load(&mut cache, text.as_bytes()).unwrap(), 3);

    assert_eq!(cache.len(), 3);
    assert_eq!(cache.peek(&"shared".to_string()), Some(&20));
    assert!(!cache.contains_key(&"old".to_string()));
    assert_eq!(
        *events.lock().unwrap(),
        [
            ("shared".to_string(), 2, RemovalCause::Replaced),
            ("old".to_string(), 1, RemovalCause::Evicted),
        ]
    );
    let stats = cache.stats();
    assert_eq!((stats.insertions, stats.updates, stats.evictions), (4, 1, 1));
}

#[test]
fn ttls_are_carried_over_relative_to_the_new_clock() {
    let clock = ManualClock::starting_at(Duration::from_secs(1000));
    let mut cache: DynCache<String, String> = CacheBuilder::new()
        .max_entries(4)
        .ttl(Duration::from_secs(30))
        .clock(clock.clone())
        .build()
        .unwrap();
    cache.put("default".to_string(), "a".to_string());
    cache.put_with_ttl("short".to_string(), "b".to_string(), Duration::from_secs(5));
    clock.advance(Duration::from_secs(2));

    let path = scratch_file("ttl.snap");
    snapshot::save(&cache, File::create(&path).unwrap()).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text, "cachekit-snapshot v1\ndefault\ta\t28000\nshort\tb\t3000\n");

    // The receiving cache has a different clock and a different default.
    let later = ManualClock::starting_at(Duration::from_secs(5));
    let mut restored: DynCache<String, String> = CacheBuilder::new()
        .max_entries(4)
        .ttl(Duration::from_secs(600))
        .clock(later.clone())
        .build()
        .unwrap();
    snapshot::load(&mut restored, text.as_bytes()).unwrap();
    let remaining = |cache: &DynCache<String, String>, key: &str| {
        cache.remaining_ttl(&key.to_string()).map(|ttl| ttl.as_secs())
    };
    assert_eq!(remaining(&restored, "default"), Some(28));
    assert_eq!(remaining(&restored, "short"), Some(3));

    later.advance(Duration::from_secs(3));
    assert!(!restored.contains_key(&"short".to_string()));
    assert!(restored.contains_key(&"default".to_string()));
}

#[test]
fn entries_without_a_ttl_get_the_receiving_default() {
    let clock = ManualClock::new();
    let mut cache: DynCache<String, u8> = CacheBuilder::new()
        .max_entries(2)
        .ttl(Duration::from_secs(10))
        .clock(clock.clone())
        .build()
        .unwrap();
    snapshot::load(&mut cache, "cachekit-snapshot v1\nk\t1\t-\n".as_bytes()).unwrap();
    assert_eq!(
        cache.remaining_ttl(&"k".to_string()),
        Some(Duration::from_secs(10))
    );
}

#[test]
fn unicode_and_control_characters_survive_a_file_round_trip() {
    let mut cache: LruCache<String, String> = LruCache::new(8);
    let pairs = [
        ("plain", "value"),
        ("emoji \u{1F980}", "\u{00E9}t\u{00E9}\t\u{4E2D}\u{6587}"),
        ("multi\nline\r\nkey", "trailing backslash \\"),
        ("\\t literal", "\\n is not a newline here"),
        ("spaces  inside ", "  padded  "),
    ];
    for (key, value) in pairs {
        cache.put(key.to_string(), value.to_string());
    }

    let path = scratch_file("unicode.snap");
    snapshot::save(&cache, File::create(&path).unwrap()).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 6);

    let mut restored: LruCache<String, String> = LruCache::new(8);
    snapshot::load(&mut restored, BufReader::new(File::open(&path).unwrap())).unwrap();
    for (key, value) in pairs {
        assert_eq!(
            restored.peek(&key.to_string()).map(String::as_str),
            Some(value),
            "key {key:?}"
        );
    }
}

#[test]
fn typed_keys_and_values_round_trip() {
    let mut cache: LruCache<u64, f64> = LruCache::new(4);
    cache.put(18_446_744_073_709_551_615, 0.1);
    cache.put(0, -2.5e-9);
    cache.put(42, f64::INFINITY);

    let mut buffer = Vec::new();
    snapshot::save(&cache, &mut buffer).unwrap();
    let mut restored: LruCache<u64, f64> = LruCache::new(4);
    snapshot::load(&mut restored, buffer.as_slice()).unwrap();
    assert_eq!(restored.peek(&u64::MAX), Some(&0.1));
    assert_eq!(restored.peek(&0), Some(&-2.5e-9));
    assert_eq!(restored.peek(&42), Some(&f64::INFINITY));

    // The same file cannot be read as a cache of small integers.
    let mut wrong: LruCache<u8, f64> = LruCache::new(4);
    let error = snapshot::load(&mut wrong, buffer.as_slice()).unwrap_err();
    assert!(matches!(error, SnapshotError::BadKey { line: 2, .. }));
    assert!(wrong.is_empty());
}

#[test]
fn a_synchronised_cache_is_saved_and_restored_through_with() {
    let cache: SyncCache<String, u32> = CacheBuilder::new().max_entries(4).build_sync().unwrap();
    cache.put("a".to_string(), 1);
    cache.put("b".to_string(), 2);

    let path = scratch_file("sync.snap");
    let written = cache.with(|inner| snapshot::save(inner, File::create(&path).unwrap()));
    assert_eq!(written.unwrap(), 2);

    let restored: SyncCache<String, u32> =
        CacheBuilder::new().max_entries(4).build_sync().unwrap();
    let file = BufReader::new(File::open(&path).unwrap());
    assert_eq!(restored.with(|inner| snapshot::load(inner, file)).unwrap(), 2);
    assert_eq!(restored.get(&"a".to_string()), Some(1));
    assert_eq!(restored.get(&"b".to_string()), Some(2));
}

#[test]
fn a_truncated_file_is_rejected_and_changes_nothing() {
    let mut cache: LruCache<String, String> = LruCache::new(4);
    cache.put("keep".to_string(), "me".to_string());

    let path = scratch_file("truncated.snap");
    fs::write(&path, "cachekit-snapshot v1\ngood\tvalue\t-\nbroken\tval").unwrap();
    let error = snapshot::load(&mut cache, BufReader::new(File::open(&path).unwrap()));
    let error = error.unwrap_err();
    assert_eq!(
        error.to_string(),
        "snapshot line 3: expected 3 tab-separated fields, found 2"
    );
    assert_eq!(cache.len(), 1);
    assert!(cache.contains_key(&"keep".to_string()));
    assert_eq!(cache.stats().insertions, 1);
}
