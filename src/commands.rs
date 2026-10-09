//! The credhop subcommands.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::provider::{Provider, Tool, provider_for};
use crate::store::Store;

/// Everything a command needs: the profile store and the home directory of the live credentials.
pub struct Ctx {
    pub store: Store,
    pub home: PathBuf,
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
}

/// `" (alice@example.com)"`, or an empty string when there is no label.
fn label(provider: &dyn Provider, snapshot: &Value) -> String {
    provider
        .display(snapshot)
        .map(|display| format!(" ({display})"))
        .unwrap_or_default()
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

    // Tokens rotate while a tool runs: save the live ones before replacing them.
    if let Some(live) = provider.read_live()? {
        match ctx.find_owner(tool, provider.as_ref(), &live)? {
            Some(owner) => ctx.store.save_profile(tool, &owner, &live)?,
            None if force => writeln!(out, "Discarding unsaved {tool} credentials (--force)")?,
            None => bail!(
                "current {tool} credentials are not saved in any profile; \
                 run `credhop save {tool} <name>` first or pass --force"
            ),
        }
    }

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
    fn use_unknown_profile_fails() {
        let (_home, ctx) = setup();
        let err = use_profile(&ctx, Tool::Claude, "nope", false, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("profile 'claude/nope' not found"));
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
}
