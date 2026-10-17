//! Claude Code provider (Linux and Windows: credentials live in files under the home directory).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::fsutil::{
    create_private_dir, read_json, remove_file_if_exists, replace_json, write_private_json,
};
use crate::http::{Http, host};
use crate::provider::{EXPIRY_MARGIN_SECS, Provider};
use crate::time::parse_rfc3339;
use crate::usage::{Usage, Window};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Claude Code's OAuth client id.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

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
                Some(config)
            }
            None => None,
        };
        let previous = read_json(&self.credentials_path())?;

        create_private_dir(&self.dir)?;
        write_private_json(&self.credentials_path(), credentials)?;
        if let Some(config) = config
            && let Err(e) = replace_json(&self.config_path, &config)
        {
            // Put the old credentials back so they stay paired with the old
            // account; otherwise the next write-back would save the new tokens
            // into the old account's profile.
            let restored = match previous {
                Some(previous) => write_private_json(&self.credentials_path(), &previous),
                None => remove_file_if_exists(&self.credentials_path()),
            };
            return Err(match restored {
                Ok(()) => e,
                Err(restore) => e.context(format!(
                    "the previous credentials could not be restored either: {restore:#}"
                )),
            });
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
            replace_json(&self.config_path, &config)?;
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

    fn token_expired(&self, snapshot: &Value, now: i64) -> bool {
        snapshot
            .pointer("/credentials/claudeAiOauth/expiresAt")
            .and_then(Value::as_i64)
            .is_some_and(|expires_at_ms| expires_at_ms <= (now + EXPIRY_MARGIN_SECS) * 1000)
    }

    fn refresh(&self, snapshot: &Value, http: &dyn Http, now: i64) -> Result<Value> {
        let mut refreshed = snapshot.clone();
        let oauth = refreshed
            .pointer_mut("/credentials/claudeAiOauth")
            .and_then(Value::as_object_mut)
            .context("no refresh token; log in to this account again")?;
        let refresh_token = oauth
            .get("refreshToken")
            .and_then(Value::as_str)
            .context("no refresh token; log in to this account again")?;
        let scope = oauth
            .get("scopes")
            .and_then(Value::as_array)
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let body = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "scope": scope,
        });
        let response = http.post_json(TOKEN_URL, &body)?;
        let (Some(access_token), Some(expires_in)) = (
            response.get("access_token").and_then(Value::as_str),
            response.get("expires_in").and_then(Value::as_i64),
        ) else {
            bail!("unexpected response from {}", host(TOKEN_URL));
        };
        // Like Claude Code: keep the old refresh token when no new one is returned.
        oauth.insert("accessToken".to_owned(), access_token.into());
        oauth.insert("expiresAt".to_owned(), ((now + expires_in) * 1000).into());
        if let Some(token) = response.get("refresh_token").and_then(Value::as_str) {
            oauth.insert("refreshToken".to_owned(), token.into());
        }
        if let Some(seconds) = response
            .get("refresh_token_expires_in")
            .and_then(Value::as_i64)
        {
            oauth.insert(
                "refreshTokenExpiresAt".to_owned(),
                ((now + seconds) * 1000).into(),
            );
        }
        Ok(refreshed)
    }

    fn usage(&self, snapshot: &Value, http: &dyn Http) -> Result<Option<Usage>> {
        let token = snapshot
            .pointer("/credentials/claudeAiOauth/accessToken")
            .and_then(Value::as_str)
            .context("no access token; log in to this account again")?;
        let authorization = format!("Bearer {token}");
        let response = http.get_json(USAGE_URL, &[("Authorization", authorization.as_str())])?;
        let limits = response
            .get("limits")
            .and_then(Value::as_array)
            .with_context(|| format!("unexpected response from {}", host(USAGE_URL)))?;
        let windows: Vec<Window> = limits.iter().filter_map(window).collect();
        Ok((!windows.is_empty()).then_some(Usage { windows }))
    }
}

