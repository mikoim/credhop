//! Small filesystem helpers shared by the store and the providers.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// Atomically replace `path` with `bytes` and set its permission bits to `mode`.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    let name = path.file_name().context("path has no file name")?;
    let tmp = dir.join(format!(".{}.credhop-tmp", name.to_string_lossy()));

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("failed to create {}", tmp.display()))?;
    // The mode passed to open() is filtered by the umask, so set it explicitly.
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))
}

/// Create `dir` and any missing parents with mode 0700.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("failed to create {}", dir.display()))
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

/// Atomically write `value` as pretty-printed JSON.
pub fn write_json(path: &Path, value: &Value, mode: u32) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn atomic_write_replaces_content_and_sets_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.json");
        fs::write(&path, "old").unwrap();

        atomic_write(&path, b"new", 0o600).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(mode(&path), 0o600);
        assert!(!dir.path().join(".a.json.credhop-tmp").exists());
    }

    #[test]
    fn create_private_dir_creates_parents_with_0700() {
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

    #[test]
    fn write_json_round_trips_and_keeps_key_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.json");
        let value = json!({"b": 1, "a": 2});

        write_json(&path, &value, 0o600).unwrap();

        assert!(fs::read_to_string(&path).unwrap().starts_with("{\n  \"b\""));
        assert_eq!(read_json(&path).unwrap(), Some(value));
    }
}
