//! The credhop subcommands.

use std::io::Write;
use std::path::PathBuf;
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::http::{Http, StatusError};
use crate::provider::{Provider, Tool, provider_for};
use crate::store::Store;
use crate::usage::{Usage, format_windows};

/// Everything a command needs: the profile store and the home directory of the live credentials.
pub struct Ctx {
    pub store: Store,
    pub home: PathBuf,
}

/// One account `usage` reports on.
struct Row {
    tool: Tool,
    /// The profile name, or `(unsaved)`.
    name: String,
    /// The credentials to query with: the live ones for the live account.
    snapshot: Value,
    /// Whether `snapshot` holds the live credentials, which credhop never refreshes.
    live: bool,
}

/// What `usage` found out about one account.
enum Outcome {
    Limits(Usage),
    NoLimits,
    Expired,
    Failed(anyhow::Error),
}
impl Ctx {
    fn provider(&self, tool: Tool) -> Box<dyn Provider> {
        provider_for(tool, &self.home)
    }

    /// Name of the saved profile holding the same account as `snapshot`.
    fn find_owner(
        &self,
        tool: Tool,
        provider: &dyn Provider,
        snapshot: &Value,
    ) -> Result<Option<String>> {
        let Some(id) = provider.account_id(snapshot) else {
            return Ok(None);
        };
        for name in self.store.list_profiles(tool)? {
            if let Some(profile) = self.store.load_profile(tool, &name)?
                && provider.account_id(&profile).as_deref() == Some(id.as_str())
            {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }

    /// Writes the live credentials back to the profile of the same account
    /// before they are replaced, since tokens rotate while a tool runs.
    /// Returns that profile's name, or None if nothing was saved.
    fn write_back_live(
        &self,
        tool: Tool,
        provider: &dyn Provider,
        force: bool,
        out: &mut dyn Write,
    ) -> Result<Option<String>> {
        let Some(live) = provider.read_live()? else {
            return Ok(None);
        };
        match self.find_owner(tool, provider, &live)? {
            Some(owner) => {
                self.store.save_profile(tool, &owner, &live)?;
                Ok(Some(owner))
            }
            None if force => {
                writeln!(out, "Discarding unsaved {tool} credentials (--force)")?;
                Ok(None)
            }
            None => bail!(
                "current {tool} credentials are not saved in any profile; \
                 run `credhop save {tool} <name>` first or pass --force"
            ),
        }
    }

    /// The accounts of `tool` that `usage` reports on: the profile `name`, or
    /// every profile plus the live account when it is not saved.
    fn usage_rows(&self, tool: Tool, name: Option<&str>) -> Result<Vec<Row>> {
        let provider = self.provider(tool);
        let live = provider.read_live()?;
        let live_id = live.as_ref().and_then(|live| provider.account_id(live));
        let profile_names = match name {
            Some(name) => vec![name.to_owned()],
            None => self.store.list_profiles(tool)?,
        };
        let mut rows = Vec::new();
        for profile_name in profile_names {
            let profile = self
                .store
                .load_profile(tool, &profile_name)?
                .with_context(|| format!("profile '{tool}/{profile_name}' not found"))?;
            // The live credentials hold the tokens the tool rotated since the last switch.
            let row = match &live {
                Some(live) if live_id.is_some() && provider.account_id(&profile) == live_id => {
                    Row {
                        tool,
                        name: profile_name,
                        snapshot: live.clone(),
                        live: true,
                    }
                }
                _ => Row {
                    tool,
                    name: profile_name,
                    snapshot: profile,
                    live: false,
                },
            };
            rows.push(row);
        }
        if name.is_none()
            && let Some(live) = live
            && !rows.iter().any(|row| row.live)
        {
            rows.insert(
                0,
                Row {
                    tool,
                    name: "(unsaved)".to_owned(),
                    snapshot: live,
                    live: true,
                },
            );
        }
        Ok(rows)
    }

    /// Queries the usage of one account, first refreshing and saving the
    /// tokens of a profile whose access token expired.
    fn fetch_usage(&self, row: &Row, http: &dyn Http, now: i64) -> Outcome {
        let provider = self.provider(row.tool);
        let refreshed;
        let mut snapshot = &row.snapshot;
        if provider.token_expired(snapshot, now) {
            // The running tool may be refreshing the live credentials at the same time.
            if row.live {
                return Outcome::Expired;
            }
            refreshed = match provider.refresh(snapshot, http, now) {
                Ok(refreshed) => refreshed,
                Err(e) => return Outcome::Failed(refresh_error(e)),
            };
            // Save at once: the old refresh token no longer works.
            if let Err(e) = self.store.save_profile(row.tool, &row.name, &refreshed) {
                return Outcome::Failed(anyhow!(
                    "tokens were refreshed but saving the profile failed: {e:#}; log in again"
                ));
            }
            snapshot = &refreshed;
        }
        match provider.usage(snapshot, http) {
            Ok(Some(usage)) => Outcome::Limits(usage),
            Ok(None) => Outcome::NoLimits,
            Err(e) => Outcome::Failed(usage_error(e)),
        }
    }
}

/// `" (alice@example.com)"`, or an empty string when there is no label.
fn label(provider: &dyn Provider, snapshot: &Value) -> String {
    provider
        .display(snapshot)
        .map(|display| format!(" ({display})"))
        .unwrap_or_default()
}

/// A rejected refresh token (HTTP 400 or 401) means logging in again.
fn refresh_error(e: anyhow::Error) -> anyhow::Error {
    if let Some(&StatusError {
        status: status @ (400 | 401),
        ..
    }) = e.downcast_ref::<StatusError>()
    {
        return anyhow!(
            "token refresh failed (HTTP {status}); log in to this account again and run `credhop save`"
        );
    }
    e.context("token refresh failed")
}

/// HTTP 401 means the saved tokens no longer work.
fn usage_error(e: anyhow::Error) -> anyhow::Error {
    if let Some(StatusError { status: 401, host }) = e.downcast_ref::<StatusError>() {
        return anyhow!("unauthorized (HTTP 401 from {host}); log in to this account again");
    }
    e
}

pub fn list(ctx: &Ctx, out: &mut dyn Write) -> Result<()> {
    for tool in Tool::ALL {
        let provider = ctx.provider(tool);
        let active = ctx.store.active(tool)?;
        writeln!(out, "{tool}")?;
        let names = ctx.store.list_profiles(tool)?;
        if names.is_empty() {
            writeln!(out, "  (no profiles)")?;
        }
        for name in names {
            let marker = if active.as_deref() == Some(name.as_str()) {
                '*'
            } else {
                ' '
            };
            let display = ctx
                .store
                .load_profile(tool, &name)?
                .and_then(|profile| provider.display(&profile))
                .unwrap_or_default();
            let line = format!("{marker} {name:<16} {display}");
            writeln!(out, "{}", line.trim_end())?;
        }
    }
    Ok(())
}

pub fn save(ctx: &Ctx, tool: Tool, name: &str, force: bool, out: &mut dyn Write) -> Result<()> {
    let exists = ctx.store.load_profile(tool, name)?.is_some();
    let provider = ctx.provider(tool);
    let live = provider
        .read_live()?
        .with_context(|| format!("not logged in to {tool}"))?;
    if provider.account_id(&live).is_none() {
        bail!("cannot identify the current {tool} account; log in again and retry");
    }
    // One account per profile, so `use` knows where to write rotated tokens back.
    if let Some(owner) = ctx.find_owner(tool, provider.as_ref(), &live)?
        && owner != name
    {
        bail!("this {tool} account is already saved as '{owner}'");
    }
    if exists && !force {
        bail!("profile '{tool}/{name}' already exists; pass --force to overwrite");
    }
    ctx.store.save_profile(tool, name, &live)?;
    ctx.store.set_active(tool, Some(name))?;
    writeln!(
        out,
        "Saved {tool} profile '{name}'{}",
        label(provider.as_ref(), &live)
    )?;
    Ok(())
}

pub fn use_profile(
    ctx: &Ctx,
    tool: Tool,
    name: &str,
    force: bool,
    out: &mut dyn Write,
) -> Result<()> {
    if ctx.store.load_profile(tool, name)?.is_none() {
        bail!("profile '{tool}/{name}' not found");
    }
    let provider = ctx.provider(tool);

    ctx.write_back_live(tool, provider.as_ref(), force, out)?;

    // Load after the write-back, which may have updated this very profile.
    let target = ctx
        .store
        .load_profile(tool, name)?
        .with_context(|| format!("profile '{tool}/{name}' not found"))?;
    provider.write_live(&target)?;
    ctx.store.set_active(tool, Some(name))?;
    writeln!(
        out,
        "Switched {tool} to '{name}'{}. Restart running {tool} sessions to apply.",
        label(provider.as_ref(), &target)
    )?;
    Ok(())
}

/// Logs out locally so the next login can add a new account without losing
/// the rotated tokens of the current one.
pub fn logout(ctx: &Ctx, tool: Tool, force: bool, out: &mut dyn Write) -> Result<()> {
    let provider = ctx.provider(tool);
    if provider.read_live()?.is_none() {
        writeln!(out, "Not logged in to {tool}")?;
        return Ok(());
    }
    let owner = ctx.write_back_live(tool, provider.as_ref(), force, out)?;
    provider.clear_live()?;
    ctx.store.set_active(tool, None)?;
    match owner {
        Some(owner) => writeln!(out, "Logged out of {tool}; credentials saved to '{owner}'")?,
        None => writeln!(out, "Logged out of {tool}")?,
    }
    // Claude Code skips the login screen once onboarding is done, so say how to log in.
    let login = match tool {
        Tool::Claude => "Start claude and run /login",
        Tool::Codex => "Run `codex login`",
    };
    writeln!(
        out,
        "{login}, then run `credhop save {tool} <name>` to add a profile."
    )?;
    Ok(())
}

pub fn remove(ctx: &Ctx, tool: Tool, name: &str, out: &mut dyn Write) -> Result<()> {
    if !ctx.store.remove_profile(tool, name)? {
        bail!("profile '{tool}/{name}' not found");
    }
    if ctx.store.active(tool)?.as_deref() == Some(name) {
        ctx.store.set_active(tool, None)?;
    }
    writeln!(out, "Removed {tool} profile '{name}'")?;
    Ok(())
}

pub fn status(ctx: &Ctx, out: &mut dyn Write) -> Result<()> {
    for tool in Tool::ALL {
        let provider = ctx.provider(tool);
        let line = match provider.read_live()? {
            None => "not logged in".to_owned(),
            Some(live) => {
                let profile = ctx
                    .find_owner(tool, provider.as_ref(), &live)?
                    .unwrap_or_else(|| "(unsaved)".to_owned());
                format!("{profile}{}", label(provider.as_ref(), &live))
            }
        };
        writeln!(out, "{tool:<7} {line}")?;
    }
    Ok(())
}

/// Shows how much of the subscription usage limits is left for the profile
/// `name` of `tool`, or for every account. `name` is only given with `tool`.
pub fn usage(
    ctx: &Ctx,
    http: &dyn Http,
    tool: Option<Tool>,
    name: Option<&str>,
    now: i64,
    out: &mut dyn Write,
) -> Result<()> {
    // Refresh tokens work once: without the lock, two runs at the same time
    // could refresh a profile with the same token and break it.
    let _lock = ctx.store.lock()?;
    let tools = tool.as_ref().map_or(&Tool::ALL[..], std::slice::from_ref);
    let mut rows = Vec::new();
    for &tool in tools {
        rows.extend(ctx.usage_rows(tool, name)?);
    }
    let outcomes: Vec<Outcome> = thread::scope(|scope| {
        let handles: Vec<_> = rows
            .iter()
            .map(|row| scope.spawn(move || ctx.fetch_usage(row, http, now)))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });

    let displays: Vec<String> = rows
        .iter()
        .map(|row| {
            ctx.provider(row.tool)
                .display(&row.snapshot)
                .unwrap_or_default()
        })
        .collect();
    let width = displays
        .iter()
        .map(|display| display.chars().count())
        .max()
        .unwrap_or(0);
    let mut failed = 0;
    for &tool in tools {
        writeln!(out, "{tool}")?;
        let mut empty = true;
        for ((row, outcome), display) in rows.iter().zip(&outcomes).zip(&displays) {
            if row.tool != tool {
                continue;
            }
            empty = false;
            let detail = match outcome {
                Outcome::Limits(usage) => format_windows(&usage.windows, now),
                Outcome::NoLimits => "no subscription limits".to_owned(),
                Outcome::Expired => format!("access token expired; run {tool} to refresh it"),
                Outcome::Failed(e) => {
                    failed += 1;
                    format!("error: {e:#}")
                }
            };
            let marker = if row.live { '*' } else { ' ' };
            let line = format!("{marker} {:<16} {display:<width$}  {detail}", row.name);
            writeln!(out, "{}", line.trim_end())?;
        }
        if empty {
            writeln!(out, "  (no profiles)")?;
        }
    }
    if failed > 0 {
        bail!("could not get the usage of {failed} account(s)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Ctx) {
        let home = TempDir::new().unwrap();
        let ctx = Ctx {
            store: Store::new(home.path().join("data")),
            home: home.path().to_path_buf(),
        };
        (home, ctx)
    }

    /// Simulates Claude Code being logged in as `user` with access token `token`.
    fn login_claude(ctx: &Ctx, user: &str, token: &str) {
        fs::create_dir_all(ctx.home.join(".claude")).unwrap();
        let credentials = json!({"claudeAiOauth": {"accessToken": token}});
        fs::write(
            ctx.home.join(".claude/.credentials.json"),
            credentials.to_string(),
        )
        .unwrap();
        let config = json!({
            "numStartups": 1,
            "oauthAccount": {"accountUuid": user, "emailAddress": format!("{user}@example.com")},
        });
        fs::write(ctx.home.join(".claude.json"), config.to_string()).unwrap();
    }

    fn live_token(ctx: &Ctx) -> String {
        let bytes = fs::read(ctx.home.join(".claude/.credentials.json")).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        value["claudeAiOauth"]["accessToken"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn profile_token(ctx: &Ctx, name: &str) -> String {
        let profile = ctx.store.load_profile(Tool::Claude, name).unwrap().unwrap();
        profile["credentials"]["claudeAiOauth"]["accessToken"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn output(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> String {
        let mut out = Vec::new();
        f(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn save_stores_profile_and_marks_it_active() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");

        let out = output(|out| save(&ctx, Tool::Claude, "work", false, out));

        assert_eq!(out, "Saved claude profile 'work' (alice@example.com)\n");
        assert_eq!(profile_token(&ctx, "work"), "a1");
        assert_eq!(
            ctx.store.active(Tool::Claude).unwrap().as_deref(),
            Some("work")
        );
    }

    #[test]
    fn save_fails_when_not_logged_in() {
        let (_home, ctx) = setup();
        let err = save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("not logged in to claude"));
    }

    #[test]
    fn save_rejects_login_without_account_id() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        fs::write(ctx.home.join(".claude.json"), "{}").unwrap();

        let err = save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap_err();

        assert_eq!(
            err.to_string(),
            "cannot identify the current claude account; log in again and retry"
        );
    }

    #[test]
    fn save_rejects_same_account_under_another_name() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();

        let err = save(&ctx, Tool::Claude, "other", false, &mut Vec::new()).unwrap_err();

        assert!(err.to_string().contains("already saved as 'work'"));
    }

    #[test]
    fn save_requires_force_to_overwrite() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "alice", "a2");

        assert!(save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).is_err());
        save(&ctx, Tool::Claude, "work", true, &mut Vec::new()).unwrap();

        assert_eq!(profile_token(&ctx, "work"), "a2");
    }

    #[test]
    fn use_writes_back_rotated_tokens_and_switches() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "bob", "b1");
        save(&ctx, Tool::Claude, "personal", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "bob", "b2"); // Claude Code refreshed the token.

        let out = output(|out| use_profile(&ctx, Tool::Claude, "work", false, out));

        assert!(out.starts_with("Switched claude to 'work' (alice@example.com)"));
        assert_eq!(profile_token(&ctx, "personal"), "b2");
        assert_eq!(live_token(&ctx), "a1");
        assert_eq!(
            ctx.store.active(Tool::Claude).unwrap().as_deref(),
            Some("work")
        );
    }

    #[test]
    fn use_current_profile_keeps_rotated_tokens() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "alice", "a2");

