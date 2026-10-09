# credhop

Switch the login credentials of [Claude Code](https://claude.com/claude-code) and
[Codex](https://github.com/openai/codex) between saved profiles.

Linux only for now.

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

## How it works

| Tool | Live credentials |
|---|---|
| Claude Code | `~/.claude/.credentials.json` and the `oauthAccount` key of `~/.claude.json` |
| Codex | `~/.codex/auth.json` |

Profiles are stored in `$XDG_DATA_HOME/credhop` (default `~/.local/share/credhop`)
with the same `0600` permissions as the original files. They are not encrypted.

OAuth tokens are refreshed while a tool runs. Before switching, `credhop use`
writes the current live credentials back to the profile of the same account,
so saved profiles stay valid. If the live credentials belong to an account
that is not saved, `use` stops; save it first or pass `--force` to discard it.

## Caveats

- Quit Claude Code and Codex before switching. Running sessions keep the old
  credentials, and Claude Code may overwrite `~/.claude.json` while it runs.
- `CLAUDE_CONFIG_DIR` and `CODEX_HOME` are not supported.
- credhop replaces files atomically, so a symlinked `~/.claude.json`,
  `~/.claude/.credentials.json` or `~/.codex/auth.json` becomes a regular file.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
