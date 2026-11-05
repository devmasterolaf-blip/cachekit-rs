//! The `cachekit` command line tool.
//!
//! The tool lives in the library so that it can be driven from tests: the
//! binary only collects the process arguments and calls [`run`].

use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::str::FromStr;
use std::time::Duration;

use crate::builder::CacheBuilder;
use crate::cache::{Cache, DynCache};
use crate::clock::ManualClock;
use crate::error::ConfigError;
use crate::policy::PolicyKind;
use crate::snapshot::{self, SnapshotError};
use crate::trace::{self, Op, ReplaySummary, TraceError};

/// Help text printed by `cachekit help`.
pub const USAGE: &str = "\
cachekit - replay cache traces against an eviction policy

Usage:
  cachekit replay <trace> [options]    run one policy and print its statistics
  cachekit compare <trace> [options]   run every policy and print a table
  cachekit help
  cachekit --version

<trace> is a file with one operation per line (get, put, del, tick, purge,
clear); use - to read it from standard input.

Options:
  --policy <name>     eviction policy: lru, lfu or fifo (default: lru;
                      replay only)
  --capacity <n>      keep at most <n> entries
  --max-weight <n>    keep at most <n> bytes of keys plus values
  --ttl-ms <n>        expire entries <n> simulated milliseconds after a write
  --load <file>       fill the cache from a snapshot before the replay
                      (replay only)
  --save <file>       write the final contents to a snapshot (replay only)

At least one of --capacity and --max-weight is required.";

/// Why the tool stopped without doing its job.
#[derive(Debug)]
#[non_exhaustive]
pub enum CliError {
    /// The command line is wrong; the message says how.
    Usage(String),
    /// A file could not be opened.
    Open {
        /// The path as given on the command line.
        path: String,
        /// The underlying error.
        source: io::Error,
    },
    /// The trace could not be read or parsed.
    Trace {
        /// The path as given on the command line.
        path: String,
        /// The underlying error.
        source: TraceError,
    },
    /// A snapshot could not be loaded or saved.
    Snapshot {
        /// The path as given on the command line.
        path: String,
        /// The underlying error.
        source: SnapshotError,
    },
    /// Writing the report failed.
    Output(io::Error),
}

impl CliError {
    /// The process exit code for this error: 2 for a wrong command line,
    /// 1 for everything else.
    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::Usage(_) => 2,
            _ => 1,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Usage(message) => f.write_str(message),
            CliError::Open { path, source } => write!(f, "cannot open '{path}': {source}"),
            CliError::Trace { path, source } => write!(f, "{path}: {source}"),
            CliError::Snapshot { path, source } => write!(f, "{path}: {source}"),
            CliError::Output(source) => write!(f, "cannot write output: {source}"),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            CliError::Usage(_) => None,
            CliError::Open { source, .. } => Some(source),
            CliError::Trace { source, .. } => Some(source),
            CliError::Snapshot { source, .. } => Some(source),
            CliError::Output(source) => Some(source),
        }
    }
}

fn usage(message: impl Into<String>) -> CliError {
    CliError::Usage(message.into())
}

/// Runs the tool with `args` (the process arguments without the program
/// name), reading `-` traces from `stdin` and writing the report to `out`.
pub fn run(args: &[String], stdin: &mut dyn BufRead, out: &mut dyn Write) -> Result<(), CliError> {
    let Some((command, rest)) = args.split_first() else {
        return Err(usage("missing command"));
    };
    let wants_help = |arg: &String| arg == "--help" || arg == "-h";
    match command.as_str() {
        "help" | "--help" | "-h" => writeln!(out, "{USAGE}").map_err(CliError::Output),
        "replay" | "compare" if rest.iter().any(wants_help) => {
            writeln!(out, "{USAGE}").map_err(CliError::Output)
        }
        "--version" | "-V" => {
            writeln!(out, "cachekit {}", env!("CARGO_PKG_VERSION")).map_err(CliError::Output)
        }
        "replay" => {
            let options = Options::parse(rest)?;
            let ops = read_trace(&options.trace, stdin)?;
            replay_command(&options, &ops, out)
        }
        "compare" => {
            let options = Options::parse(rest)?;
            if options.policy.is_some() {
                return Err(usage("--policy cannot be used with compare"));
            }
            if options.load.is_some() || options.save.is_some() {
                return Err(usage("--load and --save cannot be used with compare"));
            }
            let ops = read_trace(&options.trace, stdin)?;
            compare_command(&options, &ops, out)
        }
        other => Err(usage(format!("unknown command '{other}'"))),
    }
}

