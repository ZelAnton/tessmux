# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Add entries to `[Unreleased]` as you work — manual bullets always win over the
git-cliff auto-fill (config: `cliff.toml`). On release, promote `[Unreleased]`
to a dated version section.

## [Unreleased]

### Added
- Cargo workspace skeleton (from rust-repo-template) with the `poc0-pty`
  milestone placeholder.
- PoC 0: interactive ConPTY session via `portable-pty` behind the `PtyBackend`
  trait boundary — raw I/O forwarding, console resize propagation, Ctrl+] clean
  kill, `--help`/`--version`, runtime key-binding hint.
- `tessmux-pty` layer crate (`crates/pty`): the L0 boundary extracted from the
  PoC binary, with `close()` (guaranteed reader EOF), non-blocking `try_wait()`,
  a typed `PtyError`, the shared `pump_reader` helper, and a scripted
  `FakePtyBackend` (feature `testing`) proven equivalent by dual-backend
  contract tests.

### Changed
- Release workflow is hard-disabled by a guard step while the workspace has no
  publishable crates (PoC phase).
- `poc0-pty` teardown is deterministic (`wait` → `close` → `join`): the 150 ms
  drain-grace timer is gone, exits are immediate.
- `PtySession::writer()` renamed to `take_writer()` (honest move-out
  semantics).

### Fixed
- Windows kill results are no longer inverted: a compensating shim flips the
  `TerminateProcess` BOOL that portable-pty (≤ 0.9) misreads, so a successful
  kill reports `Ok`.

[Unreleased]: https://github.com/ZelAnton/tessmux/commits/HEAD
