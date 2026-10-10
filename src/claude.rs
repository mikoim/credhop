//! Claude Code provider (Linux: credentials live in files under the home directory).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::fsutil::{create_private_dir, read_json, remove_file_if_exists, write_json};
use crate::provider::Provider;

/// Claude Code stores OAuth tokens in `~/.claude/.credentials.json` and the
/// account metadata in the `oauthAccount` key of `~/.claude.json`.
pub struct Claude {
    dir: PathBuf,
    config_path: PathBuf,
}

impl Claude {
    pub fn new(home: &Path) -> Self {
        Self {
            dir: home.join(".claude"),
            config_path: home.join(".claude.json"),
        }
    }

    fn credentials_path(&self) -> PathBuf {
        self.dir.join(".credentials.json")
    }
}

impl Provider for Claude {
    fn read_live(&self) -> Result<Option<Value>> {
        let Some(credentials) = read_json(&self.credentials_path())? else {
            return Ok(None);
        };
        let oauth_account = read_json(&self.config_path)?
            .and_then(|config| config.get("oauthAccount").cloned())
            .unwrap_or(Value::Null);
        Ok(Some(
            json!({"credentials": credentials, "oauthAccount": oauth_account}),
        ))
    }

    fn write_live(&self, snapshot: &Value) -> Result<()> {
        let credentials = snapshot
            .get("credentials")
            .context("invalid Claude profile: missing \"credentials\"")?;

        // Prepare ~/.claude.json (which holds many other settings; only
        // oauthAccount is replaced) before writing anything, so a failure
        // cannot leave new credentials paired with the old account.
        let config = match read_json(&self.config_path)? {
            Some(mut config) => {
                let object = config.as_object_mut().with_context(|| {
                    format!("{} is not a JSON object", self.config_path.display())
                })?;
                match snapshot.get("oauthAccount") {
                    Some(account) if !account.is_null() => {
                        object.insert("oauthAccount".to_owned(), account.clone());
                    }
                    _ => {
                        object.shift_remove("oauthAccount");
                    }
                }
                let mode = fs::metadata(&self.config_path)?.permissions().mode() & 0o777;
                Some((config, mode))
            }
            None => None,
        };

        create_private_dir(&self.dir)?;
        write_json(&self.credentials_path(), credentials, 0o600)?;
        if let Some((config, mode)) = config {
            write_json(&self.config_path, &config, mode)?;
        }
        Ok(())
    }

    fn clear_live(&self) -> Result<()> {
        // Credentials first: without them the tool is simply logged out, whereas
        // credentials without oauthAccount could not be matched to a profile.
        remove_file_if_exists(&self.credentials_path())?;
        if let Some(mut config) = read_json(&self.config_path)?
            && let Some(object) = config.as_object_mut()
            && object.shift_remove("oauthAccount").is_some()
        {
            let mode = fs::metadata(&self.config_path)?.permissions().mode() & 0o777;
            write_json(&self.config_path, &config, mode)?;
        }
        Ok(())
    }

    fn account_id(&self, snapshot: &Value) -> Option<String> {
        let account = snapshot.get("oauthAccount")?;
        let user = account.get("accountUuid")?.as_str()?;
        Some(
            match account.get("organizationUuid").and_then(Value::as_str) {
                Some(org) => format!("{user}:{org}"),
                None => user.to_owned(),
            },
        )
    }

