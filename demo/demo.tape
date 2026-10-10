# Records demo/demo.gif. Run from the repository root:
#
#   vhs demo/demo.tape
#   gifsicle -b -O3 --colors 16 --lossy=40 demo/demo.gif
#
# The recording uses a throwaway HOME, so real credentials are never touched.
# Logins are simulated with dummy credentials by demo/fake-login.sh.

Output demo/demo.gif

Require cargo

Set Shell bash
Set FontSize 18
Set Width 1100
Set Height 480
Set Padding 20
Set TypingSpeed 40ms
Set Theme "Catppuccin Mocha"

Hide
Type `export PS1='\[\e[1;35m\]$\[\e[0m\] '` Enter
Type `export CARGO_TARGET_DIR="$PWD/target" DEMO="$PWD/demo"` Enter
Type `cargo build --release --locked --quiet` Enter
Wait@5m /\$$/
Type `export DEMO_HOME="$(mktemp -d)" && export CARGO_INSTALL_ROOT="$DEMO_HOME/.local" PATH="$DEMO_HOME/.local/bin:$PATH"` Enter
Type "clear" Enter
Show

Type "# Install" Sleep 300ms Enter
Type "cargo install --path ." Sleep 500ms Enter
Wait+Screen@5m /Installed package/
Sleep 1.5s
Hide
Type `export HOME="$DEMO_HOME"` Enter
Show
Type "clear" Enter

Type "# Enable shell completion (add this to ~/.bashrc)" Sleep 300ms Enter
Type "source <(credhop completions bash)" Sleep 500ms Enter
Sleep 1s

Type "# Log in to Claude Code as usual, then save it as a profile" Sleep 300ms Enter
Hide
Type `"$DEMO/fake-login.sh" claude alice` Enter
Show
Type "credhop save claude work" Sleep 500ms Enter
Sleep 1s

Type "# To add another account, log out with credhop first" Sleep 300ms Enter
Type "credhop logout claude" Sleep 500ms Enter
Sleep 2.5s

Type "# Run /login in Claude Code as another account, then save it" Sleep 300ms Enter
Hide
Type `"$DEMO/fake-login.sh" claude bob` Enter
Show
Type "credhop save claude personal" Sleep 500ms Enter
Sleep 1.5s
Type "clear" Enter

Type "# Codex works the same way" Sleep 300ms Enter
Hide
Type `"$DEMO/fake-login.sh" codex alice` Enter
Show
Type "credhop save codex work" Sleep 500ms Enter
Sleep 1s
Type "credhop list" Sleep 500ms Enter
Sleep 2s

Type "# Tab completes commands, tools and profile names" Sleep 300ms Enter
Type "credhop u" Sleep 300ms Tab Sleep 500ms
Type "cl" Sleep 300ms Tab Sleep 500ms
Tab@300ms 2
Sleep 1.5s
Type "w" Sleep 300ms Tab Sleep 500ms Enter
Sleep 2s
Type "credhop status" Sleep 500ms Enter
Sleep 2.5s
Type "clear" Enter

Type "# Remove a profile you no longer need" Sleep 300ms Enter
Type "credhop remove codex work" Sleep 500ms Enter
Sleep 1s
Type "credhop list" Sleep 500ms Enter
Sleep 3s

Hide
Type `rm -rf "$DEMO_HOME"` Enter
