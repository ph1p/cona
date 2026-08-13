# cona — Claude Code & Codex plugin

Token-efficient code navigation. Reads **one symbol** instead of a whole file,
searches code **semantically** (identifier nodes — never strings or comments),
and edits with **syntax verification**.

This one directory is a plugin for **both** harnesses. Claude Code reads
`.claude-plugin/plugin.json`, Codex reads `.codex-plugin/plugin.json`, and the
payload underneath — `skills/`, `.mcp.json`, `hooks/hooks.json` — is shared
byte-identically. There is nothing to keep in sync between the two.

## Prerequisite

The plugin drives the `cona` binary; install it first so it is on `PATH`:

```sh
curl -fsSL https://raw.githubusercontent.com/ph1p/cona/main/install.sh | sh
# or
cargo install cona
```

Every hook is guarded with `command -v cona`, so if the binary is missing the
plugin stays inert rather than erroring on each tool call. Nothing else breaks —
but you also get no savings until it is installed.

## Install

**Claude Code** — from the slash prompt:

```sh
/plugin marketplace add ph1p/cona
/plugin install cona@cona
```

**Codex CLI** — from a checkout of this repo (the marketplace manifest lives at
`.agents/plugins/marketplace.json`):

```sh
git clone https://github.com/ph1p/cona
codex plugin marketplace add ./cona
codex plugin add cona@cona
```

Then index the repo once:

```sh
cona index
```

Two Codex specifics worth knowing:

- **A `local` source is copied, not linked.** `codex plugin add` snapshots the
  plugin into `~/.codex/plugins/cache/cona/cona/<version>/`. If you edit the
  plugin in your checkout, re-run `codex plugin add cona@cona` or the running
  copy stays on the old files.
- **Hooks are hash-trusted.** Codex asks before running a plugin's hooks the
  first time and after every change to them. `--dangerously-bypass-hook-trust`
  skips that prompt for one invocation — useful when scripting, not a default.

## What you get

| Component      | Effect                                                                     |
| :------------- | :------------------------------------------------------------------------- |
| **Skill**      | `/cona:cona` — the coarse→fine navigation workflow                          |
| **MCP server** | 8 core tools + a `more` gate that unlocks 13 advanced ones                   |
| **PreToolUse** | Redirects a large full read or a broad identifier grep to cona              |
| **PostToolUse**| Reindexes after edits, so symbol line ranges never go stale                  |
| **SessionStart**| Indexes quietly and prints a repo-orientation map (most-referenced symbols) |

Only the PreToolUse redirect returns a `permissionDecision` (`deny` plus the
cona command to run instead), and only for those two shapes. Every other hook
output is `additionalContext` — advice that leaves the permission flow
untouched. Nothing is ever auto-*allowed*: cona never bypasses a decision the
harness or you would otherwise make.

### Reads that arrive as shell commands

Claude Code has first-class `Read` and `Grep` tools, and the hook matches those
by name. Codex does not: it runs `cat f`, `sed -n '1,400p' f` and `rg Foo`
through its shell tool, so the payload arrives as `tool_name: "Bash"` with a
`/bin/zsh -lc "…"` command line. The hook parses that command line and treats a
recognised whole-file read or broad grep exactly like the native call.

It fails open, deliberately:

- One unrecognised segment makes the whole line pass. `wc -l f && sed -n '1,50p' f`
  is classified (both halves are reads/probes); `sed -n '1,50p' f && cargo build`
  is not, because blocking the line would block the build too.
- An unrecognised wrapper passes. If a shell proxy prefixes your commands
  (`myproxy cat big.rs`), cona does not recognise the program and stays out of
  the way.
- A genuinely partial read passes. `head`, `tail` and a `sed -n '1,240p'` whose
  bound is under the real line count are what cona wants you to do anyway.

## Core commands

```sh
cona tree --rank            # orient in an unknown codebase
cona outline <file>         # every symbol in a file, with line ranges
cona show <Sym>             # the source of exactly ONE symbol
cona context <Sym>          # that symbol + callee signatures + call sites
cona grep <pattern>         # code-only search (literal; --regex for a regex)
cona refs <Name>            # semantic usage sites
cona edit <Sym>             # syntax-verified write
cona impact <Sym>           # blast radius before a change
```

`<Sym>` is `Name`, `Parent.Name`, or `file.rs:Name`. Ambiguity is never resolved
silently: cona errors with the candidate list and the exact requalify syntax.

## Configuration

| Variable              | Effect                                                     |
| :-------------------- | :--------------------------------------------------------- |
| `CONA_DATA_DIR`       | Where indexes live (default `~/.cona`)                     |
| `CONA_HOOK_DISABLE`   | Set to anything — turns every hook off without uninstalling  |
| `CONA_RENUDGE_EVERY`  | `>0` enables the periodic re-nudge (off by default)         |
| `CONA_ADVISE_MIN_LINES` | Threshold for the mid-size read advisory                 |
| `CONA_NO_FETCH_HELPER` | Skip lazily fetching the semantic-resolution helper       |

## Note on overlap with `cona agents install`

`cona agents install` writes the same skill and hooks directly into the
harness's own config (`~/.claude`/`.claude` for Claude Code, `AGENTS.md` plus
`~/.codex/config.toml` for Codex). This plugin is the *alternative* distribution
path — use one or the other per harness. Running both is harmless (the installer
is marker-based and idempotent) but you'll see the guidance twice.

## Uninstall

```sh
/plugin uninstall cona@cona          # Claude Code

codex plugin remove cona@cona        # Codex
codex plugin marketplace remove cona
```