        use_profile(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();

        assert_eq!(live_token(&ctx), "a2");
    }

    #[test]
    fn use_refuses_to_discard_unsaved_account_without_force() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "carol", "c1");

        let err = use_profile(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("not saved in any profile"));
        assert_eq!(live_token(&ctx), "c1");

        use_profile(&ctx, Tool::Claude, "work", true, &mut Vec::new()).unwrap();
        assert_eq!(live_token(&ctx), "a1");
    }

    #[test]
    fn use_switches_when_not_logged_in() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        fs::remove_file(ctx.home.join(".claude/.credentials.json")).unwrap();

        use_profile(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();

        assert_eq!(live_token(&ctx), "a1");
    }

    #[test]
    fn use_treats_unidentifiable_login_as_unsaved() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "alice", "a2");
        fs::write(ctx.home.join(".claude.json"), "{}").unwrap();

        let err = use_profile(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap_err();

        assert!(err.to_string().contains("not saved in any profile"));
        assert_eq!(live_token(&ctx), "a2");
    }

    #[test]
    fn use_unknown_profile_fails() {
        let (_home, ctx) = setup();
        let err = use_profile(&ctx, Tool::Claude, "nope", false, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("profile 'claude/nope' not found"));
    }

    #[test]
    fn logout_writes_back_rotated_tokens_and_clears_live() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "alice", "a2"); // Claude Code refreshed the token.

        let out = output(|out| logout(&ctx, Tool::Claude, false, out));

        assert_eq!(
            out,
            "Logged out of claude; credentials saved to 'work'\n\
             Start claude and run /login, then run `credhop save claude <name>` to add a profile.\n"
        );
        assert_eq!(profile_token(&ctx, "work"), "a2");
        assert!(!ctx.home.join(".claude/.credentials.json").exists());
        assert_eq!(ctx.store.active(Tool::Claude).unwrap(), None);
    }

    #[test]
    fn logout_refuses_to_discard_unsaved_account_without_force() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "carol", "c1");

        let err = logout(&ctx, Tool::Claude, false, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("not saved in any profile"));
        assert_eq!(live_token(&ctx), "c1");

        let out = output(|out| logout(&ctx, Tool::Claude, true, out));
        assert!(out.starts_with(
            "Discarding unsaved claude credentials (--force)\nLogged out of claude\n"
        ));
        assert!(!ctx.home.join(".claude/.credentials.json").exists());
    }

    #[test]
    fn logout_when_not_logged_in_does_nothing() {
        let (_home, ctx) = setup();

        let out = output(|out| logout(&ctx, Tool::Claude, false, out));

        assert_eq!(out, "Not logged in to claude\n");
    }

    #[test]
    fn remove_deletes_profile_and_clears_active() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();

        remove(&ctx, Tool::Claude, "work", &mut Vec::new()).unwrap();

        assert!(
            ctx.store
                .load_profile(Tool::Claude, "work")
                .unwrap()
                .is_none()
        );
        assert_eq!(ctx.store.active(Tool::Claude).unwrap(), None);
        assert!(remove(&ctx, Tool::Claude, "work", &mut Vec::new()).is_err());
    }

    #[test]
    fn remove_inactive_profile_keeps_active() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "bob", "b1");
        save(&ctx, Tool::Claude, "personal", false, &mut Vec::new()).unwrap();

        remove(&ctx, Tool::Claude, "work", &mut Vec::new()).unwrap();

        assert_eq!(
            ctx.store.active(Tool::Claude).unwrap().as_deref(),
            Some("personal")
        );
    }

    #[test]
    fn list_and_status_show_profiles() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "bob", "b1");
        save(&ctx, Tool::Claude, "personal", false, &mut Vec::new()).unwrap();

        assert_eq!(
            output(|out| list(&ctx, out)),
            "claude\n* personal         bob@example.com\n  work             alice@example.com\ncodex\n  (no profiles)\n",
        );
        assert_eq!(
            output(|out| status(&ctx, out)),
            "claude  personal (bob@example.com)\ncodex   not logged in\n",
        );

        login_claude(&ctx, "carol", "c1");
        assert!(
            output(|out| status(&ctx, out)).starts_with("claude  (unsaved) (carol@example.com)\n")
        );
    }

    use crate::http::fake::FakeHttp;
    use crate::time::format_rfc3339;

    const NOW: i64 = 1_792_200_000;
    const CLAUDE_TOKEN: &str = "https://platform.claude.com/v1/oauth/token";

    /// Saves a Claude profile for `user` with access token `token`, expiring at `expires_at`.
    fn save_claude_profile(ctx: &Ctx, name: &str, user: &str, token: &str, expires_at: i64) {
        let profile = json!({
            "credentials": {"claudeAiOauth": {
                "accessToken": token,
                "refreshToken": format!("refresh-{token}"),
                "expiresAt": expires_at * 1000,
            }},
            "oauthAccount": {"accountUuid": user, "emailAddress": format!("{user}@example.com")},
        });
        ctx.store
            .save_profile(Tool::Claude, name, &profile)
            .unwrap();
    }

    /// Makes the live Claude access token expire at `expires_at`.
    fn expire_live_claude(ctx: &Ctx, expires_at: i64) {
        let path = ctx.home.join(".claude/.credentials.json");
        let mut credentials: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        credentials["claudeAiOauth"]["expiresAt"] = json!(expires_at * 1000);
        fs::write(path, credentials.to_string()).unwrap();
    }

    /// A Claude usage response with one 5-hour window.
    fn session_usage(percent: u32, resets_in: i64) -> Value {
        json!({"limits": [
            {"group": "session", "percent": percent, "resets_at": format_rfc3339(NOW + resets_in)},
        ]})
    }

    #[test]
    fn usage_queries_every_account_and_the_live_one_with_live_tokens() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "bob", "b1");
        save(&ctx, Tool::Claude, "personal", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "bob", "b2"); // Claude Code refreshed the token.
        let http = FakeHttp::new(|request| match request.header("Authorization") {
            Some("Bearer a1") => Ok(session_usage(23, 7980)),
            Some("Bearer b2") => Ok(session_usage(0, 3600)),
            _ => Err(401),
        });

        let out = output(|out| usage(&ctx, &http, None, None, NOW, out));

        assert_eq!(
            out,
            concat!(
                "claude\n",
                "* personal         bob@example.com    5h 100% left (resets in 1h0m)\n",
                "  work             alice@example.com  5h 77% left (resets in 2h13m)\n",
                "codex\n",
                "  (no profiles)\n",
            )
        );
        assert!(
            http.requests()
                .iter()
                .all(|request| request.method == "GET")
        );
        assert_eq!(profile_token(&ctx, "personal"), "b1"); // nothing written back
    }

    #[test]
    fn usage_refreshes_an_expired_profile_and_saves_it_first() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW - 10);
        let http = FakeHttp::new(|request| match (request.method, request.url.as_str()) {
            ("POST", CLAUDE_TOKEN) => Ok(json!({
                "access_token": "a2",
                "refresh_token": "refresh-a2",
                "expires_in": 28_800,
            })),
            _ => Err(500),
        });
        let mut out = Vec::new();

        let err = usage(&ctx, &http, Some(Tool::Claude), None, NOW, &mut out).unwrap_err();

        assert_eq!(err.to_string(), "could not get the usage of 1 account(s)");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "claude\n  work             alice@example.com  error: HTTP 500 from api.anthropic.com\n"
        );
        assert_eq!(profile_token(&ctx, "work"), "a2");
        let profile = ctx
            .store
            .load_profile(Tool::Claude, "work")
            .unwrap()
            .unwrap();
        assert_eq!(
            profile["credentials"]["claudeAiOauth"]["refreshToken"],
            "refresh-a2"
        );
        let requests = http.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].header("Authorization"), Some("Bearer a2"));
    }

    #[test]
    fn usage_never_refreshes_the_live_credentials() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        expire_live_claude(&ctx, NOW - 10);
        let http = FakeHttp::new(|_| Err(500));

        let out = output(|out| usage(&ctx, &http, Some(Tool::Claude), None, NOW, out));

        assert_eq!(
            out,
            "claude\n* work             alice@example.com  access token expired; run claude to refresh it\n"
        );
        assert!(http.requests().is_empty());
    }

    #[test]
    fn usage_reports_each_failure_and_still_shows_the_other_accounts() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW + 3600);
        save_claude_profile(&ctx, "team", "bob", "b1", NOW + 3600);
        let http = FakeHttp::new(|request| match request.header("Authorization") {
            Some("Bearer a1") => Ok(session_usage(50, 60)),
            _ => Err(401),
        });
        let mut out = Vec::new();

        let err = usage(&ctx, &http, Some(Tool::Claude), None, NOW, &mut out).unwrap_err();

        assert_eq!(err.to_string(), "could not get the usage of 1 account(s)");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "claude\n",
                "  team             bob@example.com    error: unauthorized (HTTP 401 from api.anthropic.com); log in to this account again\n",
                "  work             alice@example.com  5h 50% left (resets in 1m)\n",
            )
        );
    }

    #[test]
    fn usage_asks_to_log_in_again_when_the_refresh_token_is_rejected() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW - 10);
        let http = FakeHttp::new(|_| Err(400));
        let mut out = Vec::new();

        usage(&ctx, &http, Some(Tool::Claude), None, NOW, &mut out).unwrap_err();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "claude\n  work             alice@example.com  error: token refresh failed (HTTP 400); log in to this account again and run `credhop save`\n"
        );
        assert_eq!(profile_token(&ctx, "work"), "a1");
    }

    #[test]
    fn usage_adds_the_unsaved_live_account_unless_a_profile_is_named() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "carol", "c1");
        let http = FakeHttp::new(|_| Ok(session_usage(0, 60)));

        assert_eq!(
            output(|out| usage(&ctx, &http, Some(Tool::Claude), None, NOW, out)),
            concat!(
                "claude\n",
                "* (unsaved)        carol@example.com  5h 100% left (resets in 1m)\n",
                "  work             alice@example.com  5h 100% left (resets in 1m)\n",
            )
        );
        assert_eq!(
            output(|out| usage(&ctx, &http, Some(Tool::Claude), Some("work"), NOW, out)),
            "claude\n  work             alice@example.com  5h 100% left (resets in 1m)\n"
        );
    }

    #[test]
    fn usage_shows_accounts_without_subscription_limits() {
        let (_home, ctx) = setup();
        let api_key =
            json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-test-abcd", "tokens": null});
        ctx.store
            .save_profile(Tool::Codex, "apikey", &api_key)
            .unwrap();
        let http = FakeHttp::new(|_| Err(500));

        assert_eq!(
            output(|out| usage(&ctx, &http, Some(Tool::Codex), None, NOW, out)),
            "codex\n  apikey           API key (****abcd)  no subscription limits\n"
        );
        assert!(http.requests().is_empty());
    }

    #[test]
    fn usage_of_an_unknown_profile_fails() {
        let (_home, ctx) = setup();
        let http = FakeHttp::new(|_| Err(500));

        let err = usage(
            &ctx,
            &http,
            Some(Tool::Claude),
            Some("nope"),
            NOW,
            &mut Vec::new(),
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "profile 'claude/nope' not found");
    }
}