/// Everything the command line can configure.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    trace: String,
    policy: Option<PolicyKind>,
    capacity: Option<usize>,
    max_weight: Option<u64>,
    ttl: Option<Duration>,
    load: Option<String>,
    save: Option<String>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, CliError> {
        let mut trace = None;
        let mut policy = None;
        let mut capacity = None;
        let mut max_weight = None;
        let mut ttl = None;
        let mut load = None;
        let mut save = None;

        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "-" || !arg.starts_with('-') {
                if trace.is_some() {
                    return Err(usage(format!("unexpected argument '{arg}'")));
                }
                trace = Some(arg.clone());
                continue;
            }
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (arg.as_str(), None),
            };
            let mut value = || match inline {
                Some(value) => Ok(value.to_string()),
                None => args
                    .next()
                    .cloned()
                    .ok_or_else(|| usage(format!("{name} needs a value"))),
            };
            match name {
                "--policy" => {
                    let kind = value()?
                        .parse()
                        .map_err(|error| usage(format!("--policy: {error}")))?;
                    policy = Some(kind);
                }
                "--capacity" => capacity = Some(number(name, &value()?)?),
                "--max-weight" => max_weight = Some(number(name, &value()?)?),
                "--ttl-ms" => ttl = Some(Duration::from_millis(number(name, &value()?)?)),
                "--load" => load = Some(value()?),
                "--save" => save = Some(value()?),
                _ => return Err(usage(format!("unknown option '{name}'"))),
            }
        }

        let trace = trace.ok_or_else(|| usage("missing trace file"))?;
        let options = Options {
            trace,
            policy,
            capacity,
            max_weight,
            ttl,
            load,
            save,
        };
        options
            .builder(PolicyKind::Lru, &ManualClock::new())
            .validate()
            .map_err(|error| usage(describe(&error)))?;
        Ok(options)
    }

    /// A builder for a cache with these limits, the given policy and clock.
    fn builder(&self, policy: PolicyKind, clock: &ManualClock) -> CacheBuilder<String, String> {
        let mut builder = CacheBuilder::new().policy(policy).clock(clock.clone());
        if let Some(capacity) = self.capacity {
            builder = builder.max_entries(capacity);
        }
        if let Some(max_weight) = self.max_weight {
            builder = builder
                .max_weight(max_weight)
                .weigher(|key: &String, value: &String| (key.len() + value.len()) as u64);
        }
        if let Some(ttl) = self.ttl {
            builder = builder.ttl(ttl);
        }
        builder
    }

    /// The configured limits as shown in reports, e.g. `2 entries`.
    fn limits(&self) -> String {
        let entries = self.capacity.map(|n| format!("{n} entries"));
        let weight = self.max_weight.map(|n| format!("{n} bytes"));
        match (entries, weight) {
            (Some(entries), Some(weight)) => format!("{entries}, {weight}"),
            (Some(only), None) | (None, Some(only)) => only,
            (None, None) => "unbounded".to_string(),
        }
    }
}

fn number<T: FromStr>(option: &str, text: &str) -> Result<T, CliError> {
    text.parse()
        .map_err(|_| usage(format!("{option} expects a whole number, got '{text}'")))
}

/// Words a configuration problem in terms of command line options.
fn describe(error: &ConfigError) -> String {
    match error {
        ConfigError::Unbounded => "set --capacity or --max-weight".to_string(),
        ConfigError::ZeroCapacity => "--capacity must be at least 1".to_string(),
        ConfigError::ZeroMaxWeight => "--max-weight must be at least 1".to_string(),
        ConfigError::ZeroTtl => "--ttl-ms must be at least 1".to_string(),
        other => other.to_string(),
    }
}

fn read_trace(path: &str, stdin: &mut dyn BufRead) -> Result<Vec<Op>, CliError> {
    let parsed = if path == "-" {
        trace::parse(stdin)
    } else {
        let file = File::open(path).map_err(|source| open_error(path, source))?;
        trace::parse(BufReader::new(file))
    };
    parsed.map_err(|source| CliError::Trace {
        path: path.to_string(),
        source,
    })
}

fn open_error(path: &str, source: io::Error) -> CliError {
    CliError::Open {
        path: path.to_string(),
        source,
    }
}

fn snapshot_error(path: &str, source: SnapshotError) -> CliError {
    CliError::Snapshot {
        path: path.to_string(),
        source,
    }
}

