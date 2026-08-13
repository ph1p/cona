# CLAUDE.md — cona

Token-efficient code-navigation CLI for AI agents. Rust + tree-sitter + SQLite.

## Build & Test

```sh
cargo build --release        # → target/release/cona
cargo test                   # 192 tests: unit (db, deps, diffmap, editing, entries, fuzzy, gitmap, graph, hook, install, lang, mcp, resolve, ui) + integration (tests/basic.rs, incl. MCP handshake)
cd src/resolve-helper && cargo build --release   # → cona-resolve-helper (separate crate, own tree-sitter 0.24 runtime)
```

Deps current (clap 4.6, rusqlite 0.40, tree-sitter 0.26 + grammars 0.24/0.25,
ratatui 0.30, dirs 6, ignore 0.4, notify 8), caret ranges in `Cargo.toml` (no `=` pins),
rustc 1.95. Supply-chain rule: `Cargo.lock` avoids releases <7 days old — after
`cargo update`, roll back via `cargo update -p <crate> --precise <older>` + `cargo test`.

## Commits & Releases

Conventional Commits mandatory (`feat:`/`fix:`/`refactor:`/`build:`/`ci:`/`chore:`;
`!` or `BREAKING CHANGE:` = major). release-plz, no release PR:
release-plz.yml runs via `workflow_run` only after green CI on main (red CI = no
release; bump commit fires no workflows → no loop) → `release-plz update` commits
bump + CHANGELOG.md to main (`chore: release vX.Y.Z`) → `release-plz release` tags +
GitHub release → explicitly dispatches release.yml (binaries + `cargo publish`, env
`crates-io`) — GITHUB_TOKEN-pushed tags/commits trigger no workflows;
workflow_dispatch is exempt. Optional secret `RELEASE_PLZ_TOKEN` (PAT) overrides.

## Architecture

Full per-module rationale: **`docs/architecture.md`** — read the entry for a
module before you change it. Map only:

```
src/lib.rs       Module root (binary uses the lib; tests import cona::*)
src/main.rs      CLI only (clap) + dispatch to commands/*. 6 command groups
                 (nav/inspect/code/history/project/maint); args live ONCE in
                 *Args structs, shared by group AND hidden flat alias.
src/commands/    cmd_* impls: mod.rs (shared helpers + `defaults` = THE limits +
                 the `--path` policy), query.rs (tree/outline/find/show/refs/
                 context/diff/grep), mutate.rs (edit/insert/note/rename +
                 write_verified), insight.rs, history.rs, callgraph.rs,
                 stats.rs, mcp_server.rs
src/lang.rs      Language detection + tree-sitter symbol extraction. 30+ langs
                 with symbols; refs/grep-only for JSON/HTML/Svelte/Vue/…
                 Semantic identifier search lives here; fail-open textual
                 fallback is ONLY here — never rebuild it elsewhere.
src/indexer.rs   3-phase: walk → parallel parse → one write transaction.
                 EXCLUDED_DIRS + MAX_FILE_BYTES; registered git submodules are
                 walked back in (parse_gitmodules).
src/editing.rs   Pure, tested splice logic for edit/rename (CRLF-preserving)
src/graph.rs     In-memory call graph; narrow_by_scope = THE scope policy
src/gitmap.rs    Git mining (churn/co-change/blame), pure parsers + run_git
src/deps.rs      extract_imports + resolve_import; internal import → edge
src/entries.rs   Pure heuristics for entries/tests
src/fuzzy.rs     fuzzy_score — find's fallback ranking
src/diffmap.rs   Pure diff helpers (parse_unified/overlaps/has_uncovered)
src/resolve.rs   Optional stack-graphs tier (fail-open), separate helper binary
src/hook.rs      PreToolUse/PostToolUse hooks. PreToolUse redirect = the ONLY
                 permissionDecision (deny); every hint path is additionalContext.
                 Reads arrive in TWO shapes: native Read/Grep, and a shell
                 command line (Codex has no Read/Grep — `cat f`/`sed -n`/`rg`
                 come through as tool_name "Bash"), normalized by classify_shell
                 into the SAME try_read/try_grep. Fail-open: one unrecognised
                 segment passes the whole line
src/dashboard.rs `cona ui` — ratatui live TUI, read-only
src/ui.rs        ANSI styling (zero deps) + select/multiselect primitives
src/mcp.rs       MCP framing (stdio JSON-RPC 2.0, pure, tested)
src/install/     install/upgrade/uninstall/agents/doctor; marker-based, idempotent
src/db.rs        SQLite: per-project DB + global.db (registry/usage/meta)
```
## Invariants — DO NOT break

