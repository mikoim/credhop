# credhop

Switch the login credentials of [Claude Code](https://claude.com/claude-code) and
[Codex](https://github.com/openai/codex) between saved profiles.

Works on Linux and Windows.

![credhop demo](demo/demo.gif)

## Install

```sh
cargo install --path .
```

## Usage

Log in with the tool as usual, then save the credentials as a profile:

```sh
claude          # log in as your work account
credhop save claude work

codex login     # log in as your personal account
credhop save codex personal
```

Switch, list and inspect:

```sh
credhop list                 # list saved profiles (* = active)
credhop use claude work      # switch Claude Code to "work"
credhop use codex personal   # switch Codex to "personal"
credhop status               # show the current account of each tool
credhop remove codex personal
```

Profile names may contain letters, digits, `_` and `-`.

Check how much of the subscription usage limits is left, for every account
or for one:

```sh
credhop usage                # every account of every tool
credhop usage claude         # every Claude Code account
credhop usage claude work    # one account
```

```
claude
* work             alice@example.com  5h 77% left (resets in 2h13m)  7d 59% left (resets in 3d4h)
  personal         bob@example.com    5h 100% left  7d 88% left (resets in 5d1h)
codex
* personal         carol@example.com  5h 90% left (resets in 4h2m)  7d 45% left (resets in 1d20h)
```

`*` marks the account you are logged in to.

To add another account, log out with credhop first. It writes the current
tokens back to their profile, then deletes the live credentials so you can
log in to another account:

```sh
credhop logout claude        # current tokens are saved to "work"
claude                       # run /login as another account
credhop save claude team
```

`credhop logout` only deletes local files; it does not revoke the tokens.

## Shell completion

Commands, tools and saved profile names are completed in bash, fish and PowerShell.

```sh
# bash: add to ~/.bashrc
source <(credhop completions bash)

# fish
credhop completions fish > ~/.config/fish/completions/credhop.fish
```

```powershell
# PowerShell: add to $PROFILE
credhop completions powershell | Out-String | Invoke-Expression
```

PowerShell completion is supported on the latest PowerShell 7. It also works
on Windows PowerShell 5.1 and older PowerShell 7 releases, with known minor
issues: for example, a quoted empty argument such as `''` can shift the
position being completed.

## How it works

| Tool | Live credentials |
|---|---|
| Claude Code | `~/.claude/.credentials.json` and the `oauthAccount` key of `~/.claude.json` |
| Codex | `~/.codex/auth.json` |

On Windows, `~` is `%USERPROFILE%`.

Profiles are stored in a private directory. They are not encrypted.

| OS | Profiles | Protection |
|---|---|---|
| Linux | `$XDG_DATA_HOME/credhop` (default `~/.local/share/credhop`) | `0600` permissions, like the original files |
| Windows | `%LOCALAPPDATA%\credhop` | the ACL inherited from your user profile, like the original files |

OAuth tokens are refreshed while a tool runs. Before switching, `credhop use`
writes the current live credentials back to the profile of the same account,
so saved profiles stay valid. If the live credentials belong to an account
that is not saved, `use` stops; save it first or pass `--force` to discard it.

`credhop usage` asks the servers the tools use: `api.anthropic.com` and
`platform.claude.com` for Claude Code, `chatgpt.com` and `auth.openai.com`
for Codex. When the access token of a saved profile has expired, it
refreshes the tokens and saves them back to the profile, as the tools do.
It never refreshes the live credentials of the account you are logged in
to; if they have expired, start the tool once.

## Caveats

- Quit Claude Code and Codex before switching. Running sessions keep the old
  credentials, and Claude Code may overwrite `~/.claude.json` while it runs.
  On Windows, switching fails while a running tool has a credential file open.
- `CLAUDE_CONFIG_DIR` and `CODEX_HOME` are not supported.
- For Codex, only the default `cli_auth_credentials_store = "file"` is
  supported; credentials kept in the OS keyring are not found.
- credhop replaces files atomically, so a symlinked `~/.claude.json`,
  `~/.claude/.credentials.json` or `~/.codex/auth.json` becomes a regular file.
- The endpoints `credhop usage` calls are undocumented and may change
  without notice.
- Do not run `credhop usage` at the same time as `use`, `save` or `logout`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
