# cona — architecture

Per-module design rationale and the non-obvious constraints behind it.
Invariants, build/test, and release rules live in CLAUDE.md.

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
                          used by clap default_value_t AND the MCP fallbacks.
                          path_ok/path_matches_in/path_matches_dir = THE `--path`
                          policy (find/refs/grep/tree). Directory and prefix
                          readings CONFLICT — `--path src/commands` must exclude
                          `src/commands_old.rs`, `--path src/comm` must include
                          `src/commands/query.rs`; identical string shape,
                          opposite answers. No lexical rule separates them, so
                          path_matches_in asks the filesystem (is_dir) and
                          path_matches_dir (pure, tested) takes the answer as
                          `dir_filter`: real dir → `/`-boundary only, partial
                          name → prefix. locate_all = every candidate for
                          `show --all` (lists, never picks — invariant 4)
                 query.rs tree/outline (dir arg → points at `tree --path`)/find
                          (--path over-fetches so the SQL LIMIT can't clip
                          in-scope rows before the Rust filter; empty-but-exists-
                          elsewhere says so instead of falling through to fuzzy)/
                          show (multi-symbol; --sig = index-only signature peek,
                          no body read; --all = all candidates; a file arg
                          outlines it)/refs/context (--no-tests filters at
                          COLLECTION time — the caller cap is first-come, so
                          post-filtering would let test fns crowd out real
                          callers; hidden count disclosed)/diff/grep.
                          Matcher = THE grep line-matching rule: literal by
                          default (`foo.bar` is code, not a pattern), regex on
                          `--regex`/`-e`; Matcher::literal for identifier
                          callers (scan_ref_sites) where regex is never right.
                          prefilter_flag keeps rg/grep reading the pattern the
                          SAME way — a disagreeing prefilter drops files holding
                          real matches (rg needs no flag in regex mode: it is
                          already Rust-regex). Invalid regex = error, never a
                          silent literal fallback. Zero hits on a metachar
                          pattern in literal mode names `--regex` + the longest
                          literal run
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
                 Git submodules: `ignore` treats a nested `.git` as a separate
                 repo and walks none of it, so submodule source is invisible to
                 find/refs/grep. parse_gitmodules (pure, tested) reads the
                 `path =` declarations from `.gitmodules`, submodule_dirs keeps
                 the ones that exist, each becomes an extra WalkBuilder root,
                 and filter_entry whitelists those prefixes so EXCLUDED_DIRS
                 cannot prune them (the common checkout dir is `vendor/`, which
                 IS in the list). Declared paths are filtered for absolute and
                 `..` components — .gitmodules is repo content, not trusted
                 input, and must not aim the walker outside the root.
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
                 PreToolUse = pure decide_read/decide_grep + fail-open runner;
                 redirects large full reads + broad identifier greps (indexed
                 project, no glob/type/path filter) to cona; hook NEVER creates a
                 DB. A broad grep whose OUTPUT is already bounded (-l/-c/
                 --count/context flags, native output_mode files_with_matches|
                 count or -A/-B/-C) is `soft`: same search, but the agent showed
                 restraint, so decide_grep answers Advise (hook:grep-advise,
                 runs as-is) instead of Redirect. try_read gates (partial/
                 non-code/metadata-size) BEFORE reading bytes — multi-GB file
                 must not be slurped just to be allowed. A read the redirect
                 denied is retried verbatim by most agents: note_denied marks the
                 (project,session,path) and the SECOND attempt yields with an
                 advisory instead of denying again — a redirect that loops is
                 worse than the read. The per-session read log is split
                 peek_reads (read-only: seen? + counted volume) / record_read
                 (append; tab-prefixed lines = seen-but-uncounted, so an advised
                 read never ALSO drags the volume streak closer). Marker dirs
                 are pruned of >7-day-old files on first touch per session.
                 Every run() (both events) stamps data_dir()/hook-last-seen so
                 doctor can tell configured-but-silent hooks from healthy ones;
                 throttled to one write/hour (stat first), best-effort.
                 TWO tool shapes reach the same decision. Native Read/Grep is
                 read off tool_input. A harness whose ONLY file tool is a shell
                 (Codex sends tool_name "Bash" with command
                 `/bin/zsh -lc "sed -n '1,400p' f"`) never emits Read or Grep, so
                 classify_shell normalizes the command line into a ShellIntent
                 (Read{path,upto} | PartialRead | Grep{pattern,path,soft} |
                 Other) and
                 try_shell routes it into the SAME try_read/try_grep — the matcher
                 therefore lists the shell tool names too. Pipeline: unwrap_shell
                 _wrapper peels `sh|bash|zsh|… -lc "<script>"` → split_segments
                 splits on && || ; | newline (quote-respecting) → classify_command
                 per segment. Whole-line policy: ONE unrecognised segment makes
                 the line Other (blocking it would block that segment too —
                 `sed -n … f && cargo build` is a build); among recognised
                 segments the strongest intent wins (rank). Metadata probes
                 (wc/ls/file/stat/echo/…) are PartialRead, not Other, because an
                 agent routinely pairs one with the read it is about to do.
                 `sed -n '1,N p'` is Read{upto:N}, NOT PartialRead: that is
                 exactly how a shell-only harness spells "read the whole file"
                 (it picks an N it expects to exceed the length). The bound is
                 applied only after the real line count is known (upto < lines →
                 genuinely partial → pass), so the caller, not the parser, decides.
                 Relative paths resolve against the payload `cwd` — the hook runs
                 wherever the harness launched it. Fail-open by construction: an
                 unrecognised wrapper (a shell proxy prefixing commands) yields
                 Other and passes.
                 PostToolUse (no matcher) = periodic re-nudge,
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
                 tick_toolcall + nudge_due share session_marker_path (the ONE
                 session-identity rule via session_id(): CLAUDE_SESSION_ID, else
                 the payload's own `session_id` — Codex exports no env var but
                 sends the field — else a per-day bucket). The unindexed-repo
                 nudge fires on the FIRST eligible event of a session, then
                 again after every CONA_NUDGE_EVERY suppressed ones (default 10,
                 0 = never repeat) — one hint at the start of a long session is
                 otherwise gone for good.
                 The redirect is the ONLY permissionDecision cona ever emits, and
                 only "deny" + the cona command to run instead. Every hint path
                 (advisory, streak, nudge) is additionalContext ONLY. cona NEVER
                 emits "allow" — that would silently bypass the permission system
