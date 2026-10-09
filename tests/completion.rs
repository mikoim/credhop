use std::fs;
use std::process::Command;

use tempfile::tempdir;

/// Runs credhop the way the fish registration script does and returns the non-flag candidates.
fn complete(home: &std::path::Path, words: &[&str]) -> Vec<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_credhop"))
        .arg("--")
        .args(words)
        .env("COMPLETE", "fish")
        .env("HOME", home)
        .env_remove("XDG_DATA_HOME")
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
    let home = tempdir().unwrap();
    let profiles = home.path().join(".local/share/credhop/profiles");
    for (tool, name) in [
        ("claude", "work"),
        ("claude", "team"),
        ("codex", "personal"),
    ] {
        fs::create_dir_all(profiles.join(tool)).unwrap();
        fs::write(profiles.join(tool).join(format!("{name}.json")), "{}").unwrap();
    }

    assert_eq!(
        complete(home.path(), &["credhop", "use", "claude", ""]),
        ["team", "work"]
    );
    assert_eq!(
        complete(home.path(), &["credhop", "remove", "codex", ""]),
        ["personal"]
    );
    assert!(complete(home.path(), &["credhop", "save", "claude", ""]).is_empty());
}
