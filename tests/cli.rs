//! Runs the real `cachekit` binary.

use std::process::{Command, Output};

fn cachekit() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cachekit"))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