/// Builds a cache for `policy`, fills it from the `--load` snapshot if
/// there is one, and replays `ops` against it. The statistics are reset
/// after loading so that they describe the trace alone.
fn simulate(
    options: &Options,
    policy: PolicyKind,
    ops: &[Op],
) -> Result<(DynCache<String, String>, ReplaySummary), CliError> {
    let clock = ManualClock::new();
    let mut cache = options
        .builder(policy, &clock)
        .build()
        .expect("options were validated when they were parsed");
    if let Some(path) = &options.load {
        let file = File::open(path).map_err(|source| open_error(path, source))?;
        snapshot::load(&mut cache, BufReader::new(file))
            .map_err(|source| snapshot_error(path, source))?;
        cache.reset_stats();
    }
    let summary = trace::replay(&mut cache, &clock, ops);
    Ok((cache, summary))
}

fn replay_command(options: &Options, ops: &[Op], out: &mut dyn Write) -> Result<(), CliError> {
    let policy = options.policy.unwrap_or(PolicyKind::Lru);
    let (cache, summary) = simulate(options, policy, ops)?;
    if let Some(path) = &options.save {
        let file = File::create(path).map_err(|source| open_error(path, source))?;
        snapshot::save(&cache, io::BufWriter::new(file))
            .map_err(|source| snapshot_error(path, source))?;
    }
    let mut report = String::new();
    report.push_str(&format!("trace:       {}\n", options.trace));
    report.push_str(&format!("policy:      {policy}\n"));
    report.push_str(&format!("capacity:    {}\n", options.limits()));
    if let Some(ttl) = options.ttl {
        report.push_str(&format!("ttl:         {} ms\n", ttl.as_millis()));
    }
    report.push_str(&format!("operations:  {}\n", summary.operations));
    report.push_str(&format!("simulated:   {} ms\n", summary.simulated.as_millis()));
    report.push_str(&format!("entries:     {}\n", cache.len()));
    if options.max_weight.is_some() {
        report.push_str(&format!("bytes:       {}\n", cache.weight()));
    }
    report.push_str(&format!("{}\n", cache.stats()));
    out.write_all(report.as_bytes()).map_err(CliError::Output)
}