1. `db::project_hash` is FNV-1a and MUST stay stable (test pins
   `07d64e07b49caeb2` for `/x`). Changing it orphans all existing project DBs.
2. Index line numbers are never used blindly. Symbol lookups before line access
   go through `locate_fresh` (locate + is_stale + reindex + re-locate in ONE
   helper) — new commands use it, never rebuild the steps. `context` also
   refreshes caller files, `shape` the files of referenced types.
3. `edit` refuses to write on syntax errors (tree-sitter re-parse) except with
   `--force`. On error the file stays unchanged.
4. `locate_symbol` throws on ambiguity with a candidate list — never silently
   picks. Escape hatches: `Parent.Name`, `--kind`, `file.rs:Name` (path or
   `/`-guarded path suffix — the exact shape the candidate list prints).
5. Keep output compact: the tool exists to save agent tokens.
6. `agents install/uninstall` is idempotent and marker-based
   (`<!-- cona:begin/end -->`) — NEVER touch foreign file content; invalid
   settings.json → warning, don't overwrite.

## Adding a new language

1. Grammar crate in Cargo.toml (e.g. `tree-sitter-go`)
2. `lang.rs`: extension in `detect_lang`, language in `language_for`, node
   kinds in `classify` (label, is_container, name_field)
3. Test case in `tests/basic.rs`
4. Language lists in README.md ("Languages") and CLAUDE.md (src/lang.rs entry)
5. If it is prose/markup/data (no function-like `classify` kind), add it to the
   `has_callable_symbols` deny-list in `lang.rs` — otherwise the read-advisory
   hook tells the agent to `cona show <Symbol>` on a file that has no such
   symbols. `non_callable_languages_are_reachable` guards the reverse mistake
   (a deny-list entry `detect_lang` can never return).

## MCP server (`cona mcp`)

Framing in src/mcp.rs (pure, unit-tested); src/commands/mcp_server.rs = tool
schemas (mcp_tools) + dispatch (mcp_call → cmd_*) over ONE connection per
session (WAL makes external reindexes visible; freshness via locate_fresh).
**Progressive disclosure.** tools/list carries only `CORE_TOOLS`
(find/show/refs/outline/tree/grep/context/edit) plus a `more` gate; the other 13
(insert/batch_edit/check/impact/callers/callees/path/deps/shape/entries/tests/
note/diff) are disclosed by calling `more`. Schemas are re-sent on EVERY request,
so the full 21 cost ~2.6k tokens per turn whether or not one is called — the
whole set is ~1.7k this way. `all_tools()` is the single source both tiers
filter, so they cannot disagree about a schema. **Disclosure MUST go through
tools/list.** A client can only call what tools/list returned — naming a gated
tool in prose leaves it unreachable ("No such tool available"). So `more` returns
`ToolOut::expanding()`, `serve` flips the connection and emits
`notifications/tools/list_changed` (hence `capabilities.tools.listChanged`), and
tools/list then returns everything with the spent `more` retired. `mcp_call`
still dispatches on name alone, so a client that never re-lists is not broken,
just unable to discover the tail. `more` short-circuits BEFORE the lazy
index open — reflecting over static schemas must not build an index or fail in an
unindexed tree.

**outputSchema.** find/refs/outline declare one and therefore MUST return
matching `structuredContent` (`ToolOut::structured`); the payload comes from
re-running the same query with `json = true`, since a cmd_* returns ONE string,
either text or JSON. Text is never dropped — the spec keeps `content`
authoritative. A parse/query failure on the second pass degrades to text-only:
structured content is an optimisation, a failed call is a lost answer.

