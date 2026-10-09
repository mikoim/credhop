# Contributing

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
