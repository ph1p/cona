# cona

[![CI](https://github.com/ph1p/cona/actions/workflows/ci.yml/badge.svg)](https://github.com/ph1p/cona/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/cona.svg)](https://crates.io/crates/cona)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Your AI agent reads whole files to find one function. cona lets it read the function.**

cona is a code-navigation CLI built for AI coding agents. It indexes your project
into a symbol tree, so an agent can pull a single function, class, or method —
instead of dumping the entire file into its context. Fewer tokens, faster answers,
lower cost.

Rust + tree-sitter + SQLite. One binary. Works across all your projects.

```sh
cargo install cona   # or: curl -fsSL https://raw.githubusercontent.com/ph1p/cona/main/install.sh | sh
cd your/project
cona setup           # index + git hooks + agent integration — done
```

That's the whole setup. From here your agents (Claude Code, Cursor, Codex,
Gemini, …) navigate by symbol automatically, and the index stays fresh on every
commit and edit.

## Why it helps

- **Reads a symbol, not a file.** `cona show UserService.login` returns ~30 tokens.
  Reading the file it lives in might cost 6,000.
- **Zero babysitting.** The index is incremental and self-refreshing via git hooks.
  Set up once, forget it.
- **It proves the savings.** Every lookup logs what it returned vs. what a naive
  grep-then-read would have cost. `cona stats` shows the running total.
- **Broad language support.** 30+ languages with full symbol extraction; more with
  search-only support.
- **Plays with your agents.** Auto-wires Claude Code, Cursor, Codex, Gemini, and
  more — or runs as an MCP server.

## How it works

1. **Index** — tree-sitter parses your code into a symbol tree (functions,
   classes, methods, with exact line ranges), stored in one SQLite file under
   `~/.cona/`. Incremental: only changed files are reparsed.
2. **Navigate** — instead of reading files, the agent asks for symbols:
   `tree → outline → show → edit`. A lookup costs tens of tokens, not thousands.
3. **Redirect** — an optional hook catches an agent about to read a whole large
   file and points it at the cheap query instead. Always fails open.
4. **Measure** — `cona stats` and `cona ui` show the tokens saved over time.

## Everyday commands

Coarse to fine — the usual path through an unfamiliar codebase:

```sh
cona tree --rank            # ranked overview of the whole codebase
cona outline src/db.rs      # every symbol in one file
cona show open_project_db   # print just that symbol's source
cona context open_project_db  # the symbol + what it calls + who calls it
cona edit open_project_db --file new.rs   # replace its body (syntax-verified)
```

A few more you'll reach for often:

| Command                   | Does                                                          |
| ------------------------- | ------------------------------------------------------------- |
| `cona find <Name>`        | Locate a symbol (file, line range, signature)                 |
| `cona grep <pattern>`     | Code-only search (`--regex` opt-in), hits labeled by symbol   |
| `cona refs <Name>`        | Every usage site (semantic — skips strings/comments)          |
| `cona diff [ref]`         | Changed _symbols_ vs a git ref — start code reviews here      |
| `cona impact <Sym>`       | Blast radius before an edit: refs + callers + tests + history |
| `cona rename <Sym> <new>` | Project-wide rename: collision-guarded, all-or-nothing        |
| `cona stats`              | Tokens saved, per project and global                          |
| `cona ui`                 | Live TUI: index status + savings                              |
| `cona doctor`             | Check the installation                                        |

Scope any of `find`/`refs`/`grep`/`tree` with `--path <dir>` when a name is too
common to read repo-wide; `cona show <Sym> --all` prints every definition of an
ambiguous name instead of asking you to disambiguate, and `cona context <Sym>
--no-tests` keeps test callers from crowding out the real ones. `grep` matches
literally by default — `foo.bar` searches that text, not a pattern — and takes
`--regex` when you want a real regular expression.

**Full reference:** `cona --help`, or a group at a time —
`cona nav|inspect|code|history|project|maint --help`. Every command also works
flat (`cona show Foo` ≡ `cona nav show Foo`).

## Installation

Pick one, then run `cona setup` in a project:

| Method          | Command                                                                                               |
| --------------- | ----------------------------------------------------------------------------------------------------- |
| Install script  | `curl -fsSL https://raw.githubusercontent.com/ph1p/cona/main/install.sh \| sh`                        |
| crates.io       | `cargo install cona`                                                                                  |
| Prebuilt binary | grab it from [releases](https://github.com/ph1p/cona/releases) (Linux, macOS, Windows), put on `PATH` |
| From source     | `git clone https://github.com/ph1p/cona && cd cona && ./install.sh`                                   |

The install script downloads a prebuilt binary — no Rust needed. Running
`./install.sh` from a source checkout builds with cargo (needs Rust ≥ 1.95) and
wires upgrade hooks.

**Staying current is automatic:** every command cheaply checks (at most once a
day) for a newer release and updates itself. Force it with `cona upgrade`.

## Setting up a project

```sh
cona setup          # interactive: index + hooks, then a checklist of agents to wire
cona setup -y       # non-interactive: wire every detected agent
```

`setup` asks once whether to wire this project, your global agent configs, or
both. After that it's automatic. To adjust a single agent later:

```sh
cona agents            # interactive checklist
cona agents status     # what's wired, per agent and scope
cona agents add cursor # wire one agent (add/remove alias install/uninstall)
```

Known agents: `claude`, `agents` (Codex / OpenCode / Amp / Jules via AGENTS.md),
`cursor`, `gemini`, `pi`. Every change is idempotent and marker-based
(`<!-- cona:begin/end -->`) — your own config is never touched.

**Uninstall** mirrors setup: `cona uninstall` (interactive checklist),
`cona uninstall -y` (agents + binary), `--purge` also deletes `~/.cona`.

## MCP server

For hosts without hook support, cona speaks MCP over stdio. `cona setup` /
`cona agents install` registers the server automatically wherever a harness
config exists:

| harness | project scope | global scope |
| --- | --- | --- |
| Claude Code | `.mcp.json` | — (`~/.claude.json` is Claude's own session state) |
| Codex | `.codex/config.toml` | `~/.codex/config.toml` |
| Cursor | `.cursor/mcp.json` | `~/.cursor/mcp.json` |
| Gemini CLI | `.gemini/settings.json` | `~/.gemini/settings.json` |

Entries are written with the absolute binary path, are idempotent, and leave
foreign servers in the same file untouched; `cona agents remove` strips them
again. `cona agents status` has an `mcp` column, `cona doctor` lists every
registered target. Set `CONA_EXE` to pin the command spelling that gets written.

To wire it by hand instead:

```json
{
  "mcpServers": {
    "cona": {
      "type": "stdio",
      "command": "cona",
      "args": ["mcp"]
    }
  }
}
```

Full tool parity with the CLI. The CLI + hook integration is still recommended
(zero context overhead); MCP is the fallback.

## Under the hood

- **Languages (full symbols):** Rust, Python, JavaScript, TypeScript/TSX, Go,
  Java, C, C++, C#, Ruby, PHP, Kotlin, Swift, Scala, Elixir, Dart, Lua, Bash,
  CSS, TOML, YAML, Markdown, Zig, Haskell, OCaml, Julia, PowerShell, Objective-C,
  Protobuf, SQL, Perl, HCL/Terraform, Makefile, Dockerfile. **Search-only:** JSON,
  HTML, Nix, Svelte, Vue, R, XML, GraphQL.
- **Storage:** everything under `~/.cona/` (override with `CONA_DATA_DIR`) — one
  SQLite index per project plus a global registry + usage stats. Housekeeping
  runs itself daily; `cona doctor` shows sizes and paths. If the default home
  directory is read-only (as it often is for sandboxed agents), cona falls back
  to temporary storage and tells you how to make it persistent.
- **Strict sandboxes:** `cona --read-only <query>` inspects an existing index
  without auto-indexing, telemetry, or source/configuration writes. Initialize
  the index first from a writable environment.
- **Incremental & scoped:** only changed files are reparsed, `.gitignore` is
  respected, heavy dirs (`node_modules`, `target`, …) and files > 512 KB are
  always skipped. Your home directory is never auto-indexed.
- **Safe editing:** `edit` re-parses the result and refuses to write on syntax
  errors (`--force` overrides). CRLF stays CRLF.
- **Semantic, name-based:** refs / rename / call graph work on tree-sitter
  identifier nodes (never strings or comments) but without full type resolution —
  ambiguous same-named symbols are marked `·ambiguous`. An optional out-of-process
  stack-graphs helper resolves the hard cases for TS/JS/Python/Rust.

**Token accounting.** `tokens_saved` = a grep-then-read baseline minus the actual
output (4 chars ≈ 1 token, clamped ≥ 0). The baseline models what the _same_
lookup would cost without cona — a targeted read window (±40 lines) around each
hit, capped at the whole file — so a query can never claim to save more than a
naive read. It's a deliberately coarse trend metric, not an accountant.

**The redirect hook** is an accelerator, not a gatekeeper. In an indexed project
it redirects exactly two things — a complete read of a large code file, and a
broad identifier grep — to the cheaper query. Everything else (partial reads,
small files, non-code, regex greps) passes untouched. It always fails open;
`CONA_HOOK_DISABLE=1` turns it off.

**The re-nudge hook** is off by default. Current models keep the habit from the
session-start note alone, and repeating it on a timer is just context noise. On
a model that drifts in long sessions, set `CONA_RENUDGE_EVERY=<n>` to get a
one-line reminder every n tool calls — only in an indexed project, and only as a
hint, never a block. No reinstall needed; the env var alone enables it.

## License

MIT — see [LICENSE](LICENSE).
