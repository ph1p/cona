# cona

[![CI](https://github.com/ph1p/codenav/actions/workflows/ci.yml/badge.svg)](https://github.com/ph1p/codenav/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/codenav.svg)](https://crates.io/crates/codenav)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Token-efficient code navigation and editing CLI for AI agents.
Rust + tree-sitter + SQLite. Works globally across all your projects, keeps its
index fresh via git hooks, and tracks how many tokens it saves you.

## Quick start

```sh
cargo install codenav       # or prebuilt binary / from source, see Installation
cd your/project
cona setup               # index + git hooks + agent integration — that's it
```

`setup` asks once whether to wire the current project, your global agent
configs, or both — after that everything is automatic: the index refreshes on
every commit and agent edit, and your agents (Claude Code, Cursor, Codex,
Gemini, …) navigate by symbol instead of reading whole files.
`cona doctor` verifies the installation, `cona stats` shows tokens saved.

## How it works

1. **Index** — `cona setup` parses the project with tree-sitter into a
   symbol tree (functions, classes, methods … with exact line ranges), stored
   in one SQLite file under `~/.cona/`. Incremental: only changed files are
   reparsed; git hooks and agent hooks keep it fresh without manual reindexing.
2. **Navigate** — instead of reading files, the agent asks for symbols:
   `tree → outline → show → edit`; `context` packs a symbol's source + callees +
   callers in one call, `impact` shows its blast radius before an edit. A lookup
   costs tens of tokens instead of a whole file.
3. **Redirect** — an optional PreToolUse hook (installed by `setup`) catches
   full reads of large indexed files and broad identifier greps, and points the
   agent at the cheaper query (`outline`/`show`, or `context`/`impact` for a
   whole-symbol pack). Always fails open — small files, partial reads, and
   non-code pass through.
4. **Measure** — every query logs what it returned vs. the grep-then-Read cost
   of the same lookup without cona. `cona stats` / `cona ui` show the
   savings.

## Installation

Any one of these — then `cona setup` inside a project:

| Method          | Command                                                                                                                                        |
| --------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| Install script  | `curl -fsSL https://raw.githubusercontent.com/ph1p/codenav/main/install.sh \| sh` (no Rust — grabs the prebuilt binary; `wget -qO-` works too) |
| crates.io       | `cargo install codenav`                                                                                                                        |
| Prebuilt binary | download from [releases](https://github.com/ph1p/codenav/releases) (Linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64), put on `PATH` |
| From source     | `git clone https://github.com/ph1p/codenav && cd codenav && ./install.sh`                                                                      |

One `install.sh`, two modes: piped through `curl`/`wget` it downloads the
prebuilt release binary to `~/.local/bin` (override `BIN_DIR=`, pin
`CONA_VERSION=`); run from a source checkout (`./install.sh`, or
`--bin-dir DIR`) it builds with cargo and wires upgrade git hooks. Building
needs Rust ≥ 1.95.

### Set up a project

```sh
cd your/project
cona setup               # interactive chooser: project, global, or both
```

Non-interactive: `cona setup project`, `cona setup global`, or
`cona setup all`. That's it — the index refreshes automatically from then on.

### Staying up to date

Automatic — no action needed. Every command cheaply checks in the background:

- **Source install:** git hooks in the checkout (post-commit/-merge/-checkout)
  and an mtime check rebuild whenever the sources are newer than the binary.
- **All installs:** at most once a day cona checks crates.io for a newer
  release. Source checkouts are updated via `git pull --ff-only` + rebuild
  (local changes are never overwritten); binary installs get the prebuilt
  release binary, falling back to `cargo install`.

Manual trigger: `cona upgrade`.

### Uninstall

```sh
./uninstall.sh [--purge]    # or: cona uninstall [--purge]
```

Removes everything: the binary, upgrade hooks in the source repo, and the
agent files + git hooks both globally and in **every registered project**.
Foreign file content is never touched — only cona marker blocks and hook
entries are removed. `--purge` also deletes `~/.cona` (all indexes + stats).

## Commands

Commands are organized into six groups for discoverability — run `cona <group> --help`:

| Group      | Covers                                                    |
| ---------- | --------------------------------------------------------- |
| `nav`      | `tree` `outline` `find` `show` `refs` `grep`              |
| `inspect`  | `context` `diff` `impact` `shape` `deps` `entries` `tests` `callers` `callees` `path` |
| `code`     | `edit` `insert` `rename` `note` `check`                   |
| `history`  | `blame` `hot` `coupling`                                  |
| `project`  | `index` `stats` `projects` `reset` `forget` `tidy` `ui`   |
| `maint`    | `doctor` `setup` `install` `upgrade` `uninstall` `agents` `hooks` `skill` `mcp` |

Every command also works **flat** (`cona show Foo` ≡ `cona nav show Foo`) — the short form is the canonical one for agents (fewer tokens); the grouped form is for humans browsing `--help`.

| Command                                        | Purpose                                                                                                                            |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `cona index [--watch]`                      | Index the project (incremental) — runs automatically on first use; `--watch` stays alive and reindexes on file changes (debounced) |
| `cona tree [--rank] [--budget 2000]`        | Compact codebase overview within a token budget; `--rank` orders symbols by reference fan-in                                       |
| `cona outline <file>`                       | All symbols of one file with line ranges and signatures                                                                            |
| `cona find <Name> [--kind fn] [--json]`     | Locate a symbol (exact → LIKE → fuzzy fallback)                                                                                    |
| `cona show <Sym> [<Sym2> …] [--context 3] [--kind struct] [--sig]` | Print only those symbols' source — several names read in one call; `--context` adds surrounding lines; `--kind` disambiguates struct/impl name clashes; `--sig` prints just the signature (no body, reads nothing off disk) |
| `cona refs <Name>`                          | All usage sites as `file:line` (semantic — strings/comments never match)                                                           |
| `cona grep <pattern> [-i] [--limit 50]`     | Substring search over code files only; hits labeled with their enclosing symbol                                                    |
| `cona diff [ref]`                           | Changed **symbols** instead of lines (default: vs `HEAD`, incl. uncommitted + untracked) — start code reviews here                 |
| `cona context <Sym> [--budget 3000]`        | One context pack: symbol source + callee signatures + call sites (instead of show+refs+shows)                                      |
| `cona edit <Sym> --file new.txt`            | Replace a symbol's body — syntax-verified, rollback on error                                                                       |
| `cona edit <file> --range S-E`              | Replace absolute lines S-E of a file — patch a few lines without resending a symbol; same verify + rollback                        |
| `cona insert <Sym> --after\|--before`       | Insert new code next to a symbol without touching its body — whole-file syntax re-verified                                         |
| `cona insert --at <file> <line>`            | Insert at an absolute position (0 = prepend, past EOF = append) — works on a new/empty file with no indexed symbol                 |
| `cona check [<file>]`                       | tree-sitter parse diagnostics (syntax only, **not** a compiler); no file = all changed vs `HEAD`                                   |
| `cona impact <Sym>`                         | Pre-edit blast radius: references + immediate callers + tests + recent history, in one pack                                        |
| `cona entries [--path p]`                   | Entry points: main fns, exported/public API, test overview — first orientation                                                     |
| `cona deps [path]`                          | File-level import graph + most-imported + cycles — the architecture view                                                           |
| `cona callers <Sym> [--depth 2]`            | Transitive caller tree: who reaches this symbol                                                                                    |
| `cona callees <Sym> [--depth 2]`            | Transitive callee tree: what this symbol reaches                                                                                   |
| `cona path <A> <B>`                         | Shortest call chain from A to B                                                                                                    |
| `cona tests <Sym>`                          | Which tests exercise a symbol — loud when none do                                                                                  |
| `cona blame <Sym> [--limit 10]`             | git history of exactly this symbol's lines                                                                                         |
| `cona hot [--since '6 months ago']`         | Churn hotspots among indexed files                                                                                                 |
| `cona coupling <file>`                      | Files that historically change together with this one                                                                              |
| `cona shape <Sym> [--budget 2000]`          | Symbol source + referenced types expanded one level                                                                                |
| `cona note <Sym> <text…>`                   | Persistent note on a symbol — auto-surfaced in show/context (`note` lists, `--rm <id>` deletes)                                    |
| `cona rename <Sym> <new> [--force]`         | Project-wide semantic rename: collision guard, syntax verify, all-or-nothing                                                       |
| `cona stats [--project] [--json]`           | Stats per project + global: tokens saved, top targets, recent activity                                                             |
| `cona ui`                                   | Live TUI: index status + token savings in real time                                                                                |
| `cona doctor`                               | Diagnosis: binary, agent hooks/skill (global + project), index + storage                                                           |
| `cona tidy [--orphans]` (alias `gc`)        | DB housekeeping: prune old usage rows, drop orphaned indexes, reclaim space                                                        |
| `cona forget [path]`                        | Delete a project's index + stats (default: current project)                                                                        |
| `cona reset [--keep-stats]`                 | Reset the current project: wipe index, notes + stats, reindex fresh                                                                |
| `cona projects`                             | List all registered projects                                                                                                       |
| `cona hooks install`                        | git hooks (post-commit/-merge/-checkout) for auto-reindex                                                                          |
| `cona skill`                                | Print the agent SKILL.md                                                                                                           |
| `cona mcp`                                  | MCP server over stdio — full tool parity: find/show/refs/outline/tree/grep/context/diff/edit/batch_edit/insert/check/impact/callers/callees/path/deps/shape/entries/tests/note |
| `cona setup [project\|global\|all]`         | Everything at once: index + git hooks + `agents install` — interactive chooser when run bare                                       |
| `cona install [--bin-dir DIR]`              | Install the binary + upgrade hooks in the source repo                                                                              |
| `cona upgrade [--quiet]`                    | Rebuild from a newer source checkout, else update to the newest release                                                            |
| `cona agents install\|uninstall [names…] [--all] [--global]` | Inject/remove cona in agent configs (no names = autodetect installed; `--all` = every known agent)                              |
| `cona uninstall [--purge]`                  | Remove binary + upgrade hooks + global agent files                                                                                 |

## Architecture

- **Languages:** 30+ with symbol extraction — Rust, Python, JavaScript, TypeScript/TSX,
  Go, Java, C, C++, C#, Ruby, PHP, Kotlin, Swift, Scala, Elixir, Dart, Lua, Bash, CSS,
  TOML, YAML, Markdown, Zig, Haskell, OCaml, Julia, PowerShell, Objective-C, Protobuf,
  SQL, Perl, HCL/Terraform, Makefile, Dockerfile — plus parse-only refs/grep for
  JSON, HTML, Nix, Svelte, Vue, R, XML, GraphQL (tree-sitter grammars; adding a
  language is a few lines in `src/lang.rs`)
- **Storage:** global under `~/.cona/` (override: `CONA_DATA_DIR`; paths +
  sizes via `cona doctor`/`stats`)
  - `projects/<hash>.db` — one SQLite index per project (files + symbols + notes)
  - `global.db` — project registry + usage stats (command, duration, tokens out,
    estimated tokens saved) + `meta` (source_dir/install_path/last_tidy)
- **Automatic maintenance:** once a day (`auto_tidy`) the usage log is pruned
  (default: ≤90 days / ≤200k rows, via `CONA_USAGE_RETENTION_DAYS` /
  `CONA_MAX_USAGE_ROWS`), registry entries whose path lived under a temp
  root and is gone are dropped (test repos, scratchpads), and space is
  reclaimed via `VACUUM`. Manual: `cona tidy [--orphans]` also purges
  orphans outside temp roots. `cona forget [path]` deletes an index entirely.
- **Project detection:** nearest `.git` upwards, else cwd — cona works from
  any subdirectory. The home directory / fs root is **never** auto-indexed.
- **Incremental:** files are reparsed only on changed mtime/size; deleted files
  drop out of the index; `.gitignore` is respected. Heavy directories
  (`node_modules`, `target`, `dist`, `.venv`, `vendor`, …) are **always**
  skipped — even without git — as are files > 512 KB, keeping `~/.cona` small.
- **Symbol addressing:** qualified names (`UserService.login`) instead of
  fragile line numbers — the index supplies line ranges as extra info.
- **Safe editing:** `edit` splices exactly the symbol's line range, reparses the
  result, and refuses to write on syntax errors (`--force` overrides), then
  reindexes the file. CRLF sources stay CRLF.
- **Semantic, name-based:** `refs`/`context`/`rename`/callgraph work on
  tree-sitter identifier nodes (strings/comments never match) but without
  type/scope resolution — same-named symbols are marked `·ambiguous`.
  Unsupported languages fall back to a textual word-boundary scan (fail-open).
- **Optional semantic tier:** an out-of-process stack-graphs helper
  (`cona-resolve-helper`, fail-open) resolves same-arity ambiguity that the
  name-based heuristics can't, both same-file and cross-file, for
  typescript/tsx/javascript/python/rust. `context` and `callers`/`callees`
  consult it only when a name stays ambiguous; `doctor` reports its status.

## Token accounting

`tokens_saved` = the **grep-then-Read baseline** minus the actual output
(4 chars ≈ 1 token; clamped ≥ 0). The baseline models what the same lookup
costs an agent WITHOUT cona: a grep pass (≈free, returns only line numbers)
plus a targeted `Read offset/limit` window (±40 lines) around each hit — NOT
the whole file. Overlapping windows merge; the total is capped at the whole
file. So a query can never claim to have saved more than a naive whole-file
read, and a symbol buried in a large file credits only the realistic window an
agent would actually have opened, not the entire file (`db::baseline_tokens`).
The PreToolUse hook counts its intercepts (`hook:read-block`, `hook:grep-block`)
but credits itself no tokens — the follow-up query earns them, otherwise the
same avoided read would be counted twice. Deliberately coarse — a trend metric
across time and projects, not an accountant.

`cona stats` shows both current project **and** global, each with index
status, saved vs. used tokens (incl. % savings), redirected full reads,
per-command breakdown, top targets, and recent activity. The command table
contains **only real queries**; maintenance rows (`index`, `edit`, `hook:*`)
save nothing by definition and appear as a compact `maintenance` line below.

`cona ui` renders the same data as a live dashboard (poll ~1s, read-only):
savings gauge, query table, live feed, and a "tokens saved per minute"
sparkline. `p` toggles project/global, `q` quits.

Reset stats: `cona reset` (per project, wipes + reindexes; `--keep-stats` to
keep the numbers); `cona tidy` prunes old rows globally.

### PreToolUse hook — accelerator, not gatekeeper

cona does not want to defeat the agent's own optimizations (prompt caching
etc.) — it wants to get it there faster. The hook always fails open and
redirects only two things, both in an **indexed** project:

- a _complete_ `Read` of a _large_ code file (default > 300 lines, via
  `CONA_READ_MAX_LINES`) → `outline`/`show`, or `context`/`impact` for a
  whole-symbol pack;
- a _broad identifier_ `Grep` (no glob/type/path/`head_limit` filter) →
  `grep`/`refs`.

Everything else passes untouched: partial reads (offset/limit), small files,
non-code, regex greps, already-narrowed greps. In a git repo that simply isn't
indexed yet, the same calls are _nudged_ (allowed, one-time hint that
`cona index` unlocks the fast path) instead of redirected.
`CONA_HOOK_DISABLE=1` turns it off entirely.

## Agent integration

`cona agents install` (per project) or `--global` (home configs) injects
cona into all detected agent setups. Name one or more agents (`agents install
cursor gemini`) to target just those, or `--all` for every known agent regardless
of detection. Idempotent, marker-based (`<!-- cona:begin/end -->`), removable
without residue via `agents uninstall`:

| Agent                  | Project                                                                                                       | Global                                      |
| ---------------------- | ------------------------------------------------------------------------------------------------------------- | ------------------------------------------- |
| Claude Code skill      | `.claude/skills/cona/SKILL.md`                                                                             | `~/.claude/skills/cona/SKILL.md`         |
| Claude Code memory     | `CLAUDE.md` (marker block)                                                                                    | `~/.claude/CLAUDE.md`                       |
| Claude Code hooks      | `.claude/settings.json` — PostToolUse/SessionStart → `index --quiet`, **PreToolUse Read\|Grep → `hook PreToolUse`** | `~/.claude/settings.json`                   |
| Claude Code subagents  | `.claude/agents/*.md` (marker block in every **existing** definition; never creates new agent files)          | `~/.claude/agents/*.md`                     |
| Codex / OpenCode / Amp / Jules | `AGENTS.md` (marker block)                                                                            | `~/.codex/AGENTS.md` (if `~/.codex` exists) |
| Cursor                 | `.cursor/rules/cona.mdc` (if `.cursor/` exists)                                                            | `~/.cursor/rules/cona.mdc`               |
| Gemini CLI             | `GEMINI.md` (if `.gemini/`/`GEMINI.md` exists)                                                                | `~/.gemini/GEMINI.md`                       |
| pi.dev                 | `AGENTS.md` (marker block, shared with the Codex row above)                                                    | `~/.pi/agent/AGENTS.md` (if `~/.pi` exists) |

The hooks keep the index fresh after every agent edit; the skill/memory blocks
instruct the agent to never read whole files and to use find/show/edit with
`--json` for machine-readable output.

**Note — Claude Code may pick up hooks/skills only after a restart:** hooks and
skills are read as a snapshot at session start (security model). `cona doctor`
shows what is actually installed globally and per project.

## MCP server mode

`cona mcp` serves the core queries as MCP tools over stdio (hand-rolled
JSON-RPC, no extra deps). Register it e.g. in a project `.mcp.json`:

```json
{
  "mcpServers": {
    "cona": {
      "command": "cona",
      "args": ["mcp"]
    }
  }
}
```

Full tool parity with the CLI: `find` `show` `refs` `outline` `tree` `grep`
`context` `diff` `edit` `batch_edit` `insert` `check` `impact` `callers`
`callees` `path` `deps` `shape` `entries` `tests` `note`. The CLI + PreToolUse
hook remains the recommended integration (zero context overhead); MCP is for
hosts without hook support.
