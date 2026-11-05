//! Runs the real `cachekit` binary.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const SESSION: &str = "\
# a short browsing session
put home /index.html
put about /about.html
get home
tick 500
put blog /blog.html
get about
get home
tick 600
get blog
get home
purge
";

fn cachekit() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cachekit"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Writes `contents` to a file under Cargo's scratch directory and returns
/// its path as a string.
fn scratch_file(name: &str, contents: &str) -> String {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cli-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    path.to_str().unwrap().to_string()
}

fn run(args: &[&str]) -> Output {
    cachekit().args(args).output().unwrap()
}

fn run_with_stdin(args: &[&str], input: &str) -> Output {
    let mut child = cachekit()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// The value printed after `label` in a report.
fn field(report: &str, label: &str) -> String {
    report
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .unwrap_or_else(|| panic!("no '{label}' line in:\n{report}"))
        .trim()
        .to_string()
}

#[test]
fn replay_prints_the_full_report() {
    let trace = scratch_file("session.trace", SESSION);
    let output = run(&["replay", &trace, "--capacity", "2", "--ttl-ms", "1000"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stderr(&output), "");
    let expected = format!(
        "\
trace:       {trace}
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
"
    );
    assert_eq!(stdout(&output), expected);
}

#[test]
fn replay_reads_the_trace_from_standard_input() {
    let output = run_with_stdin(&["replay", "-", "--policy=fifo", "--capacity=2"], SESSION);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let report = stdout(&output);
    assert_eq!(field(&report, "trace:"), "-");
    assert_eq!(field(&report, "policy:"), "fifo");
    // Without a TTL nothing expires. FIFO evicts "home" although it was
    // just read, so "about" still hits and both later reads of "home" miss.
    assert_eq!(field(&report, "hits:"), "3");
    assert_eq!(field(&report, "misses:"), "2");
    assert_eq!(field(&report, "evictions:"), "1");
    assert_eq!(field(&report, "expirations:"), "0");
}

#[test]
fn compare_prints_a_row_for_every_policy() {
    let trace = scratch_file("compare.trace", SESSION);
    let output = run(&["compare", &trace, "--capacity", "2"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let report = stdout(&output);
    assert_eq!(field(&report, "operations:"), "11");
    let table: Vec<&str> = report
        .lines()
        .skip_while(|line| !line.starts_with("policy"))
        .collect();
    assert_eq!(table.len(), 4, "report:\n{report}");
    for (row, policy) in table[1..].iter().zip(["lru", "lfu", "fifo"]) {
        assert!(row.starts_with(policy), "row '{row}' should be for {policy}");
        assert!(row.contains('%'));
    }
    let widths: Vec<usize> = table.iter().map(|row| row.len()).collect();
    assert!(
        widths.iter().all(|width| *width == widths[0]),
        "columns should line up:\n{report}"
    );
}

#[test]
fn a_saved_snapshot_warms_up_the_next_run() {
    let warm_up = scratch_file("warm-up.trace", "put a 1\nput b 2\nput c 3\nget a\n");
    let snapshot = scratch_file("warm.snap", "");
    let output = run(&["replay", &warm_up, "--capacity", "3", "--save", &snapshot]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        fs::read_to_string(&snapshot).unwrap(),
        "cachekit-snapshot v1\nb\t2\t-\nc\t3\t-\na\t1\t-\n"
    );

    let reads = "get a\nget b\nget c\nget d\n";
    let cold = stdout(&run_with_stdin(&["replay", "-", "--capacity", "3"], reads));
    assert_eq!(field(&cold, "hits:"), "0");

    let args = ["replay", "-", "--capacity", "3", "--load", &snapshot];
    let warm = stdout(&run_with_stdin(&args, reads));
    assert_eq!(field(&warm, "hits:"), "3");
    assert_eq!(field(&warm, "misses:"), "1");
    // Loading is not part of the replayed trace.
    assert_eq!(field(&warm, "insertions:"), "0");
    assert_eq!(field(&warm, "entries:"), "3");
}

#[test]
fn help_goes_to_standard_output() {
    let output = run(&["help"]);
    assert!(output.status.success());
    assert!(stdout(&output).starts_with("cachekit - replay cache traces"));
    assert_eq!(stderr(&output), "");

    let version = run(&["--version"]);
    assert_eq!(
        stdout(&version).trim(),
        format!("cachekit {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn usage_mistakes_exit_with_status_two_and_a_hint() {
    for args in [
        &[][..],
        &["replay"][..],
        &["replay", "trace.txt"][..],
        &["replay", "trace.txt", "--capacity", "0"][..],
        &["replay", "trace.txt", "--capacity", "2", "--policy", "random"][..],
        &["compare", "trace.txt", "--capacity", "2", "--policy", "lru"][..],
        &["simulate", "trace.txt"][..],
    ] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
        assert_eq!(stdout(&output), "", "args: {args:?}");
        let stderr = stderr(&output);
        assert!(stderr.starts_with("cachekit: "), "args: {args:?}, stderr: {stderr}");
        assert!(
            stderr.ends_with("Run 'cachekit help' for usage.\n"),
            "args: {args:?}, stderr: {stderr}"
        );
    }
}

#[test]
fn a_missing_trace_exits_with_status_one() {
    let output = run(&["replay", "no-such-directory/missing.trace", "--capacity", "2"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    let stderr = stderr(&output);
    assert!(
        stderr.starts_with("cachekit: cannot open 'no-such-directory/missing.trace': "),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("Run 'cachekit help'"));
}

#[test]
fn a_malformed_trace_is_reported_with_file_and_line() {
    let trace = scratch_file("broken.trace", "put a 1\nget a\nget\nput b 2\n");
    let output = run(&["compare", &trace, "--capacity", "2"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "", "nothing is printed for a broken trace");
    assert_eq!(
        stderr(&output),
        format!("cachekit: {trace}: trace line 3: 'get' needs a key\n")
    );
}

#[test]
fn a_damaged_snapshot_is_reported_with_file_and_line() {
    let snapshot = scratch_file("damaged.snap", "cachekit-snapshot v1\na\t1\t-\nb\t2\tsoon\n");
    let output = run_with_stdin(
        &["replay", "-", "--capacity", "2", "--load", &snapshot],
        "get a\n",
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        format!(
            "cachekit: {snapshot}: snapshot line 3: invalid ttl 'soon' \
             (expected '-' or milliseconds > 0)\n"
        )
    );

    let not_a_snapshot = scratch_file("not-a-snapshot.txt", "put a 1\n");
    let output = run_with_stdin(
        &["replay", "-", "--capacity", "2", "--load", &not_a_snapshot],
        "get a\n",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not a cachekit snapshot"));
}

#[test]
fn an_unwritable_snapshot_path_is_an_error() {
    let output = run_with_stdin(
        &["replay", "-", "--capacity", "2", "--save", "no-such-directory/out.snap"],
        "put a 1\n",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).starts_with("cachekit: cannot open 'no-such-directory/out.snap': "));
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_are_rejected_without_a_panic() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let output = cachekit()
        .arg("replay")
        .arg(OsStr::from_bytes(b"bad\xff.trace"))
        .args(["--capacity", "2"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(stderr.contains("is not valid UTF-8"), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
}
