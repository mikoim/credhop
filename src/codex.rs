//! Codex provider: credentials live in `~/.codex/auth.json`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

use crate::fsutil::{create_private_dir, read_json, remove_file_if_exists, write_private_json};
use crate::http::{Http, host};
use crate::provider::{EXPIRY_MARGIN_SECS, Provider};
use crate::time::format_rfc3339;
use crate::usage::{Usage, Window};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Codex's OAuth client id (the audience of its id tokens).
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

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
        write_private_json(&self.auth_path(), snapshot)
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

    fn token_expired(&self, snapshot: &Value, now: i64) -> bool {
        snapshot
            .pointer("/tokens/access_token")
            .and_then(Value::as_str)
            .and_then(|token| jwt_claim(token, "exp")?.as_i64())
            .is_some_and(|exp| exp <= now + EXPIRY_MARGIN_SECS)
    }

    fn refresh(&self, snapshot: &Value, http: &dyn Http, now: i64) -> Result<Value> {
        let refresh_token = snapshot
            .pointer("/tokens/refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .context("no refresh token; log in to this account again")?;
        // RFC 6749 form encoding; without `scope`, the originally granted scope is kept.
        let response = http.post_form(
            TOKEN_URL,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", CLIENT_ID),
            ],
        )?;
        if response
            .get("access_token")
            .and_then(Value::as_str)
            .is_none()
        {
            bail!("unexpected response from {}", host(TOKEN_URL));
        }
        let mut refreshed = snapshot.clone();
        if let Some(tokens) = refreshed.get_mut("tokens").and_then(Value::as_object_mut) {
            for key in ["id_token", "access_token", "refresh_token"] {
                if let Some(token) = response.get(key).and_then(Value::as_str) {
                    tokens.insert(key.to_owned(), token.into());
                }
            }
        }
        refreshed["last_refresh"] = format_rfc3339(now).into();
        Ok(refreshed)
    }

    fn usage(&self, snapshot: &Value, http: &dyn Http) -> Result<Option<Usage>> {
        // API key mode has no subscription limits.
        let Some(token) = snapshot
            .pointer("/tokens/access_token")
            .and_then(Value::as_str)
        else {
            return Ok(None);
        };
        let account = snapshot
            .pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let authorization = format!("Bearer {token}");
        let response = http.get_json(
            USAGE_URL,
            &[
                ("Authorization", authorization.as_str()),
                ("ChatGPT-Account-Id", account),
            ],
        )?;
        let rate_limit = response
            .get("rate_limit")
            .with_context(|| format!("unexpected response from {}", host(USAGE_URL)))?;
        let windows: Vec<Window> = [
            ("primary_window", "primary"),
            ("secondary_window", "secondary"),
        ]
        .into_iter()
        .filter_map(|(key, fallback)| window(rate_limit.get(key)?, fallback))
        .collect();
        Ok((!windows.is_empty()).then_some(Usage { windows }))
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

/// Reads a string claim from the payload of `tokens.id_token`.
fn id_token_claim(snapshot: &Value, claim: &str) -> Option<String> {
    let token = snapshot.pointer("/tokens/id_token")?.as_str()?;
    match jwt_claim(token, claim)? {
        Value::String(value) => Some(value),
        _ => None,
    }
}

/// Reads a claim from the payload of a JWT.
fn jwt_claim(token: &str, claim: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let mut claims: Value = serde_json::from_slice(&bytes).ok()?;
    Some(claims.get_mut(claim)?.take())
}

/// A window of `rate_limit` in the usage response, labeled by its length;
/// `fallback` names it when the length is unknown.
fn window(window: &Value, fallback: &str) -> Option<Window> {
    let label = match window.get("limit_window_seconds").and_then(Value::as_i64) {
        Some(seconds) if seconds > 0 && seconds % 86_400 == 0 => format!("{}d", seconds / 86_400),
        Some(seconds) if seconds > 0 => format!("{}h", (seconds + 1800) / 3600),
        _ => fallback.to_owned(),
    };
    Some(Window {
        label,
        used_percent: window.get("used_percent")?.as_f64()?,
        resets_at: window.get("reset_at").and_then(Value::as_i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
    fn live_round_trip() {
        let home = tempdir().unwrap();
        let codex = Codex::new(home.path());
        assert!(codex.read_live().unwrap().is_none());

        codex.write_live(&chatgpt_auth()).unwrap();

        assert_eq!(codex.read_live().unwrap(), Some(chatgpt_auth()));
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_is_private() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let home = tempdir().unwrap();

        Codex::new(home.path()).write_live(&chatgpt_auth()).unwrap();

        let path = home.path().join(".codex/auth.json");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
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

    use crate::http::fake::{FakeHttp, Payload};
    use crate::usage::{Usage, Window};

    const NOW: i64 = 1_792_200_000;

    fn auth_expiring_at(exp: i64) -> Value {
        let mut auth = chatgpt_auth();
        auth["tokens"]["access_token"] = json!(jwt(json!({"exp": exp})));
        auth
    }

    fn api_key_auth() -> Value {
        json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-test-abcd", "tokens": null})
    }

    /// An anonymized response of the real usage endpoint.
    fn usage_response() -> Value {
        json!({
            "user_id": "user-1",
            "account_id": "ws-1",
            "email": "alice@example.com",
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {"used_percent": 23, "limit_window_seconds": 18000,
                                   "reset_after_seconds": 17974, "reset_at": 1_792_228_473},
                "secondary_window": {"used_percent": 41, "limit_window_seconds": 604800,
                                     "reset_after_seconds": 604774, "reset_at": 1_792_815_273},
            },
            "code_review_rate_limit": null,
            "additional_rate_limits": null,
            "credits": {"has_credits": false, "unlimited": false, "balance": "0"},
            "rate_limit_reached_type": null,
        })
    }

    #[test]
    fn token_expired_reads_the_access_token_exp() {
        let codex = Codex::new(Path::new("/unused"));

        assert!(!codex.token_expired(&auth_expiring_at(NOW + 61), NOW));
        assert!(codex.token_expired(&auth_expiring_at(NOW + 60), NOW));
        assert!(!codex.token_expired(&chatgpt_auth(), NOW)); // "a" is not a JWT
    }

    #[test]
    fn refresh_sends_the_form_and_updates_tokens_and_last_refresh() {
        let id_token = jwt(json!({"sub": "user-1", "email": "alice@example.com", "n": 2}));
        let returned_id_token = id_token.clone();
        let http = FakeHttp::new(move |_| {
            Ok(json!({
                "access_token": "a2",
                "id_token": returned_id_token,
                "refresh_token": "r2",
                "expires_in": 864_000,
                "earliest_refresh_at": NOW + 3600,
                "scope": "openid profile email offline_access",
                "token_type": "bearer",
            }))
        });
        let mut auth = chatgpt_auth();
        auth["last_refresh"] = json!("2026-10-06T02:32:55.809092737Z");

        let refreshed = Codex::new(Path::new("/unused"))
            .refresh(&auth, &http, NOW)
            .unwrap();

        assert_eq!(refreshed["tokens"]["access_token"], "a2");
        assert_eq!(refreshed["tokens"]["refresh_token"], "r2");
        assert_eq!(refreshed["tokens"]["id_token"], id_token.as_str());
        assert_eq!(refreshed["tokens"]["account_id"], "ws-1");
        assert_eq!(refreshed["last_refresh"], "2026-10-17T01:20:00Z");
        assert_eq!(refreshed["auth_mode"], "chatgpt");
        let requests = http.requests();
        assert_eq!(
            (requests[0].method, requests[0].url.as_str()),
            ("POST", "https://auth.openai.com/oauth/token")
        );
        assert_eq!(
            requests[0].payload,
            Payload::Form(vec![
                ("grant_type".to_owned(), "refresh_token".to_owned()),
                ("refresh_token".to_owned(), "r".to_owned()),
                (
                    "client_id".to_owned(),
                    "app_EMoamEEZ73f0CkXaXp7hrann".to_owned()
                ),
            ])
        );
    }

    #[test]
    fn refresh_keeps_tokens_that_are_not_returned() {
        let http = FakeHttp::new(|_| Ok(json!({"access_token": "a2"})));

        let refreshed = Codex::new(Path::new("/unused"))
            .refresh(&chatgpt_auth(), &http, NOW)
            .unwrap();

        assert_eq!(refreshed["tokens"]["refresh_token"], "r");
        assert_eq!(
            refreshed["tokens"]["id_token"],
            chatgpt_auth()["tokens"]["id_token"]
        );
    }

    #[test]
    fn refresh_fails_without_a_refresh_token_or_with_an_unexpected_response() {
        let http = FakeHttp::new(|_| Ok(json!({})));
        let codex = Codex::new(Path::new("/unused"));

        assert_eq!(
            codex
                .refresh(&api_key_auth(), &http, NOW)
                .unwrap_err()
                .to_string(),
            "no refresh token; log in to this account again"
        );
        assert!(http.requests().is_empty());
        assert_eq!(
            codex
                .refresh(&chatgpt_auth(), &http, NOW)
                .unwrap_err()
                .to_string(),
            "unexpected response from auth.openai.com"
        );
    }

    #[test]
    fn usage_reads_the_primary_and_secondary_windows() {
        let http = FakeHttp::new(|_| Ok(usage_response()));

        let usage = Codex::new(Path::new("/unused"))
            .usage(&chatgpt_auth(), &http)
            .unwrap();

        assert_eq!(
            usage,
            Some(Usage {
                windows: vec![
                    Window {
                        label: "5h".to_owned(),
                        used_percent: 23.0,
                        resets_at: Some(1_792_228_473),
                    },
                    Window {
                        label: "7d".to_owned(),
                        used_percent: 41.0,
                        resets_at: Some(1_792_815_273),
                    },
                ]
            })
        );
        let request = &http.requests()[0];
        assert_eq!(
            (request.method, request.url.as_str()),
            ("GET", "https://chatgpt.com/backend-api/wham/usage")
        );
        assert_eq!(request.header("Authorization"), Some("Bearer a"));
        assert_eq!(request.header("ChatGPT-Account-Id"), Some("ws-1"));
    }

    #[test]
    fn usage_has_no_limits_in_api_key_mode_or_without_rate_limit() {
        let codex = Codex::new(Path::new("/unused"));
        let http = FakeHttp::new(|_| Ok(json!({"plan_type": "free", "rate_limit": null})));
        let unexpected = FakeHttp::new(|_| Ok(json!({"detail": "?"})));

        assert_eq!(codex.usage(&api_key_auth(), &http).unwrap(), None);
        assert!(http.requests().is_empty());
        assert_eq!(codex.usage(&chatgpt_auth(), &http).unwrap(), None);
        assert_eq!(
            codex
                .usage(&chatgpt_auth(), &unexpected)
                .unwrap_err()
                .to_string(),
            "unexpected response from chatgpt.com"
        );
    }

    #[test]
    fn window_labels_come_from_the_window_length() {
        let label = |seconds: Value| {
            window(
                &json!({"used_percent": 1, "limit_window_seconds": seconds}),
                "primary",
            )
            .unwrap()
            .label
        };

        assert_eq!(label(json!(18_000)), "5h");
        assert_eq!(label(json!(604_800)), "7d");
        assert_eq!(label(json!(5_400)), "2h");
        assert_eq!(label(json!(null)), "primary");
    }
}
