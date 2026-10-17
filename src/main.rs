//! credhop: switch Claude Code and Codex credentials between saved profiles.

mod claude;
mod codex;
mod commands;
mod completion;
mod fsutil;
mod http;
mod provider;
mod store;
mod time;
mod usage;

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{ArgValueCandidates, CompleteEnv};

use crate::commands::Ctx;
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
    List,
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
    /// Show the current account of each tool
    Status,
    /// Show how much of the subscription usage limits is left for each account
    Usage {
        #[arg(value_enum)]
        tool: Option<Tool>,
        #[arg(add = ArgValueCandidates::new(completion::profile_names))]
        name: Option<String>,
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
        Command::List => commands::list(&ctx, &mut out),
        Command::Save { tool, name, force } => commands::save(&ctx, tool, &name, force, &mut out),
        Command::Use { tool, name, force } => {
            commands::use_profile(&ctx, tool, &name, force, &mut out)
        }
        Command::Logout { tool, force } => commands::logout(&ctx, tool, force, &mut out),
        Command::Remove { tool, name } => commands::remove(&ctx, tool, &name, &mut out),
        Command::Status => commands::status(&ctx, &mut out),
        Command::Usage { tool, name } => commands::usage(
            &ctx,
            &Ureq::default(),
            tool,
            name.as_deref(),
            unix_now()?,
            &mut out,
        ),
        Command::Completions { shell } => completion::write_registration(shell, &mut out),
    }
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
                name: None
            }
        ));
        let cli = Cli::try_parse_from(["credhop", "usage", "claude"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Usage {
                tool: Some(Tool::Claude),
                name: None
            }
        ));
        let cli = Cli::try_parse_from(["credhop", "usage", "codex", "work"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Usage { tool: Some(Tool::Codex), name: Some(ref name) } if name == "work"
        ));
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
        assert!(Cli::try_parse_from(["credhop", "completions", "zsh"]).is_err());
    }
}