fn compare_command(options: &Options, ops: &[Op], out: &mut dyn Write) -> Result<(), CliError> {
    let mut report = String::new();
    report.push_str(&format!("trace:       {}\n", options.trace));
    report.push_str(&format!("capacity:    {}\n", options.limits()));
    if let Some(ttl) = options.ttl {
        report.push_str(&format!("ttl:         {} ms\n", ttl.as_millis()));
    }
    report.push_str(&format!("operations:  {}\n\n", ops.len()));
    report.push_str(&format!(
        "{:<6} {:>10} {:>10} {:>9} {:>10} {:>11}\n",
        "policy", "hits", "misses", "hit rate", "evictions", "expirations"
    ));
    for policy in PolicyKind::ALL {
        let (cache, _) = simulate(options, policy, ops)?;
        let stats = cache.stats();
        report.push_str(&format!(
            "{:<6} {:>10} {:>10} {:>8.2}% {:>10} {:>11}\n",
            policy.as_str(),
            stats.hits,
            stats.misses,
            stats.hit_rate() * 100.0,
            stats.evictions,
            stats.expirations
        ));
    }
    out.write_all(report.as_bytes()).map_err(CliError::Output)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = "put a 1\nput b 2\nget a\nput c 3\nget a\nget b\n";

    fn run_with(args: &[&str], stdin: &str) -> Result<String, CliError> {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let mut out = Vec::new();
        run(&args, &mut stdin.as_bytes(), &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn usage_error(args: &[&str]) -> String {
        match run_with(args, TRACE) {
            Err(error @ CliError::Usage(_)) => {
                assert_eq!(error.exit_code(), 2);
                error.to_string()
            }
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    fn field<'a>(report: &'a str, label: &str) -> &'a str {
        report
            .lines()
            .find_map(|line| line.strip_prefix(label))
            .unwrap_or_else(|| panic!("no '{label}' line in:\n{report}"))
            .trim()
    }

    #[test]
    fn help_and_version_are_printed() {
        for flag in ["help", "--help", "-h"] {
            assert!(run_with(&[flag], "").unwrap().contains("Usage:"));
        }
        let version = run_with(&["--version"], "").unwrap();
        assert_eq!(version.trim(), format!("cachekit {}", env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn help_is_printed_when_asked_for_after_a_command() {
        for args in [
            &["replay", "--help"][..],
            &["compare", "-h"][..],
            &["replay", "-", "--capacity", "2", "--help"][..],
        ] {
            let printed = run_with(args, "").unwrap();
            assert!(printed.starts_with("cachekit - replay cache traces"), "{args:?}");
        }
        // An unknown command is still an error, with or without --help.
        assert_eq!(usage_error(&["frobnicate", "--help"]), "unknown command 'frobnicate'");
    }

    #[test]
    fn replay_reports_configuration_and_statistics() {
        let report = run_with(&["replay", "-", "--capacity", "2"], TRACE).unwrap();
        assert_eq!(field(&report, "trace:"), "-");
        assert_eq!(field(&report, "policy:"), "lru");
        assert_eq!(field(&report, "capacity:"), "2 entries");
        assert_eq!(field(&report, "operations:"), "6");
        assert_eq!(field(&report, "entries:"), "2");
        assert_eq!(field(&report, "hits:"), "2");
        assert_eq!(field(&report, "misses:"), "1");
        assert_eq!(field(&report, "hit rate:"), "66.67%");
        assert_eq!(field(&report, "evictions:"), "1");
        assert!(!report.contains("ttl:"));
        assert!(!report.contains("bytes:"));
        assert!(report.ends_with("load errors: 0\n"));
    }

    #[test]
    fn policy_option_changes_the_outcome() {
        let trace = "put a 1\nput b 2\nget a\nput c 3\nget a\n";
        let lru = run_with(&["replay", "-", "--capacity=2", "--policy=lru"], trace).unwrap();
        let fifo = run_with(&["replay", "--policy", "FIFO", "--capacity", "2", "-"], trace).unwrap();
        assert_eq!(field(&lru, "hits:"), "2");
        assert_eq!(field(&fifo, "policy:"), "fifo");
        assert_eq!(field(&fifo, "hits:"), "1");
    }

    #[test]
    fn compare_prints_one_row_per_policy() {
        // "a" is read twice early on, so LFU protects it; LRU and FIFO lose
        // it to the later one-off keys.
        let trace = "put a 1\nget a\nget a\nput b 2\nput c 3\nput d 4\nget a\nget d\n";
        let report = run_with(&["compare", "-", "--capacity", "2"], trace).unwrap();
        assert_eq!(field(&report, "capacity:"), "2 entries");
        assert_eq!(field(&report, "operations:"), "8");

        let rows: Vec<Vec<&str>> = report
            .lines()
            .skip_while(|line| !line.starts_with("policy"))
            .map(|line| line.split_whitespace().collect())
            .collect();
        assert_eq!(
            rows[0],
            ["policy", "hits", "misses", "hit", "rate", "evictions", "expirations"]
        );
        assert_eq!(rows[1], ["lru", "3", "1", "75.00%", "2", "0"]);
        assert_eq!(rows[2], ["lfu", "4", "0", "100.00%", "2", "0"]);
        assert_eq!(rows[3], ["fifo", "3", "1", "75.00%", "2", "0"]);
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn snapshot_options_are_replay_only() {
        for option in ["--load", "--save"] {
            assert_eq!(
                usage_error(&["compare", "-", "--capacity", "2", option, "cache.snap"]),
                "--load and --save cannot be used with compare"
            );
            assert_eq!(
                usage_error(&["replay", "-", "--capacity", "2", option]),
                format!("{option} needs a value")
            );
        }
    }

    #[test]
    fn missing_snapshot_is_an_open_error() {
        let args = ["replay", "-", "--capacity", "2", "--load", "no/such/cache.snap"];
        let error = run_with(&args, TRACE).unwrap_err();
        assert!(matches!(&error, CliError::Open { path, .. } if path == "no/such/cache.snap"));
        assert_eq!(error.exit_code(), 1);
    }

    #[test]
    fn compare_rejects_a_policy_option() {
        assert_eq!(
            usage_error(&["compare", "-", "--capacity", "2", "--policy", "lru"]),
            "--policy cannot be used with compare"
        );
        assert_eq!(usage_error(&["compare", "-"]), "set --capacity or --max-weight");
    }

    #[test]
    fn weight_limit_counts_key_and_value_bytes() {
        let trace = "put k1 aaaa\nput k2 bbbb\nput k3 cccc\nget k1\nget k3\n";
        let report = run_with(&["replay", "-", "--max-weight", "12"], trace).unwrap();
        assert_eq!(field(&report, "capacity:"), "12 bytes");
        assert_eq!(field(&report, "entries:"), "2");
        assert_eq!(field(&report, "bytes:"), "12");
        assert_eq!(field(&report, "evictions:"), "1");
        assert_eq!(field(&report, "hits:"), "1");

        let both = run_with(&["replay", "-", "--max-weight", "64", "--capacity", "1"], trace);
        assert_eq!(field(&both.unwrap(), "capacity:"), "1 entries, 64 bytes");
    }

    #[test]
    fn ttl_option_expires_entries_on_simulated_time() {
        let trace = "put a 1\ntick 999\nget a\ntick 1\nget a\n";
        let report = run_with(&["replay", "-", "--capacity", "4", "--ttl-ms", "1000"], trace);
        let report = report.unwrap();
        assert_eq!(field(&report, "ttl:"), "1000 ms");
        assert_eq!(field(&report, "simulated:"), "1000 ms");
        assert_eq!(field(&report, "hits:"), "1");
        assert_eq!(field(&report, "expirations:"), "1");
        assert_eq!(field(&report, "entries:"), "0");
    }

    #[test]
    fn command_line_mistakes_are_usage_errors() {
        assert_eq!(usage_error(&[]), "missing command");
        assert_eq!(usage_error(&["frobnicate"]), "unknown command 'frobnicate'");
        assert_eq!(usage_error(&["replay", "--capacity", "2"]), "missing trace file");
        assert_eq!(
            usage_error(&["replay", "-", "extra", "--capacity", "2"]),
            "unexpected argument 'extra'"
        );
        assert_eq!(
            usage_error(&["replay", "-", "--colour"]),
            "unknown option '--colour'"
        );
        assert_eq!(
            usage_error(&["replay", "-", "--capacity"]),
            "--capacity needs a value"
        );
        assert_eq!(
            usage_error(&["replay", "-", "--capacity", "many"]),
            "--capacity expects a whole number, got 'many'"
        );
        assert_eq!(
            usage_error(&["replay", "-", "--capacity", "-3"]),
            "--capacity expects a whole number, got '-3'"
        );
        assert!(usage_error(&["replay", "-", "--capacity", "2", "--policy", "arc"])
            .starts_with("--policy: unknown eviction policy 'arc'"));
    }

    #[test]
    fn limits_are_validated_before_the_trace_is_read() {
        assert_eq!(usage_error(&["replay", "-"]), "set --capacity or --max-weight");
        assert_eq!(
            usage_error(&["replay", "-", "--capacity", "0"]),
            "--capacity must be at least 1"
        );
        assert_eq!(
            usage_error(&["replay", "-", "--max-weight=0"]),
            "--max-weight must be at least 1"
        );
        assert_eq!(
            usage_error(&["replay", "-", "--capacity", "1", "--ttl-ms", "0"]),
            "--ttl-ms must be at least 1"
        );
        // A missing file would be an Open error; the usage error wins.
        assert_eq!(
            usage_error(&["replay", "no-such-file.trace"]),
            "set --capacity or --max-weight"
        );
    }

    #[test]
    fn trace_problems_name_the_source_and_line() {
        let error = run_with(&["replay", "-", "--capacity", "2"], "put a 1\nbogus\n").unwrap_err();
        assert!(matches!(error, CliError::Trace { .. }));
        assert_eq!(error.exit_code(), 1);
        assert!(error
            .to_string()
            .starts_with("-: trace line 2: unknown operation 'bogus'"));
        assert!(error.source().is_some());
    }

    #[test]
    fn missing_trace_file_is_an_open_error() {
        let error = run_with(
            &["replay", "definitely/not/here.trace", "--capacity", "2"],
            "",
        )
        .unwrap_err();
        assert!(matches!(&error, CliError::Open { path, .. } if path.ends_with("here.trace")));
        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().starts_with("cannot open 'definitely/not/here.trace': "));
    }

    #[test]
    fn output_failures_are_reported() {
        struct ClosedPipe;
        impl Write for ClosedPipe {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let args = ["replay".to_string(), "-".to_string(), "--capacity=1".to_string()];
        let error = run(&args, &mut TRACE.as_bytes(), &mut ClosedPipe).unwrap_err();
        assert!(matches!(error, CliError::Output(_)));
        assert_eq!(error.to_string(), "cannot write output: pipe closed");
    }
}
