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
/// the caller. Beyond scope/file (the original policy) it carries the declared
/// param count and whether the def is a method (implicit-receiver offset) so
/// the arity rule can match a call's arg count.
pub struct Candidate {
    pub scope: Option<String>,
    pub file: String,
    pub params: Option<usize>,
    pub is_method: bool,
}

/// THE scope-preference disambiguation policy (name-based, no type
/// resolution), shared by `Graph::prefer_scope` and `context`: when several
/// definitions share a name, prefer (1) the one in the caller's own parent
/// scope (same class/impl/namespace — covers `self.x()` / `this.x()`), then
/// (2) the one in the caller's file, then (3) the one in the caller's own
/// directory (module-local proximity), then (4) the one whose declared arity
/// matches the number of arguments at the call site. Only narrows when EXACTLY
/// one candidate survives a rule — never silently picks among equals, so
/// remaining multi-def results still surface as ambiguous.
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
    let my_dir = dir_of(my_file);
    // derive each candidate's signals ONCE (the `key` closure may re-parse a
    // signature — see cmd_context), then test the precomputed values per rule
    let derived: Vec<Candidate> = cands.iter().map(&key).collect();
    let survives = |cand: &Candidate, rule: usize| -> bool {
        match rule {
            0 => my_scope.is_some() && cand.scope.as_deref() == my_scope,
            1 => cand.file == my_file,
            // proximity: same directory as the caller — a weak but
            // false-positive-light signal for module-local resolution
            2 => dir_of(&cand.file) == my_dir,
            // arity: the def's declared params (minus an implicit receiver for
            // methods) equal the arguments passed at the call site
            _ => match (cand.params, argc) {
                (Some(p), Some(a)) => {
                    let effective = if cand.is_method {
                        p.saturating_sub(1)
                    } else {
                        p
                    };
                    effective == a
                }
                _ => false,
            },
        }
    };
    for rule in [0, 1, 2, 3] {
        // the arity rule can't fire without a known call-site arg count
        if rule == 3 && argc.is_none() {
            break;
        }
        if derived.iter().filter(|c| survives(c, rule)).count() == 1 {
            return cands
                .into_iter()
                .zip(&derived)
                .filter(|(_, c)| survives(c, rule))
                .map(|(t, _)| t)
                .collect();
        }
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
    /// declared parameter count parsed from the signature (arity signal);
    /// `None` when the signature has no parameter list to compare against
    pub params: Option<usize>,
    /// first param is an implicit receiver (`self`/`this`) not written at the
    /// call site — so effective arity is `params - 1`
    pub recv: bool,
}

/// Name-based call graph over the whole index, built in ONE pass so
/// depth-limited traversals never rescan files. Same resolution policy as
/// `context`: identifier names, no type/scope resolution — multiple
/// definitions of a name are all kept and callers mark them ambiguous.
pub struct Graph {
    pub syms: Vec<SymNode>,
    by_name: HashMap<String, Vec<usize>>,
    /// ident name → occurrences as (innermost enclosing sym, line) — ALL
    /// occurrences, so callers-of works for types and callback references too
    uses: HashMap<String, Vec<(usize, i64)>>,
    /// per sym: ordered-unique (name, arg_count) in CALL POSITION in its
    /// range — callee edges only follow actual calls, so a local variable
    /// named like a method elsewhere doesn't fabricate an edge. `arg_count`
    /// is the arity signal at the call site (`None` when unknown).
    // per symbol: the calls it makes — (callee name, arg count, first call-site
    // line). The line lets the semantic tier resolve an ambiguous callee at its
    // actual call position.
    calls: Vec<Vec<(String, Option<usize>, i64)>>,
}

impl Graph {
    /// `files`: (path, lang, source, symbols-of-that-file). Symbol vectors
    /// come from the (fresh) index; occurrences are re-derived from source
    /// with the usual fail-open policy (semantic when parseable).
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
            // one sweep instead of a per-occurrence scan over all symbols:
            // ranges nest, so a stack of open ranges keeps the latest-started
            // containing symbol on top — the same "innermost" tiebreak
            // ENCLOSING_SYMBOL_SQL uses (greatest start_line)
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
    /// excluding occurrences inside any definition of `name` itself
    /// (a recursive call still counts as self→self and is kept out here).
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

    /// Direct callees of sym `idx`: names in its body that resolve to indexed
    /// definitions. Returns (name, defs) — several defs = ambiguous (after
    /// scope-preference narrowing).
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
    /// `argc` is the arg count at the call site (arity signal), if known.
    /// NOTE: this whole-index call-graph path deliberately stops at the cheap
    /// tiers (scope/file/dir/arity). The out-of-process semantic-resolve tier
    /// (see `crate::resolve`) is applied ONLY in `cmd_context`, which has the
    /// single-file source + line to hand the helper; running a subprocess per
    /// ambiguous edge across the entire index would be far too costly here.
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
        // two `helper` defs: one method beside the caller in class A, one free
        // function in another file — the same-scope one must win, unambiguous
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
        // caller and one `util` live in dir `m/`, the other `util` in `other/`.
        // scope + same-file both fail to reduce to one; directory proximity
        // (rule 3) picks the sibling in m/.
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
        // two `emit` defs in two OTHER directories (scope/file/dir all fail to
        // reduce to one); the call passes two args → the 2-param def wins.
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
        // a free `finish(a, b, c)` and a method `.finish(self, trailer)`:
        // the method's declared params = 2 but a call `.finish(x)` passes ONE
        // arg (self is the receiver). The offset must let the method win.
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