Tool parity with CLI: find/show/refs/outline/tree/grep/context/diff/edit +
batch_edit/insert/check/impact/callers/callees/path/deps/shape/entries/tests/
note (tests/basic.rs handshake pins parity across BOTH tiers). batch_edit = multiple symbol edits
in ONE call, each syntax-verified; stops at first error (applied edits remain,
progress reported — no silent partial state). edit runs through cmd_edit_code
(replacement as &str — stdin belongs to the protocol). Usage logged `mcp:<tool>`.
SUPPORTED_PROTOCOLS is newest-first and MUST stay so (negotiate_protocol answers
with `[0]` on an unknown request); newest = 2025-11-25, whose additions (`_meta`,
`icons`, `outputSchema`) are all optional, so existing payloads stay valid.
Registration into harness configs is NOT here — it is install-time
(src/install/mcp_config.rs), run by `agents install`/`setup`.

## Plugin (`plugin/`)

ONE directory, TWO harness manifests: `.claude-plugin/plugin.json` (Claude Code)
and `.codex-plugin/plugin.json` (Codex) over a SHARED payload (`skills/`,
`.mcp.json`, `hooks/hooks.json`) — nothing to keep in sync. Marketplace manifests
sit where each CLI looks: `.claude-plugin/marketplace.json` and
`.agents/plugins/marketplace.json`, both at the repo root.
Hook payloads are wire-compatible across both harnesses, so hook.rs has NO
per-harness branch; what differs is the tool (Codex ships only a shell — see the
hook.rs entry). **The PreToolUse matcher is declared TWICE and the two must
agree**: `plugin/hooks/hooks.json` (plugin path) and the `specs` table in
install/agents.rs `claude_hooks` (`agents install` path). Reinstall reconciles a
drifted matcher, so widening it self-heals existing installs.
Codex specifics that bite: a `local` source is COPIED into
`~/.codex/plugins/cache/<mkt>/<plugin>/<version>/`, so editing the checkout does
nothing until `codex plugin add` runs again (silent — old hooks keep firing);
hooks are hash-trusted (`--dangerously-bypass-hook-trust` skips the prompt once).
Details: docs/architecture.md, user-facing docs: plugin/README.md.

## Setup / uninstall UX

`cona setup` always indexes + (project scope) installs git hooks, then picks
agents. Installing an agent also registers the MCP server wherever that
harness keeps its config (mcp_config.rs) — written with the ABSOLUTE binary
path from agent_exe(). Skipped when the parent dir does not exist (except the project root), and a
failure there warns instead of aborting the install. Interactive = no `-y`, no explicit scope arg, TTY → pick_agents shows
ONE ui::multiselect across BOTH scopes (PROJECT + HOME sections via Row::Header,
items pre-checked by `installed() || detected()` — reality first, detection only
as the first-run suggestion; descriptions state the row's state via
AgentName::state_desc, so remove-on-uncheck is visible before enter). **Setup is also the manage surface: unchecking an
installed agent REMOVES it.** pick_agents diffs checked-now vs installed-before
into a per-scope ScopePlan{add,remove}; cmd_setup uninstalls then installs
(still-checked agents are re-installed — idempotent, refreshes stale marker
blocks after a version bump). Non-interactive (`-y`, an explicit
`project|global|all`, or non-TTY) installs every detected agent in the active
scopes and removes NOTHING — there is no checklist to diff against, so nothing
expresses "I want this gone", and `-y` must never delete files under ~/.claude.
Not a special case: the same ScopePlan with an empty `remove` half.
SetupScope still gates which sections/hooks run.
**Every user-facing command prints its OWN title** (banner or `▸ heading`) —
never the dispatch site, never the caller. `cmd_hooks` prints `▸ git hooks`
itself, so `cona hooks` and setup's section both get it and a third caller
cannot forget it.
`cona uninstall` mirrors this: interactive (no `-y`, TTY) → ui::multiselect of
agents/binary/data (a UninstallPlan); non-interactive/`-y` → full teardown
(agents + binary), `~/.cona` only with `--purge`. Data removal is always
confirmed separately (irreversible). remove_all_agents/remove_binary are the
shared executors. `cona install` ends with print_next_steps(); install.sh's
binary-download path prints its own next-steps heredoc — keep the two in sync.

## Known limits / roadmap

