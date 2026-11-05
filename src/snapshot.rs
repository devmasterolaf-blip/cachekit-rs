//! Saving cache contents to a line-oriented text format and loading them
//! back.
//!
//! # Format
//!
//! ```text
//! cachekit-snapshot v1
//! <key> TAB <value> TAB <remaining ttl in ms, or "-">
//! ```
//!
//! The first line is a fixed header. Every following non-empty line is one
//! entry with three tab-separated fields. Keys and values are written with
//! their [`Display`] form and read back with
//! [`FromStr`]; backslash, tab, newline and carriage return inside them are
//! escaped as `\\`, `\t`, `\n` and `\r`. Entries appear in eviction order,
//! coldest first, so loading them in file order into an empty LRU or FIFO
//! cache reproduces the original order.
//!
//! Not everything survives a round trip: remaining TTLs are rounded up to
//! whole milliseconds, and LFU use counts are not stored, so every restored
//! entry starts with a count of one.

use std::error::Error;
use std::fmt::{self, Display};
use std::hash::Hash;
use std::io::{self, BufRead, Write};
use std::str::FromStr;
use std::time::Duration;

use crate::cache::{Cache, PolicyCache};
use crate::policy::Policy;

/// First line of every snapshot.
pub const HEADER: &str = "cachekit-snapshot v1";

/// Why a snapshot could not be written or read.
#[derive(Debug)]
#[non_exhaustive]
pub enum SnapshotError {
    /// The underlying reader or writer failed.
    Io(io::Error),
    /// The input does not start with the snapshot header. Holds the first
    /// line that was found instead (empty for empty input).
    BadHeader(String),
    /// An entry line does not have exactly three tab-separated fields.
    FieldCount {
        /// One-based line number.
        line: usize,
        /// Number of fields found.
        found: usize,
    },
    /// A key or value contains a backslash that does not start a known
    /// escape sequence.
    BadEscape {
        /// One-based line number.
        line: usize,
    },
    /// The TTL field is neither `-` nor a positive number of milliseconds.
    BadTtl {
        /// One-based line number.
        line: usize,
        /// The offending field.
        text: String,
    },
    /// The key could not be parsed into the cache's key type.
    BadKey {
        /// One-based line number.
        line: usize,
        /// The unescaped key text.
        text: String,
    },
    /// The value could not be parsed into the cache's value type.
    BadValue {
        /// One-based line number.
        line: usize,
        /// The unescaped value text.
        text: String,
    },
}

impl Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Io(error) => write!(f, "snapshot I/O failed: {error}"),
            SnapshotError::BadHeader(found) => write!(
                f,
                "not a cachekit snapshot: expected header '{HEADER}', found '{found}'"
            ),
            SnapshotError::FieldCount { line, found } => write!(
                f,
                "snapshot line {line}: expected 3 tab-separated fields, found {found}"
            ),
            SnapshotError::BadEscape { line } => {
                write!(f, "snapshot line {line}: invalid escape sequence")
            }
            SnapshotError::BadTtl { line, text } => write!(
                f,
                "snapshot line {line}: invalid ttl '{text}' (expected '-' or milliseconds > 0)"
            ),
            SnapshotError::BadKey { line, text } => {
                write!(f, "snapshot line {line}: cannot parse key '{text}'")
            }
            SnapshotError::BadValue { line, text } => {
                write!(f, "snapshot line {line}: cannot parse value '{text}'")
            }
        }
    }
}

impl Error for SnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SnapshotError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for SnapshotError {
    fn from(error: io::Error) -> Self {
        SnapshotError::Io(error)
    }
}

/// Writes the live entries of `cache` to `out` and returns how many were
/// written. Expired entries are left out. The cache is not modified.
pub fn save<K, V, P, W>(cache: &PolicyCache<K, V, P>, mut out: W) -> Result<usize, SnapshotError>
where
    K: Hash + Eq + Clone + Display,
    V: Display,
    P: Policy,
    W: Write,
{
    writeln!(out, "{HEADER}")?;
    let mut written = 0;
    for entry in cache.entries() {
        let key = escape(&entry.key.to_string());
        let value = escape(&entry.value.to_string());
        match entry.remaining_ttl {
            Some(ttl) => writeln!(out, "{key}\t{value}\t{}", ceil_millis(ttl))?,
            None => writeln!(out, "{key}\t{value}\t-")?,
        }
        written += 1;
    }
    out.flush()?;
    Ok(written)
}

