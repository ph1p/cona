//! Out-of-process stack-graphs name resolver for cona.
//!
//! Reads ONE JSON request from stdin, resolves the requested reference
//! positions to their definition site(s) via stack-graphs, writes ONE JSON
//! response to stdout. cona spawns this lazily only for the languages that
//! ship published TSG rules (typescript/tsx/javascript/python) and only when a
//! name stayed ambiguous after the cheap heuristics — so the per-call cost of
//! building a stack graph is paid rarely, never in the index write path.
//!
//! Protocol (line-free; the whole stdin is one JSON object). References are
//! identified by 1-based line + symbol name (NOT column) so the two sides never
//! have to agree on byte-vs-utf8 column encoding. The primary file carries the
//! refs to resolve; optional `deps` are extra files stitched into the SAME
//! stack graph so a reference can resolve to a definition in another file
//! (cross-file resolution). Each resolved def reports its `file` so the caller
//! knows where it landed:
//!   request : {"lang":"typescript","path":"a.ts","source":"…",
//!              "refs":[{"line":4,"name":"finish"}],
//!              "deps":[{"path":"b.ts","source":"…"}]}
//!   response: {"resolved":[{"ref":{"line":4,"name":"finish"},
//!              "defs":[{"file":"b.ts","line":1,"symbol":"finish"}]}]}
//!
//! A ref with no resolvable definition (or one that is ambiguous on its line —
//! same name twice) comes back with empty `defs`; the caller keeps its
//! name-based result. On any hard error the process prints `{"error":"…"}` and
//! exits non-zero; cona treats that (and a missing binary) as "no semantic
//! signal" and degrades gracefully.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use stack_graphs::arena::Handle;
use stack_graphs::graph::{Node, StackGraph};
use stack_graphs::partial::PartialPaths;
use stack_graphs::stitching::{
    Database, DatabaseCandidates, ForwardPartialPathStitcher, StitcherConfig,
};
use std::io::Read;
use tree_sitter_graph::Variables;
use tree_sitter_stack_graphs::loader::LanguageConfiguration;
use tree_sitter_stack_graphs::{CancellationFlag, NoCancellation};

#[derive(Deserialize)]
struct Request {
    lang: String,
    #[serde(default)]
    path: String,
    source: String,
    refs: Vec<Ref>,
    /// Extra files stitched into the same stack graph so refs can resolve to
    /// definitions outside the primary file (cross-file resolution). Optional.
    #[serde(default)]
    deps: Vec<DepFile>,
}

/// A dependency file fed alongside the primary one — same language, own path +
/// source. Only its definitions matter (we never resolve refs INSIDE a dep).
#[derive(Deserialize)]
struct DepFile {
    path: String,
    source: String,
}

/// A reference to resolve: identified by 1-based line + symbol name. No column
/// — matching is by (line, name), which avoids any byte-vs-utf8 column coupling
/// between cona and this helper.
#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
struct Ref {
    line: usize,
    name: String,
}

#[derive(Serialize)]
struct Response {
    resolved: Vec<Resolved>,
}

#[derive(Serialize)]
struct Resolved {
    #[serde(rename = "ref")]
    reference: Ref,
    defs: Vec<Def>,
}

#[derive(Serialize)]
struct Def {
    /// File the definition lives in (the primary file's path, or a dep's).
    /// Lets the caller act on cross-file resolutions, not just same-file.
    file: String,
    line: usize,
    symbol: Option<String>,
}

/// The published TSG rule sets we bundle. Returns `None` for a language we
/// don't carry rules for — the caller then simply gets no semantic signal.
fn language_config(lang: &str, cancel: &dyn CancellationFlag) -> Option<LanguageConfiguration> {
    match lang {
        "typescript" | "ts" => {
            Some(tree_sitter_stack_graphs_typescript::language_configuration_typescript(cancel))
        }
        "tsx" => Some(tree_sitter_stack_graphs_typescript::language_configuration_tsx(cancel)),
        "javascript" | "js" => Some(tree_sitter_stack_graphs_javascript::language_configuration(
            cancel,
        )),
        "python" | "py" => Some(tree_sitter_stack_graphs_python::language_configuration(
            cancel,
        )),
        // Rust has NO published TSG crate; rules are hand-authored in rust.tsg
        // (bundled at compile time). Build the LanguageConfiguration at runtime
        // from that source. Fail-open: a TSG parse error yields None, so the
        // caller simply gets no semantic signal for Rust rather than a crash.
        "rust" | "rs" => rust_language_config(cancel),
        _ => None,
    }
}

/// The hand-authored Rust name-binding rules (see rust.tsg). No published crate
/// ships these, so they are compiled into the helper and loaded at runtime.
const RUST_TSG_SOURCE: &str = include_str!("../rust.tsg");

fn rust_language_config(cancel: &dyn CancellationFlag) -> Option<LanguageConfiguration> {
    LanguageConfiguration::from_sources(
        tree_sitter_rust::LANGUAGE.into(),
        Some(String::from("source.rs")),
        None,
        vec![String::from("rs")],
        std::path::PathBuf::from("rust.tsg"),
        RUST_TSG_SOURCE,
        None,
        None,
        cancel,
    )
    .ok()
}

