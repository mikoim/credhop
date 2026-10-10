//! The provider abstraction: one implementation per supported tool.

use std::fmt;
use std::path::Path;

use anyhow::Result;
use clap::ValueEnum;
use serde_json::Value;

use crate::claude::Claude;
use crate::codex::Codex;

/// A tool whose credentials credhop manages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Tool {
    /// Claude Code
    Claude,
    /// Codex
    Codex,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::Claude, Tool::Codex];

    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
        }
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Reads and writes the live credentials of one tool.
///
/// Platform-specific storage (files, macOS Keychain, ...) lives behind this trait.
pub trait Provider {
    /// Read the live credentials into a profile snapshot. None if not logged in.
    fn read_live(&self) -> Result<Option<Value>>;
    /// Replace the live credentials with the snapshot.
    fn write_live(&self, snapshot: &Value) -> Result<()>;
    /// Delete the live credentials so the tool asks to log in again.
    fn clear_live(&self) -> Result<()>;
    /// Stable account identifier used to match live credentials to a profile.
    fn account_id(&self, snapshot: &Value) -> Option<String>;
    /// Human-readable label for `status` and `list` (e.g. an email address).
    fn display(&self, snapshot: &Value) -> Option<String>;
}

/// Returns the provider for `tool` rooted at the user's home directory.
pub fn provider_for(tool: Tool, home: &Path) -> Box<dyn Provider> {
    match tool {
        Tool::Claude => Box::new(Claude::new(home)),
        Tool::Codex => Box::new(Codex::new(home)),
    }
}
