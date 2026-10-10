//! Identifier extraction: semantic (tree-sitter identifier leaves) with a
//! word-boundary text fallback. Every fail-open policy for identifier scans
//! (refs/grep/rename/call graph) lives here.

use super::parse;
use tree_sitter::Node;

/// Tokenizer behind every textual fallback: yields identifier-shaped tokens
/// (≥2 chars, ASCII, no leading digit) in source order, duplicates included.
fn each_ident_token(src: &str, mut f: impl FnMut(&str)) {
    let mut token = String::new();
    for c in src.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_alphanumeric() || c == '_' {
            token.push(c);
            continue;
        }
        if token.len() >= 2 && !token.starts_with(|t: char| t.is_ascii_digit()) {
            f(&token);
        }
        token.clear();
    }
}

/// Ordered, de-duplicated identifier tokens in a code snippet — the textual
/// fallback for callee candidates when tree-sitter can't parse.
pub fn extract_idents(src: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    each_ident_token(src, |t| {
        if seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    });
    out
}

/// Identifier occurrences as (name, 1-based line) via tree-sitter. Only
/// identifier-kind leaves count, so strings and comments never match.
/// Errors when the language can't be parsed; callers fall back to text.
pub fn ident_occurrences(lang: &str, src: &str) -> anyhow::Result<Vec<(String, usize)>> {
    let mut out = Vec::new();
    collect_idents(parse(lang, src)?.root_node(), src, &mut out);
    Ok(out)
}

/// Iterative pre-order over `root`; `visit` returns whether to descend.
/// Recursion-free like `walk`, since generated/minified files nest deeper than
/// any thread's stack. TreeCursor keeps it allocation-free.
pub(crate) fn for_each_node<'t>(root: Node<'t>, mut visit: impl FnMut(Node<'t>) -> bool) {
    let mut cursor = root.walk();
    'down: loop {
        if visit(cursor.node()) && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'down;
            }
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

fn collect_idents(node: Node, src: &str, out: &mut Vec<(String, usize)>) {
    // Lean path for flag-less callers (refs / tree --rank). Deliberately skips
    // call_node_of: that ancestor walk is waste when the call flag is dropped,
    // and this runs over every identifier of every file on hot commands.
    for_each_node(node, |n| {
        if n.child_count() == 0 && n.kind().ends_with("identifier") {
            if let Ok(text) = n.utf8_text(src.as_bytes()) {
                out.push((text.to_string(), n.start_position().row + 1));
            }
            return false;
        }
        true
    });
}

/// 1-based lines where `name` occurs as an identifier: semantic when the
/// language parses, else fail-open word-boundary text scan.
pub fn ref_lines(lang: Option<&str>, src: &str, name: &str) -> Vec<usize> {
    // identifier hits are a subset of substring hits — a miss skips the parse
    if !src.contains(name) {
        return Vec::new();
    }
    if let Some(l) = lang {
        if let Ok(tree) = parse(l, src) {
            let mut lines = Vec::new();
            collect_named_lines(tree.root_node(), src, name, &mut lines);
            // nodes arrive in source order → adjacent dedup = one hit per line
            lines.dedup();
            return lines;
        }
    }
    ref_lines_textual(src, name)
}

fn collect_named_lines(node: Node, src: &str, name: &str, out: &mut Vec<usize>) {
    // same traversal as collect_named_positions, column dropped
    let mut pos = Vec::new();
    collect_named_positions(node, src, name, &mut pos);
    out.extend(pos.into_iter().map(|(ln, _)| ln));
}

