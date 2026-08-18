# Spike: semantic name resolution (stack-graphs)

Status: **investigated 2026-07-20, since SHIPPED** as the out-of-process
helper tier (see the SHIPPED section below; current state in
docs/architecture.md `src/resolve.rs`). This was the "eliminate `·ambiguous`"
item from the CLAUDE.md roadmap. Findings kept so the next iteration starts
from them instead of re-discovering them.

> 2026 caveat: upstream github/stack-graphs was archived 2025-09-09. The
> helper still works (vendored crates), but the foundation is unmaintained —
> further investment should weigh an LSP-based tier instead.

## Problem

`refs`, `callers`/`callees`, `context`, and `rename` are semantic at the
*identifier* level (they match identifier nodes, never strings/comments) but
purely **name-based** — no type resolving. Two methods `foo()` on different
types in the same file/scope collapse together. `prefer_scope` /
`narrow_by_scope` (src/graph.rs) resolves the common single-survivor case; the
rest is honestly marked `·ambiguous`. Full resolution needs real name binding.

## Candidate: `tree-sitter-stack-graphs` + `stack-graphs`

GitHub's name-binding engine, built on tree-sitter. Per-language TSG rule files
map syntax → a scope/definition graph; queries resolve a reference to its
definition(s).

## Why it did NOT ship in this batch — three hard blockers

1. **tree-sitter runtime collision — EMPIRICALLY CONFIRMED.** `cargo add
   tree-sitter-stack-graphs@0.10` in a throwaway worktree failed to resolve:

   ```
   error: failed to select a version for `tree-sitter`.
     ... required by tree-sitter-stack-graphs v0.10.0  (needs ^0.24)
   package `tree-sitter` links to the native library `tree-sitter`, but it
   conflicts with a previous package which links to `tree-sitter` as well:
   package `tree-sitter v0.26.3` (via tree-sitter-perl → our grammars)
   Only one package in the dependency graph may specify the same links value.
   ```

   This is a hard cargo `links = "tree-sitter"` conflict, not a warning — the
   same C-symbol class we escaped for Vue/Dockerfile by vendoring (`build.rs`
   + `extern "C"`). It cannot be a plain dependency. Options: (a) downgrade all
   32 grammars to tree-sitter 0.24 (big regression + churn), or (b) vendor +
   isolate stack-graphs against a private runtime copy — real work, not a
   drop-in.

2. **Coverage regression.** Published TSG rule sets exist for only ~5 languages
   (python, javascript, typescript, java, ruby-0.0.1). **Rust has none
   published.** cona's value proposition is 32 languages; a resolver that
   covers 5 and silently leaves 27 name-based would be an inconsistent,
   hard-to-explain UX. Would need hand-authored TSG for Rust at minimum.

3. **Index-cost invariant.** Building a full name-binding graph per file is far
   heavier than the current single-pass symbol extraction. The "~1s index"
   property (and the incremental `reindex_file` hot path used by every
   `locate_fresh`) is load-bearing. Stack-graphs would need to be built lazily
   / cached separately, not folded into the main index write path.

Additionally the **supply-chain rule** (Cargo.lock avoids releases <7 days old;
caret ranges, no `=` pins) means pulling a new dep tree needs the usual
`cargo update -p … --precise` vetting.

## Isolation is HARDER than Vue/Dockerfile — in-process is impossible (2026-07-20)

The Vue/Dockerfile trick vendors only the grammar **C** (`parser.c` + scanner)
and binds `tree_sitter_<lang>()` via `extern "C"` against our single runtime —
it works because a grammar is just C with no Rust runtime dependency of its own.
**stack-graphs is different:** `tree-sitter-stack-graphs` is a Rust crate that
depends on `tree-sitter ^0.24` (a Rust crate with `links = "tree-sitter"`). You
cannot vendor away another Rust crate's runtime dep the way you vendor a C
grammar. Empirically, a crate with JUST `tree-sitter = "0.26"` +
`tree-sitter-stack-graphs = "0.10"` fails to resolve — the `links` collision
fires (0.24 vs 0.26, only one copy of the native lib allowed). So the in-process
options are all bad: downgrade all 32 grammars to 0.24 (big regression), or fork
tree-sitter-stack-graphs onto 0.26 (real maintenance).

**Conclusion: the only clean shape is PROCESS ISOLATION** — a separate helper
binary (its own 0.24 runtime, no link conflict) that cona shells out to
lazily, exactly as the "lazy, separate, never in the index write path" guidance
below already wants. An isolated throwaway crate (stack-graphs 0.14 +
tree-sitter-stack-graphs 0.10 + the TS TSG crate, no other grammars) DOES build
cleanly, so a standalone helper is viable.

## Resolve quality — PROVEN on TypeScript (2026-07-20)

An isolated probe (`sg-probe`) built the full resolve path and confirmed
stack-graphs does genuine type/scope-aware name binding, not just name matching.
Test: `class A { finish(x){} }` + free `function finish(a,b,c){}`, then two calls:

- `a.finish(1)` (receiver call) → resolves to **A.finish** (the method) ✅
- `finish(1,2,3)` (bare call)   → resolves to **finish** (the free fn) ✅

Same two same-named defs; resolution FLIPS correctly with call syntax — exactly
the `·ambiguous` class cona's arity heuristic only approximates.

