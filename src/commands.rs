//! The credhop subcommands.

use std::io::Write;
use std::path::PathBuf;
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::http::{Http, StatusError};
use crate::provider::{Provider, Tool, provider_for};
use crate::store::Store;
use crate::time::format_rfc3339;
use crate::usage::{Usage, Window, format_duration, format_windows};

/// Shown in place of the profile name of an account that is not saved.
const UNSAVED: &str = "(unsaved)";

/// Everything a command needs: the profile store and the home directory of the live credentials.
pub struct Ctx {
    pub store: Store,
    pub home: PathBuf,
}

/// How `list`, `status` and `usage` print what they found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Lines for people.
    Text,
    /// A JSON array with one object per account, for scripts.
    Json,
}

/// One saved profile `list` reports on.
struct Listed {
    tool: Tool,
    name: String,
    /// The label of the account, such as an email address.
    account: Option<String>,
    /// Whether the tool is logged in to this account.
    live: bool,
    login_expires_at: Option<i64>,
}

/// The account `status` reports for one tool.
struct Current {
    tool: Tool,
    logged_in: bool,
    /// The profile the account is saved in.
    profile: Option<String>,
    account: Option<String>,
}

/// One account `usage` reports on.
struct Row {
    tool: Tool,
    /// The profile name; None for the live account when it is not saved.
    name: Option<String>,
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

impl Row {
    /// The profile name, or `(unsaved)`.
    fn shown_name(&self) -> &str {
        self.name.as_deref().unwrap_or(UNSAVED)
    }
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

