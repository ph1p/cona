use crate::lang;
use std::collections::{HashMap, HashSet};

/// Parent scope of a qualified name: "A.b" → Some("A"), "top" → None.
pub fn scope_parent(qualified: &str) -> Option<&str> {
    qualified.rsplit_once('.').map(|(p, _)| p)
}

/// Directory portion of a repo-relative path: "a/b/c.rs" → "a/b", "c.rs" → "".
fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// A disambiguation candidate: the signals `narrow_by_scope` compares against
/// the caller. `params`/`is_method` (implicit-receiver offset) feed the arity rule.
pub struct Candidate {
    pub scope: Option<String>,
    pub file: String,
    pub params: Option<usize>,
    pub is_method: bool,
}

enum ScopeRule<'a> {
    Parent(&'a str),
    File(&'a str),
    Directory(&'a str),
    Arity(usize),
}

/// The single ordered first-exactly-one policy. Each provider summarizes a
/// group from the ORIGINAL pool: a failed rule never narrows later rules.
fn first_unique(
    my_scope: Option<&str>,
    my_file: &str,
    argc: Option<usize>,
    mut unique_in: impl FnMut(ScopeRule<'_>) -> Option<usize>,
) -> Option<usize> {
    [
        my_scope.map(ScopeRule::Parent),
        Some(ScopeRule::File(my_file)),
        Some(ScopeRule::Directory(dir_of(my_file))),
        argc.map(ScopeRule::Arity),
    ]
    .into_iter()
    .flatten()
    .find_map(&mut unique_in)
}

fn only_one(mut ids: impl Iterator<Item = usize>) -> Option<usize> {
    let first = ids.next()?;
    ids.next().is_none().then_some(first)
}

/// Borrowed, snapshot-local summaries for ONE name's original definition pool.
/// Some(id) means exactly one candidate; None means multiple, not an empty group.
#[derive(Default)]
struct ScopeGroups<'a> {
    parents: HashMap<&'a str, Option<usize>>,
    files: HashMap<&'a str, Option<usize>>,
    directories: HashMap<&'a str, Option<usize>>,
}

impl<'a> ScopeGroups<'a> {
    fn new(syms: &'a [SymNode], defs: &[usize]) -> Self {
        let mut groups = Self::default();
        for &id in defs {
            let sym = &syms[id];
            if let Some(parent) = scope_parent(&sym.qualified) {
                Self::add(&mut groups.parents, parent, id);
            }
            Self::add(&mut groups.files, &sym.file, id);
            Self::add(&mut groups.directories, dir_of(&sym.file), id);
        }
        groups
    }

    fn add(group: &mut HashMap<&'a str, Option<usize>>, key: &'a str, id: usize) {
        group
            .entry(key)
            .and_modify(|v| *v = None)
            .or_insert(Some(id));
    }

    fn winner(&self, caller: &SymNode) -> Option<usize> {
        // Reverse references have no call arity, including type/callback uses.
        first_unique(
            scope_parent(&caller.qualified),
            &caller.file,
            None,
            |rule| match rule {
                ScopeRule::Parent(parent) => self.parents.get(parent).copied().flatten(),
                ScopeRule::File(file) => self.files.get(file).copied().flatten(),
                ScopeRule::Directory(dir) => self.directories.get(dir).copied().flatten(),
                ScopeRule::Arity(_) => None,
            },
        )
    }
}

/// THE scope-preference disambiguation policy (name-based, no type
/// resolution), shared by `Graph::prefer_scope` and `context`. Among same-named
/// defs prefer (1) the caller's parent scope (`self.x()` / `this.x()`), then
/// (2) the caller's file, (3) the caller's directory, (4) a declared arity
/// matching the call's arg count. Narrows only when EXACTLY one candidate
/// survives a rule — never picks among equals, so the rest stay ambiguous.
pub fn narrow_by_scope<T>(
    my_scope: Option<&str>,
    my_file: &str,
    argc: Option<usize>,
    cands: Vec<T>,
    key: impl Fn(&T) -> Candidate,
) -> Vec<T> {
    if cands.len() <= 1 {
        return cands;
    }
    // derive signals ONCE — `key` may re-parse a signature (see cmd_context)
    let derived: Vec<Candidate> = cands.iter().map(&key).collect();
    let winner = first_unique(my_scope, my_file, argc, |rule| {
        only_one(derived.iter().enumerate().filter_map(|(i, cand)| {
            let survives = match rule {
                ScopeRule::Parent(parent) => cand.scope.as_deref() == Some(parent),
                ScopeRule::File(file) => cand.file == file,
                ScopeRule::Directory(dir) => dir_of(&cand.file) == dir,
                ScopeRule::Arity(args) => cand.params.is_some_and(|p| {
                    let effective = if cand.is_method {
                        p.saturating_sub(1)
                    } else {
                        p
                    };
                    effective == args
                }),
            };
            survives.then_some(i)
        }))
    });
    if let Some(i) = winner {
        return cands.into_iter().nth(i).into_iter().collect();
    }
    cands
}

/// One indexed symbol as a call-graph node.
#[derive(Debug, Clone)]
pub struct SymNode {
    pub qualified: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub start: i64,
    pub end: i64,
    /// declared parameter count from the signature; `None` without a param list
    pub params: Option<usize>,
    /// first param is an implicit receiver (`self`/`this`), so effective
    /// arity is `params - 1`
    pub recv: bool,
}

/// Name-based call graph over the whole index, built in ONE pass so traversals
/// never rescan files. Same policy as `context`: no type resolution — all defs
/// of a name are kept and callers mark them ambiguous.
pub struct Graph {
    pub syms: Vec<SymNode>,
    by_name: HashMap<String, Vec<usize>>,
    /// ident name → occurrences as (innermost enclosing sym, line) — ALL
    /// occurrences, so callers-of works for types and callback references too
    uses: HashMap<String, Vec<(usize, i64)>>,
    /// per sym: ordered-unique (callee name, arg count, first call-site line)
    /// in CALL POSITION only, so a local variable named like a method doesn't
    /// fabricate an edge. The line lets the semantic tier resolve an ambiguous
    /// callee at its call position.
    calls: Vec<Vec<(String, Option<usize>, i64)>>,
}

impl Graph {
    /// `files`: (path, lang, source, symbols). Symbols come from the (fresh)
    /// index; occurrences are re-derived from source, fail-open.
    pub fn build(files: &[(String, Option<&str>, String, Vec<SymNode>)]) -> Graph {
        let mut syms: Vec<SymNode> = Vec::new();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut uses: HashMap<String, Vec<(usize, i64)>> = HashMap::new();
        let mut calls: Vec<Vec<(String, Option<usize>, i64)>> = Vec::new();

        for (_, _, _, fsyms) in files {
            for s in fsyms {
                let idx = syms.len();
                by_name.entry(s.name.clone()).or_default().push(idx);
                syms.push(s.clone());
                calls.push(Vec::new());
            }
        }

        let mut base = 0usize;
        for (_, flang, src, fsyms) in files {
            let mut occ = lang::ident_occurrences_failopen(*flang, src);
            occ.sort_by_key(|(_, line, _, _)| *line);
            let mut seen_call: Vec<HashSet<String>> = vec![HashSet::new(); fsyms.len()];
            // one sweep: ranges nest, so a stack of open ranges keeps the
            // innermost symbol on top — the same tiebreak as
            // ENCLOSING_SYMBOL_SQL (greatest start_line)
            let mut order: Vec<usize> = (0..fsyms.len()).collect();
            order.sort_by_key(|&i| fsyms[i].start);
            let mut next_sym = 0usize;
            let mut open: Vec<usize> = Vec::new();
            for (name, line, is_call, argc) in &occ {
                let line = *line as i64;
                while next_sym < order.len() && fsyms[order[next_sym]].start <= line {
                    open.push(order[next_sym]);
                    next_sym += 1;
                }
                while let Some(&top) = open.last() {
                    if fsyms[top].end < line {
                        open.pop();
                    } else {
                        break;
                    }
                }
                let Some(&i) = open.last() else { continue };
                let gi = base + i;
                // the definition's own name token is not a use of itself
                if fsyms[i].name == *name && line == fsyms[i].start {
                    continue;
                }
                uses.entry(name.clone()).or_default().push((gi, line));
                if *is_call && name.len() >= 2 && seen_call[i].insert(name.clone()) {
                    calls[gi].push((name.clone(), *argc, line));
                }
            }
            base += fsyms.len();
        }
        Graph {
            syms,
            by_name,
            uses,
            calls,
        }
    }

    /// Definitions matching a name or qualified name.
    pub fn find(&self, symbol: &str) -> Vec<usize> {
        let exact: Vec<usize> = self
            .syms
            .iter()
            .enumerate()
            .filter(|(_, s)| s.qualified == symbol)
            .map(|(i, _)| i)
            .collect();
        if !exact.is_empty() {
            return exact;
        }
        self.by_name.get(symbol).cloned().unwrap_or_default()
    }

    /// Direct callers of `name`: (enclosing sym, line), deduped per sym,
    /// excluding occurrences inside any def of `name` (recursion is kept out).
    pub fn callers_of(&self, name: &str, exclude: &HashSet<usize>) -> Vec<(usize, i64)> {
        let mut out: Vec<(usize, i64)> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        for (sym, line) in self.uses.get(name).into_iter().flatten() {
            if exclude.contains(sym) || self.syms[*sym].name == name {
                continue;
            }
            if seen.insert(*sym) {
                out.push((*sym, *line));
            }
        }
        out
    }

    /// Reverse reference adjacency, built once for bounded evidence traversals.
    /// Includes callbacks/types, not only calls. Ambiguity is retained after
    /// the shared scope policy; no semantic subprocesses or source scans.
    pub fn reverse_references(&self) -> Vec<Vec<(usize, i64, bool)>> {
        let mut reverse = vec![Vec::new(); self.syms.len()];
        for (name, uses) in &self.uses {
            let Some(defs) = self.by_name.get(name) else {
                continue;
            };
            let groups = (defs.len() > 1).then(|| ScopeGroups::new(&self.syms, defs));
            for &(caller, line) in uses {
                let winner = groups.as_ref().and_then(|g| g.winner(&self.syms[caller]));
                let candidates = winner.as_ref().map(std::slice::from_ref).unwrap_or(defs);
                let ambiguous = candidates.len() > 1;
                for &target in candidates {
                    if target != caller {
                        reverse[target].push((caller, line, ambiguous));
                    }
                }
            }
        }
        for edges in &mut reverse {
            edges.sort_unstable();
            edges.dedup_by_key(|e| e.0);
        }
        reverse
    }

    /// Direct callees of sym `idx` that resolve to indexed defs: (name, defs,
    /// line). Several defs after scope narrowing = ambiguous.
    pub fn callees_of(&self, idx: usize) -> Vec<(String, Vec<usize>, i64)> {
        let me = &self.syms[idx];
        let mut out = Vec::new();
        for (name, argc, line) in &self.calls[idx] {
            if *name == me.name {
                continue;
            }
            let Some(defs) = self.by_name.get(name) else {
                continue;
            };
            let defs: Vec<usize> = defs
                .iter()
                .copied()
                .filter(|d| *d != idx && self.syms[*d].kind != "mod")
                .collect();
            let defs = self.prefer_scope(idx, defs, *argc);
            if !defs.is_empty() {
                out.push((name.clone(), defs, *line));
            }
        }
        out
    }

    /// `narrow_by_scope` applied to def indexes relative to a caller sym.
    /// `argc` is the call-site arg count, if known.
    /// NOTE: deliberately stops at the cheap tiers. The out-of-process semantic
    /// tier (`crate::resolve`) runs ONLY in `cmd_context` — a subprocess per
    /// ambiguous edge across the whole index would be far too costly.
    pub fn prefer_scope(&self, caller: usize, defs: Vec<usize>, argc: Option<usize>) -> Vec<usize> {
        let me = &self.syms[caller];
        narrow_by_scope(scope_parent(&me.qualified), &me.file, argc, defs, |d| {
            let s = &self.syms[*d];
            Candidate {
                scope: scope_parent(&s.qualified).map(String::from),
                file: s.file.clone(),
                params: s.params,
                is_method: s.recv,
            }
        })
    }

    /// Shortest call chain from any def of `from` to any def of `to`, BFS over
    /// callee edges (name-based). Returns the chain of sym indexes.
    pub fn path(&self, from: &str, to: &str, max_depth: usize) -> Option<Vec<usize>> {
        let starts = self.find(from);
        let goals: HashSet<usize> = self.find(to).into_iter().collect();
        if starts.is_empty() || goals.is_empty() {
            return None;
        }
        let mut prev: HashMap<usize, usize> = HashMap::new();
        let mut frontier: Vec<usize> = starts.clone();
        let mut visited: HashSet<usize> = starts.iter().copied().collect();
        for start in &starts {
            if goals.contains(start) {
                return Some(vec![*start]);
            }
        }
        for _ in 0..max_depth {
            let mut next = Vec::new();
            for cur in &frontier {
                for (_, defs, _) in self.callees_of(*cur) {
                    for d in defs {
                        if !visited.insert(d) {
                            continue;
                        }
                        prev.insert(d, *cur);
                        if goals.contains(&d) {
                            let mut chain = vec![d];
                            let mut at = d;
                            while let Some(p) = prev.get(&at) {
                                chain.push(*p);
                                at = *p;
                            }
                            chain.reverse();
                            return Some(chain);
                        }
                        next.push(d);
                    }
                }
            }
            if next.is_empty() {
                return None;
            }
            frontier = next;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang;

    fn nodes(
        path: &str,
        lang_name: &str,
        src: &str,
    ) -> (String, Option<&'static str>, String, Vec<SymNode>) {
        let syms = lang::extract_symbols(lang_name, src)
            .unwrap()
            .into_iter()
            .map(|s| SymNode {
                params: lang::param_count(&s.signature),
                recv: lang::first_param_is_receiver(&s.signature),
                qualified: s.qualified,
                name: s.name,
                kind: s.kind.to_string(),
                file: path.to_string(),
                start: s.start_line as i64,
                end: s.end_line as i64,
            })
            .collect();
        let l: &'static str = match lang_name {
            "rust" => "rust",
            "python" => "python",
            _ => "typescript",
        };
        (path.to_string(), Some(l), src.to_string(), syms)
    }

    const SRC: &str = "fn low() {}\nfn mid() { low(); }\nfn high() { mid(); }\nfn other() {}\n";

    // Deliberately retain the pre-index implementation as an edge-level oracle.
    fn reverse_reference_oracle(g: &Graph) -> Vec<Vec<(usize, i64, bool)>> {
        let mut reverse = vec![Vec::new(); g.syms.len()];
        for (name, uses) in &g.uses {
            let Some(defs) = g.by_name.get(name) else {
                continue;
            };
            for &(caller, line) in uses {
                let candidates = g.prefer_scope(caller, defs.clone(), None);
                let ambiguous = candidates.len() > 1;
                for target in candidates {
                    if target != caller {
                        reverse[target].push((caller, line, ambiguous));
                    }
                }
            }
        }
        for edges in &mut reverse {
            edges.sort_unstable();
            edges.dedup_by_key(|e| e.0);
        }
        reverse
    }

    #[test]
    fn reverse_references_match_original_pool_oracle() {
        // Enumerate scope/file/directory combinations, including root directory,
        // overloaded definitions, self references, and repeated reference lines.
        let locations = [
            ("A.helper", "near/a.rs"),
            ("A.helper", "far/a.rs"),
            ("B.helper", "near/caller.rs"),
            ("B.helper", "near/b.rs"),
            ("helper", "root.rs"),
            ("helper", "other.rs"),
        ];
        for mask in 1usize..(1 << locations.len()) {
            let mut syms = Vec::new();
            for (i, (qualified, file)) in locations.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    syms.push(SymNode {
                        qualified: (*qualified).into(),
                        name: "helper".into(),
                        kind: "fn".into(),
                        file: (*file).into(),
                        start: i as i64 + 1,
                        end: i as i64 + 1,
                        params: Some(i),
                        recv: i % 2 == 0,
                    });
                }
            }
            let defs: Vec<_> = (0..syms.len()).collect();
            for (qualified, file) in [
                ("A.run", "near/caller.rs"),
                ("C.run", "near/caller.rs"),
                ("run", "caller.rs"),
                ("run", "absent/caller.rs"),
            ] {
                syms.push(SymNode {
                    qualified: qualified.into(),
                    name: "run".into(),
                    kind: "fn".into(),
                    file: file.into(),
                    start: 10,
                    end: 30,
                    params: None,
                    recv: false,
                });
            }
            let uses = (0..syms.len())
                .flat_map(|i| [(i, 23), (i, 12), (i, 23)])
                .collect();
            let g = Graph {
                calls: vec![Vec::new(); syms.len()],
                syms,
                by_name: HashMap::from([("helper".into(), defs)]),
                uses: HashMap::from([("helper".into(), uses)]),
            };
            assert_eq!(
                g.reverse_references(),
                reverse_reference_oracle(&g),
                "candidate mask {mask}"
            );
            if mask == 0b000111 {
                // A's scope matches TWO candidates; same-file must consider
                // the ORIGINAL pool and select B.helper, not intersect it.
                let caller = 3;
                assert_eq!(g.prefer_scope(caller, vec![0, 1, 2], None), vec![2]);
            }
            if mask == 0b110000 {
                // The root directory shared by many candidates cannot win.
                assert_eq!(g.prefer_scope(4, vec![0, 1], None), vec![0, 1]);
            }
        }
    }

    #[test]
    fn reverse_references_preserve_types_callbacks_and_overloads() {
        let g = Graph::build(&[
            nodes(
                "near/a.rs",
                "rust",
                "struct Item;\nfn helper() {}\nfn worker(x: Item) {\n helper();\n let callback = helper;\n helper();\n worker(x);\n}\n",
            ),
            nodes("far/b.rs", "rust", "struct Item;\nfn helper(x: i32) {}\n"),
            nodes(
                "tests.ts",
                "typescript",
                "function test_target() { helper(); }\nfunction test_target(x) { helper(x); }\n",
            ),
        ]);
        let reverse = g.reverse_references();
        assert_eq!(reverse, reverse_reference_oracle(&g));
        let worker = g.find("worker")[0];
        let helper = g.find("helper")[0];
        let item = g.find("Item")[0];
        assert_eq!(reverse[helper][0], (worker, 4, false));
        assert_eq!(reverse[item], vec![(worker, 3, false)]);
        assert!(reverse[worker].is_empty(), "self edge is excluded");
        for test in g.find("test_target") {
            assert!(reverse[helper].iter().any(|e| e.0 == test && e.2));
        }
    }

    #[test]
    fn callers_and_callees_direct() {
        let g = Graph::build(&[nodes("a.rs", "rust", SRC)]);
        let low = g.find("low")[0];
        let mid = g.find("mid")[0];
        let callers = g.callers_of("low", &HashSet::new());
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].0, mid);
        let callees = g.callees_of(mid);
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0].1, vec![low]);
    }

    #[test]
    fn path_finds_transitive_chain() {
        let g = Graph::build(&[nodes("a.rs", "rust", SRC)]);
        let chain = g.path("high", "low", 5).unwrap();
        let names: Vec<&str> = chain.iter().map(|i| g.syms[*i].name.as_str()).collect();
        assert_eq!(names, vec!["high", "mid", "low"]);
        assert!(g.path("other", "low", 5).is_none());
    }

    #[test]
    fn scope_preference_narrows_same_class_and_same_file() {
        // a method beside the caller in class A beats a free fn elsewhere
        let a = "class A {\n  helper() {}\n  run() { this.helper(); }\n}\n";
        let b = "function helper() {}\n";
        let g = Graph::build(&[
            nodes("a.ts", "typescript", a),
            nodes("b.ts", "typescript", b),
        ]);
        let run = g.find("A.run")[0];
        let callees = g.callees_of(run);
        let helper = callees.iter().find(|(n, _, _)| n == "helper").unwrap();
        assert_eq!(helper.1.len(), 1, "{:?}", helper.1);
        assert_eq!(g.syms[helper.1[0]].qualified, "A.helper");

        // same-file preference: caller has no parent scope
        let c = "function helper2() {}\nfunction go() { helper2(); }\n";
        let d = "class Z {\n  helper2() {}\n}\n";
        let g2 = Graph::build(&[
            nodes("c.ts", "typescript", c),
            nodes("d.ts", "typescript", d),
        ]);
        let go = g2.find("go")[0];
        let callees = g2.callees_of(go);
        let h2 = callees.iter().find(|(n, _, _)| n == "helper2").unwrap();
        assert_eq!(h2.1.len(), 1);
        assert_eq!(g2.syms[h2.1[0]].file, "c.ts");

        // two equal candidates in two other files → still ambiguous (both kept)
        let e = "function f() { pick(); }\n";
        let f1 = "function pick() {}\n";
        let f2 = "function pick() {}\n";
        let g3 = Graph::build(&[
            nodes("e.ts", "typescript", e),
            nodes("f1.ts", "typescript", f1),
            nodes("f2.ts", "typescript", f2),
        ]);
        let f = g3.find("f")[0];
        let callees = g3.callees_of(f);
        let pick = callees.iter().find(|(n, _, _)| n == "pick").unwrap();
        assert_eq!(pick.1.len(), 2);
    }

    #[test]
    fn scope_preference_falls_back_to_same_directory() {
        // scope + same-file fail; directory proximity (rule 3) picks the
        // `util` beside the caller in m/ over the one in other/.
        let caller = "function go() { util(); }\n";
        let near = "function util() {}\n";
        let far = "function util() {}\n";
        let g = Graph::build(&[
            nodes("m/caller.ts", "typescript", caller),
            nodes("m/near.ts", "typescript", near),
            nodes("other/far.ts", "typescript", far),
        ]);
        let go = g.find("go")[0];
        let callees = g.callees_of(go);
        let util = callees.iter().find(|(n, _, _)| n == "util").unwrap();
        assert_eq!(util.1.len(), 1, "{:?}", util.1);
        assert_eq!(g.syms[util.1[0]].file, "m/near.ts");

        // but two candidates in the caller's own dir stay ambiguous (no guess)
        let g2 = Graph::build(&[
            nodes("m/caller.ts", "typescript", caller),
            nodes("m/a.ts", "typescript", near),
            nodes("m/b.ts", "typescript", far),
        ]);
        let go2 = g2.find("go")[0];
        let util2 = g2
            .callees_of(go2)
            .into_iter()
            .find(|(n, _, _)| n == "util")
            .unwrap();
        assert_eq!(util2.1.len(), 2);
    }

    #[test]
    fn arity_narrows_when_scope_and_dir_fail() {
        // scope/file/dir all fail; the call passes two args → 2-param def wins
        let caller = "function go() { emit(1, 2); }\n";
        let one = "function emit(a) {}\n";
        let two = "function emit(a, b) {}\n";
        let g = Graph::build(&[
            nodes("m/caller.ts", "typescript", caller),
            nodes("x/one.ts", "typescript", one),
            nodes("y/two.ts", "typescript", two),
        ]);
        let go = g.find("go")[0];
        let emit = g
            .callees_of(go)
            .into_iter()
            .find(|(n, _, _)| n == "emit")
            .unwrap();
        assert_eq!(emit.1.len(), 1, "arity should resolve: {:?}", emit.1);
        assert_eq!(g.syms[emit.1[0]].params, Some(2));

        // same two defs but the call arity matches NEITHER → stays ambiguous
        let caller3 = "function go3() { emit(1, 2, 3); }\n";
        let g2 = Graph::build(&[
            nodes("m/caller.ts", "typescript", caller3),
            nodes("x/one.ts", "typescript", one),
            nodes("y/two.ts", "typescript", two),
        ]);
        let go3 = g2.find("go3")[0];
        let emit3 = g2
            .callees_of(go3)
            .into_iter()
            .find(|(n, _, _)| n == "emit")
            .unwrap();
        assert_eq!(emit3.1.len(), 2, "no arity match → ambiguous");
    }

    #[test]
    fn arity_accounts_for_method_receiver_offset() {
        // `.finish(self, trailer)` declares 2 params but `.finish(x)` passes
        // ONE arg; the receiver offset must let the method win.
        let caller = "fn go() { obj.finish(x); }\n";
        let free = "fn finish(a: i32, b: i32, c: i32) {}\n";
        let method = "struct B;\nimpl B {\n    fn finish(self, trailer: i32) {}\n}\n";
        let g = Graph::build(&[
            nodes("m/caller.rs", "rust", caller),
            nodes("x/free.rs", "rust", free),
            nodes("y/method.rs", "rust", method),
        ]);
        let go = g.find("go")[0];
        let fin = g
            .callees_of(go)
            .into_iter()
            .find(|(n, _, _)| n == "finish")
            .unwrap();
        assert_eq!(
            fin.1.len(),
            1,
            "receiver offset should resolve: {:?}",
            fin.1
        );
        assert_eq!(g.syms[fin.1[0]].qualified, "B.finish");
    }

    #[test]
    fn local_variable_does_not_fabricate_callee_edge() {
        let src = "struct G;\nimpl G {\n    fn path(&self) {}\n}\nfn calls_it(g: &G) { g.path(); }\nfn just_a_var() { let path = 1; let _ = path + 1; }\n";
        let g = Graph::build(&[nodes("a.rs", "rust", src)]);
        let path_def = g.find("G.path")[0];
        let caller = g.find("calls_it")[0];
        let var_user = g.find("just_a_var")[0];
        let caller_callees: Vec<usize> = g
            .callees_of(caller)
            .into_iter()
            .flat_map(|(_, d, _)| d)
            .collect();
        assert!(
            caller_callees.contains(&path_def),
            "method call must edge to G.path"
        );
        let var_callees: Vec<String> = g
            .callees_of(var_user)
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        assert!(
            var_callees.is_empty(),
            "local var must not fabricate edges: {var_callees:?}"
        );
    }

    #[test]
    fn scoped_and_python_calls_are_call_position() {
        let occ = lang::ident_occurrences_failopen(Some("rust"), "fn f() { m::g(); h!(); }\n");
        assert!(occ.iter().any(|(n, _, c, _)| n == "g" && *c));
        assert!(occ.iter().any(|(n, _, c, _)| n == "h" && *c));
        assert!(occ.iter().any(|(n, _, c, _)| n == "m" && !*c), "{occ:?}");
        let occ = lang::ident_occurrences_failopen(
            Some("python"),
            "def f(x):\n    x.close()\n    y = close\n",
        );
        assert!(occ.iter().any(|(n, l, c, _)| n == "close" && *l == 2 && *c));
        assert!(occ
            .iter()
            .any(|(n, l, c, _)| n == "close" && *l == 3 && !*c));
    }

    #[test]
    fn definition_token_is_not_a_use() {
        let g = Graph::build(&[nodes("a.rs", "rust", "fn solo() {}\n")]);
        assert!(g.callers_of("solo", &HashSet::new()).is_empty());
    }
}
