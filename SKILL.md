---
name: cona
description: Token-efficient code navigation. Use INSTEAD of reading whole files or grep when you need to locate, inspect, or edit specific functions/classes in a codebase.
---

# cona — navigate code by symbols, not by reading files

## The 90% case — start here

```
cona outline <file>        # every symbol in the file + exact line ranges
cona show <Sym>            # print ONLY that symbol's source
cona context <Sym>         # its source + what it calls + who calls it, in ONE call
cona grep <Name>           # semantic search; each hit labelled with its symbol
```

**The bright line:** reading a whole file over ~100 lines to understand ONE function is
always the wrong move — `outline` then `show`. Two cheap calls beat one expensive read.
A full Read is right only when you genuinely need the whole file (about to rewrite it).

Never re-read a file already in your context — including one the user pasted or
`@`-mentioned. Those bytes are already spent; `show` the symbol you need instead.

`<Sym>` = `Name`, `Parent.Name` (methods), or `file.rs:Name` (disambiguates).

Everything below is reference — reach for it when the four above don't cover the job.

## Commands
- `cona index` — build/update the index (fast, incremental; auto-runs on first use); `--watch` keeps running and reindexes on file changes (debounced). If the default `~/.cona` is read-only (common in sandboxes), cona uses temporary storage; set `CONA_DATA_DIR` to keep an index across sessions.
- `cona --read-only <query>` — inspect an existing index without auto-indexing, telemetry, or any source/configuration changes; use this in a strict read-only agent sandbox.
- `cona tree --budget 2000` — compact overview of the whole codebase within a token budget
- `cona tree --path src/api` — overview of a subdirectory
- `cona tree --rank` — top-level symbols ranked by reference fan-in (most load-bearing first) — the fastest way to orient in an unknown codebase
- `cona outline <file>` — every symbol in a file with exact line ranges
- `cona find <Name>` — locate a symbol: kind, file:start-end, signature
- `cona find <Name> --kind fn --json` — filtered / machine-readable
- `cona show <Sym> [<Sym2> …] [--context 3] [--kind struct] [--sig] [--all]` — print ONLY those symbols' source (numbered lines); pass several names to read them in one call; `--kind` resolves struct/impl same-name ambiguity; `--sig` prints just the signature line (no body — leanest peek, reads nothing off disk); `--all` prints EVERY definition of an ambiguous name instead of erroring — use it when you'd otherwise re-run with a disambiguator just to see which one you want. `cona show <file>` outlines that file
- `<Sym>` anywhere = `Name`, `Parent.Name` (methods), or `file.rs:Name` (same-named top-level symbols in different files — the shape ambiguity errors print)
- `cona refs <Name>` — all call/usage sites as file:line (semantic: identifier nodes only, string/comment mentions don't match)
- `cona grep <pattern> [-i] [--regex] [--limit 50]` — search over code files only; each hit shows its enclosing symbol (`file:line (in Class.method): …`) so you can jump straight to `show`. **Literal by default** — `foo.bar` matches that text, not "any char"; pass `--regex` (`-e`) for a real regular expression (Rust regex syntax). An invalid regex is an error, never a silent literal search
- `--path <dir-or-file>` scopes `find`/`refs`/`grep` (and `tree`) to a subtree — the fix for a name with 100+ hits repo-wide. A filter naming a real directory matches only inside it; a partial name (`src/comm`) prefix-matches
- `cona diff [ref]` — changed SYMBOLS instead of changed lines (default vs HEAD, includes uncommitted + untracked); review a branch via `cona diff main` then `show` the touched functions
- `cona context <Sym> [--budget 3000] [--no-tests]` — ONE context pack: the symbol's source + signatures of what it calls + where it's called from. Use this instead of chaining show + refs + more shows when working on a function. The source is always printed in full; --budget bounds only the calls/called-by sections. Callees whose name matches several definitions are marked `·ambiguous`. `--no-tests` drops test callers (heavily-tested symbols otherwise fill the caller list with test fns); the hidden count is reported
- `cona edit <Class.method> --file new.txt` — replace a symbol's body; syntax-verified, rolls back on parse errors. Or pipe: `cat new.txt | cona edit Foo.bar`
- `cona edit <file> --range S-E` — replace just absolute lines S-E of a file (patch a few lines without resending a whole symbol); same syntax-verify + rollback
- `cona insert <Sym> --after|--before` — add new code next to a symbol without touching its body; whole-file syntax re-verified. `--file` or pipe like `edit`
- `cona insert --at <file> <line>` — insert at an absolute position (0 = prepend, past EOF = append); works on a brand-new/empty file with no indexed symbol
- `cona check [<file>]` — tree-sitter parse diagnostics (NOT a compiler; syntax only) with the enclosing symbol per error; no file = every file changed vs HEAD. Confirm a file still parses after a manual edit without shelling a build
- `cona impact <Sym>` — pre-edit blast radius in one pack: references + immediate callers + tests + recent history. "Is it safe to change?"
- `cona entries` — where execution starts: main fns, exported/public API, test summary. First command in an unknown repo
- `cona deps [path]` — file-level import graph over indexed files + most-imported + cycle warnings — the architecture view
- `cona callers <Sym> [--depth 2]` / `cona callees <Sym>` — transitive call tree up/down. Callee edges follow only real CALL sites (local vars/types never fabricate edges); shared names flagged ·ambiguous
- `cona path <A> <B>` — shortest call chain from A to B ("how does the request reach the DB?")
- `cona tests <Sym>` — which tests exercise a symbol; says loudly when NOTHING does
- `cona blame <Sym>` — git history of exactly that symbol's lines (who, when, why)
- `cona hot [--since '6 months ago']` — churn hotspots among indexed files
- `cona coupling <file>` — files that historically change together with this one (hidden dependencies)
- `cona shape <Sym>` — a symbol's source + the types it references, expanded one level (small types in full)
- `cona note <Sym> <text…>` — attach a persistent note (invariants, gotchas); notes surface automatically in show/context. `cona note` lists, `--rm <id>` deletes
- `cona rename <Sym> <new>` — project-wide identifier rename: semantic occurrences only, collision-guarded, syntax-verified, all-or-nothing

## Workflow for "change function X"
1. `cona context X` → its source + what it calls + who calls it, in one shot
   (or step by step: `find X` → `show X` → `refs X`)
2. write replacement, then `cona edit X --file repl.txt`
3. edit re-verifies syntax and re-indexes automatically

## Rules
- **Wanting one symbol means `show`, not Read.** `find`/`show` reach one function for a fraction of a whole-file read. This is not a preference to weigh against habit — a full Read of an indexed file you only needed one function from is a mistake, at any file size.
- **A file already in context is never re-read.** Pasted, `@`-mentioned or Read earlier in the session — the bytes are spent; `show`/`context` the symbol instead.
- Prefer qualified names (`UserService.login`) to disambiguate.
- Use `--json` when you need to parse output programmatically.
- Line numbers in the index can go stale after manual edits — `cona index` refreshes in ms (or install git hooks: `cona hooks install`).
- `--json` works on find, outline, show, tree, refs, grep, diff, context, stats, entries, tests, blame, hot, coupling, callers, callees, path, shape, deps, check, impact.
- Orientation in an unknown repo: `entries` → `tree --rank` → `deps` → `hot`. Before editing: `context`/`shape` + `tests` + `blame`. Leave `note`s for the next agent.
- `cona setup` = index + git hooks + agent integration in one shot.
- `cona stats` (per-project + global: savings, top targets, recent) shows how many tokens you've saved.
- `cona ui` opens a live dashboard of index state + token savings in real time.
- With agent hooks installed, a full `Read` of a large indexed code file is redirected here — reach for `outline`/`show` first; pass an explicit offset/limit to force a full read. Mid-size files (≥120 lines), repeat reads of the same file, and a run of several full reads in one session get a non-blocking hint instead. Tune with `CONA_READ_MAX_LINES` (block threshold, default 300), `CONA_ADVISE_MIN_LINES` (hint threshold, default 120, 0=off), `CONA_READ_STREAK` (reads per session before the volume hint, default 4, 0=off).

## Lifecycle (rarely needed)
- `cona install` / `./install.sh` — install binary + upgrade git hooks (run from the source checkout)
- `cona upgrade` — rebuild when the source checkout changed, else update to the newest release (also auto-triggers in the background, remote check ≤1×/day)
- `cona agents install|uninstall [names…] [--all] [--global]` — inject/remove agent configs (Claude Code, AGENTS.md, Cursor, Gemini); no names = autodetect installed
- `cona uninstall [--purge]` / `./uninstall.sh` — remove binary, hooks and global agent files (`--purge` deletes ~/.cona)
