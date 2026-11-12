//! credhop: switch Claude Code and Codex credentials between saved profiles.

mod claude;
mod codex;
mod commands;
mod completion;
mod fsutil;
mod http;
mod lock;
mod provider;
mod store;
mod time;
mod usage;

use std::io::IsTerminal;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{ArgValueCandidates, CompleteEnv};

use crate::commands::{Ctx, Format};
use crate::completion::Shell;
use crate::http::Ureq;
use crate::provider::Tool;
use crate::store::{Store, data_root};

#[derive(Parser)]
#[command(
    version,
    about = "Switch Claude Code and Codex credentials between saved profiles"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List saved profiles
    List {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Save the current credentials as a profile
    Save {
        #[arg(value_enum)]
        tool: Tool,
        name: String,
        /// Overwrite an existing profile
        #[arg(long)]
        force: bool,
    },
    /// Switch to a saved profile
    Use {
        #[arg(value_enum)]
        tool: Tool,
        #[arg(add = ArgValueCandidates::new(completion::profile_names))]
        name: String,
        /// Discard current credentials that are not saved in any profile
        #[arg(long)]
        force: bool,
    },
    /// Delete the current credentials (after saving them) to log in to another account
    Logout {
        #[arg(value_enum)]
        tool: Tool,
        /// Discard current credentials that are not saved in any profile
        #[arg(long)]
        force: bool,
    },
    /// Delete a saved profile
    Remove {
        #[arg(value_enum)]
        tool: Tool,
        #[arg(add = ArgValueCandidates::new(completion::profile_names))]
        name: String,
    },
    /// Rename a saved profile
    Rename {
        #[arg(value_enum)]
        tool: Tool,
        #[arg(add = ArgValueCandidates::new(completion::profile_names))]
        old: String,
        new: String,
    },
    /// Show the current account of each tool
    Status {
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Show how much of the subscription usage limits is left for each account
    Usage {
        #[arg(value_enum)]
        tool: Option<Tool>,
        #[arg(add = ArgValueCandidates::new(completion::profile_names))]
        name: Option<String>,
        /// Print JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Switch to the profile with the most usage left when the current one runs low
    Auto {
        #[arg(value_enum)]
        tool: Tool,
        /// Switch when a usage limit of the current account has this percentage or less left
        #[arg(long, value_name = "PERCENT", default_value_t = 10, value_parser = clap::value_parser!(u8).range(..=100))]
        threshold: u8,
        /// Keep checking at this interval, such as 90s, 5m or 1h, instead of once
        #[arg(long, value_name = "INTERVAL", value_parser = watch_interval)]
        watch: Option<Duration>,
    },
    /// Print the shell completion script
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

fn main() -> Result<()> {
    CompleteEnv::with_factory(Cli::command)
        .var(completion::COMPLETE_VAR)
        .complete();
    let cli = Cli::parse();
    let home = std::env::home_dir().context("cannot determine the home directory")?;
    let store = Store::new(data_root(&home));
    let ctx = Ctx { store, home };
    let mut out = std::io::stdout().lock();

    match cli.command {
        Command::List { json } => commands::list(&ctx, unix_now()?, format(json), &mut out),
        Command::Save { tool, name, force } => commands::save(&ctx, tool, &name, force, &mut out),
        Command::Use { tool, name, force } => {
            commands::use_profile(&ctx, tool, &name, force, unix_now()?, &mut out)
        }
        Command::Logout { tool, force } => commands::logout(&ctx, tool, force, &mut out),
        Command::Remove { tool, name } => commands::remove(&ctx, tool, &name, &mut out),
        Command::Rename { tool, old, new } => commands::rename(&ctx, tool, &old, &new, &mut out),
        Command::Status { json } => commands::status(&ctx, format(json), &mut out),
        Command::Usage { tool, name, json } => {
            let format = match format(json) {
                Format::Text if shows_colors(&out) => Format::Color,
                format => format,
            };
            commands::usage(
                &ctx,
                &Ureq::default(),
                tool,
                name.as_deref(),
                unix_now()?,
                format,
                &mut out,
            )
        }
        Command::Auto {
            tool,
            threshold,
            watch,
        } => {
            let http = Ureq::default();
            let Some(interval) = watch else {
                return commands::auto(&ctx, &http, tool, threshold, unix_now()?, &mut out);
            };
            let mut err = std::io::stderr();
            loop {
                commands::auto_step(
                    &ctx,
                    &http,
                    tool,
                    threshold,
                    unix_now()?,
                    &mut out,
                    &mut err,
                )?;
                thread::sleep(interval);
            }
        }
        Command::Completions { shell } => completion::write_registration(shell, &mut out),
    }
}

/// The output format `--json` selects.
fn format(json: bool) -> Format {
    if json { Format::Json } else { Format::Text }
}

/// Whether to color what is written to `out`: on a terminal, unless
/// `NO_COLOR` is set. Never on Windows, where a console prints the color codes
/// as text unless the program turns them on first.
fn shows_colors(out: &impl IsTerminal) -> bool {
    !cfg!(windows)
        && out.is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
}

/// Parses the interval of `auto --watch`: a number and `s`, `m` or `h`.
fn watch_interval(s: &str) -> Result<Duration, String> {
    let seconds = s
        .char_indices()
        .next_back()
        .and_then(|(at, unit)| {
            let per_unit = match unit {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                _ => return None,
            };
            let digits = &s[..at];
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            digits.parse::<u64>().ok()?.checked_mul(per_unit)
        })
        .ok_or("expected a number and a unit, such as 90s, 5m or 1h")?;
    // The servers limit how often the usage may be asked for.
    if seconds < 60 {
        return Err("the interval must be at least 1m".to_owned());
    }
    Ok(Duration::from_secs(seconds))
}

/// The current time in Unix seconds.
fn unix_now() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("the system clock is set before 1970")?;
    Ok(i64::try_from(elapsed.as_secs())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_use_command() {
        let cli = Cli::try_parse_from(["credhop", "use", "codex", "personal", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Use { tool: Tool::Codex, ref name, force: true } if name == "personal"
        ));
        assert!(Cli::try_parse_from(["credhop", "use", "gemini", "x"]).is_err());
    }

    #[test]
    fn parses_usage_command() {
        let cli = Cli::try_parse_from(["credhop", "usage"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Usage {
                tool: None,
                name: None,
                json: false
            }
        ));
        let cli = Cli::try_parse_from(["credhop", "usage", "claude"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Usage {
                tool: Some(Tool::Claude),
                name: None,
                json: false
            }
        ));
        let cli = Cli::try_parse_from(["credhop", "usage", "codex", "work"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Usage { tool: Some(Tool::Codex), name: Some(ref name), json: false } if name == "work"
        ));
    }

    #[test]
    fn parses_auto_command() {
        let cli = Cli::try_parse_from(["credhop", "auto", "claude"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Auto {
                tool: Tool::Claude,
                threshold: 10,
                watch: None
            }
        ));
        let cli = Cli::try_parse_from([
            "credhop",
            "auto",
            "codex",
            "--threshold",
            "25",
            "--watch",
            "5m",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Auto {
                tool: Tool::Codex,
                threshold: 25,
                watch: Some(interval)
            } if interval == Duration::from_secs(300)
        ));
        assert!(Cli::try_parse_from(["credhop", "auto"]).is_err());
        assert!(Cli::try_parse_from(["credhop", "auto", "claude", "--threshold", "101"]).is_err());
        assert!(Cli::try_parse_from(["credhop", "auto", "claude", "--watch"]).is_err());
    }

    #[test]
    fn watch_intervals_take_a_unit_and_are_at_least_a_minute() {
        assert_eq!(watch_interval("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(watch_interval("1m"), Ok(Duration::from_secs(60)));
        assert_eq!(watch_interval("2h"), Ok(Duration::from_secs(7200)));
        for input in [
            "",
            "5",
            "m",
            "5x",
            "-5m",
            "1.5h",
            "5 m",
            "99999999999999999999h",
        ] {
            assert_eq!(
                watch_interval(input),
                Err("expected a number and a unit, such as 90s, 5m or 1h".to_owned()),
                "{input}"
            );
        }
        for input in ["59s", "0m"] {
            assert_eq!(
                watch_interval(input),
                Err("the interval must be at least 1m".to_owned()),
                "{input}"
            );
        }
    }

    #[test]
    fn parses_json_flag() {
        let cli = Cli::try_parse_from(["credhop", "list", "--json"]).unwrap();
        assert!(matches!(cli.command, Command::List { json: true }));
        let cli = Cli::try_parse_from(["credhop", "list"]).unwrap();
        assert!(matches!(cli.command, Command::List { json: false }));
        let cli = Cli::try_parse_from(["credhop", "status", "--json"]).unwrap();
        assert!(matches!(cli.command, Command::Status { json: true }));
        let cli = Cli::try_parse_from(["credhop", "usage", "claude", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Usage {
                tool: Some(Tool::Claude),
                name: None,
                json: true
            }
        ));
        assert!(Cli::try_parse_from(["credhop", "save", "claude", "work", "--json"]).is_err());
    }

    #[test]
    fn parses_completions_command() {
        let cli = Cli::try_parse_from(["credhop", "completions", "fish"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Completions { shell: Shell::Fish }
        ));
        let cli = Cli::try_parse_from(["credhop", "completions", "powershell"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Completions {
                shell: Shell::Powershell
            }
        ));
        let cli = Cli::try_parse_from(["credhop", "completions", "zsh"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Completions { shell: Shell::Zsh }
        ));
        assert!(Cli::try_parse_from(["credhop", "completions", "elvish"]).is_err());
    }

    #[test]
    fn parses_rename_command() {
        let cli = Cli::try_parse_from(["credhop", "rename", "claude", "work", "main"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Rename { tool: Tool::Claude, ref old, ref new } if old == "work" && new == "main"
        ));
        assert!(Cli::try_parse_from(["credhop", "rename", "claude", "work"]).is_err());
    }
}