src/dashboard.rs `cona ui` — ratatui live TUI, read-only. DBs opened ONCE (not
                 per tick); cheap usage stats refresh 1s, the expensive index-
                 state scan (one fs stat/file) throttled to 5s. Keys: q quit,
                 p scope, s sort by-command (saved/calls/avg-ms), r force
                 refresh; Resize handled. Requires a TTY (else clean error, no
                 panic)
src/ui.rs        ANSI styling (zero deps): NO_COLOR/CLICOLOR_FORCE/TERM=dumb +
                 IsTerminal — piped output stays plain (agents!). All CLI colors
                 run through here; clap help styles in main.rs. ui::cmd_table =
                 THE two-column `command  description` table (install's next
                 steps, setup's try-it, `agents status`'s manage list) — one
                 alignment + dim/highlight decision, padded BEFORE coloring.
                 ui::select = THE
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
                          upsert_block/remove_block (tested); Mark is DATA
                          ({label, verb, path}), never pre-rendered text —
                          changed()/render() are methods, so grouping + "did a
                          claude target move?" read fields and a quiet run that
                          records 100+ marks and prints none pays nothing for
                          display (the auto-refresh path is the query hot path);
                          short_path (tested) = THE display path rule
                          (cwd → ./…, $HOME → ~/…, else absolute), symlink-
                          tolerant in 3 attempts cheapest-first — as-spelled
                          (0 syscalls, the common case since paths are built by
                          joining onto the anchor), then both sides canonicalized
                          (macOS /tmp vs /private/tmp), then as-spelled under the
                          resolved anchor (anchor via symlink whose subtree holds
                          a further symlink); GITHUB_REPO/
                          USER_AGENT + fetch_release_archive = THE release
                          download/extract path (self-upgrade AND helper fetch)
                 upgrade.rs cmd_install/upgrade/uninstall/hooks; ONE git-hook
                          policy: append_hook_line/strip_git_hook_lines.
                          Auto-upgrade: mtime check + remote check vs crates.io
                          sparse index (curl, fail-open) ≤1×/day (meta
                          last_remote_check) → fetches release binary from
                          GitHub, so upgrade works without source_dir
                          (cargo-install users). After ANY binary swap,
                          cmd_upgrade → refresh_config: ONE loop over global +
                          every registered project (db::registered_project_paths)
                          re-runs idempotent `agents install`, targeting ONLY
                          installed_agents (explicit names — a bare install
                          autodetects and would ADD agents the user never
                          selected; refresh = rustup model, update what IS
                          installed; empty scope → just stamped, no pre-gate —
                          installed_agents IS the gate) — SKILL/guide/hooks
                          (include_str!, baked at build) never lag the binary —
                          and stamps config_ver:<path>=CARGO_PKG_VERSION.
                          Non-quiet prints one dim `path — agents` line per
                          refreshed scope under a lazy heading.
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
                          def) — consumed by sync_subagents AND Claude's
                          Presence::SubagentDefs footprint probe, so a
                          nested-only footprint still counts as installed (else
                          uninstall + the version-gated re-sync skip the
                          scope). Bounded: SUBAGENT_MAX_DEPTH +
                          file_type() (no symlink follow). Subagents can't be
                          reached by one shared file — they run on their own
                          system prompt — so per-def marker blocks are the only
                          mechanism; sync_subagents reads each file ONCE (gate +
                          splice share it; upsert_block_file would re-read) and
                          patches only YAML-frontmatter defs (never README/
                          runbook prose), uninstall strips any marked .md.
                          **Output prints only marks that MOVED**; unchanged
                          ones collapse to one per-label tally line — a big
                          ~/.claude/agents tree is 113 "subagent unchanged"
                          rows that would scroll the real result away;
                          the tally is a linear scan, not a map: the label set is
                          closed (≤8) and first-seen order = touch order;
                          cmd_agents (thin) + cmd_agents_q (fully
                          silent, reports changes via per-mark Mark::changed() —
                          no output-string scanning); claude_hooks (settings.json
                          via serde_json); AgentName ValueEnum + AgentSel::want =
                          THE selection rule: named/--all override detection,
                          bare install autodetects, bare uninstall cleans all.
                          prune_empty_dirs = THE post-delete cleanup, called at
                          EVERY uninstall site that removes a file (skill,
                          cursor rule, guide loop both branches, mcp_register,
                          claude_hooks): walks up to the scope anchor with
                          remove_dir — never remove_dir_all — so it stops at the
                          first dir holding anything else and a project that had
                          no .cursor/.claude/.github before cona is left with
                          none after. claude_hooks additionally remembers which
                          event arrays were ALREADY empty on entry: the uninstall
                          sweep drops empty arrays as husks of our own hooks, and
                          without that set it would take a user's empty event —
                          and, if it was the only key, the whole settings.json.
                          xdg_config = THE $XDG_CONFIG_HOME rule (opencode/zed/
                          crush): honoured only when absolute AND under the
                          `home` being probed — an inherited value would let a
                          synthetic test/probe home escape into the developer's
                          real ~/.config; uninstall.sh guards the same way.
                          mark_label = the per-agent status label, capped by
                          install::LABEL_COL (label_widths_fit_the_column) since
                          Mark::render pads to a fixed column.
                          The six guide-only harnesses (opencode/windsurf/zed/
                          qwen/crush/copilot) share ONE writer loop in
                          cmd_agents_q driven off config_paths — the Presence tag
                          IS the write mode (Marker = splice into a file the user
                          also owns, Exists = ours alone, deleted outright), so
                          writer and uninstall probe cannot disagree about
                          ownership.
                          TWO per-scope path lists, deliberately separate:
                          config_paths = the guide/skill/hook targets a scope can
                          ACT on (empty at project scope for pi/opencode/zed —
                          the latter two read the project AGENTS.md the generic
                          bucket already owns, so a second writer would fight
                          over one marker block) → agents_in_scope +
                          the n/a cells; footprint_paths = those PLUS the MCP
                          entry (Presence::McpServer) + Claude's subagent-defs
                          probe (Presence::SubagentDefs) = "is cona installed
                          here?" → installed() / project_has_cona (a thin
                          any(installed) over ALL) / the ✓ cells. Merging them
                          would let an MCP-only scope be offered in the picker
                          and then receive nothing. installed_agents = THE
                          refresh target set (plain installed() filter) —
                          upgrade.rs sync_scope_config feeds it as explicit
                          names so refresh never autodetects. state_desc = THE
                          picker state wording (`· installed — uncheck to
                          remove` / `· detected`), shared by setup's pick_agents
                          and cmd_agents_interactive.
                          mcp_registrations = THE single
                          traversal of ALL × scopes × mcp_path, shared by
                          cmd_agents_status (folds to one cell) and doctor
                          (prints registered rows).
                          installed() probes for real markers/skill/
                          hook — feeds cmd_agents_status (ONE row per agent:
                          name + ✓on/–off/n-a per scope + desc; pad BEFORE
                          coloring or ANSI breaks every column) and
                          cmd_agents_interactive (pre-checked checklist; diff of
                          before/after → install added, uninstall removed).
                          Restart-note gated on a claude-labeled mark actually
                          moving. CLI: `agents [status|add|remove|install|
                          uninstall]`, action optional (bare+TTY → interactive,
                          bare+names → add, bare non-TTY → status); add/remove =
                          ValueEnum aliases of install/uninstall
                 mcp_config.rs THE MCP-registration writer, driven by
                          AgentName::mcp_path (per-agent + per-scope; claude
                          .mcp.json project-only — ~/.claude.json is Claude's
                          live session state, never rewritten; windsurf/copilot
                          global-only; pi none) and AgentName::mcp_key. The two
                          are SPLIT because they vary independently: ServerKey
                          says which top-level map + entry shape a harness
                          speaks — McpServers {type:"stdio",command,args} for
                          most (`type` REQUIRED by Cursor, harmless elsewhere),
                          Mcp {type:"local",command:[argv…],enabled} for
                          opencode/crush, ContextServers {source:"custom",…} for
                          zed. A wrong key does NOT error — the harness simply
                          never sees the server — so each agent names its own and
                          entry_shapes_match_each_harness_contract pins all
                          three. json_server_keyed
                          parses + re-serializes (serde_json preserve_order:
                          foreign servers AND key order survive), errors rather
                          than clobbering invalid JSON, and deletes a file that
                          held only cona. toml_server owns a `# cona:begin/end`
                          block (no toml crate dep) — replaced in place, so a
                          moved binary self-heals; backslashes/quotes escaped
                          for Windows paths. registered() = the Presence probe
                          footprint_paths uses, so an MCP-only scope still counts
                          as installed — a SUBSTRING probe, not a parse, because
                          it sits on the auto-refresh hot path (same trade as
                          Presence::Needle); the writers still parse properly.
                          Install/uninstall drive it from ONE loop over
                          AgentName::ALL in cmd_agents_q, never a call per agent
                          block — a new agent's mcp_path arm is then the whole
                          change
                 doctor.rs cmd_doctor: binary/PATH, hooks+skill (global+project),
                          index, per-scope config freshness, helper status,
                          mcp-server registration (informational, never an issue
                          — MCP is the optional second surface), hook liveness
                          (mtime of data_dir()/hook-last-seen, stamped throttled
                          at the top of hook::run — configured-but-silent >7d =
                          issue, the "harness snapshots hooks at startup"
                          failure), Codex plugin-cache staleness (cache versions
                          vs binary — editing the checkout does nothing until
                          `codex plugin add` re-runs)
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