    /// Name of the saved profile holding the refresh token of `snapshot` for
    /// another account: the tool paired the tokens with the wrong account, for
    /// example by recording an account it fetched with replaced tokens.
    fn mispaired_profile(
        &self,
        tool: Tool,
        provider: &dyn Provider,
        snapshot: &Value,
    ) -> Result<Option<String>> {
        let Some(token) = provider.refresh_token(snapshot) else {
            return Ok(None);
        };
        let id = provider.account_id(snapshot);
        for name in self.store.list_profiles(tool)? {
            if let Some(profile) = self.store.load_profile(tool, &name)?
                && provider.refresh_token(&profile) == Some(token)
                && provider.account_id(&profile) != id
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
        if let Some(name) = self.mispaired_profile(tool, provider, &live)? {
            // Nothing is lost: the tokens are saved in that profile already.
            if !force {
                return Err(mispaired_error(tool, &name));
            }
            writeln!(out, "Discarding mismatched {tool} credentials (--force)")?;
            return Ok(None);
        }
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
                        name: Some(profile_name),
                        snapshot: live.clone(),
                        live: true,
                    }
                }
                _ => Row {
                    tool,
                    name: Some(profile_name),
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
                    name: None,
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
            // Past its time limit, a login cannot be refreshed, by the tool either.
            if let Some(expires_at) = provider.login_expires_at(snapshot)
                && expires_at <= now
            {
                let expired = format_login(expires_at, now);
                // The live credentials need no `save`: `use` writes them back.
                let save = if row.live {
                    String::new()
                } else {
                    format!(
                        " and run `credhop save {} {} --force`",
                        row.tool,
                        row.shown_name()
                    )
                };
                return Outcome::Failed(anyhow!("{expired}; log in to this account again{save}"));
            }
            // The running tool may be refreshing the live credentials at the same time.
            if row.live {
                return Outcome::Expired;
            }
            refreshed = match provider.refresh(snapshot, http, now) {
                Ok(refreshed) => refreshed,
                Err(e) => return Outcome::Failed(refresh_error(e, row.tool, row.shown_name())),
            };
            // Save at once: the old refresh token no longer works.
            if let Err(e) = self
                .store
                .save_profile(row.tool, row.shown_name(), &refreshed)
            {
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

/// `login expires in 24d11h` or `login expired 2d1h ago`, for a login that
/// reaches its time limit at `expires_at`.
fn format_login(expires_at: i64, now: i64) -> String {
    if expires_at > now {
        format!("login expires in {}", format_duration(expires_at - now))
    } else {
        format!("login expired {} ago", format_duration(now - expires_at))
    }
}

/// `2026-12-06T10:00:00Z`, or null when the time is unknown.
fn json_time(unix: Option<i64>) -> Value {
    unix.map(format_rfc3339).into()
}

fn json_window(window: &Window) -> Value {
    json!({
        "label": window.label,
        "used_percent": window.used_percent,
        "resets_at": json_time(window.resets_at),
    })
}

/// Prints `rows` as a JSON array.
fn write_json(rows: Vec<Value>, out: &mut dyn Write) -> Result<()> {
    serde_json::to_writer_pretty(&mut *out, &rows)?;
    writeln!(out)?;
    Ok(())
}

/// What `usage` shows for the live account while its access token is expired.
fn expired_token(tool: Tool) -> String {
    format!("access token expired; run {tool} to refresh it")
}

/// How to log in to `tool`. Claude Code skips the login screen once
/// onboarding is done, so it takes a command.
fn login_step(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "Start claude and run /login",
        Tool::Codex => "Run `codex login`",
    }
}

fn mispaired_error(tool: Tool, name: &str) -> anyhow::Error {
    anyhow!(
        "the current {tool} tokens belong to '{name}' but are paired with another account; \
         run `credhop use {tool} {name} --force` to restore '{name}'"
    )
}

/// A rejected refresh token (HTTP 400 or 401) means logging in again.
fn refresh_error(e: anyhow::Error, tool: Tool, name: &str) -> anyhow::Error {
    if let Some(&StatusError {
        status: status @ (400 | 401),
        ..
    }) = e.downcast_ref::<StatusError>()
    {
        return anyhow!(
            "token refresh failed (HTTP {status}); log in to this account again and run `credhop save {tool} {name} --force`"
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

pub fn list(ctx: &Ctx, now: i64, format: Format, out: &mut dyn Write) -> Result<()> {
    let mut rows = Vec::new();
    for tool in Tool::ALL {
        let provider = ctx.provider(tool);
        let live = provider.read_live()?;
        let live_id = live.as_ref().and_then(|live| provider.account_id(live));
        for name in ctx.store.list_profiles(tool)? {
            let profile = ctx.store.load_profile(tool, &name)?;
            let is_live = live_id.is_some()
                && profile.as_ref().and_then(|p| provider.account_id(p)) == live_id;
            // The live credentials hold the login of the account logged in to,
            // which is newer after logging in again.
            let snapshot = if is_live { &live } else { &profile };
            rows.push(Listed {
                tool,
                login_expires_at: snapshot
                    .as_ref()
                    .and_then(|snapshot| provider.login_expires_at(snapshot)),
                account: profile.and_then(|profile| provider.display(&profile)),
                live: is_live,
                name,
            });
        }
    }

    if format == Format::Json {
        let rows = rows.iter().map(|row| {
            json!({
                "tool": row.tool.as_str(),
                "name": row.name,
                "account": row.account,
                "live": row.live,
                "login_expires_at": json_time(row.login_expires_at),
            })
        });
        return write_json(rows.collect(), out);
    }
    for tool in Tool::ALL {
        writeln!(out, "{tool}")?;
        let rows: Vec<&Listed> = rows.iter().filter(|row| row.tool == tool).collect();
        if rows.is_empty() {
            writeln!(out, "  (no profiles)")?;
        }
        let width = rows
            .iter()
            .map(|row| row.account.as_deref().unwrap_or_default().chars().count())
            .max()
            .unwrap_or(0);
        for row in rows {
            let marker = if row.live { '*' } else { ' ' };
            let account = row.account.as_deref().unwrap_or_default();
            let login = row
                .login_expires_at
                .map(|expires_at| format_login(expires_at, now))
                .unwrap_or_default();
            let line = format!("{marker} {:<16} {account:<width$}  {login}", row.name);
            writeln!(out, "{}", line.trim_end())?;
        }
    }
    Ok(())
}

pub fn save(ctx: &Ctx, tool: Tool, name: &str, force: bool, out: &mut dyn Write) -> Result<()> {
    let _lock = ctx.store.lock()?;
    let exists = ctx.store.load_profile(tool, name)?.is_some();
    let provider = ctx.provider(tool);
    let _live_lock = provider.lock_live()?;
    let live = provider
        .read_live()?
        .with_context(|| format!("not logged in to {tool}"))?;
    if let Some(owner) = ctx.mispaired_profile(tool, provider.as_ref(), &live)? {
        return Err(mispaired_error(tool, &owner));
    }
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
    now: i64,
    out: &mut dyn Write,
) -> Result<()> {
    let _lock = ctx.store.lock()?;
    if ctx.store.load_profile(tool, name)?.is_none() {
        bail!("profile '{tool}/{name}' not found");
    }
    let provider = ctx.provider(tool);
    // Held until the switch is done, so the tool cannot rotate the tokens in between.
    let _live_lock = provider.lock_live()?;

    ctx.write_back_live(tool, provider.as_ref(), force, out)?;

    // Load after the write-back, which may have updated this very profile.
    let target = ctx
        .store
        .load_profile(tool, name)?
        .with_context(|| format!("profile '{tool}/{name}' not found"))?;
    provider.write_live(&target)?;
    // Claude Code rereads the files before each request; Codex keeps the
    // account it started with, also in the daemon that outlives its sessions.
    let apply = match tool {
        Tool::Claude => "Running claude sessions switch to it on their next request.",
        Tool::Codex => {
            "To apply, run `codex app-server daemon stop` and restart running codex sessions."
        }
    };
    writeln!(
        out,
        "Switched {tool} to '{name}'{}. {apply}",
        label(provider.as_ref(), &target)
    )?;
    // Switched all the same: logging in again from here renews this profile,
    // since the next switch writes the new login back to it.
    if let Some(expires_at) = provider.login_expires_at(&target)
        && expires_at <= now
    {
        writeln!(
            out,
            "Warning: {}. {} to log in again.",
            format_login(expires_at, now),
            login_step(tool)
        )?;
    }
    Ok(())
}

/// Logs out locally so the next login can add a new account without losing
/// the rotated tokens of the current one.
pub fn logout(ctx: &Ctx, tool: Tool, force: bool, out: &mut dyn Write) -> Result<()> {
    let _lock = ctx.store.lock()?;
    let provider = ctx.provider(tool);
    let _live_lock = provider.lock_live()?;
    if provider.read_live()?.is_none() {
        writeln!(out, "Not logged in to {tool}")?;
        return Ok(());
    }
    let owner = ctx.write_back_live(tool, provider.as_ref(), force, out)?;
    provider.clear_live()?;
    match owner {
        Some(owner) => writeln!(out, "Logged out of {tool}; credentials saved to '{owner}'")?,
        None => writeln!(out, "Logged out of {tool}")?,
    }
    writeln!(
        out,
        "{}, then run `credhop save {tool} <name>` to add a profile.",
        login_step(tool)
    )?;
    Ok(())
}

pub fn remove(ctx: &Ctx, tool: Tool, name: &str, out: &mut dyn Write) -> Result<()> {
    let _lock = ctx.store.lock()?;
    if !ctx.store.remove_profile(tool, name)? {
        bail!("profile '{tool}/{name}' not found");
    }
    writeln!(out, "Removed {tool} profile '{name}'")?;
    Ok(())
}

pub fn rename(ctx: &Ctx, tool: Tool, old: &str, new: &str, out: &mut dyn Write) -> Result<()> {
    let _lock = ctx.store.lock()?;
    if ctx.store.load_profile(tool, old)?.is_none() {
        bail!("profile '{tool}/{old}' not found");
    }
    if ctx.store.load_profile(tool, new)?.is_some() {
        bail!("profile '{tool}/{new}' already exists");
    }
    ctx.store.rename_profile(tool, old, new)?;
    writeln!(out, "Renamed {tool} profile '{old}' to '{new}'")?;
    Ok(())
}

pub fn status(ctx: &Ctx, format: Format, out: &mut dyn Write) -> Result<()> {
    let mut rows = Vec::new();
    for tool in Tool::ALL {
        let provider = ctx.provider(tool);
        let live = provider.read_live()?;
        let profile = match &live {
            Some(live) => ctx.find_owner(tool, provider.as_ref(), live)?,
            None => None,
        };
        rows.push(Current {
            tool,
            logged_in: live.is_some(),
            profile,
            account: live.and_then(|live| provider.display(&live)),
        });
    }

    if format == Format::Json {
        let rows = rows.iter().map(|row| {
            json!({
                "tool": row.tool.as_str(),
                "logged_in": row.logged_in,
                "profile": row.profile,
                "account": row.account,
            })
        });
        return write_json(rows.collect(), out);
    }
    for row in rows {
        let line = if row.logged_in {
            let profile = row.profile.as_deref().unwrap_or(UNSAVED);
            let account = row
                .account
                .map(|account| format!(" ({account})"))
                .unwrap_or_default();
            format!("{profile}{account}")
        } else {
            "not logged in".to_owned()
        };
        writeln!(out, "{:<7} {line}", row.tool)?;
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
    format: Format,
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

    let failed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Outcome::Failed(_)))
        .count();
    let accounts: Vec<Option<String>> = rows
        .iter()
        .map(|row| ctx.provider(row.tool).display(&row.snapshot))
        .collect();
    let lines = rows.iter().zip(&outcomes).zip(&accounts);

    if format == Format::Json {
        let rows = lines.map(|((row, outcome), account)| {
            let (windows, error) = match outcome {
                Outcome::Limits(usage) => (Some(usage.windows.as_slice()), None),
                Outcome::NoLimits => (Some(&[][..]), None),
                Outcome::Expired => (None, Some(expired_token(row.tool))),
                Outcome::Failed(e) => (None, Some(format!("{e:#}"))),
            };
            let windows =
                windows.map(|windows| windows.iter().map(json_window).collect::<Vec<_>>());
            let login = ctx.provider(row.tool).login_expires_at(&row.snapshot);
            json!({
                "tool": row.tool.as_str(),
                "name": row.name,
                "account": account,
                "live": row.live,
                "windows": windows,
                "login_expires_at": json_time(login),
                "error": error,
            })
        });
        write_json(rows.collect(), out)?;
    } else {
        let width = accounts
            .iter()
            .map(|account| account.as_deref().unwrap_or_default().chars().count())
            .max()
            .unwrap_or(0);
        for &tool in tools {
            writeln!(out, "{tool}")?;
            let mut empty = true;
            for ((row, outcome), account) in lines.clone() {
                if row.tool != tool {
                    continue;
                }
                empty = false;
                let detail = match outcome {
                    Outcome::Limits(usage) => format_windows(&usage.windows, now),
                    Outcome::NoLimits => "no subscription limits".to_owned(),
                    Outcome::Expired => expired_token(tool),
                    Outcome::Failed(e) => format!("error: {e:#}"),
                };
                // An error already says what to do about the account.
                let login = match outcome {
                    Outcome::Failed(_) => None,
                    _ => ctx.provider(tool).login_expires_at(&row.snapshot),
                }
                .map(|expires_at| format_login(expires_at, now))
                .unwrap_or_default();
                let marker = if row.live { '*' } else { ' ' };
                let account = account.as_deref().unwrap_or_default();
                let line = format!(
                    "{marker} {:<16} {account:<width$}  {detail}  {login}",
                    row.shown_name()
                );
                writeln!(out, "{}", line.trim_end())?;
            }
            if empty {
                writeln!(out, "  (no profiles)")?;
            }
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
        let credentials = json!({"claudeAiOauth": {
            "accessToken": token,
            "refreshToken": format!("refresh-{token}"),
        }});
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

    /// Pairs the live Claude tokens with the account of `user`, as Claude Code
    /// does when it records the profile it fetched with tokens replaced since.
    fn pair_live_claude_with(ctx: &Ctx, user: &str) {
        let path = ctx.home.join(".claude.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["oauthAccount"] =
            json!({"accountUuid": user, "emailAddress": format!("{user}@example.com")});
        fs::write(path, config.to_string()).unwrap();
    }

    /// Logs in as alice and bob, saved as `work` and `personal`, then pairs bob's tokens with alice.
    fn mispair_claude(ctx: &Ctx) {
        login_claude(ctx, "alice", "a1");
        save(ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(ctx, "bob", "b1");
        save(ctx, Tool::Claude, "personal", false, &mut Vec::new()).unwrap();
        pair_live_claude_with(ctx, "alice");
    }

    const MISPAIRED: &str = "the current claude tokens belong to 'personal' but are paired with \
        another account; run `credhop use claude personal --force` to restore 'personal'";

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

    /// Runs `f` while Claude Code refreshes the token of `user` to `token`: it
    /// holds its refresh lock from before `f` starts until the new token is written.
    fn while_claude_refreshes(ctx: &Ctx, user: &str, token: &str, f: impl FnOnce()) {
        let lock = ctx.home.join(".claude/.oauth_refresh.lock");
        fs::create_dir(&lock).unwrap();
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(std::time::Duration::from_millis(300));
                login_claude(ctx, user, token);
                fs::remove_dir(&lock).unwrap();
            });
            f();
        });
    }

    /// Runs `f` while another credhop process, such as `usage`, holds the
    /// store lock and saves `token` of alice as the profile `name` before releasing it.
    fn while_another_credhop_saves(ctx: &Ctx, name: &str, token: &str, f: impl FnOnce()) {
        let lock = ctx.store.lock().unwrap();
        thread::scope(|scope| {
            scope.spawn(move || {
                thread::sleep(std::time::Duration::from_millis(300));
                save_claude_profile(ctx, name, "alice", token, NOW);
                drop(lock);
            });
            f();
        });
    }

    fn output(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> String {
        let mut out = Vec::new();
        f(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn save_stores_profile() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");

        let out = output(|out| save(&ctx, Tool::Claude, "work", false, out));

        assert_eq!(out, "Saved claude profile 'work' (alice@example.com)\n");
        assert_eq!(profile_token(&ctx, "work"), "a1");
    }

    #[test]
    fn save_waits_for_claude_code_to_finish_refreshing() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");

        while_claude_refreshes(&ctx, "alice", "a2", || {
            save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        });

        assert_eq!(profile_token(&ctx, "work"), "a2");
    }

    #[test]
    fn save_refuses_tokens_paired_with_another_account() {
        let (_home, ctx) = setup();
        mispair_claude(&ctx);

        let err = save(&ctx, Tool::Claude, "work", true, &mut Vec::new()).unwrap_err();

        assert_eq!(err.to_string(), MISPAIRED);
        assert_eq!(profile_token(&ctx, "work"), "a1");
    }

    #[test]
    fn save_waits_for_another_credhop() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "alice", "a2");

        while_another_credhop_saves(&ctx, "work", "z", || {
            save(&ctx, Tool::Claude, "work", true, &mut Vec::new()).unwrap();
        });

        assert_eq!(profile_token(&ctx, "work"), "a2");
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

        let out = output(|out| use_profile(&ctx, Tool::Claude, "work", false, NOW, out));

        assert_eq!(
            out,
            "Switched claude to 'work' (alice@example.com). \
             Running claude sessions switch to it on their next request.\n"
        );
        assert_eq!(profile_token(&ctx, "personal"), "b2");
        assert_eq!(live_token(&ctx), "a1");
    }

    #[test]
    fn use_waits_for_claude_code_to_finish_refreshing() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "bob", "b1");
        save(&ctx, Tool::Claude, "personal", false, &mut Vec::new()).unwrap();

        while_claude_refreshes(&ctx, "bob", "b2", || {
            use_profile(&ctx, Tool::Claude, "work", false, NOW, &mut Vec::new()).unwrap();
        });

        assert_eq!(profile_token(&ctx, "personal"), "b2");
        assert_eq!(live_token(&ctx), "a1");
    }

    #[test]
    fn use_tells_to_restart_codex_and_its_daemon() {
        let (_home, ctx) = setup();
        fs::create_dir(ctx.home.join(".codex")).unwrap();
        let auth = json!({"OPENAI_API_KEY": "sk-test-abcd"});
        fs::write(ctx.home.join(".codex/auth.json"), auth.to_string()).unwrap();
        save(&ctx, Tool::Codex, "api", false, &mut Vec::new()).unwrap();

        let out = output(|out| use_profile(&ctx, Tool::Codex, "api", false, NOW, out));

        assert_eq!(
            out,
            "Switched codex to 'api' (API key (****abcd)). To apply, run \
             `codex app-server daemon stop` and restart running codex sessions.\n"
        );
    }

    #[test]
    fn use_waits_for_another_credhop() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "bob", "b1");
        save(&ctx, Tool::Claude, "personal", false, &mut Vec::new()).unwrap();