- `refs`/`context`/`tree --rank` are semantic (identifier nodes) but name-based
  — no real type resolution. narrow_by_scope handles common cases; rest stays
  `·ambiguous`. Full resolution = LSP integration.
- Unsupported languages fall back to textual word-boundary scan (fail-open,
  matches strings/comments there).
- Token estimate = 4 chars ≈ 1 token (heuristic, trend metric).
- Git submodules are indexed as part of the parent project (one DB), not as
  separate projects — a submodule shared by two repos is indexed twice.
  Unregistered nested repos (a `.git` with no `.gitmodules` entry) stay skipped.
- `--path` resolves directory-vs-prefix by stat-ing the filter, so a filter that
  names a dir which no longer exists on disk degrades to the prefix reading.
- `grep` matches literally by DEFAULT, regex only on `--regex`/`-e`. Literal is
  the default because `foo.bar`/`Vec<T>` are ordinary code — reinterpreting them
  would change what existing queries mean. `regex` is a direct dependency at
  zero cost: it was already in `Cargo.lock` via `tree-sitter` (`cargo tree -i
  regex`), so it adds no crates and ~32 KB of binary. A regex is matched
  in-process by the `regex` crate, and rg/grep get the matching flag so the
  prefilter agrees.
- `rename` semantic (identifier positions) but name-based: collision guard +
  syntax verify + all-or-nothing rollback; unparsable files only with --force.
- `deps`: internal imports → edges; external packages counted + listed but not
  resolved into a real graph (no versions/transitive); std invisible.
- `callees`/`path` follow call positions only: callbacks (`map(f)`) create no
  callee edge (caller side sees them).
- Arity disambiguation (rule 4 in narrow_by_scope, SHIPPED) separates same-named
  defs of differing param count. Phase 2: receiver-type hints (real `x.foo()`
  type resolution).
- stack-graphs tier (resolve.rs, SHIPPED — details in Architecture) separates
  same-arity same-named defs. Open subtleties: (line,name) addressing can't
  separate two same-name calls on ONE line; rust.tsg covers inherent methods
  with `let x = T{..}`/`T::ctor()` typing — no traits/generics/macros/path
  resolution (fail-open → empty defs); `use` imports best-effort. Semantic
  filtering of refs = follow-up. Findings: docs/spike-semantic-resolution.md

## Statistics schema (global.db)

`usage(ts, project, cmd, ms, results, tokens_out, tokens_saved, detail)` —
every query logs via `db::log_usage`/`log_usage_detail`. `tokens_saved` =
baseline − actual output, clamped ≥0. Baseline = **grep-then-Read** model
(`db::baseline_tokens`): grep pass (≈free) + targeted Read window
(±`READ_PAD_LINES`=40, overlapping windows merged) per hit, capped at whole
file — NOT whole file per hit (that inflated symbol-scoped show/refs/grep
2–3×). show/refs/grep pass hit lines through it; context/diff keep whole-file
for the extra files they force open (you'd genuinely reopen those). Each
`cmd_*` computes baseline inline from line lengths its scan already holds,
returns `(String, i64)`. `detail` = query target (symbol/file) for top-target
aggregation; added via guarded migration (`ALTER TABLE … ADD COLUMN`,
`column_exists`). Hook logs redirected reads as `cmd = "hook:read-block"`,
greps as `"hook:grep-block"`, and the advisory (non-blocking) outcomes as
`"hook:read-advise"` (mid-size ≥`CONA_ADVISE_MIN_LINES` or repeat read of a
path already read this session), `"hook:read-streak"` (every
`CONA_READ_STREAK`-th full read in one session) and `"hook:read-nudge"` /
`"hook:grep-nudge"` (unindexed repo) — all count lines with
`tokens_saved = 0` (saving credited to the follow-up query, else
double-counting). `cmd_grep` prefilters candidates
via rg (fallback system grep, else full scan — fail-open); hit labeling stays
with the index. `db::is_maintenance_cmd` (index/edit/rename/note/hook:*)
separates maintenance from query lines; `stats` + TUI show maintenance as
compact one-liners under the query table, never in savings columns. Aggregate
helpers (totals/per_command/top_targets/recent/savings_series) feed both
`stats` and the `ui` dashboard.
