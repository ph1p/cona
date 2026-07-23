# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
