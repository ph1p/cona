# cona — Claude Code plugin

Token-efficient code navigation. Reads **one symbol** instead of a whole file,
searches code **semantically** (identifier nodes — never strings or comments),
and edits with **syntax verification**.

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

```sh
/plugin marketplace add ph1p/cona
/plugin install cona@cona
```

Then index the repo once:

```sh
cona index
```

## What you get

| Component      | Effect                                                                     |
| :------------- | :------------------------------------------------------------------------- |
| **Skill**      | `/cona:cona` — the coarse→fine navigation workflow                          |
| **MCP server** | 8 core tools + a `more` gate that unlocks 13 advanced ones                   |
| **PreToolUse** | Redirects large full reads and broad identifier greps to cona (advisory)     |
| **PostToolUse**| Reindexes after edits, so symbol line ranges never go stale                  |
| **SessionStart**| Indexes quietly and prints a repo-orientation map (most-referenced symbols) |

The hooks emit `additionalContext` only — never a `permissionDecision` — so they
advise and never silently bypass the permission system.

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
| `CONA_RENUDGE_EVERY`  | `>0` enables the periodic re-nudge (off by default)         |
| `CONA_ADVISE_MIN_LINES` | Threshold for the mid-size read advisory                 |
| `CONA_NO_FETCH_HELPER` | Skip lazily fetching the semantic-resolution helper       |

## Note on overlap with `cona agents install`

`cona agents install` writes the same skill and hooks directly into
`~/.claude`/`.claude`. This plugin is the *alternative* distribution path — use
one or the other. Running both is harmless (the installer is marker-based and
idempotent) but you'll see the guidance twice.

## Uninstall

```sh
/plugin uninstall cona@cona
```
