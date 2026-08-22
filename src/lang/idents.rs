//! Identifier extraction: semantic (tree-sitter identifier leaves) with the
//! textual word-boundary fallback. Every fail-open policy for refs/grep/
//! rename/call-graph identifier scans lives here.

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
/// *identifier-kind leaf nodes* are collected, so occurrences inside string
/// literals and comments never match — the semantic upgrade over a text scan.
/// Errors when the language can't be parsed; callers fall back to text.
pub fn ident_occurrences(lang: &str, src: &str) -> anyhow::Result<Vec<(String, usize)>> {
    let mut out = Vec::new();
    collect_idents(parse(lang, src)?.root_node(), src, &mut out);
    Ok(out)
}

/// Iterative pre-order over `root` and every descendant. `visit` returns
/// whether to descend into the node's children. Recursion-free like `walk`:
/// traversal depth would equal AST depth, and generated/minified files nest
/// deeper than any thread's stack. TreeCursor keeps it allocation-free.
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
    // Lean traversal for the flag-less callers (refs / tree --rank): pushes
    // (name, line) directly. Deliberately does NOT run call_node_of per ident —
    // that ancestor walk is pure waste when the call flag is thrown away, and
    // this path runs over every identifier of every file on hot commands.
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

/// 1-based lines where `name` occurs as an identifier. Semantic via
/// tree-sitter when the language parses; word-boundary text scan otherwise
/// (fail-open: an unparseable file still yields its textual hits).
pub fn ref_lines(lang: Option<&str>, src: &str, name: &str) -> Vec<usize> {
    // substring pre-check: identifier occurrences are a subset of substring
    // hits, so a miss here skips the whole (dominant) tree-sitter parse
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

/// Occurrence counts of the `names` of interest in `src` — semantic when the
/// language parses, token-scan fallback otherwise. The fail-open policy for
/// counting lives only here.
pub fn ident_counts(
    lang: Option<&str>,
    src: &str,
    names: &std::collections::HashSet<&str>,
) -> std::collections::HashMap<String, i64> {
    let mut counts = std::collections::HashMap::new();
    match lang.and_then(|l| ident_occurrences(l, src).ok()) {
        Some(occ) => {
            for (n, _) in occ {
                if names.contains(n.as_str()) {
                    *counts.entry(n).or_insert(0) += 1;
                }
            }
        }
        None => each_ident_token(src, |t| {
            if names.contains(t) {
                *counts.entry(t.to_string()).or_insert(0) += 1;
            }
        }),
    }
    counts
}

/// Ordered-unique identifier names (≥2 chars) within lines [start, end] of
/// `src` — semantic when parseable, token scan of the sliced lines otherwise.
/// Used by `context` for callee candidates; the whole file is parsed (not the
/// body slice) because fragment parsing is unreliable for indentation-based
/// grammars.
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

/// Identifier occurrences with the standard fail-open policy: semantic when
/// the language parses, per-line token scan otherwise. Feeds the call graph.
/// The bool marks CALL POSITION: the identifier is the function of a call /
/// method call / macro invocation. The trailing `Option<usize>` is the arg
/// count at that call site (`None` when not a call, or when the arg group
/// isn't recognisable) — the arity signal for scope narrowing. Textual
/// fallback can't see syntax and marks everything as a (potential) call with
/// no arg count so edges stay fail-open.
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

/// Is this identifier node the callee of a call? Covers plain calls
/// (`foo(…)`), method/attribute calls (`x.foo(…)`) and rust macros
/// (`foo!(…)`) across the bundled grammars:
/// rust call_expression/macro_invocation, python call, js/ts call_expression
/// (+ new_expression), with field/member/attribute hops in between.
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

/// The enclosing call node when `node` is the callee identifier of a call
/// (plain, method, or macro), else `None` — i.e. `Some(_)` marks CALL
/// POSITION. Arg counting derives from the returned node.
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

/// Number of arguments passed at a call node — the arity signal paired with
/// `param_count`. Finds the arguments group (`arguments`/`argument_list`/…)
/// and counts its NAMED children (skips the `(` `)` `,` anonymous tokens).
/// `None` when the group is absent (e.g. a macro without a plain arg list) so
/// the arity tiebreak simply doesn't fire rather than guessing.
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

/// Byte-exact positions of `name` as an identifier: (1-based line, byte col).
/// Semantic when parseable; word-boundary text scan otherwise (fallback also
/// matches strings/comments — rename callers must warn on that path).
/// Returns (positions, semantic?).
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

/// THE word-boundary text scanner — every textual fallback that needs
/// positions or lines derives from this one implementation.
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
