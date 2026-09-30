# cona

[![CI](https://github.com/ph1p/cona/actions/workflows/ci.yml/badge.svg)](https://github.com/ph1p/cona/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/cona.svg)](https://crates.io/crates/cona)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Your AI agent reads whole files to find one function. cona lets it read the function.**

cona indexes your project into a symbol tree (functions, classes, methods with
exact line ranges), so a coding agent pulls the one symbol it needs instead of
the whole file. Fewer tokens, faster answers, lower cost. One Rust binary,
tree-sitter + SQLite, works across all your projects.

```sh
cargo install cona      # or the install script, see below
cd your/project
cona setup              # index + git hooks + agent integration
```

That's it. Your agents (Claude Code, Codex, Cursor, Gemini, …) now navigate by
symbol, and the index refreshes itself on every commit and edit.

## How it works

1. **Index.** tree-sitter parses your code into symbols, stored in one SQLite
   file per project under `~/.cona/`. Only changed files are reparsed.
2. **Navigate.** The agent asks for symbols instead of files:
   `tree → outline → show → edit`. A lookup costs tens of tokens, not thousands.
3. **Redirect.** A hook catches a full read of a large file (or a broad grep for
   a name) and points the agent at the cheaper query. It never blocks anything
   else and always fails open.
4. **Measure.** Every query logs what it returned vs. what grep-then-read would
   have cost. `cona stats` and `cona ui` show the savings.

## Commands

From coarse to fine:

```sh
cona tree --rank              # ranked overview of the codebase
cona outline src/indexer.rs   # every symbol in a file
cona show open_project_db     # just that symbol's source
cona context open_project_db  # the symbol + what it calls + who calls it
cona edit open_project_db --file new.rs   # replace it, syntax-verified
```

| Command                   | Does                                                           |
| ------------------------- | -------------------------------------------------------------- |
| `cona find <Name>`        | Locate a symbol: file, line range, signature                   |
| `cona grep <text>`        | Code-only search, hits labeled by symbol (`--regex` for regex) |
| `cona refs <Name>`        | Every usage site; skips strings and comments                   |
| `cona diff [ref]`         | Changed _symbols_ vs a git ref; a good start for reviews       |
| `cona impact <Sym>`       | Before an edit: refs, callers, tests, history                  |
| `cona insert <Sym> [--after]` | Add code before/after a symbol (stdin or `--file`), syntax-verified |
| `cona rename <Sym> <new>` | Project-wide rename, collision-guarded, all-or-nothing         |
| `cona stats` / `cona ui`  | Tokens saved (text / live TUI)                                 |
| `cona doctor`             | Check the installation                                         |

Handy flags:

- `--path <dir>` scopes `find`/`refs`/`grep`/`tree` to a subtree.
- `show <Sym> --all` prints every definition of an ambiguous name.
- A symbol can be written as `Name`, `Parent.Name` or `file.rs:Name`.
- `grep --include-deps` also searches `node_modules`/`vendor`/… (not indexed).
- `context --no-tests` keeps test callers out of the list.

Full reference: `cona --help`, or one group at a time with
`cona nav|inspect|code|history|project|maint --help`. Every command also works
without its group (`cona show Foo` = `cona nav show Foo`).

## Install

| Method          | Command                                                                                 |
| --------------- | --------------------------------------------------------------------------------------- |
| Install script  | `curl -fsSL https://raw.githubusercontent.com/ph1p/cona/main/install.sh \| sh`          |
| crates.io       | `cargo install cona`                                                                    |
| Prebuilt binary | [Releases](https://github.com/ph1p/cona/releases) (Linux, macOS, Windows), put on `PATH` |
| From source     | `git clone https://github.com/ph1p/cona && cd cona && ./install.sh` (Rust ≥ 1.95)       |

The install script downloads a prebuilt binary (no Rust needed) and verifies its
sha256 checksum. `CONA_VERIFY_ATTESTATION=1` also checks the SLSA build
provenance via `gh`; `CONA_SKIP_VERIFY=1` skips verification (e.g. an offline
mirror); `CONA_VERSION=x.y.z` pins a version.

cona **updates itself**: at most once a day a command checks for a new release
in the background. `cona upgrade` forces it and refreshes your agent configs.

## Set up agents

```sh
cona setup            # interactive: index, git hooks, then pick agents
cona setup -y         # non-interactive: every detected agent
```

`setup` offers the project and your global (home) configs in one checklist.
Unchecking an installed agent removes it. To manage agents later:

```sh
cona agents                        # the same checklist
cona agents status                 # what is installed, per agent and scope
cona agents install cursor         # one agent, this project
cona agents install claude --global  # one agent, home config
cona agents uninstall cursor       # remove it again
```

Supported: `claude`, `agents` (project `AGENTS.md`, read by Codex, Amp, Jules,
Cline), `cursor`, `gemini`, `opencode`, `windsurf`, `zed`, `qwen`, `crush`,
`copilot`, `pi`. Installing writes a short usage guide, and where the harness
supports it, hooks, a skill and the MCP server. Every change sits between
`<!-- cona:begin/end -->` markers or in cona's own entries; your own config is
never touched, and re-running is safe.

### Plugin (Claude Code, Codex)

Instead of `cona agents install`, both harnesses can load cona as a plugin
(skill + hooks + MCP server). The binary is still required.

```sh
# Claude Code
/plugin marketplace add ph1p/cona
/plugin install cona@cona

# Codex
git clone https://github.com/ph1p/cona
codex plugin marketplace add ./cona
codex plugin add cona@cona
```

`cona agents install claude` detects an enabled plugin: it writes only the
usage guide and removes hooks, skill and MCP entries that an earlier install
left behind, so nothing fires twice. `cona doctor` flags any leftover
duplicates. Codex caveats (cached copies, hook trust):
[`plugin/README.md`](plugin/README.md).

### Uninstall

```sh
cona uninstall          # interactive: agents, binary, data
cona uninstall -y       # remove agent configs + binary
cona uninstall -y --purge   # also delete ~/.cona (indexes + stats)
```

## MCP server

`cona mcp` serves the same tools over stdio. `cona agents install` registers it
automatically wherever the harness config directory exists:

| Harness     | Project                 | Global                                |
| ----------- | ----------------------- | ------------------------------------- |
| Claude Code | `.mcp.json`             | —                                     |
| Codex       | `.codex/config.toml`    | `~/.codex/config.toml`                |
| Cursor      | `.cursor/mcp.json`      | `~/.cursor/mcp.json`                  |
| Gemini CLI  | `.gemini/settings.json` | `~/.gemini/settings.json`             |
| OpenCode    | `opencode.json`         | `~/.config/opencode/opencode.json`    |
| Zed         | `.zed/settings.json`    | `~/.config/zed/settings.json`         |
| Qwen Code   | `.qwen/settings.json`   | `~/.qwen/settings.json`               |
| Crush       | `.crush.json`           | `~/.config/crush/crush.json`          |
| Windsurf    | —                       | `~/.codeium/windsurf/mcp_config.json` |
| Copilot CLI | —                       | `~/.copilot/mcp-config.json`          |

To add it by hand:

```json
{ "mcpServers": { "cona": { "type": "stdio", "command": "cona", "args": ["mcp"] } } }
```

The server lists 8 core tools plus a `more` tool that unlocks 13 advanced ones,
which keeps the per-turn schema cost low. Where hooks are available, the CLI +
hook setup is still the cheapest option; MCP is the fallback.

## Languages

**Full symbols:** Rust, Python, JavaScript, TypeScript/TSX, Go, Java, C, C++,
C#, Ruby, PHP, Kotlin, Swift, Scala, Elixir, Dart, Lua, Bash, CSS, TOML, YAML,
Markdown, Zig, Haskell, OCaml, Julia, PowerShell, Objective-C, Protobuf, SQL,
Perl, HCL/Terraform, Makefile, Dockerfile, XML, HTML.

**Search only:** JSON, Nix, Svelte, Vue, R, GraphQL.

XML/HTML elements become symbols named `tag#identity`, e.g.
`profile#with-frontend-build` in a `pom.xml`, or `form#login` in HTML.

## Good to know

- **Storage.** Everything lives in `~/.cona/` (override: `CONA_DATA_DIR`): one
  index per project, plus a global registry and usage stats. Cleanup runs
  daily. If home isn't writable (sandboxed agents), cona uses temp storage and
  says so.
- **What gets indexed.** `.gitignore` is respected; `node_modules`, `target` and
  similar dirs and files over 512 KB are skipped; registered git submodules are
  included. Your home directory is never auto-indexed.
- **Safe edits.** `edit`, `insert` and `rename` re-parse the result and refuse to
  write on a syntax error (`--force` overrides). CRLF line endings are kept.
- **Name-based, not type-based.** `refs`, `rename` and the call graph use real
  identifier nodes but no full type resolution; unresolvable same-named symbols
  are marked `·ambiguous`. An optional stack-graphs helper resolves more cases
  for TS/JS/Python/Rust.
- **Read-only mode.** `cona --read-only <query>` reads an existing index without
  indexing, logging stats or writing anything.
- **Savings are an estimate.** Baseline = grep, then read ±40 lines around each
  hit (capped at the whole file), at 4 characters per token. A trend, not an
  invoice.

### Environment variables

| Variable                    | Effect                                                     |
| --------------------------- | ---------------------------------------------------------- |
| `CONA_DATA_DIR`             | Where indexes and stats live (default `~/.cona`)           |
| `CONA_HOOK_DISABLE=1`       | Turn the redirect hook off                                 |
| `CONA_READ_MAX_LINES`       | File size (lines) from which a full read is redirected (300) |
| `CONA_ADVISE_MIN_LINES`     | Mid-size reads get a hint instead (120, `0` = off)        |
| `CONA_READ_STREAK`          | Hint every n-th full read in a session (4, `0` = off)      |
| `CONA_PARTIAL_STREAK`       | Hint after n slices of the same file (3, `0` = off)        |
| `CONA_RENUDGE_EVERY`        | Reminder every n tool calls (off by default)               |
| `CONA_NUDGE_EVERY`          | In unindexed repos: suggest `cona index` every n events (10, `0` = never) |
| `CONA_USAGE_RETENTION_DAYS` | Keep usage stats this long (90)                            |
| `CONA_MAX_USAGE_ROWS`       | Cap on stored usage rows (200k)                            |
| `CONA_RESOLVE_HELPER`       | Path to the stack-graphs helper binary                     |
| `CONA_NO_FETCH_HELPER`      | Never download the helper                                  |

Design notes per module: [`docs/architecture.md`](docs/architecture.md).

## License

MIT, see [LICENSE](LICENSE).
