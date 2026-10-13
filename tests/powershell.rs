//! Completes credhop command lines in real PowerShell sessions through `TabExpansion2`.

mod common;

use std::path::Path;
use std::process::Command;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use tempfile::tempdir;

/// Loads the registration script and prints the completions of `$env:CREDHOP_INPUT`
/// with the cursor at its end. Strict mode and a preset `$env:COMPLETE` stand in
/// for a user's profile; the preset must survive the completion.
const SCRIPT: &str = r#"
Set-StrictMode -Version Latest
credhop completions powershell | Out-String | Invoke-Expression
$env:COMPLETE = 'preset'
$completion = TabExpansion2 -inputScript $env:CREDHOP_INPUT -cursorColumn $env:CREDHOP_INPUT.Length
foreach ($match in $completion.CompletionMatches) { $match.CompletionText }
if ($env:COMPLETE -ne 'preset') { throw "COMPLETE was not restored: $env:COMPLETE" }
"#;

/// The installed PowerShell executables: Windows PowerShell 5.1 and PowerShell 7.
fn shells() -> Vec<&'static str> {
    let candidates: &[&'static str] = if cfg!(windows) {
        &["powershell", "pwsh"]
    } else {
        &["pwsh"]
    };
    candidates
        .iter()
        .copied()
        .filter(|shell| {
            let found = Command::new(shell)
                .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
                .output()
                .is_ok_and(|output| output.status.success());
            if !found {
                eprintln!("skipping {shell}: not installed");
            }
            found
        })
        .collect()
}

/// Completes `input` in `shell` with credhop isolated in `home` and returns the non-flag candidates.
fn complete(shell: &str, home: &Path, input: &str) -> Vec<String> {
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_credhop")).parent().unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin_dir.to_path_buf()).chain(std::env::split_paths(&path)),
    )
    .unwrap();
    // -EncodedCommand takes base64 of UTF-16LE and avoids quoting the script.
    let script: Vec<u8> = SCRIPT.encode_utf16().flat_map(u16::to_le_bytes).collect();

    let mut command = Command::new(shell);
    let output = common::isolate(&mut command, home)
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
        .arg(STANDARD.encode(script))
        .env("PATH", path)
        .env("CREDHOP_INPUT", input)
        .current_dir(home)
        .output()
        .unwrap();
    assert!(output.status.success(), "{shell}: {output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .filter(|value| !value.starts_with('-'))
        .collect()
}

#[test]
fn completes_subcommands_and_profile_names() {
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

    for shell in shells() {
        let commands = complete(shell, &home, "credhop ");
        for command in [
            "list",
            "save",
            "use",
            "logout",
            "remove",
            "status",
            "completions",
        ] {
            assert!(
                commands.iter().any(|c| c == command),
                "{shell}: {commands:?}"
            );
        }
        assert_eq!(
            complete(shell, &home, "credhop use claude "),
            ["team", "work"],
            "{shell}"
        );
        assert_eq!(
            complete(shell, &home, "credhop remove codex p"),
            ["personal"],
            "{shell}"
        );
        assert_eq!(
            complete(shell, &home, "Get-Date; credhop use claude "),
            ["team", "work"],
            "{shell}"
        );
        if cfg!(windows) {
            assert_eq!(
                complete(shell, &home, "credhop.exe use claude "),
                ["team", "work"],
                "{shell}"
            );
        }
    }
}

#[test]
fn does_not_run_code_on_the_command_line() {
    let dir = tempdir().unwrap();
    let home = common::unusual_home(dir.path());
    let marker = dir.path().join("marker");

    for shell in shells() {
        let input = format!("credhop use $(New-Item -Path '{}') ", marker.display());
        complete(shell, &home, &input);
        assert!(!marker.exists(), "{shell} ran the subexpression");
    }
}