/// Reads a snapshot from `input` and stores its entries in `cache`, in file
/// order, through the normal write path. Returns the number of entries read.
///
/// The whole input is parsed before the cache is touched, so a malformed
/// snapshot leaves the cache unchanged. Entries are added to whatever the
/// cache already holds; the usual capacity rules apply, so loading more
/// entries than fit evicts according to the policy. Entries recorded with a
/// TTL keep the remaining time; entries recorded without one get the
/// cache's default TTL, if it has one.
pub fn load<K, V, P, R>(cache: &mut PolicyCache<K, V, P>, input: R) -> Result<usize, SnapshotError>
where
    K: Hash + Eq + Clone + FromStr,
    V: FromStr,
    P: Policy,
    R: BufRead,
{
    let records = parse(input)?;
    let count = records.len();
    for (key, value, ttl) in records {
        match ttl {
            Some(ttl) => cache.put_with_ttl(key, value, ttl),
            None => cache.put(key, value),
        };
    }
    Ok(count)
}

type Record<K, V> = (K, V, Option<Duration>);

fn parse<K, V, R>(input: R) -> Result<Vec<Record<K, V>>, SnapshotError>
where
    K: FromStr,
    V: FromStr,
    R: BufRead,
{
    let mut lines = input.lines();
    let header = lines.next().transpose()?.unwrap_or_default();
    if header != HEADER {
        return Err(SnapshotError::BadHeader(header));
    }
    let mut records = Vec::new();
    for (index, line) in lines.enumerate() {
        let line = line?;
        let number = index + 2;
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let [key, value, ttl] = fields[..] else {
            return Err(SnapshotError::FieldCount {
                line: number,
                found: fields.len(),
            });
        };
        let key = unescape(key).ok_or(SnapshotError::BadEscape { line: number })?;
        let value = unescape(value).ok_or(SnapshotError::BadEscape { line: number })?;
        let ttl = parse_ttl(ttl).ok_or_else(|| SnapshotError::BadTtl {
            line: number,
            text: ttl.to_string(),
        })?;
        let key = key.parse::<K>().map_err(|_| SnapshotError::BadKey {
            line: number,
            text: key,
        })?;
        let value = value.parse::<V>().map_err(|_| SnapshotError::BadValue {
            line: number,
            text: value,
        })?;
        records.push((key, value, ttl));
    }
    Ok(records)
}

/// Parses the TTL field: `-` for none, otherwise milliseconds above zero.
/// The outer `Option` is `None` for invalid text.
fn parse_ttl(text: &str) -> Option<Option<Duration>> {
    if text == "-" {
        return Some(None);
    }
    match text.parse::<u64>() {
        Ok(millis) if millis > 0 => Some(Some(Duration::from_millis(millis))),
        _ => None,
    }
}