        while_another_credhop_saves(&ctx, "work", "a2", || {
            use_profile(&ctx, Tool::Claude, "work", false, NOW, &mut Vec::new()).unwrap();
        });

        assert_eq!(live_token(&ctx), "a2");
    }

    #[test]
    fn use_current_profile_keeps_rotated_tokens() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "alice", "a2");

        use_profile(&ctx, Tool::Claude, "work", false, NOW, &mut Vec::new()).unwrap();

        assert_eq!(live_token(&ctx), "a2");
    }

    #[test]
    fn use_refuses_to_discard_unsaved_account_without_force() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "carol", "c1");

        let err = use_profile(&ctx, Tool::Claude, "work", false, NOW, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("not saved in any profile"));
        assert_eq!(live_token(&ctx), "c1");

        use_profile(&ctx, Tool::Claude, "work", true, NOW, &mut Vec::new()).unwrap();
        assert_eq!(live_token(&ctx), "a1");
    }

    #[test]
    fn use_refuses_tokens_paired_with_another_account() {
        let (_home, ctx) = setup();
        mispair_claude(&ctx);

        let err = use_profile(&ctx, Tool::Claude, "work", false, NOW, &mut Vec::new()).unwrap_err();

        assert_eq!(err.to_string(), MISPAIRED);
        assert_eq!(profile_token(&ctx, "work"), "a1");
        assert_eq!(live_token(&ctx), "b1");
    }

    #[test]
    fn use_force_restores_tokens_paired_with_another_account() {
        let (_home, ctx) = setup();
        mispair_claude(&ctx);

        let out = output(|out| use_profile(&ctx, Tool::Claude, "personal", true, NOW, out));

        assert!(out.starts_with(
            "Discarding mismatched claude credentials (--force)\n\
             Switched claude to 'personal' (bob@example.com)"
        ));
        assert_eq!(profile_token(&ctx, "work"), "a1");
        assert_eq!(live_token(&ctx), "b1");
        assert!(
            output(|out| status(&ctx, Format::Text, out))
                .starts_with("claude  personal (bob@example.com)\n")
        );
    }

    #[test]
    fn use_switches_when_not_logged_in() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        fs::remove_file(ctx.home.join(".claude/.credentials.json")).unwrap();

        use_profile(&ctx, Tool::Claude, "work", false, NOW, &mut Vec::new()).unwrap();

        assert_eq!(live_token(&ctx), "a1");
    }

    #[test]
    fn use_treats_unidentifiable_login_as_unsaved() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "alice", "a2");
        fs::write(ctx.home.join(".claude.json"), "{}").unwrap();

        let err = use_profile(&ctx, Tool::Claude, "work", false, NOW, &mut Vec::new()).unwrap_err();

        assert!(err.to_string().contains("not saved in any profile"));
        assert_eq!(live_token(&ctx), "a2");
    }

    #[test]
    fn use_warns_when_the_login_of_the_profile_has_expired() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW + 3600);
        end_profile_login(&ctx, "work", NOW - 2 * 86_400 - 3600);

        let out = output(|out| use_profile(&ctx, Tool::Claude, "work", false, NOW, out));

        assert_eq!(
            out,
            concat!(
                "Switched claude to 'work' (alice@example.com). ",
                "Running claude sessions switch to it on their next request.\n",
                "Warning: login expired 2d1h ago. Start claude and run /login to log in again.\n",
            )
        );
        assert_eq!(live_token(&ctx), "a1"); // Switched all the same, to log in again from there.
    }

    #[test]
    fn use_does_not_warn_before_the_login_expires() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW + 3600);
        end_profile_login(&ctx, "work", NOW + 60);

        let out = output(|out| use_profile(&ctx, Tool::Claude, "work", false, NOW, out));

        assert_eq!(
            out,
            "Switched claude to 'work' (alice@example.com). Running claude sessions switch to it on their next request.\n"
        );
    }

    #[test]
    fn use_unknown_profile_fails() {
        let (_home, ctx) = setup();
        let err = use_profile(&ctx, Tool::Claude, "nope", false, NOW, &mut Vec::new()).unwrap_err();
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
    }

    #[test]
    fn logout_waits_for_claude_code_to_finish_refreshing() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();

        while_claude_refreshes(&ctx, "alice", "a2", || {
            logout(&ctx, Tool::Claude, false, &mut Vec::new()).unwrap();
        });

        assert_eq!(profile_token(&ctx, "work"), "a2");
    }

    #[test]
    fn logout_waits_for_another_credhop() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "alice", "a2");

        while_another_credhop_saves(&ctx, "work", "z", || {
            logout(&ctx, Tool::Claude, false, &mut Vec::new()).unwrap();
        });

        assert_eq!(profile_token(&ctx, "work"), "a2");
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
    fn remove_deletes_profile() {
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
        assert!(remove(&ctx, Tool::Claude, "work", &mut Vec::new()).is_err());
    }

    #[test]
    fn remove_waits_for_another_credhop() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();

        while_another_credhop_saves(&ctx, "work", "z", || {
            remove(&ctx, Tool::Claude, "work", &mut Vec::new()).unwrap();
        });

        assert!(
            ctx.store
                .load_profile(Tool::Claude, "work")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rename_moves_the_profile_to_the_new_name() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW);

        let out = output(|out| rename(&ctx, Tool::Claude, "work", "main", out));

        assert_eq!(out, "Renamed claude profile 'work' to 'main'\n");
        assert_eq!(ctx.store.list_profiles(Tool::Claude).unwrap(), ["main"]);
        assert_eq!(profile_token(&ctx, "main"), "a1");
    }

    #[test]
    fn rename_keeps_both_profiles_when_the_new_name_is_taken() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW);
        save_claude_profile(&ctx, "team", "bob", "b1", NOW);

        let err = rename(&ctx, Tool::Claude, "work", "team", &mut Vec::new()).unwrap_err();

        assert_eq!(err.to_string(), "profile 'claude/team' already exists");
        assert_eq!(profile_token(&ctx, "work"), "a1");
        assert_eq!(profile_token(&ctx, "team"), "b1");
    }

    #[test]
    fn rename_fails_for_an_unknown_profile_or_an_invalid_name() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW);

        let err = rename(&ctx, Tool::Claude, "nope", "main", &mut Vec::new()).unwrap_err();
        assert_eq!(err.to_string(), "profile 'claude/nope' not found");
        // Profiles are per tool: the Claude one is not a Codex one.
        assert!(rename(&ctx, Tool::Codex, "work", "main", &mut Vec::new()).is_err());
        assert!(rename(&ctx, Tool::Claude, "work", "../x", &mut Vec::new()).is_err());
        assert_eq!(ctx.store.list_profiles(Tool::Claude).unwrap(), ["work"]);
    }

    #[test]
    fn rename_waits_for_another_credhop() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW);

        while_another_credhop_saves(&ctx, "work", "a2", || {
            rename(&ctx, Tool::Claude, "work", "main", &mut Vec::new()).unwrap();
        });

        // The tokens the other process refreshed moved along.
        assert_eq!(profile_token(&ctx, "main"), "a2");
    }

    #[test]
    fn list_and_status_show_profiles() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "bob", "b1");
        save(&ctx, Tool::Claude, "personal", false, &mut Vec::new()).unwrap();

        assert_eq!(
            output(|out| list(&ctx, NOW, Format::Text, out)),
            "claude\n* personal         bob@example.com\n  work             alice@example.com\ncodex\n  (no profiles)\n",
        );
        assert_eq!(
            output(|out| status(&ctx, Format::Text, out)),
            "claude  personal (bob@example.com)\ncodex   not logged in\n",
        );

        login_claude(&ctx, "carol", "c1");
        assert!(
            output(|out| status(&ctx, Format::Text, out))
                .starts_with("claude  (unsaved) (carol@example.com)\n")
        );
    }

    #[test]
    fn list_marks_the_account_logged_in_to() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "bob", "b1");
        save(&ctx, Tool::Claude, "personal", false, &mut Vec::new()).unwrap();
        login_claude(&ctx, "alice", "a2"); // Logged in again without credhop.

        assert_eq!(
            output(|out| list(&ctx, NOW, Format::Text, out)),
            "claude\n  personal         bob@example.com\n* work             alice@example.com\ncodex\n  (no profiles)\n",
        );

        login_claude(&ctx, "carol", "c1");
        assert!(!output(|out| list(&ctx, NOW, Format::Text, out)).contains('*'));
    }

    #[test]
    fn list_shows_when_each_login_expires() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW);
        save_claude_profile(&ctx, "team", "bob", "b1", NOW);
        save_claude_profile(&ctx, "old", "carol", "c1", NOW);
        end_profile_login(&ctx, "work", NOW + 24 * 86_400 + 11 * 3600);
        end_profile_login(&ctx, "old", NOW - 2 * 86_400 - 3600);

        assert_eq!(
            output(|out| list(&ctx, NOW, Format::Text, out)),
            concat!(
                "claude\n",
                "  old              carol@example.com  login expired 2d1h ago\n",
                "  team             bob@example.com\n",
                "  work             alice@example.com  login expires in 24d11h\n",
                "codex\n",
                "  (no profiles)\n",
            )
        );
    }

    #[test]
    fn list_reads_the_login_of_the_live_account_from_the_live_credentials() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        end_profile_login(&ctx, "work", NOW - 10);
        set_live_claude_time(&ctx, "refreshTokenExpiresAt", NOW + 3 * 86_400); // Logged in again.

        assert_eq!(
            output(|out| list(&ctx, NOW, Format::Text, out)),
            "claude\n* work             alice@example.com  login expires in 3d0h\ncodex\n  (no profiles)\n"
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
        set_live_claude_time(ctx, "expiresAt", expires_at);
    }

    /// Sets the time `field` of the live Claude credentials to `at`.
    fn set_live_claude_time(ctx: &Ctx, field: &str, at: i64) {
        let path = ctx.home.join(".claude/.credentials.json");
        let mut credentials: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        credentials["claudeAiOauth"][field] = json!(at * 1000);
        fs::write(path, credentials.to_string()).unwrap();
    }

    /// Makes the login of the saved Claude profile `name` reach its time limit at `expires_at`.
    fn end_profile_login(ctx: &Ctx, name: &str, expires_at: i64) {
        let mut profile = ctx.store.load_profile(Tool::Claude, name).unwrap().unwrap();
        profile["credentials"]["claudeAiOauth"]["refreshTokenExpiresAt"] = json!(expires_at * 1000);
        ctx.store
            .save_profile(Tool::Claude, name, &profile)
            .unwrap();
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

        let out = output(|out| usage(&ctx, &http, None, None, NOW, Format::Text, out));

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

        let err = usage(
            &ctx,
            &http,
            Some(Tool::Claude),
            None,
            NOW,
            Format::Text,
            &mut out,
        )
        .unwrap_err();

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

    #[cfg(unix)]
    #[test]
    fn usage_reports_tokens_that_were_refreshed_but_could_not_be_saved() {
        use std::os::unix::fs::PermissionsExt;
        struct Restore(std::path::PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW - 10);
        let profiles = ctx.home.join("data/profiles/claude");
        fs::set_permissions(&profiles, fs::Permissions::from_mode(0o555)).unwrap();
        let _restore = Restore(profiles);
        let http = FakeHttp::new(|request| match (request.method, request.url.as_str()) {
            ("POST", CLAUDE_TOKEN) => Ok(json!({
                "access_token": "a2",
                "refresh_token": "refresh-a2",
                "expires_in": 28_800,
            })),
            _ => Err(500),
        });
        let mut out = Vec::new();

        let err = usage(
            &ctx,
            &http,
            Some(Tool::Claude),
            None,
            NOW,
            Format::Text,
            &mut out,
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "could not get the usage of 1 account(s)");
        let out = String::from_utf8(out).unwrap();
        let detail = out
            .strip_prefix("claude\n  work             alice@example.com  error: ")
            .unwrap();
        assert!(
            detail.starts_with("tokens were refreshed but saving the profile failed: "),
            "{detail}"
        );
        assert!(detail.ends_with("; log in again\n"), "{detail}");
        assert_eq!(http.requests().len(), 1); // no usage request
    }

    #[test]
    fn usage_never_refreshes_the_live_credentials() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        expire_live_claude(&ctx, NOW - 10);
        let http = FakeHttp::new(|_| Err(500));

        let out = output(|out| {
            usage(
                &ctx,
                &http,
                Some(Tool::Claude),
                None,
                NOW,
                Format::Text,
                out,
            )
        });

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

        let err = usage(
            &ctx,
            &http,
            Some(Tool::Claude),
            None,
            NOW,
            Format::Text,
            &mut out,
        )
        .unwrap_err();

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

        usage(
            &ctx,
            &http,
            Some(Tool::Claude),
            None,
            NOW,
            Format::Text,
            &mut out,
        )
        .unwrap_err();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "claude\n  work             alice@example.com  error: token refresh failed (HTTP 400); log in to this account again and run `credhop save claude work --force`\n"
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
            output(|out| usage(
                &ctx,
                &http,
                Some(Tool::Claude),
                None,
                NOW,
                Format::Text,
                out
            )),
            concat!(
                "claude\n",
                "* (unsaved)        carol@example.com  5h 100% left (resets in 1m)\n",
                "  work             alice@example.com  5h 100% left (resets in 1m)\n",
            )
        );
        assert_eq!(
            output(|out| usage(
                &ctx,
                &http,
                Some(Tool::Claude),
                Some("work"),
                NOW,
                Format::Text,
                out
            )),
            "claude\n  work             alice@example.com  5h 100% left (resets in 1m)\n"
        );
    }

    #[test]
    fn usage_shows_when_each_login_expires() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW + 3600);
        save_claude_profile(&ctx, "team", "bob", "b1", NOW + 3600);
        save_claude_profile(&ctx, "old", "carol", "c1", NOW + 3600);
        end_profile_login(&ctx, "work", NOW + 24 * 86_400 + 11 * 3600);
        end_profile_login(&ctx, "old", NOW - 3600); // The access token outlives the login.
        let http = FakeHttp::new(|_| Ok(session_usage(50, 60)));

        assert_eq!(
            output(|out| usage(
                &ctx,
                &http,
                Some(Tool::Claude),
                None,
                NOW,
                Format::Text,
                out
            )),
            concat!(
                "claude\n",
                "  old              carol@example.com  5h 50% left (resets in 1m)  login expired 1h0m ago\n",
                "  team             bob@example.com    5h 50% left (resets in 1m)\n",
                "  work             alice@example.com  5h 50% left (resets in 1m)  login expires in 24d11h\n",
            )
        );
    }

    #[test]
    fn usage_does_not_refresh_a_login_past_its_time_limit() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW - 10);
        end_profile_login(&ctx, "work", NOW - 2 * 86_400 - 3600);
        let http = FakeHttp::new(|_| Err(500));
        let mut out = Vec::new();

        let err = usage(
            &ctx,
            &http,
            Some(Tool::Claude),
            None,
            NOW,
            Format::Text,
            &mut out,
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "could not get the usage of 1 account(s)");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "claude\n  work             alice@example.com  error: login expired 2d1h ago; log in to this account again and run `credhop save claude work --force`\n"
        );
        assert!(http.requests().is_empty());
    }

    #[test]
    fn usage_asks_to_log_in_again_when_the_live_login_is_past_its_time_limit() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        expire_live_claude(&ctx, NOW - 10);
        set_live_claude_time(&ctx, "refreshTokenExpiresAt", NOW - 3600);
        let http = FakeHttp::new(|_| Err(500));
        let mut out = Vec::new();

        let err = usage(
            &ctx,
            &http,
            Some(Tool::Claude),
            None,
            NOW,
            Format::Text,
            &mut out,
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "could not get the usage of 1 account(s)");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "claude\n* work             alice@example.com  error: login expired 1h0m ago; log in to this account again\n"
        );
        assert!(http.requests().is_empty());
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
            output(|out| usage(&ctx, &http, Some(Tool::Codex), None, NOW, Format::Text, out)),
            "codex\n  apikey           API key (****abcd)  no subscription limits\n"
        );
        assert!(http.requests().is_empty());
    }

    /// Parses what a command printed with `Format::Json`.
    fn json_output(out: Vec<u8>) -> Value {
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn list_prints_each_profile_as_json() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "team", "bob", "b1", NOW);
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        set_live_claude_time(&ctx, "refreshTokenExpiresAt", NOW + 86_400);
        let mut out = Vec::new();

        list(&ctx, NOW, Format::Json, &mut out).unwrap();

        assert_eq!(
            json_output(out),
            json!([
                {
                    "tool": "claude",
                    "name": "team",
                    "account": "bob@example.com",
                    "live": false,
                    "login_expires_at": null,
                },
                {
                    "tool": "claude",
                    "name": "work",
                    "account": "alice@example.com",
                    "live": true,
                    "login_expires_at": "2026-10-18T01:20:00Z",
                },
            ])
        );
    }

    #[test]
    fn list_prints_an_empty_json_array_without_profiles() {
        let (_home, ctx) = setup();
        let mut out = Vec::new();

        list(&ctx, NOW, Format::Json, &mut out).unwrap();

        assert_eq!(json_output(out), json!([]));
    }

    #[test]
    fn status_prints_every_tool_as_json() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "alice", "a1");
        save(&ctx, Tool::Claude, "work", false, &mut Vec::new()).unwrap();
        let mut out = Vec::new();

        status(&ctx, Format::Json, &mut out).unwrap();

        assert_eq!(
            json_output(out),
            json!([
                {
                    "tool": "claude",
                    "logged_in": true,
                    "profile": "work",
                    "account": "alice@example.com",
                },
                {"tool": "codex", "logged_in": false, "profile": null, "account": null},
            ])
        );
    }

    #[test]
    fn status_json_names_no_profile_for_an_unsaved_account() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "carol", "c1");
        let mut out = Vec::new();

        status(&ctx, Format::Json, &mut out).unwrap();

        assert_eq!(
            json_output(out)[0],
            json!({
                "tool": "claude",
                "logged_in": true,
                "profile": null,
                "account": "carol@example.com",
            })
        );
    }

    #[test]
    fn usage_prints_limits_failures_and_accounts_without_limits_as_json() {
        let (_home, ctx) = setup();
        save_claude_profile(&ctx, "work", "alice", "a1", NOW + 3600);
        end_profile_login(&ctx, "work", NOW + 86_400);
        save_claude_profile(&ctx, "team", "bob", "b1", NOW + 3600);
        let api_key =
            json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-test-abcd", "tokens": null});
        ctx.store
            .save_profile(Tool::Codex, "apikey", &api_key)
            .unwrap();
        let http = FakeHttp::new(|request| match request.header("Authorization") {
            Some("Bearer a1") => Ok(session_usage(23, 7980)),
            _ => Err(401),
        });
        let mut out = Vec::new();

        let err = usage(&ctx, &http, None, None, NOW, Format::Json, &mut out).unwrap_err();

        assert_eq!(err.to_string(), "could not get the usage of 1 account(s)");
        assert_eq!(
            json_output(out),
            json!([
                {
                    "tool": "claude",
                    "name": "team",
                    "account": "bob@example.com",
                    "live": false,
                    "windows": null,
                    "login_expires_at": null,
                    "error": "unauthorized (HTTP 401 from api.anthropic.com); log in to this account again",
                },
                {
                    "tool": "claude",
                    "name": "work",
                    "account": "alice@example.com",
                    "live": false,
                    "windows": [
                        {"label": "5h", "used_percent": 23.0, "resets_at": "2026-10-17T03:33:00Z"},
                    ],
                    "login_expires_at": "2026-10-18T01:20:00Z",
                    "error": null,
                },
                {
                    "tool": "codex",
                    "name": "apikey",
                    "account": "API key (****abcd)",
                    "live": false,
                    "windows": [],
                    "login_expires_at": null,
                    "error": null,
                },
            ])
        );
    }

    #[test]
    fn usage_json_names_no_profile_for_the_unsaved_account() {
        let (_home, ctx) = setup();
        login_claude(&ctx, "carol", "c1");
        expire_live_claude(&ctx, NOW - 10);
        let http = FakeHttp::new(|_| Err(500));
        let mut out = Vec::new();

        // Not a failure: the tool refreshes the token the next time it runs.
        usage(
            &ctx,
            &http,
            Some(Tool::Claude),
            None,
            NOW,
            Format::Json,
            &mut out,
        )
        .unwrap();

        assert_eq!(
            json_output(out),
            json!([{
                "tool": "claude",
                "name": null,
                "account": "carol@example.com",
                "live": true,
                "windows": null,
                "login_expires_at": null,
                "error": "access token expired; run claude to refresh it",
            }])
        );
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
            Format::Text,
            &mut Vec::new(),
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "profile 'claude/nope' not found");
    }
}
