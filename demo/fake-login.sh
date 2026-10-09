#!/bin/sh
# Writes dummy credentials under $HOME, as if `claude` or `codex login` had
# just been run. Used only by demo.tape; never run it on a real home directory.
#
# Usage: fake-login.sh claude|codex <user>
set -eu

tool=$1
user=$2
email="$user@example.com"

case $tool in
claude)
	mkdir -p "$HOME/.claude"
	printf '{"claudeAiOauth":{"accessToken":"demo-%s"}}\n' "$user" >"$HOME/.claude/.credentials.json"
	printf '{"oauthAccount":{"accountUuid":"%s","organizationUuid":"demo","emailAddress":"%s"}}\n' \
		"$user" "$email" >"$HOME/.claude.json"
	;;
codex)
	payload=$(printf '{"sub":"%s","email":"%s"}' "$user" "$email" | base64 -w0 | tr '+/' '-_' | tr -d '=')
	mkdir -p "$HOME/.codex"
	printf '{"tokens":{"account_id":"demo","id_token":"e30.%s.sig"}}\n' "$payload" >"$HOME/.codex/auth.json"
	;;
esac
chmod 600 "$HOME"/.claude/.credentials.json "$HOME"/.codex/auth.json 2>/dev/null || true

# Erase the command line that ran this script, so it does not show in the recording.
printf '\033[1A\033[2K'