The driving code is small once the (undocumented, gleaned from the crate's
`cli`/`test.rs`) stitcher sequence is known: per file build a `StackGraph` via
`lc.sgl.build_stack_graph_into`, seed a `PartialPaths`/`Database` with
`ForwardPartialPathStitcher::find_minimal_partial_path_set_in_file`, then per
query `find_all_complete_partial_paths` over the reference handle — each
complete path's `end_node` is the resolved definition
(`graph.source_info(n).span.start.line`). ~15 lines/file + ~10 lines/query.

So of the three blockers: **runtime isolation** has a clean answer (separate
binary), **resolve quality** is proven, and **no Rust TSG** is the one genuinely
open, effort-unbounded item (hand-author `.tsg` name-binding rules for Rust).

## SHIPPED — out-of-process helper for TSG languages (2026-07-20)

The suggested path below was implemented for the languages that already have
published TSG rules. `src/resolve-helper/` is a **standalone crate** (own
tree-sitter 0.24, its own `[workspace]`, excluded from the cona package and
build) producing the `cona-resolve-helper` binary. It reads one JSON request
on stdin (`{lang, path, source, refs:[{line,name}]}`) and writes the resolved
definition line(s) per ref; references are addressed by (line, name) so the two
sides never have to agree on a byte-vs-utf8 column encoding.

cona side lives in `src/resolve.rs` (all FAIL-OPEN: missing binary / spawn
error / non-zero exit / parse error → `None`, caller keeps its name-based
result). `cmd_context` consults it as the sharpest disambiguation tier: for a
callee name still ambiguous after scope/file/dir/arity, if the helper resolves
it to exactly one definition **in the same file**, the ambiguous rows collapse
to that one and the `·ambiguous` mark is cleared. Binary discovery:
`CONA_RESOLVE_HELPER` env → sibling of the cona binary → PATH.

Verified end-to-end on the case Phase-1 arity CANNOT solve — a method and a
free function of the **same arity** (`class Box { send(x) }` + `function
send(x)`, called `b.send(1)`): without the helper both are `·ambiguous`; with
it, resolves cleanly to `Box.send`. Fail-open verified (broken/crashing/missing
helper → stays ambiguous, never panics, never wrong).

**Distribution (all in-tool):** `release.yml` builds the helper per target
(best-effort — a target where it fails ships without it) and bundles it in the
same tarball. `install.sh` and source-checkout `cona install`/`upgrade`
place it beside the cona binary; the GitHub-release upgrade path extracts it
from the tarball. `cargo install cona` users (no sibling) get it auto-fetched
from the matching GitHub release into `~/.cona/bin` on first ambiguous TSG
query — fail-open, 24h back-off on failure, opt-out `CONA_NO_FETCH_HELPER`.
`cona doctor` reports which path found it. It is NEVER a cargo dependency of
cona (the whole point — see the links collision above).

**Scope shipped:** typescript/tsx/javascript/python via published TSG rules,
plus rust via a hand-authored rust.tsg (inherent methods, no traits/generics/
macros — fail-open); cross-file resolution via `deps` stitching landed after
this spike. **Still open:** mtime-keyed caching (currently the
graph is rebuilt per call — fine because it only fires on rare ambiguity), and
**Rust** (still no published TSG — needs hand-authored `.tsg`, the one
effort-unbounded item). Wiring `refs`/`callers`/`rename` to the same tier is
straightforward follow-on now that the plumbing exists.

## Original suggested path (for reference / the still-open items)

1. **Isolate via a separate BINARY, not a vendored runtime.** ✅ done.
2. **Lazy + cached.** Lazy ✅ (only on ambiguity, out of the index write path);
   mtime cache still open.
3. **Graceful downgrade.** ✅ fail-open, unsupported language never spawns.
4. **Prove it on Rust first** — inverted: proved on TypeScript (published TSG)
   instead, since Rust has none. Rust TSG remains the open item.

## Cheaper alternative — PARTLY DONE

Sharpening `narrow_by_scope` (the existing heuristic) cuts ambiguous cases
without any new dependency. **Shipped:** rule 3 — directory proximity
(same-scope → same-file → same-directory, each firing only on a single
survivor) — and rule 4 — **argument arity** (call-site arg count == declared
param count, minus an implicit receiver for methods via
`lang::first_param_is_receiver`). Arg counts come from `call_node_of` /
`arg_count_of`, param counts from `lang::param_count`; the signals ride on
`graph::Candidate`. On this repo `context cmd_context` dropped from 7 ambiguous
callees to 1 (a closure-param false positive), and the free-`finish` vs.
`BudgetOut.finish` case now resolves cleanly.
**Still open (would tighten further):** receiver-*type* hints from the
call-site AST (real `x.foo()` type resolving) — that is essentially the
name-binding problem below, so it belongs with stack-graphs, not the heuristic.
Lower ceiling than stack-graphs, but zero supply-chain / runtime risk.

## Effort estimate

- Cheaper heuristic route: ~1–2 days, no new deps.
- Stack-graphs, Rust-only proof: ~1 week (vendoring + TSG authoring + lazy cache).
- Full multi-language parity: multi-week, ongoing TSG maintenance per language.
