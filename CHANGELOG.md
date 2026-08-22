# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.26](https://github.com/ph1p/cona/compare/v0.0.25...v0.0.26) - 2026-08-22

### Added

- *(hook)* close the three gaps the rule keeps falling through

### Other

- sync module maps with the directory splits
- *(tests)* split tests/basic.rs into tests/basic/ modules
- *(install)* split upgrade.rs into upgrade/ module
- *(db)* split db.rs into db/ module
- *(cli)* extract clap definitions from main.rs into cli.rs
- *(install)* split agents.rs into agents/ module
- *(commands)* split query.rs into query/ module
- *(lang)* split lang.rs into lang/ module
- *(hook)* split hook.rs into hook/ module

## [0.0.25](https://github.com/ph1p/cona/compare/v0.0.24...v0.0.25) - 2026-08-22

### Added

- *(install)* skip Claude pieces the enabled plugin already ships

### Other

- *(install)* rustfmt line wrapping in agents.rs

## [0.0.24](https://github.com/ph1p/cona/compare/v0.0.23...v0.0.24) - 2026-08-22

### Added

- *(install)* make guide text imperative, not advisory

### Fixed

- *(hook)* resolve relative grep paths against payload cwd

## [0.0.23](https://github.com/ph1p/cona/compare/v0.0.22...v0.0.23) - 2026-08-20

### Added

- *(grep)* add --include-deps to search dependency trees
- *(lang)* index xml and html elements as symbols

### Fixed

- *(test)* gate include_deps test to unix and rg
- *(html)* stop vue's bundled scanner hijacking tree-sitter-html

### Other

- *(lang)* single-pass markup naming, skip dep reindex on grep
- *(outline)* print leaf names, not the qualified chain
- *(index)* skip dependency lock files

## [0.0.22](https://github.com/ph1p/cona/compare/v0.0.21...v0.0.22) - 2026-08-18

### Fixed

- *(lang)* deny-list parse-only languages in has_callable_symbols
- *(install)* harden the install and upgrade paths

### Other

- install verification, matcher single-source, spike status
- dedupe plugin manifest JSON loading
- *(plugin)* sync plugin manifest versions in the release flow

## [0.0.21](https://github.com/ph1p/cona/compare/v0.0.20...v0.0.21) - 2026-08-16

### Fixed

- *(index)* never let the session hook walk $HOME, dedupe concurrent walks

### Other

- skip the $HOME session-start test on Windows

## [0.0.20](https://github.com/ph1p/cona/compare/v0.0.19...v0.0.20) - 2026-08-15

### Added

- *(cli)* flag clipped lists with a shared --limit trailer
- *(mcp)* expose limit/budget/max_depth knobs on the query tools
- *(doctor)* report hook liveness and stale Codex plugin caches
- *(hook)* advise instead of block on output-bounded greps
- *(hook)* count reads honestly and repeat the nudge on a cadence
- *(mcp)* name the CLI fallback in more's disclosure
- *(show)* render small ambiguities instead of erroring
- *(doctor)* implement the advertised --json output

### Fixed

- *(lang)* make remaining AST walkers iterative
- *(lang)* walk symbol trees with a worklist, not recursion
- *(install)* say when the MCP server is skipped for a missing config dir
- *(cli)* point the read-only bail at CONA_DATA_DIR
- *(hook)* break the read-redirect loop
- *(outline)* dead-end error now says which repair applies
- *(cli)* deps takes --path like every other path filter

### Other

- dedup limit trailers, hook markers, doctor liveness
- *(cli)* make find's --limit a usize like every other limit
- *(install)* share ONE agent picker between setup and cona agents
- *(install)* fold the per-agent guide blocks into the config_paths loop
- *(edit)* share the write-path locate via locate_for_write
- *(cli)* fold query dispatch arms into one queried() envelope
- describe the new hook tiers and show auto-all
- *(edit)* drop dead conn param from write_verified

## [0.0.19](https://github.com/ph1p/cona/compare/v0.0.18...v0.0.19) - 2026-08-13

### Added

- *(plugin)* ship the plugin for Codex as well
- *(hook)* intercept reads and greps issued through a shell tool

### Other

- *(hook)* derive the PreToolUse matcher from one tool-name list

## [0.0.18](https://github.com/ph1p/cona/compare/v0.0.17...v0.0.18) - 2026-08-13

### Added

- *(plugin)* ship a Claude Code plugin
- *(index)* walk registered git submodules
- *(mcp)* progressive tool disclosure + outputSchema on row tools
- *(agents)* support 6 more harnesses

### Fixed

- *(index)* reject escaping submodule paths lexically
- *(mcp)* disclose gated tools via tools/list_changed

### Other

- apply cargo fmt
- move per-module rationale to docs/architecture.md

## [0.0.17](https://github.com/ph1p/cona/compare/v0.0.16...v0.0.17) - 2026-08-07

### Fixed

- *(skill)* list pi in the agents install targets
- *(show)* render --all candidates from located rows

## [0.0.16](https://github.com/ph1p/cona/compare/v0.0.15...v0.0.16) - 2026-08-06

### Added

- *(grep)* point zero-hit searches at find
- *(lang)* index fn-valued JS/TS bindings and Swift declaration kinds

### Fixed

- *(install)* refresh only installed agents on upgrade
- *(install)* verify the release checksum in install.sh
- honour --read-only for stats and projects
- *(check)* count checked files in the sweep summary
- *(edit)* replace files atomically in write_verified
- *(hook)* nudge on cold open in unindexed repos
- *(indexer)* take immediate transactions for index writes

### Other

- *(install)* deflake the quiet-reinstall no-op check
- *(skill)* list check and impact under --json

## [0.0.15](https://github.com/ph1p/cona/compare/v0.0.14...v0.0.15) - 2026-08-06

### Added

- *(mcp)* register the server in harness configs on agents install

### Other

- *(install)* drive MCP registration from one enumeration

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
