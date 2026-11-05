//! Traces: recorded sequences of cache operations in a small text format.
//!
//! A trace has one operation per line:
//!
//! ```text
//! # comment lines and blank lines are ignored
//! put <key> <value...>   store a value (the rest of the line, may be empty)
//! get <key>              look a key up
//! del <key>              remove a key
//! tick <milliseconds>    advance the simulated clock
//! purge                  drop every expired entry now
//! clear                  empty the cache
//! ```
//!
//! Operation names are case-insensitive. Keys are single whitespace-free
//! words; values run to the end of the line with surrounding whitespace
//! trimmed. There is no quoting, so a key cannot contain whitespace and a
//! `#` only starts a comment at the beginning of a line.
//!
//! [`replay`] runs parsed operations against a cache whose clock is a
//! [`ManualClock`], so traces with `tick` lines exercise expiry without
//! waiting:
//!
//! ```
//! use cachekit::trace::{self, ReplaySummary};
//! use cachekit::{Cache, CacheBuilder, ManualClock, PolicyKind};
//!
//! let ops = trace::parse("put a 1\nget a\nget b\n".as_bytes()).unwrap();
//! let clock = ManualClock::new();
//! let mut cache = CacheBuilder::new()
//!     .policy(PolicyKind::Lru)
//!     .max_entries(8)
//!     .clock(clock.clone())
//!     .build()
//!     .unwrap();
//! let summary: ReplaySummary = trace::replay(&mut cache, &clock, &ops);
//! assert_eq!(summary.operations, 3);
//! assert_eq!(cache.stats().hits, 1);
//! assert_eq!(cache.stats().misses, 1);
//! ```

use std::error::Error;
use std::fmt;
use std::io::{self, BufRead};
use std::time::Duration;

use crate::cache::{Cache, PolicyCache};
use crate::clock::ManualClock;
use crate::policy::Policy;

/// One operation of a trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Look a key up.
    Get(String),
    /// Store a value under a key.
    Put(String, String),
    /// Remove a key.
    Delete(String),
    /// Advance the simulated clock.
    Tick(Duration),
    /// Drop every expired entry.
    Purge,
    /// Empty the cache.
    Clear,
}

impl Op {
    /// The operation's name as written in a trace.
    pub fn name(&self) -> &'static str {
        match self {
            Op::Get(_) => "get",
            Op::Put(..) => "put",
            Op::Delete(_) => "del",
            Op::Tick(_) => "tick",
            Op::Purge => "purge",
            Op::Clear => "clear",
        }
    }
}

impl fmt::Display for Op {
    /// Writes the operation as a trace line (without the line ending).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Op::Get(key) => write!(f, "get {key}"),
            Op::Put(key, value) if value.is_empty() => write!(f, "put {key}"),
            Op::Put(key, value) => write!(f, "put {key} {value}"),
            Op::Delete(key) => write!(f, "del {key}"),
            Op::Tick(by) => write!(f, "tick {}", by.as_millis()),
            Op::Purge => f.write_str("purge"),
            Op::Clear => f.write_str("clear"),
        }
    }
}

/// What is wrong with a single trace line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LineError {
    /// The first word is not a known operation.
    UnknownOp(String),
    /// The operation needs an argument that is not there.
    MissingArgument {
        /// The operation being parsed.
        op: &'static str,
        /// The name of the missing argument.
        argument: &'static str,
    },
    /// There is text after the operation's last argument.
    UnexpectedArgument {
        /// The operation being parsed.
        op: &'static str,
        /// The text that should not be there.
        extra: String,
    },
    /// The argument of `tick` is not a whole number of milliseconds.
    BadNumber(String),
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LineError::UnknownOp(op) => write!(
                f,
                "unknown operation '{op}' (expected get, put, del, tick, purge or clear)"
            ),
            LineError::MissingArgument { op, argument } => {
                write!(f, "'{op}' needs a {argument}")
            }
            LineError::UnexpectedArgument { op, extra } => {
                write!(f, "unexpected text '{extra}' after '{op}'")
            }
            LineError::BadNumber(text) => {
                write!(f, "'{text}' is not a whole number of milliseconds")
            }
        }
    }
}

impl Error for LineError {}