fn ceil_millis(duration: Duration) -> u128 {
    duration.as_nanos().div_ceil(1_000_000)
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Reverses [`escape`]. Returns `None` for an unknown or unfinished escape.
fn unescape(text: &str) -> Option<String> {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            plain.push(ch);
            continue;
        }
        match chars.next()? {
            '\\' => plain.push('\\'),
            't' => plain.push('\t'),
            'n' => plain.push('\n'),
            'r' => plain.push('\r'),
            _ => return None,
        }
    }
    Some(plain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::CacheBuilder;
    use crate::cache::LruCache;
    use crate::clock::ManualClock;
    use crate::policy::Lru;

    fn saved<K, V, P>(cache: &PolicyCache<K, V, P>) -> String
    where
        K: Hash + Eq + Clone + Display,
        V: Display,
        P: Policy,
    {
        let mut buffer = Vec::new();
        save(cache, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    fn load_error(text: &str) -> SnapshotError {
        let mut cache: LruCache<String, u32> = LruCache::new(8);
        let error = load(&mut cache, text.as_bytes()).unwrap_err();
        assert!(cache.is_empty(), "a failed load must not change the cache");
        error
    }

    #[test]
    fn escape_round_trips_special_characters() {
        let original = "tab\there\nnew\\line\rend";
        let escaped = escape(original);
        assert!(!escaped.contains('\t') && !escaped.contains('\n') && !escaped.contains('\r'));
        assert_eq!(unescape(&escaped).as_deref(), Some(original));
    }

    #[test]
    fn unescape_rejects_unknown_and_dangling_escapes() {
        assert_eq!(unescape("bad\\x"), None);
        assert_eq!(unescape("dangling\\"), None);
        assert_eq!(unescape("plain").as_deref(), Some("plain"));
    }

    #[test]
    fn save_writes_header_and_entries_coldest_first() {
        let mut cache = LruCache::new(3);
        cache.put("a".to_string(), 1);
        cache.put("b".to_string(), 2);
        cache.get(&"a".to_string());
        assert_eq!(saved(&cache), "cachekit-snapshot v1\nb\t2\t-\na\t1\t-\n");
    }

    #[test]
    fn empty_cache_saves_just_the_header() {
        let cache: LruCache<String, u32> = LruCache::new(1);
        assert_eq!(saved(&cache), "cachekit-snapshot v1\n");
        let mut restored: LruCache<String, u32> = LruCache::new(1);
        assert_eq!(load(&mut restored, saved(&cache).as_bytes()).unwrap(), 0);
    }

    #[test]
    fn round_trip_preserves_contents_and_lru_order() {
        let mut cache = LruCache::new(4);
        for (key, value) in [("x", 1), ("y", 2), ("z", 3)] {
            cache.put(key.to_string(), value);
        }
        cache.get(&"x".to_string());
        let text = saved(&cache);

        let mut restored: LruCache<String, i32> = LruCache::new(4);
        assert_eq!(load(&mut restored, text.as_bytes()).unwrap(), 3);
        let order: Vec<&String> = restored.keys().collect();
        assert_eq!(order, ["y", "z", "x"]);
        assert_eq!(restored.peek(&"z".to_string()), Some(&3));
    }

    #[test]
    fn round_trip_preserves_awkward_strings() {
        let mut cache = LruCache::new(4);
        cache.put("key\twith tab".to_string(), "line1\nline2".to_string());
        cache.put("back\\slash".to_string(), String::new());
        cache.put(String::new(), "empty key".to_string());
        let text = saved(&cache);
        assert_eq!(text.lines().count(), 4);

        let mut restored: LruCache<String, String> = LruCache::new(4);
        load(&mut restored, text.as_bytes()).unwrap();
        assert_eq!(
            restored.peek(&"key\twith tab".to_string()).map(String::as_str),
            Some("line1\nline2")
        );
        assert_eq!(
            restored.peek(&"back\\slash".to_string()).map(String::as_str),
            Some("")
        );
        assert_eq!(
            restored.peek(&String::new()).map(String::as_str),
            Some("empty key")
        );
    }

    #[test]
    fn remaining_ttl_survives_and_expired_entries_are_skipped() {
        let clock = ManualClock::new();
        let mut cache = CacheBuilder::new()
            .max_entries(4)
            .clock(clock.clone())
            .build_with(Lru::new())
            .unwrap();
        cache.put_with_ttl("gone".to_string(), 1, Duration::from_secs(1));
        cache.put_with_ttl("soon".to_string(), 2, Duration::from_millis(2500));
        cache.put("forever".to_string(), 3);
        clock.advance(Duration::from_secs(1));
        let text = saved(&cache);
        assert_eq!(text, "cachekit-snapshot v1\nsoon\t2\t1500\nforever\t3\t-\n");

        let other_clock = ManualClock::new();
        let mut restored: LruCache<String, i32> = CacheBuilder::new()
            .max_entries(4)
            .clock(other_clock.clone())
            .build_with(Lru::new())
            .unwrap();
        load(&mut restored, text.as_bytes()).unwrap();
        assert_eq!(
            restored.remaining_ttl(&"soon".to_string()),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(restored.remaining_ttl(&"forever".to_string()), None);
        other_clock.advance(Duration::from_millis(1500));
        assert!(!restored.contains_key(&"soon".to_string()));
        assert!(restored.contains_key(&"forever".to_string()));
    }

    #[test]
    fn sub_millisecond_ttl_is_rounded_up() {
        assert_eq!(ceil_millis(Duration::from_micros(1)), 1);
        assert_eq!(ceil_millis(Duration::from_millis(7)), 7);
        assert_eq!(ceil_millis(Duration::from_micros(7001)), 8);
    }

    #[test]
    fn loading_into_a_smaller_cache_keeps_the_hottest_entries() {
        let mut cache = LruCache::new(4);
        for key in ["a", "b", "c", "d"] {
            cache.put(key.to_string(), 0u8);
        }
        let mut small: LruCache<String, u8> = LruCache::new(2);
        assert_eq!(load(&mut small, saved(&cache).as_bytes()).unwrap(), 4);
        let kept: Vec<&String> = small.keys().collect();
        assert_eq!(kept, ["c", "d"]);
    }

    #[test]
    fn blank_lines_and_crlf_are_tolerated() {
        let text = "cachekit-snapshot v1\r\n\r\nk\t5\t-\r\n\n";
        let mut cache: LruCache<String, u32> = LruCache::new(2);
        assert_eq!(load(&mut cache, text.as_bytes()).unwrap(), 1);
        assert_eq!(cache.peek(&"k".to_string()), Some(&5));
    }

    #[test]
    fn missing_or_wrong_header_is_reported() {
        assert!(matches!(load_error(""), SnapshotError::BadHeader(found) if found.is_empty()));
        let error = load_error("cachekit-snapshot v2\nk\t1\t-\n");
        assert!(matches!(&error, SnapshotError::BadHeader(found) if found == "cachekit-snapshot v2"));
        assert!(error.to_string().contains("not a cachekit snapshot"));
    }

    #[test]
    fn malformed_lines_report_their_line_number() {
        let error = load_error("cachekit-snapshot v1\nok\t1\t-\nonly-two\t1\n");
        assert!(matches!(
            error,
            SnapshotError::FieldCount { line: 3, found: 2 }
        ));

        let error = load_error("cachekit-snapshot v1\nbad\\q\t1\t-\n");
        assert!(matches!(error, SnapshotError::BadEscape { line: 2 }));

        for ttl in ["0", "-5", "soon", ""] {
            let error = load_error(&format!("cachekit-snapshot v1\nk\t1\t{ttl}\n"));
            assert!(
                matches!(&error, SnapshotError::BadTtl { line: 2, text } if text == ttl),
                "ttl {ttl:?} gave {error:?}"
            );
        }

        let error = load_error("cachekit-snapshot v1\nk\tnot-a-number\t-\n");
        assert!(
            matches!(&error, SnapshotError::BadValue { line: 2, text } if text == "not-a-number")
        );
        assert_eq!(
            error.to_string(),
            "snapshot line 2: cannot parse value 'not-a-number'"
        );
    }

    #[test]
    fn unparsable_key_is_reported() {
        let mut cache: LruCache<u32, u32> = LruCache::new(2);
        let error = load(&mut cache, "cachekit-snapshot v1\nabc\t1\t-\n".as_bytes()).unwrap_err();
        assert!(matches!(&error, SnapshotError::BadKey { line: 2, text } if text == "abc"));
    }

    #[test]
    fn io_errors_are_wrapped() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::Other, "disk full"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let cache: LruCache<String, u32> = LruCache::new(1);
        let error = save(&cache, FailingWriter).unwrap_err();
        assert!(matches!(error, SnapshotError::Io(_)));
        assert!(error.source().is_some());
        assert!(error.to_string().contains("disk full"));
    }
}
