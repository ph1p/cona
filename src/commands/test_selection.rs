//! Static test-reference evidence, shared by tests, editing context and diff.
//! Paths are relationships, never a claim of runtime coverage.
use super::{callgraph::build_graph, locate_fresh, scan_ref_sites};
use crate::{db, entries, graph::Graph};
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde::Serialize;
use std::{
    collections::{HashSet, VecDeque},
    path::Path,
};

pub const MAX_TEST_DEPTH: usize = 8;
pub const COVERAGE_NOTE: &str =
    "Static name-based references, not proven coverage; no match does not mean untested.";

#[derive(Debug, Clone, Serialize)]
pub struct EvidenceEdge {
    pub from: String,
    pub to: String,
    pub file: String,
    pub line: i64,
    pub ambiguous: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestEvidence {
    pub file: String,
    pub line: i64,
    #[serde(rename = "in")]
    pub enclosing: String,
    pub selector: String,
    pub depth: usize,
    pub ambiguous: bool,
    pub path: Vec<EvidenceEdge>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestSelection {
    pub symbol: String,
    pub tests: Vec<TestEvidence>,
    pub non_test_refs: usize,
    #[serde(skip)]
    pub baseline: i64,
    pub depth: usize,
    pub note: &'static str,
}

/// Reuse one graph and adjacency across a batch of changed symbols.
/// Depth one retains the inexpensive, existing direct-reference scan.
pub struct TestSelector<'a> {
    root: &'a Path,
    conn: &'a Connection,
    depth: usize,
    graph: Option<Graph>,
    reverse: Vec<Vec<(usize, i64, bool)>>,
}

impl<'a> TestSelector<'a> {
    pub fn new(root: &'a Path, conn: &'a Connection, depth: usize) -> Result<Self> {
        if depth == 0 {
            bail!("test selection depth must be at least 1");
        }
        let depth = depth.min(MAX_TEST_DEPTH);
        let graph = if depth > 1 {
            Some(build_graph(root, conn)?.0)
        } else {
            super::refresh_candidate_files(root, conn)?;
            None
        };
        let reverse = graph
            .as_ref()
            .map(Graph::reverse_references)
            .unwrap_or_default();
        Ok(Self {
            root,
            conn,
            depth,
            graph,
            reverse,
        })
    }

    pub fn select(&self, symbol: &str) -> Result<TestSelection> {
        let (def_file, start, end, qualified) = locate_fresh(self.root, self.conn, symbol, None)?;
        self.select_located(def_file, start, end, qualified, false)
    }

    /// Name-only evidence for deleted definitions. Identity is unknown, so
    /// every first edge is explicitly ambiguous, even with no surviving defs.
    pub fn select_name(&self, name: &str) -> Result<TestSelection> {
        self.select_located(String::new(), 0, 0, name.to_string(), true)
    }

    pub fn graph(&self) -> Option<&Graph> {
        self.graph.as_ref()
    }

    fn select_located(
        &self,
        def_file: String,
        start: i64,
        end: i64,
        qualified: String,
        name_only: bool,
    ) -> Result<TestSelection> {
        let name = db::name_tail(&qualified).to_string();
        let target = if name_only {
            qualified.clone()
        } else {
            format!("{def_file}:{qualified}")
        };
        let defs: i64 = self.conn.query_row(
            "SELECT count(*) FROM symbols WHERE name = ?1",
            [&name],
            |r| r.get(0),
        )?;
        let mut result = TestSelection {
            symbol: name.clone(),
            tests: Vec::new(),
            non_test_refs: 0,
            baseline: 0,
            depth: self.depth,
            note: COVERAGE_NOTE,
        };
        let mut seen_files = HashSet::new();
        let mut seen_tests = HashSet::new();
        let mut seeds = Vec::new();
        scan_ref_sites(
            self.root,
            self.conn,
            &name,
            None,
            None,
            |file, line, enclosing, _, source| {
                if seen_files.insert(file.to_string()) {
                    result.baseline += db::est_tokens(source.len());
                }
                if file == def_file
                    && line >= start
                    && line <= end
                    && db::name_tail(enclosing) == name
                {
                    return true;
                }
                let selector = if enclosing.is_empty() {
                    format!("{file}:{line}-{line}")
                } else {
                    format!("{file}:{enclosing}")
                };
                let edge = EvidenceEdge {
                    from: selector.clone(),
                    to: target.clone(),
                    file: file.to_string(),
                    line,
                    ambiguous: name_only || defs > 1,
                };
                seeds.push((file.to_string(), enclosing.to_string(), vec![edge.clone()]));
                if entries::is_test_path(file) || entries::is_test_symbol(enclosing) {
                    if seen_tests.insert((file.to_string(), enclosing.to_string())) {
                        result.tests.push(TestEvidence {
                            file: file.to_string(),
                            line,
                            enclosing: enclosing.to_string(),
                            selector,
                            depth: 1,
                            ambiguous: edge.ambiguous,
                            path: vec![edge],
                        });
                    }
                } else {
                    result.non_test_refs += 1;
                }
                true
            },
        )?;
        if let Some(graph) = &self.graph {
            let mut queue = VecDeque::new();
            let mut visited = HashSet::new();
            // Never walk back into the target, including through a cycle.
            for (i, s) in graph.syms.iter().enumerate() {
                if s.file == def_file && s.qualified == qualified {
                    visited.insert(i);
                }
            }
            for (file, enclosing, path) in seeds {
                if let Some(i) = graph
                    .syms
                    .iter()
                    .position(|s| s.file == file && s.qualified == enclosing)
                {
                    if visited.insert(i) {
                        queue.push_back((i, path));
                    }
                }
            }
            while let Some((current, path)) = queue.pop_front() {
                if path.len() >= self.depth {
                    continue;
                }
                for &(caller, line, ambiguous) in &self.reverse[current] {
                    if !visited.insert(caller) {
                        continue;
                    }
                    let s = &graph.syms[caller];
                    let callee = &graph.syms[current];
                    let selector = format!("{}:{}", s.file, s.qualified);
                    let mut next = path.clone();
                    next.push(EvidenceEdge {
                        from: selector.clone(),
                        to: format!("{}:{}", callee.file, callee.qualified),
                        file: s.file.clone(),
                        line,
                        ambiguous,
                    });
                    if (entries::is_test_path(&s.file) || entries::is_test_symbol(&s.qualified))
                        && seen_tests.insert((s.file.clone(), s.qualified.clone()))
                    {
                        result.tests.push(TestEvidence {
                            file: s.file.clone(),
                            line,
                            enclosing: s.qualified.clone(),
                            selector,
                            depth: next.len(),
                            ambiguous: next.iter().any(|e| e.ambiguous),
                            path: next.clone(),
                        });
                    }
                    queue.push_back((caller, next));
                }
            }
        }
        result.tests.sort_by(|a, b| {
            (a.depth, &a.file, a.line, &a.enclosing).cmp(&(b.depth, &b.file, b.line, &b.enclosing))
        });
        Ok(result)
    }
}

pub fn select_tests(
    root: &Path,
    conn: &Connection,
    symbol: &str,
    depth: usize,
) -> Result<TestSelection> {
    TestSelector::new(root, conn, depth)?.select(symbol)
}

#[cfg(test)]
#[path = "test_selection_tests.rs"]
mod tests;