/// Occurrence counts of `names` in `src`, semantic or token-scan fallback.
/// The fail-open policy for counting lives only here.
pub fn ident_counts(
    lang: Option<&str>,
    src: &str,
    names: &std::collections::HashSet<&str>,
) -> std::collections::HashMap<String, i64> {
    let mut counts = std::collections::HashMap::new();
    // Callers count uses of TOP-LEVEL items, which Rust never reaches as
    // `x.name` — so a method call or field (`.ok()`, `.len`) is not a use of a
    // same-named free fn. Go's `pkg.Func()` shares the leaf kind, hence rust-only.
    let skip_fields = lang == Some("rust");
    match lang.and_then(|l| parse(l, src).ok()) {
        Some(tree) => for_each_node(tree.root_node(), |n| {
            if n.child_count() == 0 && n.kind().ends_with("identifier") {
                if !(skip_fields && n.kind() == "field_identifier") {
                    if let Ok(t) = n.utf8_text(src.as_bytes()) {
                        if names.contains(t) {
                            *counts.entry(t.to_string()).or_insert(0) += 1;
                        }
                    }
                }
                return false;
            }
            true
        }),
        None => each_ident_token(src, |t| {
            if names.contains(t) {
                *counts.entry(t.to_string()).or_insert(0) += 1;
            }
        }),
    }
    counts
}

/// Ordered-unique identifiers (≥2 chars) in lines [start, end] — `context`'s
/// callee candidates. Parses the whole file, not the slice: fragment parsing
/// is unreliable for indentation-based grammars.
pub fn idents_in_range(lang: Option<&str>, src: &str, start: usize, end: usize) -> Vec<String> {
    match lang.and_then(|l| ident_occurrences(l, src).ok()) {
        Some(occ) => {
            let mut seen = std::collections::HashSet::new();
            occ.into_iter()
                .filter(|(_, ln)| *ln >= start && *ln <= end)
                .map(|(n, _)| n)
                .filter(|n| n.len() >= 2 && seen.insert(n.clone()))
                .collect()
        }
        None => {
            let body: Vec<&str> = src
                .lines()
                .skip(start.saturating_sub(1))
                .take(end.saturating_sub(start) + 1)
                .collect();
            extract_idents(&body.join("\n"))
        }
    }
}

/// Identifier occurrences for the call graph, fail-open. The bool marks CALL
/// POSITION (callee of a call / method call / macro); the `Option<usize>` is
/// the arg count there (`None` if not a call or no recognisable arg group) —
/// the arity signal for scope narrowing. The text fallback can't see syntax,
/// so it marks everything as a potential call with no arg count.
pub fn ident_occurrences_failopen(
    lang: Option<&str>,
    src: &str,
) -> Vec<(String, usize, bool, Option<usize>)> {
    if let Some(l) = lang {
        if let Ok(tree) = parse(l, src) {
            let mut out = Vec::new();
            collect_idents_with_call(tree.root_node(), src, &mut out);
            return out;
        }
    }
    let mut out = Vec::new();
    for (ln, line) in src.lines().enumerate() {
        each_ident_token(line, |t| out.push((t.to_string(), ln + 1, true, None)));
    }
    out
}

/// Call node kinds across the bundled grammars: plain calls (`foo(…)`),
/// constructors (`new`), and rust macros (`foo!(…)`).
fn is_call_kind(k: &str) -> bool {
    matches!(
        k,
        "call_expression"
            | "call"
            | "function_call"
            | "new_expression"
            | "macro_invocation"
            | "method_invocation"
            | "object_creation_expression"
    )
}

/// The enclosing call node when `node` is its callee (plain, method, macro);
/// `Some(_)` marks CALL POSITION, and arg counting uses the returned node.
fn call_node_of(node: Node) -> Option<Node> {
    let parent = node.parent()?;
    let pk = parent.kind();
    if is_call_kind(pk) {
        for field in ["function", "macro", "constructor", "name", "type"] {
            if parent
                .child_by_field_name(field)
                .map(|f| f.id() == node.id())
                .unwrap_or(false)
            {
                return Some(parent);
            }
        }
        return None;
    }
    // method call: the identifier is the field/property/attribute of an
    // access expression that is itself the function of a call
    if matches!(
        pk,
        "field_expression"
            | "member_expression"
            | "attribute"
            | "scoped_identifier"
            | "selector_expression"
            | "qualified_identifier"
    ) {
        let named_me = ["field", "property", "attribute", "name"].iter().any(|f| {
            parent
                .child_by_field_name(f)
                .map(|c| c.id() == node.id())
                .unwrap_or(false)
        });
        if !named_me {
            return None;
        }
        let gp = parent.parent()?;
        if is_call_kind(gp.kind())
            && gp
                .child_by_field_name("function")
                .map(|f| f.id() == parent.id())
                .unwrap_or(false)
        {
            return Some(gp);
        }
    }
    None
}

