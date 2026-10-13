//! Small filesystem helpers shared by the store and the providers.
//!
//! Platform differences stay in this module: on Unix, private files and
//! directories get modes 0600 and 0700; on Windows they inherit the ACL of
//! their parent directory, like the files Claude Code and Codex write.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// Atomically replace `path` with `value` as pretty-printed JSON that only the user can read.
pub fn write_private_json(path: &Path, value: &Value) -> Result<()> {
    atomic_write(path, &json_bytes(value)?, None)
}

/// Atomically replace `path` with `value` as pretty-printed JSON, keeping the
/// permissions of the existing file (on Windows, its read-only flag). A new
/// file is written like [`write_private_json`].
pub fn replace_json(path: &Path, value: &Value) -> Result<()> {
    let permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    atomic_write(path, &json_bytes(value)?, permissions)
}

/// Create `dir` and any missing parents that only the user can access.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    create_dir_all_private(dir).with_context(|| format!("failed to create {}", dir.display()))
}

/// Read a JSON file. Returns `None` if the file does not exist.
pub fn read_json(path: &Path) -> Result<Option<Value>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    let value = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;
    Ok(Some(value))
}

/// Delete the file at `path`. A missing file is not an error.
pub fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Write `bytes` to a private temporary file next to `path`, apply
/// `permissions` if given, and rename the file over `path`.
fn atomic_write(path: &Path, bytes: &[u8], permissions: Option<Permissions>) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    let name = path.file_name().context("path has no file name")?;
    let tmp = dir.join(format!(".{}.credhop-tmp", name.to_string_lossy()));

    let mut file =
        create_private_file(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)?;
    }
    // Close the file before renaming it over `path`.
    drop(file);
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // The mode passed to open() is filtered by the umask, so set it explicitly.
    file.set_permissions(Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(unix)]
fn create_dir_all_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dir_all_private(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
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
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn write_private_json_replaces_content_and_leaves_no_temp_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.json");
        fs::write(&path, "old").unwrap();

        write_private_json(&path, &json!({"new": true})).unwrap();

        assert_eq!(read_json(&path).unwrap(), Some(json!({"new": true})));
        assert!(!dir.path().join(".a.json.credhop-tmp").exists());
    }

    #[test]
    fn write_private_json_overwrites_a_leftover_temp_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.json");
        fs::write(dir.path().join(".a.json.credhop-tmp"), "partial").unwrap();

        write_private_json(&path, &json!({})).unwrap();

        assert_eq!(read_json(&path).unwrap(), Some(json!({})));
        assert!(!dir.path().join(".a.json.credhop-tmp").exists());
    }

    #[test]
    fn write_private_json_keeps_key_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.json");
        let value = json!({"b": 1, "a": 2});

        write_private_json(&path, &value).unwrap();

        assert!(fs::read_to_string(&path).unwrap().starts_with("{\n  \"b\""));
        assert_eq!(read_json(&path).unwrap(), Some(value));
    }

    #[cfg(unix)]
    #[test]
    fn write_private_json_sets_mode_0600() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.json");
        fs::write(&path, "old").unwrap();
        set_mode(&path, 0o644);

        write_private_json(&path, &json!({})).unwrap();

        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn replace_json_replaces_existing_and_creates_missing_files() {
        let dir = tempdir().unwrap();
        let existing = dir.path().join("existing.json");
        fs::write(&existing, "{}").unwrap();
        let missing = dir.path().join("missing.json");

        replace_json(&existing, &json!({"a": 1})).unwrap();
        replace_json(&missing, &json!({"b": 2})).unwrap();

        assert_eq!(read_json(&existing).unwrap(), Some(json!({"a": 1})));
        assert_eq!(read_json(&missing).unwrap(), Some(json!({"b": 2})));
        assert!(!dir.path().join(".existing.json.credhop-tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn replace_json_keeps_the_mode_of_the_existing_file() {
        let dir = tempdir().unwrap();
        let existing = dir.path().join("existing.json");
        fs::write(&existing, "{}").unwrap();
        set_mode(&existing, 0o644);
        let missing = dir.path().join("missing.json");

        replace_json(&existing, &json!({})).unwrap();
        replace_json(&missing, &json!({})).unwrap();

        assert_eq!(mode(&existing), 0o644);
        assert_eq!(mode(&missing), 0o600);
    }

    #[test]
    fn create_private_dir_creates_parents_and_accepts_existing() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b");

        create_private_dir(&nested).unwrap();
        create_private_dir(&nested).unwrap();

        assert!(nested.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn create_private_dir_uses_mode_0700() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a/b");

        create_private_dir(&nested).unwrap();

        assert_eq!(mode(&dir.path().join("a")), 0o700);
        assert_eq!(mode(&nested), 0o700);
    }

    #[test]
    fn read_json_returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        assert!(
            read_json(&dir.path().join("missing.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn read_json_reports_invalid_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, "{").unwrap();
        assert!(
            read_json(&path)
                .unwrap_err()
                .to_string()
                .contains("invalid JSON")
        );
    }

    #[test]
    fn remove_file_if_exists_ignores_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.json");
        fs::write(&path, "{}").unwrap();

        remove_file_if_exists(&path).unwrap();
        remove_file_if_exists(&path).unwrap();

        assert!(!path.exists());
    }
}