/// Why a trace could not be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum TraceError {
    /// The underlying reader failed.
    Io(io::Error),
    /// A line could not be parsed.
    Syntax {
        /// One-based line number.
        line: usize,
        /// What is wrong with the line.
        error: LineError,
    },
}

impl TraceError {
    /// The one-based number of the offending line, if the problem is tied
    /// to one.
    pub fn line(&self) -> Option<usize> {
        match self {
            TraceError::Io(_) => None,
            TraceError::Syntax { line, .. } => Some(*line),
        }
    }
}

impl fmt::Display for TraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TraceError::Io(error) => write!(f, "cannot read trace: {error}"),
            TraceError::Syntax { line, error } => write!(f, "trace line {line}: {error}"),
        }
    }
}

impl Error for TraceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            TraceError::Io(error) => Some(error),
            TraceError::Syntax { error, .. } => Some(error),
        }
    }
}

impl From<io::Error> for TraceError {
    fn from(error: io::Error) -> Self {
        TraceError::Io(error)
    }
}

/// Parses one line of a trace. Blank lines and `#` comments give
/// `Ok(None)`.
pub fn parse_line(line: &str) -> Result<Option<Op>, LineError> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let (word, rest) = split_word(line);
    let op = match word.to_ascii_lowercase().as_str() {
        "get" => Op::Get(single_argument("get", "key", rest)?),
        "del" => Op::Delete(single_argument("del", "key", rest)?),
        "put" => {
            let (key, value) = split_word(rest);
            if key.is_empty() {
                return Err(LineError::MissingArgument {
                    op: "put",
                    argument: "key",
                });
            }
            Op::Put(key.to_string(), value.to_string())
        }
        "tick" => {
            let text = single_argument("tick", "duration in milliseconds", rest)?;
            let millis = text
                .parse::<u64>()
                .map_err(|_| LineError::BadNumber(text))?;
            Op::Tick(Duration::from_millis(millis))
        }
        "purge" => {
            no_arguments("purge", rest)?;
            Op::Purge
        }
        "clear" => {
            no_arguments("clear", rest)?;
            Op::Clear
        }
        _ => return Err(LineError::UnknownOp(word.to_string())),
    };
    Ok(Some(op))
}

/// Reads a whole trace, skipping blank lines and comments.
pub fn parse<R: BufRead>(input: R) -> Result<Vec<Op>, TraceError> {
    let mut ops = Vec::new();
    for (index, line) in input.lines().enumerate() {
        let line = line?;
        match parse_line(&line) {
            Ok(Some(op)) => ops.push(op),
            Ok(None) => {}
            Err(error) => {
                return Err(TraceError::Syntax {
                    line: index + 1,
                    error,
                })
            }
        }
    }
    Ok(ops)
}

/// Totals of one [`replay`] run. Hit and miss counts live in the cache's
/// own [`Stats`](crate::Stats).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplaySummary {
    /// Number of operations applied.
    pub operations: usize,
    /// Total time the `tick` operations moved the clock forward.
    pub simulated: Duration,
    /// Entries dropped by `purge` operations.
    pub purged: usize,
}

/// Applies one operation to `cache`. `clock` must be the clock the cache
/// was built with, otherwise `tick` has no effect on expiry. Returns the
/// number of entries a `purge` dropped (zero for every other operation).
pub fn apply<P: Policy>(
    cache: &mut PolicyCache<String, String, P>,
    clock: &ManualClock,
    op: &Op,
) -> usize {
    match op {
        Op::Get(key) => {
            cache.get(key);
        }
        Op::Put(key, value) => {
            cache.put(key.clone(), value.clone());
        }
        Op::Delete(key) => {
            cache.remove(key);
        }
        Op::Tick(by) => clock.advance(*by),
        Op::Purge => return cache.purge_expired(),
        Op::Clear => cache.clear(),
    }
    0
}

/// Applies `ops` to `cache` in order and returns the totals.
pub fn replay<'a, P, I>(
    cache: &mut PolicyCache<String, String, P>,
    clock: &ManualClock,
    ops: I,
) -> ReplaySummary
where
    P: Policy,
    I: IntoIterator<Item = &'a Op>,
{
    let mut summary = ReplaySummary::default();
    for op in ops {
        summary.purged += apply(cache, clock, op);
        summary.operations += 1;
        if let Op::Tick(by) = op {
            summary.simulated = summary.simulated.saturating_add(*by);
        }
    }
    summary
}