/// Arguments at a call node (pairs with `param_count`): NAMED children of the
/// arg group, skipping `(` `)` `,`. `None` without a group (e.g. a macro), so
/// the arity tiebreak doesn't fire rather than guessing.
fn arg_count_of(call: Node) -> Option<usize> {
    if let Some(args) = call.child_by_field_name("arguments") {
        return Some(args.named_child_count());
    }
    let mut cursor = call.walk();
    for c in call.children(&mut cursor) {
        if matches!(
            c.kind(),
            "arguments" | "argument_list" | "arg_list" | "argument"
        ) {
            return Some(c.named_child_count());
        }
    }
    None
}

fn collect_idents_with_call(
    node: Node,
    src: &str,
    out: &mut Vec<(String, usize, bool, Option<usize>)>,
) {
    for_each_node(node, |n| {
        if n.child_count() == 0 && n.kind().ends_with("identifier") {
            if let Ok(text) = n.utf8_text(src.as_bytes()) {
                let call = call_node_of(n);
                out.push((
                    text.to_string(),
                    n.start_position().row + 1,
                    call.is_some(),
                    call.and_then(arg_count_of),
                ));
            }
            return false;
        }
        true
    });
}

/// Positions of `name` as an identifier, (1-based line, byte col), plus
/// whether the scan was semantic. The text fallback also matches
/// strings/comments — rename callers must warn on that path.
pub fn ident_positions(lang: Option<&str>, src: &str, name: &str) -> (Vec<(usize, usize)>, bool) {
    if !src.contains(name) {
        return (Vec::new(), true); // no occurrences — no fallback was needed
    }
    if let Some(l) = lang {
        if let Ok(tree) = parse(l, src) {
            let mut out = Vec::new();
            collect_named_positions(tree.root_node(), src, name, &mut out);
            return (out, true);
        }
    }
    (textual_positions(src, name), false)
}

/// THE word-boundary text scanner; every textual fallback needing positions
/// or lines derives from it.
fn textual_positions(src: &str, name: &str) -> Vec<(usize, usize)> {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = Vec::new();
    for (ln, line) in src.lines().enumerate() {
        let mut from = 0usize;
        while let Some(pos) = line[from..].find(name) {
            let i = from + pos;
            let before_ok = i == 0 || !is_ident(line[..i].chars().next_back().unwrap_or(' '));
            let after = line[i + name.len()..].chars().next().unwrap_or(' ');
            if before_ok && !is_ident(after) {
                out.push((ln + 1, i));
            }
            from = i + name.len();
        }
    }
    out
}

fn collect_named_positions(node: Node, src: &str, name: &str, out: &mut Vec<(usize, usize)>) {
    for_each_node(node, |n| {
        if n.child_count() == 0 && n.kind().ends_with("identifier") {
            if n.utf8_text(src.as_bytes()) == Ok(name) {
                out.push((n.start_position().row + 1, n.start_position().column));
            }
            return false;
        }
        true
    });
}

fn ref_lines_textual(src: &str, name: &str) -> Vec<usize> {
    let mut lines: Vec<usize> = textual_positions(src, name)
        .into_iter()
        .map(|(ln, _)| ln)
        .collect();
    lines.dedup(); // one hit per line is enough
    lines
}