fn main() {
    match run() {
        Ok(resp) => {
            println!("{}", serde_json::to_string(&resp).unwrap());
        }
        Err(e) => {
            // structured error on stdout so the caller can log it, non-zero exit
            println!("{}", serde_json::json!({ "error": e.to_string() }));
            std::process::exit(1);
        }
    }
}

fn run() -> Result<Response> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("read stdin")?;
    let req: Request = serde_json::from_str(&input).context("parse request json")?;

    let cancel = NoCancellation;
    let Some(lc) = language_config(&req.lang, &cancel) else {
        bail!("no TSG rules for language {:?}", req.lang);
    };

    let primary_name = if req.path.is_empty() {
        "input"
    } else {
        req.path.as_str()
    };

    // build ONE stack graph spanning the primary file plus any dep files, so a
    // reference in the primary file can resolve to a definition in a dep.
    let mut graph = StackGraph::new();
    let globals = Variables::new();
    let primary = graph.get_or_create_file(primary_name);
    lc.sgl
        .build_stack_graph_into(&mut graph, primary, &req.source, &globals, &cancel)
        .context("build stack graph (primary)")?;
    // dep files: a failed parse of one dep must not sink the whole request —
    // best-effort, we just skip a dep we can't build.
    let mut files = vec![primary];
    for dep in &req.deps {
        if dep.path == primary_name {
            continue; // never double-add the primary under a dep entry
        }
        let f = graph.get_or_create_file(&dep.path);
        if lc
            .sgl
            .build_stack_graph_into(&mut graph, f, &dep.source, &globals, &cancel)
            .is_ok()
        {
            files.push(f);
        }
    }

    // index every file's partial paths once, reused across all ref queries
    let config = StitcherConfig::default();
    let mut partials = PartialPaths::new();
    let mut db = Database::new();
    for &f in &files {
        ForwardPartialPathStitcher::find_minimal_partial_path_set_in_file(
            &graph,
            &mut partials,
            f,
            config,
            &(&cancel as &dyn CancellationFlag),
            |g, ps, p| {
                db.add_partial_path(g, ps, p.clone());
            },
        )
        .context("seed partial paths")?;
    }

    let line_of = |g: &StackGraph, n: Handle<Node>| -> Option<usize> {
        g.source_info(n).map(|si| si.span.start.line + 1)
    };
    let sym_of = |g: &StackGraph, n: Handle<Node>| g[n].symbol().map(|s| g[s].to_string());
    let file_of = |g: &StackGraph, n: Handle<Node>| -> Option<String> {
        g[n].file().map(|f| g[f].name().to_string())
    };

    // reference nodes grouped by (line, name). stack-graphs can emit MORE than
    // one reference node for a single source occurrence (member-access
    // scaffolding), so a key maps to all of them; we resolve each and union the
    // definitions. A definition NODE also carries its own name as a "reference"
    // in some grammars — those resolve to themselves and are filtered out by
    // dropping defs that sit on the reference's own line.
    let mut ref_by_key: std::collections::HashMap<(usize, String), Vec<Handle<Node>>> =
        std::collections::HashMap::new();
    for n in graph.iter_nodes() {
        if !graph[n].is_reference() {
            continue;
        }
        // refs are addressed by (line, name) within the PRIMARY file only — a
        // dep file's own references are never something the caller asked about.
        if graph[n].file() != Some(primary) {
            continue;
        }
        let (Some(line), Some(name)) = (line_of(&graph, n), sym_of(&graph, n)) else {
            continue;
        };
        ref_by_key.entry((line, name)).or_default().push(n);
    }

    let mut resolved = Vec::with_capacity(req.refs.len());
    for want in &req.refs {
        let nodes = ref_by_key
            .get(&(want.line, want.name.clone()))
            .cloned()
            .unwrap_or_default();

        let mut defs: Vec<Def> = Vec::new();
        for r in nodes {
            ForwardPartialPathStitcher::find_all_complete_partial_paths(
                &mut DatabaseCandidates::new(&graph, &mut partials, &mut db),
                vec![r],
                config,
                &(&cancel as &dyn CancellationFlag),
                |g, _ps, p| {
                    let end = p.end_node;
                    if g[end].is_definition() {
                        if let (Some(line), Some(file)) = (line_of(g, end), file_of(g, end)) {
                            // a def on the ref's own line IN THE PRIMARY FILE is
                            // the self-reference of a definition node — not a
                            // real resolution. A same-line def in a DEP file is
                            // a genuine cross-file hit and must be kept.
                            if !(file == primary_name && line == want.line) {
                                defs.push(Def {
                                    file,
                                    line,
                                    symbol: sym_of(g, end),
                                });
                            }
                        }
                    }
                },
            )
            .context("resolve reference")?;
        }

        // union across the occurrence's ref nodes; dedup identical defs
        defs.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
        defs.dedup_by(|a, b| a.file == b.file && a.line == b.line);

        resolved.push(Resolved {
            reference: want.clone(),
            defs,
        });
    }

    Ok(Response { resolved })
}
