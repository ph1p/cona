# cona

[![CI](https://github.com/ph1p/cona/actions/workflows/ci.yml/badge.svg)](https://github.com/ph1p/cona/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/cona.svg)](https://crates.io/crates/cona)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Your AI agent reads whole files to find one function. cona lets it read the function.**

cona indexes your project into a symbol tree (functions, classes, methods with
exact line ranges), so a coding agent pulls the one symbol it needs instead of
the whole file. In a measured A/B run with Claude Opus that meant **43% lower
cost, a third fewer turns and answers in half the time** ([details](#does-it-actually-save-tokens)).
One Rust binary, tree-sitter + SQLite, works across all your projects.

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
   have cost. `cona stats` and `cona ui` show that estimate per project.

## Does it actually save tokens?

Measured, not estimated: the same 9 questions about this repo, each asked 3
times by Claude Opus 5.5 in a fresh `claude -p --bare` session, once with cona
(MCP server only, no hooks, no extra prompt) and once without (2026-10-20,
cona 0.0.39).

| Mean per session          | Without cona | With cona | Change   |
| ------------------------- | -----------: | --------: | -------: |
| Cost                      |      $0.023  |   $0.013  | **−43%** |
| Uncached input tokens     |       2,486  |    1,337  | −46%     |
| Output tokens             |         429  |      204  | −52%     |
| Turns                     |         3.8  |      2.6  | −32%     |
| Wall time                 |       12.7 s |     6.0 s | −53%     |
| Correct answers           |       27/27  |    27/27  |          |

Where it comes from: fewer round trips. `find` → `show` lands on the answer
where grep → read → read takes more turns, so less output and less fresh
context. What it costs: cona's tool schemas ride along on every turn, so
*cached* input goes up (+34%) — cheap, which is why the bill still drops.
Quick lookups gain most (down to ¼–⅓ of the cost); multi-file tracing
questions use as many or more total tokens but still cost 23–43% less.

Limits: one repo of 85 files and read-only questions. Bigger repos make grep
and full reads more expensive, so the gap should widen, but that is untested.
Hooks were off, so their effect is not part of these numbers. The whole run
cost under $1.

`cona stats` reports a much higher share (~90%+) because it compares against a
modeled grep-then-read baseline per query and ignores the per-turn schema cost.
Read it as a trend, not a bill.

## Commands

From coarse to fine:

```sh
cona tree --rank              # ranked overview of the codebase
cona outline src/indexer.rs   # every symbol in a file
cona show open_project_db     # just that symbol's source
cona show src/db/mod.rs:40-80 # just those lines (no sed -n)
cona context open_project_db  # the symbol + what it calls + who calls it
cona edit open_project_db --file new.rs   # replace it, syntax-verified
```

| Command                   | Does                                                           |
| ------------------------- | -------------------------------------------------------------- |
| `cona find <Name>`        | Locate a symbol: file, line range, signature                   |
| `cona search "<behavior>"` | Rank lexical matches in names, signatures, docs and bodies   |
| `cona grep <text>`        | Code-only search, hits labeled by symbol (`--regex` for regex) |
| `cona refs <Name>`        | Every usage site; skips strings and comments, folds test sites |
| `cona diff [ref]`         | Changed _symbols_ vs a git ref; a good start for reviews       |
| `cona impact <Sym>`       | Before an edit: refs, callers, tests, history                  |
| `cona insert <Sym> [--after]` | Add code before/after a symbol (stdin or `--file`), syntax-verified |
| `cona rename <Sym> <new>` | Project-wide rename, collision-guarded, all-or-nothing         |
| `cona stats` / `cona ui`  | Tokens saved (text / live TUI); `--daily`/`--weekly` trend     |
| `cona learn`              | Lookups that keep failing, and what would have worked          |
| `cona discover`           | Scan Claude transcripts for reads cona could have answered     |
| `cona doctor`             | Check the installation                                         |

Handy flags:

- `--path <dir>` scopes `find`/`search`/`refs`/`grep`/`tree`/`show` to a subtree.
- `show <Sym> --all` prints every definition of an ambiguous name.
- A symbol can be written as `Name`, `Parent.Name` or `file.rs:Name`.
- `grep --include-deps` also searches `node_modules`/`vendor`/… (not indexed).
- `context --no-tests` keeps test callers out of the list.

Full reference: `cona --help`, or one group at a time with
`cona nav|inspect|code|history|project|maint --help`. Every command also works
without its group (`cona show Foo` = `cona nav show Foo`).

### Discover an implementation without its name

```sh
cona search "refuse write syntax errors" --path src --limit 5
cona nav search "retry timeout" --json
# Follow a returned file-qualified selector:
cona show src/commands/mutate.rs:write_verified
```

`search` splits identifiers and ranks lexical evidence from names, signatures,
documentation and bodies. It returns reasons, a short numbered excerpt and a
reusable `file:qualified_name` selector, or `file:start-end` when a same-file
identity is ambiguous (for example, overloaded functions). Exact names rank
strongly; ties are stable.
The default is 10 results (hard cap 100, with an additional output-size bound);
`--limit 0` returns no rows. JSON includes
`lexical: true`, `truncated`, and `results`; scores are ranking weights, **not
confidence**. Blank queries return no matches; stopwords are ignored unless the
entire query is an exact symbol name.

Search reads indexed symbol-bearing files, not dependency trees or arbitrary
unindexed prose. It does not understand synonyms or prove semantic identity:
inspect the evidence with `show` or `context` before editing. For a known name use
`find`; for exact text use `grep`. MCP clients discover `search` after calling
`more` and refreshing `tools/list`.

### Investigation and review

```sh
cona tests src/service.rs:process --depth 3 --explain
cona context src/service.rs:process --edit --budget 3000
cona diff HEAD --impact --budget 3000
```

`tests` selects candidate tests through static name-based references, not proven
execution coverage. Depth defaults to one (direct references), is bounded at
eight, and indirect paths can contain explicitly ambiguous relationships. No
static match does not mean untested; cona never runs tests or guesses a test
runner's selection syntax.

`context --edit` is opt-in. It selects referenced definitions, small helpers,
caller excerpts and test evidence under a whole-output estimated budget. The
default context still prints the target in full and budgets only its related
sections. Editing mode instead explicitly omits bodies that cannot fit and
provides a reusable `show` selector. A budget too small for even that notice is
an error, not a silently truncated success.

`diff --impact` adds old/new definition evidence, candidate impacted callers and
test evidence across changed symbols. It retains module-level changes and
deletions rather than guessing rename identity. Impact is a static review aid,
not a list of runtime failures. The opt-in budget includes rendered output;
omission notices disclose incomplete reports. Both modes support `--json`.

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
mirror); `CONA_VERSION=x.y.z` pins a version. On a terminal it then runs
`cona setup` (inside a repo) or `cona setup global` (elsewhere);
`CONA_NO_SETUP=1` stops after the binary.

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
`copilot`, `devin`, `pi`. Installing writes a short usage guide, and where the harness
supports it, hooks, a skill and the MCP server. Every change sits between
`<!-- cona:begin/end -->` markers or in cona's own entries; your own config is
never touched, and re-running is safe.

### Plugin (Claude Code, Codex)

Instead of `cona agents install`, both harnesses can load cona as a plugin
(skill + hooks + MCP server). No separate binary install: if `cona` is missing,
the first session installs it to `~/.local/bin` (checksum-verified;
`CONA_NO_AUTO_INSTALL=1` forbids it) and indexes the repo.

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

| Harness     | Project                  | Global                                |
| ----------- | ------------------------ | ------------------------------------- |
| Claude Code | `.mcp.json`              | —                                     |
| Codex       | `.codex/config.toml`     | `~/.codex/config.toml`                |
| Cursor      | `.cursor/mcp.json`       | `~/.cursor/mcp.json`                  |
| Gemini CLI  | `.gemini/settings.json`  | `~/.gemini/settings.json`             |
| OpenCode    | `opencode.json`          | `~/.config/opencode/opencode.json`    |
| Zed         | `.zed/settings.json`     | `~/.config/zed/settings.json`         |
| Qwen Code   | `.qwen/settings.json`    | `~/.qwen/settings.json`               |
| Crush       | `.crush.json`            | `~/.config/crush/crush.json`          |
| Devin CLI   | `.devin/mcp_config.json` | `~/.config/devin/mcp_config.json`     |
| Windsurf    | —                        | `~/.codeium/windsurf/mcp_config.json` |
| Copilot CLI | —                        | `~/.copilot/mcp-config.json`          |

To add it by hand:

```json
{ "mcpServers": { "cona": { "type": "stdio", "command": "cona", "args": ["mcp"] } } }
```

The server lists 8 core tools plus a `more` tool that unlocks 14 advanced ones,
which keeps the per-turn schema cost low (~1.8k tokens, mostly cached). The
benchmark above used MCP alone; where hooks are available they add the read
redirect on top.

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
- **`stats` savings are an estimate.** Baseline = grep, then read ±40 lines
  around each hit (capped at the whole file), at 4 characters per token; `find`
  claims nothing (the follow-up `show` does), and identical queries within 2s
  count once. It overstates the real saving — see
  [Does it actually save tokens?](#does-it-actually-save-tokens).

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
| `CONA_NO_STATS`             | Don't log usage stats (benchmarks, scripted loops)         |
| `CONA_RESOLVE_HELPER`       | Path to the stack-graphs helper binary                     |
| `CONA_NO_FETCH_HELPER`      | Never download the helper                                  |

Design notes per module: [`docs/architecture.md`](docs/architecture.md).

## License

MIT, see [LICENSE](LICENSE).
