# tessmux
Terminal grid for coordinating concurrent agent sessions. Native Rust, no wrappers.

## Status

Experimental — built bottom-up as a ladder of PoC milestones living under
`poc/`. Current milestone: **PoC 0 — PTY wiring** (`poc0-pty`): an interactive
ConPTY/PTY passthrough proving the lowest layer — spawn a shell, forward raw
I/O, propagate console resizes, kill the child cleanly.

## Run

```sh
cargo run -p poc0-pty             # interactive pwsh in a ConPTY session
cargo run -p poc0-pty -- cmd      # or any other program
cargo run -p poc0-pty -- --help   # usage and key bindings
```

Run it from a real terminal (not a pipe or an IDE debug console).
Ctrl+C is forwarded to the child; **Ctrl+]** kills it.

## License

MIT — see [LICENSE](LICENSE).
