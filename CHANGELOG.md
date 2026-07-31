# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.14](https://github.com/ph1p/cona/compare/v0.0.13...v0.0.14) - 2026-07-31

### Added

- *(setup)* manage agents from setup, and modernize CLI output

## [0.0.13](https://github.com/ph1p/cona/compare/v0.0.12...v0.0.13) - 2026-07-31

### Added

- support sandboxed agent navigation
- *(grep)* opt-in regex via --regex
- *(query)* --path scoping, show --all, context --no-tests

### Fixed

- *(hook)* grep redirect no longer sends regex searches away

### Other

- *(commands)* make --path one policy, resolved once

## [0.0.12](https://github.com/ph1p/cona/compare/v0.0.11...v0.0.12) - 2026-07-30

### Fixed

- *(agents)* don't assert index state in the baked guide

## [0.0.11](https://github.com/ph1p/cona/compare/v0.0.10...v0.0.11) - 2026-07-30

### Fixed

- *(deps)* keep byte offsets in one space when scanning a use tree
- *(install)* refuse to install an unverified release binary

## [0.0.10](https://github.com/ph1p/cona/compare/v0.0.9...v0.0.10) - 2026-07-29

### Added

- *(hook)* advisory read tier for mid-size, repeat and high-volume reads

## [0.0.9](https://github.com/ph1p/cona/compare/v0.0.8...v0.0.9) - 2026-07-29

### Fixed

- reach nested subagent definitions when installing the guide

## [0.0.8](https://github.com/ph1p/cona/compare/v0.0.7...v0.0.8) - 2026-07-27

### Other

- right-size agent guidance for current models

## [0.0.7](https://github.com/ph1p/cona/compare/v0.0.6...v0.0.7) - 2026-07-24

### Other

- dedup json + freshness helpers, tidy dashboard footer

## [0.0.6](https://github.com/ph1p/cona/compare/v0.0.5...v0.0.6) - 2026-07-23

### Fixed

- passive-heal global scope config, not just the current project

## [0.0.5](https://github.com/ph1p/cona/compare/v0.0.4...v0.0.5) - 2026-07-23

### Added

- reinforce cona in long contexts (persistence framing + periodic re-nudge)

### Fixed

- treat missing release checksum as non-fatal in cona upgrade

## [0.0.4](https://github.com/ph1p/cona/compare/v0.0.3...v0.0.4) - 2026-07-23

### Fixed

- classify system temp dir as ephemeral on Windows
- update tests
- update cargo and improve readme

## [0.0.3] - 2026-07-23

### Added

- Integration coverage for mutation and MCP flows.
- SHA-256 verification of downloaded release archives before extraction; the
  release workflow now publishes a `.sha256` checksum per target.

### Changed

- `index_project` now uses an RAII transaction, so a failed write no longer
  leaves a poisoned transaction on the connection reused by `index --watch`.
- `rename` reports index-refresh failures instead of silently discarding them,
  making clear when source files were written but the index could not refresh.

### Fixed

- File mutations (`edit --range`, `insert --at`, and their MCP equivalents) are
  now constrained to the project root; absolute paths and traversal/symlink
  escapes are rejected.
- Clippy empty-header lint and a rustfmt discrepancy that were breaking the CI
  baseline.

## [0.0.2] - 2026-07-23

### Added

- Redesigned `install`/`upgrade`/`uninstall`/`setup` CLI with a simplified TUI.
- pi.dev as a supported agent integration target.
- Per-agent install/uninstall UX: status table, interactive multiselect
  checklist spanning project + home scopes, and non-interactive flags.
