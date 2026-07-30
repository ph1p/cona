# CLAUDE.md — cona

Token-efficient code-navigation CLI for AI agents. Rust + tree-sitter + SQLite.

## Build & Test

```sh
cargo build --release        # → target/release/cona
cargo test                   # 141 tests: unit (db, deps, diffmap, editing, entries, fuzzy, gitmap, graph, hook, install, lang, mcp, resolve, ui) + integration (tests/basic.rs, incl. MCP handshake)
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

```
src/lib.rs       Module root (binary uses the lib; tests import cona::*)
src/main.rs      CLI only (clap) + dispatch to commands/* (+ cmd_setup).
                 6 command groups (nav/inspect/code/history/project/maint, one
                 subcommand enum each) = canonical grouped --help. Args live ONCE
                 in *Args structs (clap::Args), shared by group AND flat alias;
                 every flat shorthand (`cona show …`) kept as hidden alias
                 (backward compat + fewer agent tokens). run() matches Cmd
                 directly; grouped + flat share ONE body via or-pattern — no
                 second dispatch enum. hooks/agents take IntegrationAction
                 (ValueEnum), not a raw String.
src/commands/    cmd_* implementations, split by concern:
                 mod.rs   shared: open_indexed/finish/jout/BudgetOut/locate_*/
                          render_symbol_body/scan_ref_sites/ENCLOSING_SYMBOL_SQL;
                          `defaults` module = THE per-command limits/budgets,
                          used by clap default_value_t AND the MCP fallbacks
                 query.rs tree/outline/find/show (multi-symbol; --sig =
                          index-only signature peek, no body read)/refs/context/
                          diff/grep
                 mutate.rs edit / edit --range / insert (--before/--after <Sym>
                          OR --at <file> <line>, incl. new/empty file) / note /
                          rename. write_verified = THE shared syntax-verify +
                          write gate for all edit paths (invariant 3)
                 insight.rs entries/tests/shape/deps/check/impact (check =
                          syntax_errors as command; impact = refs+callers+tests+
                          blame in ONE pack)
                 history.rs blame/hot/coupling
                 callgraph.rs callers/callees/path + build_graph
                 stats.rs, mcp_server.rs (mcp_tools/mcp_call/cmd_mcp)
