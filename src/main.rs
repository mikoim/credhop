//! credhop: switch Claude Code and Codex credentials between saved profiles.

mod claude;
mod codex;
mod commands;
mod fsutil;
mod provider;
mod store;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::commands::Ctx;
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
        name: String,
        /// Discard current credentials that are not saved in any profile
        #[arg(long)]
        force: bool,
    },
    /// Delete a saved profile
    Remove {
        #[arg(value_enum)]
        tool: Tool,
        name: String,
    },
    /// Show the current account of each tool
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = std::env::home_dir().context("cannot determine the home directory")?;
    let store = Store::new(data_root(std::env::var_os("XDG_DATA_HOME"), &home));
    let ctx = Ctx { store, home };
    let mut out = std::io::stdout().lock();

    match cli.command {
        Command::List => commands::list(&ctx, &mut out),
        Command::Save { tool, name, force } => commands::save(&ctx, tool, &name, force, &mut out),
        Command::Use { tool, name, force } => {
            commands::use_profile(&ctx, tool, &name, force, &mut out)
        }
        Command::Remove { tool, name } => commands::remove(&ctx, tool, &name, &mut out),
        Command::Status => commands::status(&ctx, &mut out),
    }
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
}