## Plugin packaging (`plugin/`)

One directory, two harness manifests. `plugin/.claude-plugin/plugin.json` is
read by Claude Code, `plugin/.codex-plugin/plugin.json` by Codex; the payload
they both point at — `skills/`, `.mcp.json`, `hooks/hooks.json` — is shared, so
there is nothing to keep in sync. The marketplace manifests sit where each CLI
looks for them: `.claude-plugin/marketplace.json` and
`.agents/plugins/marketplace.json`, both at the repo root.

Two Codex behaviours shape how this is used and documented:

- **`local` sources are copied, not linked.** `codex plugin add` snapshots the
  plugin into `~/.codex/plugins/cache/<marketplace>/<plugin>/<version>/`. Editing
  the checkout does not affect an installed plugin until `codex plugin add` runs
  again — the failure mode is silent (old hooks keep firing), so it is called out
  in plugin/README.md.
- **Hooks are hash-trusted.** Codex prompts before running a plugin's hooks the
  first time and after any change to them.

Hook payloads are wire-compatible between the two harnesses (same JSON fields,
same `hookSpecificOutput` response), so `src/hook.rs` needs no per-harness
branch. What differs is the TOOL, not the protocol: Codex has no Read/Grep, only
a shell — which is what classify_shell in hook.rs exists for (see its entry
above), and why the PreToolUse matcher lists the shell tool names alongside
Read|Grep. The matcher is declared in TWO places that must agree:
plugin/hooks/hooks.json (plugin path) and the `specs` table in
install/agents.rs claude_hooks (`cona agents install` path). The installer
reconciles a drifted matcher on reinstall, so widening it self-heals existing
installs.

Distribution overlap is deliberate: the plugin and `cona agents install` deliver
the same components by different routes. Both are idempotent; running both only
duplicates the guidance.