/// Splits off the first whitespace-delimited word; the remainder is
/// trimmed. Both halves are empty for blank input.
fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    match text.find(char::is_whitespace) {
        Some(end) => (&text[..end], text[end..].trim()),
        None => (text, ""),
    }
}

fn single_argument(
    op: &'static str,
    argument: &'static str,
    rest: &str,
) -> Result<String, LineError> {
    let (word, extra) = split_word(rest);
    if word.is_empty() {
        return Err(LineError::MissingArgument { op, argument });
    }
    if !extra.is_empty() {
        return Err(LineError::UnexpectedArgument {
            op,
            extra: extra.to_string(),
        });
    }
    Ok(word.to_string())
}

fn no_arguments(op: &'static str, rest: &str) -> Result<(), LineError> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(LineError::UnexpectedArgument {
            op,
            extra: rest.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::CacheBuilder;
    use crate::cache::DynCache;
    use crate::policy::PolicyKind;

    fn replayed(
        policy: PolicyKind,
        capacity: usize,
        ttl: Option<Duration>,
        text: &str,
    ) -> (DynCache<String, String>, ReplaySummary) {
        let clock = ManualClock::new();
        let mut builder = CacheBuilder::new()
            .policy(policy)
            .max_entries(capacity)
            .clock(clock.clone());
        if let Some(ttl) = ttl {
            builder = builder.ttl(ttl);
        }
        let mut cache = builder.build().unwrap();
        let ops = parse(text.as_bytes()).unwrap();
        let summary = replay(&mut cache, &clock, &ops);
        (cache, summary)
    }

    fn op(line: &str) -> Op {
        parse_line(line).unwrap().unwrap()
    }

    #[test]
    fn parses_every_operation() {
        assert_eq!(op("get a"), Op::Get("a".into()));
        assert_eq!(op("put a 1"), Op::Put("a".into(), "1".into()));
        assert_eq!(op("del a"), Op::Delete("a".into()));
        assert_eq!(op("tick 250"), Op::Tick(Duration::from_millis(250)));
        assert_eq!(op("purge"), Op::Purge);
        assert_eq!(op("clear"), Op::Clear);
    }

    #[test]
    fn names_are_case_insensitive_and_whitespace_is_flexible() {
        assert_eq!(op("  GET\tkey  "), Op::Get("key".into()));
        assert_eq!(op("Put  k   v"), Op::Put("k".into(), "v".into()));
        assert_eq!(op("TICK 0"), Op::Tick(Duration::ZERO));
    }

    #[test]
    fn put_value_runs_to_the_end_of_the_line() {
        assert_eq!(
            op("put greeting hello  wide   world "),
            Op::Put("greeting".into(), "hello  wide   world".into())
        );
        assert_eq!(op("put empty"), Op::Put("empty".into(), String::new()));
        assert_eq!(op("put k # not a comment"), Op::Put("k".into(), "# not a comment".into()));
    }

    #[test]
    fn blank_lines_and_comments_are_skipped() {
        assert_eq!(parse_line(""), Ok(None));
        assert_eq!(parse_line("   \t"), Ok(None));
        assert_eq!(parse_line("# get a"), Ok(None));
        assert_eq!(parse_line("   #indented"), Ok(None));
    }

    #[test]
    fn missing_arguments_are_reported() {
        assert_eq!(
            parse_line("get"),
            Err(LineError::MissingArgument {
                op: "get",
                argument: "key"
            })
        );
        assert_eq!(
            parse_line("put"),
            Err(LineError::MissingArgument {
                op: "put",
                argument: "key"
            })
        );
        let error = parse_line("tick").unwrap_err();
        assert_eq!(error.to_string(), "'tick' needs a duration in milliseconds");
    }

    #[test]
    fn extra_arguments_are_reported() {
        assert_eq!(
            parse_line("get a b c"),
            Err(LineError::UnexpectedArgument {
                op: "get",
                extra: "b c".into()
            })
        );
        assert_eq!(
            parse_line("purge now"),
            Err(LineError::UnexpectedArgument {
                op: "purge",
                extra: "now".into()
            })
        );
        assert!(parse_line("clear all").is_err());
        assert!(parse_line("del a b").is_err());
    }

    #[test]
    fn bad_numbers_and_unknown_operations_are_reported() {
        for text in ["soon", "-5", "1.5", "99999999999999999999999"] {
            assert_eq!(
                parse_line(&format!("tick {text}")),
                Err(LineError::BadNumber(text.into())),
            );
        }
        let error = parse_line("touch a").unwrap_err();
        assert_eq!(error, LineError::UnknownOp("touch".into()));
        assert!(error.to_string().contains("unknown operation 'touch'"));
    }

    #[test]
    fn parse_collects_operations_in_order() {
        let text = "# warm up\nput a 1\n\nget a\r\ntick 10\nget b\n";
        let ops = parse(text.as_bytes()).unwrap();
        assert_eq!(
            ops,
            [
                Op::Put("a".into(), "1".into()),
                Op::Get("a".into()),
                Op::Tick(Duration::from_millis(10)),
                Op::Get("b".into()),
            ]
        );
        assert!(parse("".as_bytes()).unwrap().is_empty());
    }

    #[test]
    fn parse_reports_the_line_number() {
        let error = parse("put a 1\n# fine\nfrobnicate\nget a\n".as_bytes()).unwrap_err();
        assert_eq!(error.line(), Some(3));
        assert_eq!(
            error.to_string(),
            "trace line 3: unknown operation 'frobnicate' \
             (expected get, put, del, tick, purge or clear)"
        );
        assert!(error.source().is_some());
    }

    #[test]
    fn invalid_utf8_is_an_io_error() {
        let error = parse(&b"get \xff\n"[..]).unwrap_err();
        assert!(matches!(error, TraceError::Io(_)));
        assert_eq!(error.line(), None);
    }

    #[test]
    fn display_round_trips_through_the_parser() {
        let ops = [
            Op::Get("a".into()),
            Op::Put("a".into(), "some value".into()),
            Op::Put("b".into(), String::new()),
            Op::Delete("a".into()),
            Op::Tick(Duration::from_millis(1500)),
            Op::Purge,
            Op::Clear,
        ];
        for original in ops {
            assert_eq!(op(&original.to_string()), original);
            assert!(original.to_string().starts_with(original.name()));
        }
    }

    #[test]
    fn replay_counts_hits_and_misses() {
        let text = "put a 1\nput b 2\nget a\nget c\ndel b\nget b\n";
        let (cache, summary) = replayed(PolicyKind::Lru, 4, None, text);
        assert_eq!(summary.operations, 6);
        assert_eq!(summary.simulated, Duration::ZERO);
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses), (1, 2));
        assert_eq!((stats.insertions, stats.removals), (2, 1));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn replay_shows_the_difference_between_policies() {
        // "a" is read again before "c" arrives: LRU then evicts "b", FIFO
        // still evicts "a" because it arrived first.
        let text = "put a 1\nput b 2\nget a\nput c 3\nget a\nget b\n";
        let (lru, _) = replayed(PolicyKind::Lru, 2, None, text);
        let (fifo, _) = replayed(PolicyKind::Fifo, 2, None, text);
        assert_eq!((lru.stats().hits, lru.stats().misses), (2, 1));
        assert_eq!((fifo.stats().hits, fifo.stats().misses), (2, 1));
        assert!(lru.contains_key(&"a".to_string()));
        assert!(!fifo.contains_key(&"a".to_string()));
        assert_eq!(lru.stats().evictions, 1);
    }

    #[test]
    fn tick_drives_expiry_and_purge_is_counted() {
        let text = "put a 1\ntick 400\nput b 2\ntick 700\nget a\npurge\ntick 500\npurge\n";
        let (cache, summary) =
            replayed(PolicyKind::Lru, 4, Some(Duration::from_secs(1)), text);
        assert_eq!(summary.operations, 8);
        assert_eq!(summary.simulated, Duration::from_millis(1600));
        // "a" expired on lookup, "b" was dropped by the second purge.
        assert_eq!(summary.purged, 1);
        assert_eq!(cache.stats().expirations, 2);
        assert!(cache.is_empty());
    }

    #[test]
    fn clear_is_replayed() {
        let (cache, summary) = replayed(PolicyKind::Lfu, 4, None, "put a 1\nput b 2\nclear\n");
        assert_eq!(summary.operations, 3);
        assert!(cache.is_empty());
        assert_eq!(cache.stats().removals, 2);
    }
}
