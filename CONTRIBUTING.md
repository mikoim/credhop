# Contributing

## Running the tests

```sh
cargo test
```

The tests run on Linux and Windows. `tests/powershell.rs` completes command
lines in Windows PowerShell 5.1 and PowerShell 7 (`pwsh`) and skips any that
is not installed, except in CI (when `CI` is set), where a missing shell
fails the tests. CI runs the tests on both platforms.

## Recording the demo

The demo GIF in the README (`demo/demo.gif`) is recorded with
[VHS](https://github.com/charmbracelet/vhs) and shrunk with
[gifsicle](https://www.lcdf.org/gifsicle/). Run from the repository root:

```sh
vhs demo/demo.tape
gifsicle -b -O3 --colors 16 --lossy=40 demo/demo.gif
```

The recording runs in a throwaway home directory, and logins are simulated
with dummy credentials by `demo/fake-login.sh`, so your real credentials are
never touched.
