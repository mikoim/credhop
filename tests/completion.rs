mod common;

use std::path::Path;
use std::process::Command;

use tempfile::tempdir;

/// Runs credhop the way the `shell` registration script does and returns the non-flag candidates.
fn complete(shell: &str, home: &Path, words: &[&str]) -> Vec<String> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_credhop"));
    let output = common::isolate(&mut command, home)
        .arg("--")
        .args(words)
        .env("COMPLETE", shell)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| line.split('\t').next().unwrap().to_owned())
        .filter(|value| !value.starts_with('-'))
        .collect()
}

#[test]
fn completes_profile_names_of_the_typed_tool() {
    let dir = tempdir().unwrap();
    let home = common::unusual_home(dir.path());
    common::save_profiles(
        &home,
        &[
            ("claude", "work"),
            ("claude", "team"),
            ("codex", "personal"),
        ],
    );

    assert_eq!(
        complete("fish", &home, &["credhop", "use", "claude", ""]),
        ["team", "work"]
    );
    assert_eq!(
        complete("fish", &home, &["credhop", "remove", "codex", ""]),
        ["personal"]
    );
    assert!(complete("fish", &home, &["credhop", "save", "claude", ""]).is_empty());
}