src/lang.rs      Language detection + tree-sitter symbol extraction (classify()).
                 Symbols: Rust, Python, JS/TS/TSX, Go, Java, C, C++, CSS, Ruby,
                 PHP, C#, Kotlin, Swift, Bash, Lua, Scala, Elixir, Dart, TOML,
                 YAML, Markdown, Zig, Haskell, OCaml, Julia, PowerShell, ObjC,
                 Protobuf, SQL, Perl, HCL/Terraform, Makefile, Dockerfile.
                 Refs/grep only (no symbols): JSON/HTML/Nix/Svelte/Vue/R/XML/
                 GraphQL. Name sentinels in node_name: FIRST_CHILD (CSS selector,
                 Bash word, TOML key, PowerShell/ObjC/Proto), DEF_CALL (Elixir —
                 everything is a `call`, name = def/defp/defmodule arg), HEADING
                 (Markdown), DOCKER_FROM (stage = AS alias, else image_spec),
                 HCL_BLOCK (everything is a `block`; name = type + string labels
                 via `.`, e.g. resource.aws_instance.web), NESTED (name is a
                 known descendant kind behind wrappers — OCaml bindings, Julia
                 signature/type_head, SQL object_reference, Perl package_name,
                 ObjC method, Makefile target; per-language in nested_name).
                 Makefile+Dockerfile detected via basename (Dockerfile also
                 Dockerfile.<flavour>/<name>.dockerfile/Containerfile).
                 Vue+Dockerfile grammars vendored (vendor/*, build.rs + cc) —
                 their crates.io crates pin an incompatible tree-sitter runtime
                 (C symbol collision); bound via extern "C".
                 Kotlin/Swift: interface/struct/enum collapse to
                 class_declaration → label "class". C/C++: node_name drills into
                 nested declarators; needs_body filters bare struct refs.
                 Semantic identifier search (identifier nodes only — strings/
                 comments never match): ref_lines() (refs + context caller),
                 idents_in_range() (context callees), ident_counts() (tree
                 --rank). Fail-open fallback (unparsable → textual scan via
                 each_ident_token) lives ONLY here — never rebuild elsewhere.
src/indexer.rs   3-phase: walk → parallel parse → one write transaction.
                 EXCLUDED_DIRS (node_modules/target/… pruned even without git) +
                 MAX_FILE_BYTES (512 KB) keep index small. watch_project
                 (`index --watch`, notify, 300ms debounce) runs through
                 index_project — the ONE write path.
src/editing.rs   splice_lines/splice_insert/apply_renames — pure, tested splice
                 logic for edit/rename (CRLF-preserving, right-to-left per line).
                 join_lines = shared tail assembly; a source with no final
                 newline keeps that state (no spurious EOF-newline diff flip)
src/graph.rs     In-memory call graph (one pass over all files): callers_of/
                 callees_of/path. Name-based. narrow_by_scope = THE scope
                 preference policy (used by prefer_scope AND cmd_context): on
                 multiple defs ONE candidate wins (1) same parent scope, (2)
                 same file, (3) same directory, (4) matching arity (call arg
                 count == declared param count; methods minus implicit receiver,
                 first_param_is_receiver in lang.rs) — each rule fires ONLY on
                 exactly one survivor, never guessing among equals. Signals in
                 struct Candidate; arg count via call_node_of/arg_count_of
                 (lang.rs), param count via param_count. Callee EDGES only from
                 call positions (fn call/method call/macro) — locals create no
                 false edges. `uses` (caller side) keeps ALL occurrences (types,
                 callbacks). Residual ambiguity → ·ambiguous
src/gitmap.rs    Git mining, pure parser + run_git: parse_numstat/churn/
                 co_change (hot/coupling), parse_log_headers for blame
                 (git log -L, \x01-marked headers, diff hunks skipped)
src/deps.rs      extract_imports (line-based, fail-open; multi-line Rust use,
                 expand_use_tree for nested brace groups) + resolve_import
                 (rust crate::/self::/super::/own crate via
                 crate_names_from_manifests; python absolute/relative; js
                 relative with extension swap + index.*). Internal import →
                 edge. external_name mirrors the resolve_* skip logic:
                 unresolvable → external package name (rust first segment minus
                 std/core/alloc/crate/self/super/self_crates; python/js
                 top-level, js scoped @scope/name); cmd_deps counts 1× per
                 importing file, local module names filtered → "── external
                 deps ──". mutual_pairs = cycle signal
src/entries.rs   Pure heuristics for entries/tests: entry_class (main/api/test),
                 is_test_path/is_test_symbol (all-language conventions)
src/fuzzy.rs     fuzzy_score — fallback ranking for find when exact+LIKE empty
src/diffmap.rs   Pure, tested diff helpers for diff: parse_unified (git hunks →
                 line ranges), overlaps, has_uncovered (keep container when its
                 own lines changed)
src/resolve.rs   Optional semantic resolution tier (fail-open): spawns
                 cona-resolve-helper (src/resolve-helper/, own tree-sitter
                 0.24 runtime via stack-graphs — NOT in the workspace, excluded
                 from the package) for ts/tsx/js/python AND rust (rust via
                 hand-authored rust.tsg — no published TSG crate; include_str! +
                 LanguageConfiguration::from_sources at runtime). stdin/stdout
                 JSON, refs addressed by (line, name). Request carries optional
                 `deps` (extra files stitched into the SAME stack graph →
                 cross-file resolution); resolved defs report their `file`.
                 resolve_refs caches process-wide, keyed (lang, path+mtime,
                 deps+mtimes, refs) — repeated ambiguity in one run is free;
                 helper stays stateless. Binary discovery:
                 CONA_RESOLVE_HELPER → sibling path → PATH; missing/crash/
                 unsupported → None (caller keeps name-based result).
                 resolve::disambiguate = THE shared policy (context AND
                 callers/callees): ask helper (with deps), keep only resolved
                 defs coinciding with a candidate line (name,file,line) —
                 stack-graphs intermediate nodes (import bindings) never mask
                 the real resolution. cmd_context collapses ·ambiguous on
                 exactly one survivor; cmd_calls narrows ambiguous callees at
                 the call site (line carried in graph::calls). Only on ambiguity
                 — never in the index write path. rename stays deliberately
                 name-based (refs carry OLD name, collision def the new one → no
                 overlap to filter; ambiguous target = error per invariant 4).
                 Distribution "all in-tool": release.yml bundles the helper per
                 target (best-effort) into the same tarball; install.sh +
                 install/upgrade place it next to cona; cargo-install users
                 lazily fetch from GitHub release (~/.cona/bin, 24h backoff,
                 opt-out CONA_NO_FETCH_HELPER); doctor reports status. NEVER
                 a cargo dependency of cona (links collision). Findings:
                 docs/spike-semantic-resolution.md
src/hook.rs      PreToolUse + PostToolUse hooks (`cona hook <event>`):
                 PreToolUse (matcher Read|Grep) = pure decide_read/decide_grep +
                 fail-open runner; redirects large full reads + broad identifier
                 greps (indexed project, no glob/type/path filter) to cona; hook
                 NEVER creates a DB. try_read gates (partial/non-code/metadata-
                 size) BEFORE reading bytes — multi-GB file must not be slurped
                 just to be allowed. PostToolUse (no matcher) = periodic re-nudge,
                 OFF by default (DEFAULT_RENUDGE_EVERY = 0): repeating the same
                 rule across SessionStart + guide + timer is over-constraint for
                 current models. Opt in with CONA_RENUDGE_EVERY=<n> → a
                 per-(project,session) tool-call counter (tick_toolcall) emits a
                 one-line reminder every n calls in an indexed project
                 (should_renudge = pure, tested). Hook entry stays registered but
                 shell-gated (`[ "$CONA_RENUDGE_EVERY" -gt 0 ] && …`) — the env
                 var alone flips it on, no reinstall, and disabled the cona
                 binary never even spawns. Hot-path order: cheap project_db_path stat
                 + counter tick gate BEFORE the expensive has_index DB-open, so
                 non-nudging calls never open the DB.
                 tick_toolcall + nudge_once share session_marker_path (the ONE
                 session-identity rule: CLAUDE_SESSION_ID, else per-day bucket).
                 ALL hint paths emit additionalContext ONLY, never a
                 permissionDecision ("allow" would silently bypass the permission
                 system)
src/dashboard.rs `cona ui` — ratatui live TUI, read-only. DBs opened ONCE (not
                 per tick); cheap usage stats refresh 1s, the expensive index-
                 state scan (one fs stat/file) throttled to 5s. Keys: q quit,
                 p scope, s sort by-command (saved/calls/avg-ms), r force
                 refresh; Resize handled. Requires a TTY (else clean error, no
                 panic)
src/ui.rs        ANSI styling (zero deps): NO_COLOR/CLICOLOR_FORCE/TERM=dumb +
                 IsTerminal — piped output stays plain (agents!). All CLI colors
                 run through here; clap help styles in main.rs. ui::select = THE
                 raw-mode single-select; ui::multiselect = THE raw-mode
                 checklist over ui::Row (Header|Item) — headers/spacers skipped
                 by the cursor, `a` toggles all, returns a bool-per-row mask;
                 both share ONE drop-guard prompt discipline — never re-roll
src/mcp.rs       MCP framing (stdio JSON-RPC 2.0, newline-delimited, pure over
                 BufRead/Write, tested): initialize/ping/tools/list/tools/call;
                 tool error = result with isError, never a protocol error.
                 initialize negotiates protocol version (negotiate_protocol over
                 SUPPORTED_PROTOCOLS, newest-first — echo when supported, else
                 our latest; never blindly echo unknown) + serverInfo.name+title.
                 tool_annotated attaches behaviour hints; read_only (queries) vs
                 writes (edit/insert/batch_edit/note, destructiveHint on
                 body-overwriting tools)
src/install/     install/upgrade/uninstall/agents/doctor:
                 mod.rs   SKILL_MD = include_str!("../../SKILL.md") (single
                          source); marker-block + write_if_changed primitives;
                          idempotent writes (Change: Created/Updated/Unchanged),
                          upsert_block/remove_block (tested); GITHUB_REPO/
                          USER_AGENT + fetch_release_archive = THE release
                          download/extract path (self-upgrade AND helper fetch)
                 upgrade.rs cmd_install/upgrade/uninstall/hooks; ONE git-hook
                          policy: append_hook_line/strip_git_hook_lines.
                          Auto-upgrade: mtime check + remote check vs crates.io
                          sparse index (curl, fail-open) ≤1×/day (meta
                          last_remote_check) → fetches release binary from
                          GitHub, so upgrade works without source_dir
                          (cargo-install users). After ANY binary swap,
                          cmd_upgrade → refresh_config: re-runs idempotent
                          `agents install` (quiet) globally + in every
                          registered project (db::registered_project_paths)
                          that has cona config — SKILL/guide/hooks
                          (include_str!, baked at build) never lag the binary —
                          and stamps config_ver:<path>=CARGO_PKG_VERSION.
                          Freshness VERSION-keyed, not timed: maybe_auto_update
                          → maybe_refresh_project_config compares
                          config_ver:<root> to env!("CARGO_PKG_VERSION") FIRST
                          (one sqlite read on hot path); only on mismatch does
                          the fs scan + quiet re-sync — self-heals stale project
                          once per version (!= catches downgrades, no unbounded
                          meta rows)
                 agents.rs GUIDE_MD; subagent_defs = THE .claude/agents
                          enumeration, RECURSIVE (collections nest by category:
                          design/, engineering/, …; a flat read_dir misses every
                          def) — consumed by BOTH sync_subagents and
                          project_has_cona, so a nested-only footprint still
                          counts as installed (else uninstall + the version-gated
                          re-sync skip the scope). Bounded: SUBAGENT_MAX_DEPTH +
                          file_type() (no symlink follow). Subagents can't be
                          reached by one shared file — they run on their own
                          system prompt — so per-def marker blocks are the only
                          mechanism; sync_subagents reads each file ONCE (gate +
                          splice share it; upsert_block_file would re-read) and
                          patches only YAML-frontmatter defs (never README/
                          runbook prose), uninstall strips any marked .md;
                          cmd_agents (thin) + cmd_agents_q (fully
                          silent, reports changes via per-mark Mark.changed —
                          no output-string scanning); claude_hooks (settings.json
                          via serde_json); AgentName ValueEnum + AgentSel::want =
                          THE selection rule: named/--all override detection,
                          bare install autodetects, bare uninstall cleans all.
                          AgentName::config_paths = THE per-scope file list an
                          agent's integration lives in (Pi empty at project
                          scope); installed() probes it for real markers/skill/
                          hook — feeds cmd_agents_status (per-agent×scope ✓/–/
                          n/a table + copy-paste manage hints) and
                          cmd_agents_interactive (pre-checked checklist; diff of
                          before/after → install added, uninstall removed).
                          Restart-note gated on a claude-labeled mark actually
                          moving. CLI: `agents [status|add|remove|install|
                          uninstall]`, action optional (bare+TTY → interactive,
                          bare+names → add, bare non-TTY → status); add/remove =
                          ValueEnum aliases of install/uninstall
                 doctor.rs cmd_doctor: binary/PATH, hooks+skill (global+project),
                          index, per-scope config freshness, helper status
src/db.rs        SQLite: ~/.cona/projects/<fnv1a-hash>.db per project
                 (data dir overridable via CONA_DATA_DIR — tests set it,
                 real ~/.cona never sees test repos)
                 (files/symbols + notes: persistent symbol annotations,
                 auto-shown in show/context); ~/.cona/global.db (registry +
                 usage stats + meta: source_dir/install_path/last_tidy).
                 Storage introspection (total_storage_bytes/global_db_size;
                 storage_summary = THE struct both `stats` and `doctor` render),
                 tidy/auto_tidy (usage prune ≤90d/≤200k + VACUUM; dead paths
                 under temp roots auto-purged daily via is_ephemeral_path,
                 full orphan purge = manual tidy --orphans),
                 forget_project/remove_project_data (feeds `reset
                 [--keep-stats]`), is_home_or_fs_root guard against accidental
                 home indexing
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
Tool parity with CLI: find/show/refs/outline/tree/grep/context/diff/edit +
batch_edit/insert/check/impact/callers/callees/path/deps/shape/entries/tests/
note (tests/basic.rs handshake pins parity). batch_edit = multiple symbol edits
in ONE call, each syntax-verified; stops at first error (applied edits remain,
progress reported — no silent partial state). edit runs through cmd_edit_code
(replacement as &str — stdin belongs to the protocol). Usage logged `mcp:<tool>`.

## Setup / uninstall UX

`cona setup` always indexes + (project scope) installs git hooks, then picks
agents. Interactive = no `-y`, no explicit scope arg, TTY → pick_agents shows
ONE ui::multiselect across BOTH scopes (PROJECT + HOME sections via Row::Header,
items pre-checked by AgentName::detected). Non-interactive (`-y`, an explicit
`project|global|all`, or non-TTY) installs every detected agent in the active
scopes, no prompt. SetupScope still gates which sections/hooks run.
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
<!-- cona:begin -->
## cona — token-efficient code navigation

Once a repo is cona-indexed, reading ONE symbol costs a fraction of a whole
file, and `cona grep`/`refs` search code semantically (identifier nodes — never
strings or comments). Prefer them over a full Read or a broad Grep when you want
a specific function, class, or usage site.

Coarse → fine: `cona tree --rank` (orient) → `cona outline <file>` (map a file) →
`cona show <Sym>` (read one symbol) → `cona edit <Sym>` (syntax-verified write).

`<Sym>` = `Name`, `Parent.Name`, or `file.rs:Name`. Index auto-refreshes;
`cona index` (~1s) if a repo isn't indexed yet.

Everything else — `context` `impact` `diff` `deps` `callers` `tests` `blame`
`insert` `rename` `note` `check` — is listed in `cona --help`, with details per
group (`cona nav --help`, `inspect`, `code`, `history`, `project`, `maint`).
<!-- cona:end -->
