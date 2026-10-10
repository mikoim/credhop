//! Codex provider: credentials live in `~/.codex/auth.json`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

use crate::fsutil::{create_private_dir, read_json, remove_file_if_exists, write_json};
use crate::provider::Provider;

pub struct Codex {
    dir: PathBuf,
}

impl Codex {
    pub fn new(home: &Path) -> Self {
        Self {
            dir: home.join(".codex"),
        }
    }

    fn auth_path(&self) -> PathBuf {
        self.dir.join("auth.json")
    }
}

impl Provider for Codex {
    fn read_live(&self) -> Result<Option<Value>> {
        read_json(&self.auth_path())
    }

    fn write_live(&self, snapshot: &Value) -> Result<()> {
        create_private_dir(&self.dir)?;
        write_json(&self.auth_path(), snapshot, 0o600)
    }

    fn clear_live(&self) -> Result<()> {
        remove_file_if_exists(&self.auth_path())
    }

    fn account_id(&self, snapshot: &Value) -> Option<String> {
        // `account_id` identifies the ChatGPT workspace; `sub` identifies the user in it.
        if let Some(workspace) = snapshot
            .pointer("/tokens/account_id")
            .and_then(Value::as_str)
        {
            let user = id_token_claim(snapshot, "sub");
            return Some(match user {
                Some(user) => format!("{workspace}:{user}"),
                None => workspace.to_owned(),
            });
        }
        api_key(snapshot).map(|key| format!("apikey:{}", last4(key)))
    }

    fn display(&self, snapshot: &Value) -> Option<String> {
        id_token_claim(snapshot, "email")
            .or_else(|| api_key(snapshot).map(|key| format!("API key (****{})", last4(key))))
    }
}

fn api_key(snapshot: &Value) -> Option<&str> {
    snapshot
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
}

fn last4(s: &str) -> String {
    let skip = s.chars().count().saturating_sub(4);
    s.chars().skip(skip).collect()
}

/// Reads a string claim from the payload of `tokens.id_token` (a JWT).
fn id_token_claim(snapshot: &Value, claim: &str) -> Option<String> {
    let token = snapshot.pointer("/tokens/id_token")?.as_str()?;
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.get(claim)?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn jwt(claims: Value) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    fn chatgpt_auth() -> Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": jwt(json!({"sub": "user-1", "email": "alice@example.com"})),
                "access_token": "a",
                "refresh_token": "r",
                "account_id": "ws-1",
            },
        })
    }

    #[test]
    fn live_round_trip_with_private_mode() {
        let home = tempdir().unwrap();
        let codex = Codex::new(home.path());
        assert!(codex.read_live().unwrap().is_none());

        codex.write_live(&chatgpt_auth()).unwrap();

        let path = home.path().join(".codex/auth.json");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(codex.read_live().unwrap(), Some(chatgpt_auth()));
    }

    #[test]
    fn clear_live_removes_auth_file() {
        let home = tempdir().unwrap();
        let codex = Codex::new(home.path());
        codex.write_live(&chatgpt_auth()).unwrap();

        codex.clear_live().unwrap();
        codex.clear_live().unwrap();

        assert!(codex.read_live().unwrap().is_none());
    }

    #[test]
    fn chatgpt_account_id_and_display() {
        let home = tempdir().unwrap();
        let codex = Codex::new(home.path());
        let auth = chatgpt_auth();

        assert_eq!(codex.account_id(&auth).as_deref(), Some("ws-1:user-1"));
        assert_eq!(codex.display(&auth).as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn account_id_without_readable_id_token_uses_workspace_only() {
        let home = tempdir().unwrap();
        let auth = json!({"tokens": {"id_token": "garbage", "account_id": "ws-1"}});

        assert_eq!(
            Codex::new(home.path()).account_id(&auth).as_deref(),
            Some("ws-1")
        );
        assert_eq!(Codex::new(home.path()).display(&auth), None);
    }

    #[test]
    fn api_key_account_id_and_display() {
        let home = tempdir().unwrap();
        let codex = Codex::new(home.path());
        let auth = json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-test-abcd", "tokens": null});

        assert_eq!(codex.account_id(&auth).as_deref(), Some("apikey:abcd"));
        assert_eq!(codex.display(&auth).as_deref(), Some("API key (****abcd)"));
    }

    #[test]
    fn empty_api_key_gives_no_account_id() {
        let home = tempdir().unwrap();
        let codex = Codex::new(home.path());

        assert_eq!(codex.account_id(&json!({"OPENAI_API_KEY": ""})), None);
    }
}
