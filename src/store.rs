//! Profile and state storage under the credhop data directory.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::fsutil::{create_private_dir, read_json, write_private_json};
use crate::provider::Tool;

/// The directory credhop keeps its profiles in: `$XDG_DATA_HOME/credhop` on Unix
/// and `%LOCALAPPDATA%\credhop` on Windows, each falling back to its default
/// under `home` when the variable is unset or not an absolute path.
#[cfg(unix)]
pub fn data_root(home: &Path) -> PathBuf {
    xdg_data_root(std::env::var_os("XDG_DATA_HOME"), home)
}

#[cfg(windows)]
pub fn data_root(home: &Path) -> PathBuf {
    local_app_data_root(std::env::var_os("LOCALAPPDATA"), home)
}

#[cfg(unix)]
fn xdg_data_root(xdg_data_home: Option<OsString>, home: &Path) -> PathBuf {
    absolute(xdg_data_home)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("credhop")
}

#[cfg(windows)]
fn local_app_data_root(local_app_data: Option<OsString>, home: &Path) -> PathBuf {
    absolute(local_app_data)
        .unwrap_or_else(|| home.join(r"AppData\Local"))
        .join("credhop")
}

fn absolute(path: Option<OsString>) -> Option<PathBuf> {
    path.map(PathBuf::from).filter(|path| path.is_absolute())
}

/// Profile names become file names, so only allow a safe character set.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        bail!("invalid profile name '{name}': use only letters, digits, '_' and '-'");
    }
    Ok(())
}

/// Saved profiles and the active-profile state.
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn load_profile(&self, tool: Tool, name: &str) -> Result<Option<Value>> {
        read_json(&self.profile_path(tool, name)?)
    }

    pub fn save_profile(&self, tool: Tool, name: &str, snapshot: &Value) -> Result<()> {
        let path = self.profile_path(tool, name)?;
        create_private_dir(&self.tool_dir(tool))?;
        write_private_json(&path, snapshot)
    }

    /// Returns `false` if the profile did not exist.
    pub fn remove_profile(&self, tool: Tool, name: &str) -> Result<bool> {
        let path = self.profile_path(tool, name)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
        }
    }

    /// Profile names of `tool`, sorted.
    pub fn list_profiles(&self, tool: Tool) -> Result<Vec<String>> {
        let dir = self.tool_dir(tool);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", dir.display())),
        };
        let mut names = Vec::new();
        for entry in entries {
            let file_name = entry?.file_name();
            let Some(name) = file_name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            if validate_name(name).is_ok() {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn active(&self, tool: Tool) -> Result<Option<String>> {
        Ok(self.read_state()?.remove(tool.as_str()))
    }

    pub fn set_active(&self, tool: Tool, name: Option<&str>) -> Result<()> {
        let mut state = self.read_state()?;
        match name {
            Some(name) => state.insert(tool.as_str().to_owned(), name.to_owned()),
            None => state.remove(tool.as_str()),
        };
        create_private_dir(&self.root)?;
        write_private_json(&self.state_path(), &serde_json::to_value(&state)?)
    }

    fn tool_dir(&self, tool: Tool) -> PathBuf {
        self.root.join("profiles").join(tool.as_str())
    }

    fn profile_path(&self, tool: Tool, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self.tool_dir(tool).join(format!("{name}.json")))
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    fn read_state(&self) -> Result<BTreeMap<String, String>> {
        match read_json(&self.state_path())? {
            Some(value) => serde_json::from_value(value).context("invalid state.json"),
            None => Ok(BTreeMap::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn xdg_data_root_prefers_absolute_xdg_data_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            xdg_data_root(Some("/xdg".into()), home),
            PathBuf::from("/xdg/credhop")
        );
        for unusable in [None, Some(""), Some("rel")] {
            assert_eq!(
                xdg_data_root(unusable.map(Into::into), home),
                PathBuf::from("/home/u/.local/share/credhop"),
                "{unusable:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn local_app_data_root_prefers_absolute_local_app_data() {
        let home = Path::new(r"C:\Users\u");
        assert_eq!(
            local_app_data_root(Some(r"D:\Local".into()), home),
            PathBuf::from(r"D:\Local\credhop")
        );
        for unusable in [None, Some(""), Some("rel"), Some(r"\no-drive")] {
            assert_eq!(
                local_app_data_root(unusable.map(Into::into), home),
                PathBuf::from(r"C:\Users\u\AppData\Local\credhop"),
                "{unusable:?}"
            );
        }
    }

    #[test]
    fn validate_name_accepts_safe_names_only() {
        for ok in ["work", "my_work-2", "A"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", "../x", "a/b", "a b", "é"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn profiles_round_trip() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().join("credhop"));
        let snapshot = json!({"token": "t"});

        store.save_profile(Tool::Claude, "work", &snapshot).unwrap();
        store
            .save_profile(Tool::Claude, "a-first", &snapshot)
            .unwrap();

        assert_eq!(
            store.load_profile(Tool::Claude, "work").unwrap(),
            Some(snapshot)
        );
        assert_eq!(store.load_profile(Tool::Claude, "none").unwrap(), None);
        assert_eq!(
            store.list_profiles(Tool::Claude).unwrap(),
            ["a-first", "work"]
        );
        assert!(store.list_profiles(Tool::Codex).unwrap().is_empty());

        assert!(store.remove_profile(Tool::Claude, "work").unwrap());
        assert!(!store.remove_profile(Tool::Claude, "work").unwrap());
        assert_eq!(store.list_profiles(Tool::Claude).unwrap(), ["a-first"]);
    }

    #[test]
    fn list_profiles_skips_foreign_files() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        store
            .save_profile(Tool::Claude, "work", &json!({}))
            .unwrap();
        let profiles = dir.path().join("profiles/claude");
        fs::write(profiles.join("notes.txt"), "").unwrap();
        fs::write(profiles.join("bad name.json"), "{}").unwrap();

        assert_eq!(store.list_profiles(Tool::Claude).unwrap(), ["work"]);
    }

    #[test]
    fn profile_operations_reject_invalid_names() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        assert!(store.save_profile(Tool::Codex, "../x", &json!({})).is_err());
        assert!(store.load_profile(Tool::Codex, "../x").is_err());
    }

    #[test]
    fn active_profile_round_trips() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().join("credhop"));

        assert_eq!(store.active(Tool::Codex).unwrap(), None);
        store.set_active(Tool::Codex, Some("personal")).unwrap();
        store.set_active(Tool::Claude, Some("work")).unwrap();
        assert_eq!(
            store.active(Tool::Codex).unwrap().as_deref(),
            Some("personal")
        );

        store.set_active(Tool::Codex, None).unwrap();
        assert_eq!(store.active(Tool::Codex).unwrap(), None);
        assert_eq!(store.active(Tool::Claude).unwrap().as_deref(), Some("work"));
    }

    #[cfg(unix)]
    #[test]
    fn store_files_and_directories_are_private() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("credhop");
        let store = Store::new(root.clone());

        store
            .save_profile(Tool::Claude, "work", &json!({}))
            .unwrap();
        store.set_active(Tool::Claude, Some("work")).unwrap();

        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("profiles/claude")), 0o700);
        assert_eq!(mode(&root.join("profiles/claude/work.json")), 0o600);
        assert_eq!(mode(&root.join("state.json")), 0o600);
    }
}
