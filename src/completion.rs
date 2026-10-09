//! Shell completion: the registration scripts and the profile name candidates.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Write;

use anyhow::Result;
use clap::ValueEnum;
use clap_complete::CompletionCandidate;
use clap_complete::env::{self, EnvCompleter};

use crate::provider::Tool;
use crate::store::{Store, data_root};

/// The environment variable that switches credhop into completion mode.
pub const COMPLETE_VAR: &str = "COMPLETE";

/// The command the scripts complete and call back, found on `PATH`.
const BIN: &str = "credhop";

/// A shell `credhop completions` can print a script for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Fish,
}

/// Writes the script that makes `shell` ask credhop for completions.
pub fn write_registration(shell: Shell, out: &mut dyn Write) -> Result<()> {
    let completer: &dyn EnvCompleter = match shell {
        Shell::Bash => &env::Bash,
        Shell::Fish => &env::Fish,
    };
    completer.write_registration(COMPLETE_VAR, BIN, BIN, BIN, out)?;
    Ok(())
}

/// Candidates for `<name>` of `use` and `remove`: the saved profiles of the typed tool.
pub fn profile_names() -> Vec<CompletionCandidate> {
    let Some(home) = std::env::home_dir() else {
        return Vec::new();
    };
    let store = Store::new(data_root(std::env::var_os("XDG_DATA_HOME"), &home));
    let args: Vec<OsString> = std::env::args_os().collect();
    names(&store, typed_tool(&args))
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}

/// Finds the `<tool>` already typed on the command line being completed.
///
/// While completing, the shell runs `credhop -- <words>`, where `<words>` is that command line
/// (see the scripts printed by `credhop completions`). Its positionals are `<command> <tool> ...`.
fn typed_tool(args: &[OsString]) -> Option<Tool> {
    let tool = args
        .iter()
        .skip_while(|arg| *arg != "--")
        .skip(2) // "--" and the program name
        .filter(|word| !word.as_encoded_bytes().starts_with(b"-"))
        .nth(1)?;
    Tool::from_str(tool.to_str()?, false).ok()
}

/// Profile names of `tool`, or of every tool when it is unknown. Errors yield no names.
fn names(store: &Store, tool: Option<Tool>) -> Vec<String> {
    let tools = tool.as_ref().map_or(&Tool::ALL[..], std::slice::from_ref);
    let names: BTreeSet<String> = tools
        .iter()
        .flat_map(|&tool| store.list_profiles(tool).unwrap_or_default())
        .collect();
    names.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn typed_tool_reads_the_word_after_the_command() {
        let cases = [
            (
                &["credhop", "--", "credhop", "use", "claude", ""][..],
                Some(Tool::Claude),
            ),
            (
                &["credhop", "--", "credhop", "remove", "codex", "p"],
                Some(Tool::Codex),
            ),
            (
                &["credhop", "--", "credhop", "use", "--force", "codex", ""],
                Some(Tool::Codex),
            ),
            (&["credhop", "--", "credhop", "use", "gemini", ""], None),
            (&["credhop", "--", "credhop", "use", ""], None),
            (&["credhop", "use", "claude", ""], None),
        ];
        for (input, want) in cases {
            assert_eq!(typed_tool(&args(input)), want, "{input:?}");
        }
    }

    #[test]
    fn registration_calls_back_credhop_from_each_shell() {
        let script = |shell| {
            let mut out = Vec::new();
            write_registration(shell, &mut out).unwrap();
            String::from_utf8(out).unwrap()
        };

        let bash = script(Shell::Bash);
        assert!(bash.contains(r#"COMPLETE="bash""#), "{bash}");
        assert!(
            bash.contains(" -F _clap_complete_credhop credhop\n"),
            "{bash}"
        );
        let fish = script(Shell::Fish);
        assert!(
            fish.contains("complete --keep-order --exclusive --command credhop "),
            "{fish}"
        );
        assert!(fish.contains("COMPLETE=fish credhop -- "), "{fish}");
    }

    #[test]
    fn names_are_limited_to_the_tool_when_known() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        for (tool, name) in [
            (Tool::Claude, "work"),
            (Tool::Claude, "team"),
            (Tool::Codex, "work"),
            (Tool::Codex, "personal"),
        ] {
            store.save_profile(tool, name, &json!({})).unwrap();
        }

        assert_eq!(names(&store, Some(Tool::Claude)), ["team", "work"]);
        assert_eq!(names(&store, Some(Tool::Codex)), ["personal", "work"]);
        assert_eq!(names(&store, None), ["personal", "team", "work"]);
    }
}