    fn display(&self, snapshot: &Value) -> Option<String> {
        snapshot
            .pointer("/oauthAccount/emailAddress")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn write(path: &Path, value: &Value, mode: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value.to_string()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn snapshot(token: &str, user: &str) -> Value {
        json!({
            "credentials": {"claudeAiOauth": {"accessToken": token}},
            "oauthAccount": {"accountUuid": user, "organizationUuid": "org", "emailAddress": format!("{user}@example.com")},
        })
    }

    #[test]
    fn read_live_is_none_without_credentials() {
        let home = tempdir().unwrap();
        assert!(Claude::new(home.path()).read_live().unwrap().is_none());
    }

    #[test]
    fn read_live_combines_credentials_and_oauth_account() {
        let home = tempdir().unwrap();
        let expected = snapshot("t1", "alice");
        write(
            &home.path().join(".claude/.credentials.json"),
            &expected["credentials"],
            0o600,
        );
        write(
            &home.path().join(".claude.json"),
            &json!({"numStartups": 3, "oauthAccount": expected["oauthAccount"]}),
            0o644,
        );

        assert_eq!(
            Claude::new(home.path()).read_live().unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn read_live_uses_null_when_oauth_account_is_missing() {
        let home = tempdir().unwrap();
        write(
            &home.path().join(".claude/.credentials.json"),
            &json!({}),
            0o600,
        );

        let live = Claude::new(home.path()).read_live().unwrap().unwrap();
        assert!(live["oauthAccount"].is_null());
    }

    #[test]
    fn write_live_replaces_only_credentials_and_oauth_account() {
        let home = tempdir().unwrap();
        let config = home.path().join(".claude.json");
        write(
            &config,
            &json!({"zeta": 1, "oauthAccount": {"accountUuid": "old"}, "alpha": 2}),
            0o644,
        );

        let target = snapshot("t2", "bob");
        Claude::new(home.path()).write_live(&target).unwrap();

        let credentials = home.path().join(".claude/.credentials.json");
        assert_eq!(
            read_json(&credentials).unwrap(),
            Some(target["credentials"].clone())
        );
        assert_eq!(mode(&credentials), 0o600);
        let written = read_json(&config).unwrap().unwrap();
        let keys: Vec<&str> = written
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["zeta", "oauthAccount", "alpha"]);
        assert_eq!(written["oauthAccount"], target["oauthAccount"]);
        assert_eq!(mode(&config), 0o644);
    }

    #[test]
    fn write_live_removes_oauth_account_when_snapshot_has_none() {
        let home = tempdir().unwrap();
        let config = home.path().join(".claude.json");
        write(&config, &json!({"a": 1, "oauthAccount": {}, "b": 2}), 0o600);

        Claude::new(home.path())
            .write_live(&json!({"credentials": {}, "oauthAccount": null}))
            .unwrap();

        assert_eq!(read_json(&config).unwrap(), Some(json!({"a": 1, "b": 2})));
    }

    #[test]
    fn write_live_skips_missing_config_and_rejects_invalid_snapshot() {
        let home = tempdir().unwrap();
        let claude = Claude::new(home.path());

        claude.write_live(&snapshot("t", "alice")).unwrap();
        assert!(!home.path().join(".claude.json").exists());
        assert!(claude.write_live(&json!({})).is_err());
    }

    #[test]
    fn write_live_changes_nothing_when_config_is_not_an_object() {
        let home = tempdir().unwrap();
        let credentials = home.path().join(".claude/.credentials.json");
        let original = json!({"claudeAiOauth": {"accessToken": "old"}});
        write(&credentials, &original, 0o600);
        write(&home.path().join(".claude.json"), &json!([]), 0o600);

        let result = Claude::new(home.path()).write_live(&snapshot("t2", "bob"));

        assert!(result.is_err());
        assert_eq!(read_json(&credentials).unwrap(), Some(original));
    }

    #[test]
    fn clear_live_removes_credentials_and_oauth_account_only() {
        let home = tempdir().unwrap();
        let credentials = home.path().join(".claude/.credentials.json");
        let config = home.path().join(".claude.json");
        write(&credentials, &json!({}), 0o600);
        write(
            &config,
            &json!({"zeta": 1, "oauthAccount": {"accountUuid": "alice"}, "alpha": 2}),
            0o644,
        );
        let claude = Claude::new(home.path());

        claude.clear_live().unwrap();

        assert!(!credentials.exists());
        assert_eq!(
            read_json(&config).unwrap(),
            Some(json!({"zeta": 1, "alpha": 2}))
        );
        assert_eq!(mode(&config), 0o644);
        assert!(claude.read_live().unwrap().is_none());
    }

    #[test]
    fn clear_live_succeeds_when_nothing_exists() {
        let home = tempdir().unwrap();

        Claude::new(home.path()).clear_live().unwrap();

        assert!(!home.path().join(".claude.json").exists());
    }

    #[test]
    fn account_id_and_display() {
        let home = tempdir().unwrap();
        let claude = Claude::new(home.path());
        let snap = snapshot("t", "alice");

        assert_eq!(claude.account_id(&snap).as_deref(), Some("alice:org"));
        assert_eq!(claude.display(&snap).as_deref(), Some("alice@example.com"));
        let no_org = json!({"oauthAccount": {"accountUuid": "alice"}});
        assert_eq!(claude.account_id(&no_org).as_deref(), Some("alice"));
        assert_eq!(claude.account_id(&json!({"oauthAccount": null})), None);
    }
}