/// An entry of `limits` in the usage response, if it is a 5-hour (`session`)
/// or weekly limit. Limits scoped to a model carry the model's name.
fn window(limit: &Value) -> Option<Window> {
    let length = match limit.get("group")?.as_str()? {
        "session" => "5h",
        "weekly" => "7d",
        _ => return None,
    };
    let label = match limit
        .pointer("/scope/model/display_name")
        .and_then(Value::as_str)
    {
        Some(model) => format!("{length} {model}"),
        None => length.to_owned(),
    };
    Some(Window {
        label,
        used_percent: limit.get("percent")?.as_f64()?,
        resets_at: limit
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value.to_string()).unwrap();
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
        );
        write(
            &home.path().join(".claude.json"),
            &json!({"numStartups": 3, "oauthAccount": expected["oauthAccount"]}),
        );

        assert_eq!(
            Claude::new(home.path()).read_live().unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn read_live_uses_null_when_oauth_account_is_missing() {
        let home = tempdir().unwrap();
        write(&home.path().join(".claude/.credentials.json"), &json!({}));

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
        );

        let target = snapshot("t2", "bob");
        Claude::new(home.path()).write_live(&target).unwrap();

        let credentials = home.path().join(".claude/.credentials.json");
        assert_eq!(
            read_json(&credentials).unwrap(),
            Some(target["credentials"].clone())
        );
        let written = read_json(&config).unwrap().unwrap();
        let keys: Vec<&str> = written
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["zeta", "oauthAccount", "alpha"]);
        assert_eq!(written["oauthAccount"], target["oauthAccount"]);
    }

    #[test]
    fn write_live_removes_oauth_account_when_snapshot_has_none() {
        let home = tempdir().unwrap();
        let config = home.path().join(".claude.json");
        write(&config, &json!({"a": 1, "oauthAccount": {}, "b": 2}));

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
        write(&credentials, &original);
        write(&home.path().join(".claude.json"), &json!([]));

        let result = Claude::new(home.path()).write_live(&snapshot("t2", "bob"));

        assert!(result.is_err());
        assert_eq!(read_json(&credentials).unwrap(), Some(original));
    }

    #[test]
    fn clear_live_removes_credentials_and_oauth_account_only() {
        let home = tempdir().unwrap();
        let credentials = home.path().join(".claude/.credentials.json");
        let config = home.path().join(".claude.json");
        write(&credentials, &json!({}));
        write(
            &config,
            &json!({"zeta": 1, "oauthAccount": {"accountUuid": "alice"}, "alpha": 2}),
        );
        let claude = Claude::new(home.path());

        claude.clear_live().unwrap();

        assert!(!credentials.exists());
        assert_eq!(
            read_json(&config).unwrap(),
            Some(json!({"zeta": 1, "alpha": 2}))
        );
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

    #[cfg(unix)]
    #[test]
    fn credentials_are_private_and_config_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let home = tempdir().unwrap();
        let credentials = home.path().join(".claude/.credentials.json");
        let config = home.path().join(".claude.json");
        write(&config, &json!({"oauthAccount": {"accountUuid": "old"}}));
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
        let claude = Claude::new(home.path());

        claude.write_live(&snapshot("t", "alice")).unwrap();
        assert_eq!(mode(&credentials), 0o600);
        assert_eq!(mode(&config), 0o644);

        claude.clear_live().unwrap();
        assert_eq!(mode(&config), 0o644);
    }

    /// Makes replacing `~/.claude.json` fail, while it stays readable, until the guard is dropped.
    #[cfg(windows)]
    fn block_config(home: &Path) -> impl Sized {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        // Like a running tool that holds the file open without FILE_SHARE_DELETE.
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(home.join(".claude.json"))
            .unwrap()
    }

    /// Makes replacing `~/.claude.json` fail, while it stays readable, until the guard is dropped.
    #[cfg(unix)]
    fn block_config(home: &Path) -> impl Sized {
        use std::os::unix::fs::PermissionsExt;
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        fs::set_permissions(home, fs::Permissions::from_mode(0o555)).unwrap();
        Restore(home.to_path_buf())
    }

    #[test]
    fn write_live_restores_credentials_when_the_config_cannot_be_replaced() {
        let home = tempdir().unwrap();
        let alice = snapshot("a1", "alice");
        write(
            &home.path().join(".claude/.credentials.json"),
            &alice["credentials"],
        );
        write(
            &home.path().join(".claude.json"),
            &json!({"oauthAccount": alice["oauthAccount"]}),
        );
        let claude = Claude::new(home.path());

        let blocked = block_config(home.path());
        assert!(claude.write_live(&snapshot("b1", "bob")).is_err());
        drop(blocked);

        assert_eq!(claude.read_live().unwrap(), Some(alice));
    }

    #[test]
    fn write_live_removes_new_credentials_when_the_config_cannot_be_replaced() {
        let home = tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        write(&home.path().join(".claude.json"), &json!({}));
        let claude = Claude::new(home.path());

        let blocked = block_config(home.path());
        assert!(claude.write_live(&snapshot("b1", "bob")).is_err());
        drop(blocked);

        assert!(claude.read_live().unwrap().is_none());
    }

    use crate::http::fake::{FakeHttp, Payload};
    use crate::usage::{Usage, Window};

    const NOW: i64 = 1_792_200_000;

    /// A profile as Claude Code writes it, with access token `a1` and refresh token `r1`.
    fn oauth_snapshot(expires_at: i64) -> Value {
        json!({
            "credentials": {"claudeAiOauth": {
                "accessToken": "a1",
                "refreshToken": "r1",
                "expiresAt": expires_at * 1000,
                "refreshTokenExpiresAt": 1_800_000_000_000_i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "team",
            }},
            "oauthAccount": {"accountUuid": "alice", "emailAddress": "alice@example.com"},
        })
    }

    /// An anonymized response of the real usage endpoint.
    fn usage_response() -> Value {
        json!({
            "five_hour": {"utilization": 2.0, "resets_at": "2026-10-17T07:59:59.858856+00:00"},
            "seven_day": null,
            "limits": [
                {"kind": "session", "group": "session", "percent": 2, "severity": "normal",
                 "resets_at": "2026-10-17T07:59:59.858856+00:00", "scope": null, "is_active": true},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 0, "severity": "normal",
                 "resets_at": "2026-10-20T16:00:00+00:00",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null},
                 "is_active": false},
            ],
            "spend": {"percent": 0, "enabled": false},
        })
    }

    #[test]
    fn token_expired_reads_expires_at_in_milliseconds() {
        let claude = Claude::new(Path::new("/unused"));

        assert!(!claude.token_expired(&oauth_snapshot(NOW + 61), NOW));
        assert!(claude.token_expired(&oauth_snapshot(NOW + 60), NOW));
        assert!(!claude.token_expired(&snapshot("t", "alice"), NOW));
    }

    #[test]
    fn refresh_replaces_only_the_token_fields() {
        let http = FakeHttp::new(|_| {
            Ok(json!({
                "token_type": "Bearer",
                "access_token": "a2",
                "refresh_token": "r2",
                "expires_in": 28_800,
                "refresh_token_expires_in": 2_592_000,
                "scope": "user:inference user:profile",
            }))
        });

        let refreshed = Claude::new(Path::new("/unused"))
            .refresh(&oauth_snapshot(NOW), &http, NOW)
            .unwrap();

        let mut expected = oauth_snapshot(NOW);
        let oauth = &mut expected["credentials"]["claudeAiOauth"];
        oauth["accessToken"] = json!("a2");
        oauth["refreshToken"] = json!("r2");
        oauth["expiresAt"] = json!((NOW + 28_800) * 1000);
        oauth["refreshTokenExpiresAt"] = json!((NOW + 2_592_000) * 1000);
        assert_eq!(refreshed, expected);
        let keys: Vec<&str> = refreshed["credentials"]["claudeAiOauth"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "accessToken",
                "refreshToken",
                "expiresAt",
                "refreshTokenExpiresAt",
                "scopes",
                "subscriptionType"
            ]
        );
        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            (requests[0].method, requests[0].url.as_str()),
            ("POST", "https://platform.claude.com/v1/oauth/token")
        );
        assert_eq!(
            requests[0].payload,
            Payload::Json(json!({
                "grant_type": "refresh_token",
                "refresh_token": "r1",
                "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                "scope": "user:inference user:profile",
            }))
        );
    }

    #[test]
    fn refresh_keeps_the_refresh_token_when_none_is_returned() {
        let http = FakeHttp::new(|_| Ok(json!({"access_token": "a2", "expires_in": 3600})));

        let refreshed = Claude::new(Path::new("/unused"))
            .refresh(&oauth_snapshot(NOW), &http, NOW)
            .unwrap();

        let oauth = &refreshed["credentials"]["claudeAiOauth"];
        assert_eq!(oauth["refreshToken"], "r1");
        assert_eq!(oauth["refreshTokenExpiresAt"], 1_800_000_000_000_i64);
        assert_eq!(oauth["expiresAt"], (NOW + 3600) * 1000);
    }

    #[test]
    fn refresh_fails_without_a_refresh_token_or_with_an_unexpected_response() {
        let http = FakeHttp::new(|_| Ok(json!({"error": "nope"})));
        let claude = Claude::new(Path::new("/unused"));

        let err = claude
            .refresh(&snapshot("t", "alice"), &http, NOW)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "no refresh token; log in to this account again"
        );
        assert!(http.requests().is_empty());

        let err = claude
            .refresh(&oauth_snapshot(NOW), &http, NOW)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "unexpected response from platform.claude.com"
        );
    }

    #[test]
    fn usage_reads_the_session_and_weekly_limits() {
        let http = FakeHttp::new(|_| Ok(usage_response()));

        let usage = Claude::new(Path::new("/unused"))
            .usage(&oauth_snapshot(NOW), &http)
            .unwrap();

        assert_eq!(
            usage,
            Some(Usage {
                windows: vec![
                    Window {
                        label: "5h".to_owned(),
                        used_percent: 2.0,
                        resets_at: Some(1_792_223_999),
                    },
                    Window {
                        label: "7d Fable".to_owned(),
                        used_percent: 0.0,
                        resets_at: Some(1_792_512_000),
                    },
                ]
            })
        );
        let requests = http.requests();
        assert_eq!(
            (requests[0].method, requests[0].url.as_str()),
            ("GET", "https://api.anthropic.com/api/oauth/usage")
        );
        assert_eq!(requests[0].header("Authorization"), Some("Bearer a1"));
    }

    #[test]
    fn usage_labels_unscoped_weekly_limits_and_skips_other_groups() {
        let http = FakeHttp::new(|_| {
            Ok(json!({"limits": [
                {"group": "weekly", "percent": 41, "resets_at": null, "scope": null},
                {"group": "spend", "percent": 10, "resets_at": null},
            ]}))
        });

        let usage = Claude::new(Path::new("/unused"))
            .usage(&oauth_snapshot(NOW), &http)
            .unwrap();

        assert_eq!(
            usage,
            Some(Usage {
                windows: vec![Window {
                    label: "7d".to_owned(),
                    used_percent: 41.0,
                    resets_at: None,
                }]
            })
        );
    }

    #[test]
    fn usage_without_limits_or_without_a_token() {
        let claude = Claude::new(Path::new("/unused"));
        let empty = FakeHttp::new(|_| Ok(json!({"limits": []})));
        let unexpected = FakeHttp::new(|_| Ok(json!({"five_hour": null})));

        assert_eq!(claude.usage(&oauth_snapshot(NOW), &empty).unwrap(), None);
        assert_eq!(
            claude
                .usage(&oauth_snapshot(NOW), &unexpected)
                .unwrap_err()
                .to_string(),
            "unexpected response from api.anthropic.com"
        );
        assert_eq!(
            claude
                .usage(&json!({"credentials": {}}), &empty)
                .unwrap_err()
                .to_string(),
            "no access token; log in to this account again"
        );
    }
}
