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

### Changed
- Release workflow is hard-disabled by a guard step while the workspace has no
  publishable crates (PoC phase).

### Fixed
-

[Unreleased]: https://github.com/ZelAnton/tessmux/commits/HEAD
