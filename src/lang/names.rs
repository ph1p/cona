//! Symbol naming: resolves a classified node to its display name, covering
//! every sentinel `name_field` (Elixir def calls, markdown headings, HTML/XML
//! elements, HCL blocks, …) plus the shared signature line helper.

use super::classify::{
    DEF_CALL, DOCKER_FROM, FIRST_CHILD, FIXED_NAME, HCL_BLOCK, HEADING, HTML_ELEMENT, NESTED,
    XML_ELEMENT,
};
use tree_sitter::Node;

/// Resolves names for the NESTED sentinel: grammars where the identifier is a
/// known child kind buried past keywords/wrappers. Returns None → symbol skipped.
fn nested_name(lang: &str, node: Node, src: &str) -> Option<String> {
    // first descendant (shallow-first per node) whose kind is in `kinds` —
    // worklist, not recursion, so a pathological declaration can't blow the stack
    fn first_of<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            let mut cur = n.walk();
            for ch in n.named_children(&mut cur) {
                if kinds.contains(&ch.kind()) {
                    return Some(ch);
                }
            }
            // no direct hit → descend (signatures/type_head wrap the identifier)
            let base = stack.len();
            let mut cur = n.walk();
            stack.extend(n.named_children(&mut cur));
            stack[base..].reverse();
        }
        None
    }
    // Julia: the name lives in the `signature`/`type_head` wrapper (always the
    // first named child) — descend into it, else we'd grab a param/field ident.
    if lang == "julia" {
        let mut cur = node.walk();
        let wrapper = node
            .named_children(&mut cur)
            .find(|c| matches!(c.kind(), "signature" | "type_head"))
            .unwrap_or(node);
        let hit = first_of(wrapper, &["identifier"])?;
        let text = hit.utf8_text(src.as_bytes()).ok()?;
        let t = text.split(['(', '{']).next().unwrap_or(text).trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    let target: &[&str] = match lang {
        "ocaml" => &[
            "value_name",
            "type_constructor",
            "module_name",
            "class_name",
        ],
        "sql" => &["object_reference"],
        "perl" => &["package_name"],
        "objc" => &["identifier"],
        "make" => &["word"],
        _ => return None,
    };
    let hit = first_of(node, target)?;
    let text = hit.utf8_text(src.as_bytes()).ok()?;
    let t = text.split(['(', '<']).next().unwrap_or(text).trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Elixir names live in the first argument of a `def`-family call.
/// Returns None for calls that aren't definitions (so they're skipped).
fn elixir_def_name(node: Node, src: &str) -> Option<String> {
    // shape: (call (identifier "def") (arguments (call (identifier NAME) …)))
    //   or:  (call (identifier "defmodule") (arguments (alias NAME)))
    let head = node.named_child(0)?;
    if head.kind() != "identifier" {
        return None;
    }
    let kw = head.utf8_text(src.as_bytes()).ok()?;
    if !matches!(kw, "def" | "defp" | "defmodule" | "defmacro" | "defmacrop") {
        return None;
    }
    let args = {
        let mut found = node.child_by_field_name("arguments");
        if found.is_none() {
            let mut cur = node.walk();
            found = node
                .named_children(&mut cur)
                .find(|c| c.kind() == "arguments");
        }
        found
    }?;
    let first = args.named_child(0)?;
    // def bar(x) → first arg is a `call` whose head is the fn name;
    // defp baz → first arg is the identifier itself;
    // defmodule Foo → first arg is an `alias`
    let name_node = match first.kind() {
        "call" => first.named_child(0)?,
        _ => first,
    };
    let text = name_node.utf8_text(src.as_bytes()).ok()?;
    let t = text.split('(').next().unwrap_or(text).trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Attributes that identify an element, most specific first. Framework
/// directives (Thymeleaf `th:*`, Vue `v-*`, Alpine `x-*`, Angular, htmx) are
/// matched by prefix in `html_attr_identity`, not listed here.
/// `href`/`src` are deliberately absent: a URL is long, unstable, and makes a
/// worse symbol name than the bare tag.
const HTML_ID_ATTRS: &[&str] = &["id", "name", "data-testid", "data-test", "slot", "rel"];

/// Tags that are structural landmarks: worth a symbol even with no attributes,
/// because they are what an agent navigates a template by.
const HTML_STRUCTURAL_TAGS: &[&str] = &[
    "html", "head", "body", "main", "header", "footer", "nav", "aside", "section", "article",
    "form", "table", "script", "style", "template", "dialog", "h1", "h2", "h3", "h4", "h5", "h6",
];

/// HTML element name: `tag#identity` when the element carries an identifying
/// attribute, bare `tag` when it is structural, and `None` otherwise — which
/// makes the walk skip it (its children are still visited, so a nested
/// `<button id=…>` inside plain `<div>`s is not lost).
/// shape: (element (start_tag (tag_name TAG) (attribute (attribute_name K)
///                            (quoted_attribute_value (attribute_value V)))*) …)
fn html_element_name(node: Node, src: &str) -> Option<String> {
    let tag_node = child_of_kind(node, &["start_tag", "self_closing_tag"])?;
    let tag = node_text(child_of_kind(tag_node, &["tag_name"])?, src)?.to_ascii_lowercase();
    if tag.is_empty() {
        return None;
    }
    if let Some(identity) = html_attr_identity(tag_node, src) {
        return Some(format!("{tag}#{identity}"));
    }
    HTML_STRUCTURAL_TAGS.contains(&tag.as_str()).then_some(tag)
}

/// The identifying attribute value of a start tag, if any. Explicit attributes
/// win over directives: an `id` names the element better than the expression in
/// a `th:each`, which is why the directive is reported as the attribute NAME
/// (`li@th:each`) rather than its value — the value is code, not an identifier.
fn html_attr_identity(tag: Node, src: &str) -> Option<String> {
    let attr_value = |a: Node| -> Option<&str> {
        let v = child_of_kind(a, &["quoted_attribute_value", "attribute_value"])?;
        let inner = child_of_kind(v, &["attribute_value"]).unwrap_or(v);
        let t = node_text(inner, src)?;
        (!t.is_empty() && t.len() <= 60).then_some(t)
    };
    let is_directive = |n: &str| {
        n.starts_with("th:")
            || n.starts_with("v-")
            || n.starts_with("x-")
            || n.starts_with("hx-")
            || n.starts_with('*')
            || n.starts_with('@')
            || (n.starts_with('[') && n.ends_with(']'))
    };
    // One pass: keep the best HTML_ID_ATTRS rank and the first directive seen,
    // rather than re-walking every attribute once per candidate name. Names are
    // compared borrowed, so a plain <div> (the common case) allocates nothing.
    let mut cur = tag.walk();
    let mut best: Option<(usize, &str)> = None;
    let mut directive: Option<&str> = None;
    for a in tag
        .named_children(&mut cur)
        .filter(|c| c.kind() == "attribute")
    {
        // a nameless attribute must skip to the next, not abandon the search
        let Some(name) = child_of_kind(a, &["attribute_name"]).and_then(|n| node_text(n, src))
        else {
            continue;
        };
        match HTML_ID_ATTRS.iter().position(|w| *w == name) {
            Some(rank) if best.is_none_or(|(b, _)| b > rank) => {
                if let Some(v) = attr_value(a) {
                    best = Some((rank, v));
                }
            }
            // framework directives: the attribute name is the identity
            None if directive.is_none() && is_directive(name) => directive = Some(name),
            _ => {}
        }
    }
    match best {
        Some((_, v)) => Some(v.to_string()),
        None => directive.map(|n| format!("@{n}")),
    }
}

/// Child tags whose text identifies the element that contains them, most
/// specific first. `artifactId` before `id` so a Maven `<plugin>` is named by
/// its artifact rather than an `<id>` that may sit deeper in the subtree.
const XML_ID_CHILDREN: &[&str] = &["artifactId", "id", "name", "key", "Include", "groupId"];

/// First direct named child of one of `kinds`. The cursor-walk-and-find idiom
/// is otherwise hand-rolled at every markup call site.
fn child_of_kind<'t>(node: Node<'t>, kinds: &[&str]) -> Option<Node<'t>> {
    let mut cur = node.walk();
    let found = node
        .named_children(&mut cur)
        .find(|c| kinds.contains(&c.kind()));
    found
}

/// A node's source text, trimmed. Borrowed — callers allocate only on a hit.
fn node_text<'s>(node: Node, src: &'s str) -> Option<&'s str> {
    Some(node.utf8_text(src.as_bytes()).ok()?.trim())
}

/// The tag name of an element, read from its `STag`/`EmptyElemTag` opener.
fn xml_tag_name<'s>(element: Node, src: &'s str) -> Option<&'s str> {
    let opener = child_of_kind(element, &["STag", "EmptyElemTag"])?;
    node_text(child_of_kind(opener, &["Name"])?, src)
}

/// XML element name: the tag, plus an identifying child's text when present
/// (`<profile><id>with-frontend-build</id>` → `profile.with-frontend-build`).
/// shape: (element (STag (Name TAG) (Attribute …)*) CONTENT* (ETag …))
fn xml_element_name(node: Node, src: &str) -> Option<String> {
    let tag = xml_tag_name(node, src)?;
    if tag.is_empty() {
        return None;
    }
    // Only DIRECT element children may identify this one — a grandchild's <id>
    // belongs to that child, and borrowing it would give two elements one name.
    // Children sit one level down, inside the `content` wrapper:
    //   (element (STag …) (content (element …)*) (ETag …))
    //
    // One pass over the children, keeping the best XML_ID_CHILDREN rank seen,
    // rather than re-walking every child once per candidate name.
    let mut cur = node.walk();
    let mut best: Option<(usize, &str)> = None;
    for content in node
        .named_children(&mut cur)
        .filter(|c| c.kind() == "content")
    {
        let mut cc = content.walk();
        for child in content
            .named_children(&mut cc)
            .filter(|g| g.kind() == "element")
        {
            let Some(name) = xml_tag_name(child, src) else {
                continue;
            };
            let Some(rank) = XML_ID_CHILDREN.iter().position(|w| *w == name) else {
                continue;
            };
            if best.is_some_and(|(b, _)| b <= rank) {
                continue;
            }
            let text = child_of_kind(child, &["content"])
                .and_then(|c| node_text(c, src))
                .unwrap_or("");
            // a value carrying markup is not an identifier
            if text.is_empty() || text.len() > 80 || text.contains('<') {
                continue;
            }
            best = Some((rank, text));
        }
    }
    Some(match best {
        // `<tag>#<id>`: one symbol name, but the separator is not `.`, so
        // qualification (which joins on `.`) still nests this under its parent
        // instead of treating the id as another level.
        Some((_, text)) => format!("{tag}#{text}"),
        None => tag.to_string(),
    })
}

/// HCL block name: the type identifier plus each string label, joined by `.`.
/// shape: (block (identifier TYPE) (string_lit … template_literal LABEL)* …)
fn hcl_block_name(node: Node, src: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = node.walk();
    for child in node.named_children(&mut cur) {
        match child.kind() {
            "identifier" => {
                let t = child.utf8_text(src.as_bytes()).ok()?.trim();
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
            "string_lit" => {
                // label text lives in the inner template_literal
                let mut lc = child.walk();
                let lit = child
                    .named_children(&mut lc)
                    .find(|c| c.kind() == "template_literal");
                if let Some(lit) = lit {
                    let t = lit.utf8_text(src.as_bytes()).unwrap_or("").trim();
                    if !t.is_empty() {
                        parts.push(t.to_string());
                    }
                }
            }
            // body/block_start/block_end etc. end the label run
            "body" => break,
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| parts.join("."))
}

/// Markdown heading text: the `inline` child for atx headings, else first line.
fn heading_name(node: Node, src: &str) -> Option<String> {
    let mut cur = node.walk();
    let inline = node.named_children(&mut cur).find(|c| c.kind() == "inline");
    let text = match inline {
        Some(n) => n.utf8_text(src.as_bytes()).ok()?,
        None => node.utf8_text(src.as_bytes()).ok()?,
    };
    let t: String = text
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches('#')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if t.is_empty() {
        return None;
    }
    let mut t = t;
    if t.len() > 60 {
        let cut = (0..=60).rev().find(|i| t.is_char_boundary(*i)).unwrap_or(0);
        t.truncate(cut);
        t.push('…');
    }
    Some(t)
}

pub(crate) fn node_name(node: Node, src: &str, field: &str, lang: &str) -> Option<String> {
    if field == DEF_CALL {
        return elixir_def_name(node, src);
    }
    if field == HEADING {
        return heading_name(node, src);
    }
    if field == NESTED {
        return nested_name(lang, node, src);
    }
    if field == HCL_BLOCK {
        return hcl_block_name(node, src);
    }
    if field == DOCKER_FROM {
        // alias (`AS build`) wins; else the image spec (`alpine`)
        let n = node
            .child_by_field_name("as")
            .or_else(|| node.child_by_field_name("image"))
            .or_else(|| node.named_child(0))?;
        let t = n.utf8_text(src.as_bytes()).ok()?.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    if field == HTML_ELEMENT {
        return html_element_name(node, src);
    }
    if field == XML_ELEMENT {
        return xml_element_name(node, src);
    }
    if field == FIXED_NAME {
        return node.kind().strip_suffix("_declaration").map(str::to_string);
    }
    if field == FIRST_CHILD {
        let n = node.named_child(0)?;
        let text = n.utf8_text(src.as_bytes()).ok()?;
        // selectors can sprawl — first line, collapsed whitespace, capped
        let t = text.lines().next().unwrap_or("").trim();
        let t: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty() {
            return None;
        }
        let mut t = t;
        if t.len() > 60 {
            let cut = (0..=60).rev().find(|i| t.is_char_boundary(*i)).unwrap_or(0);
            t.truncate(cut);
            t.push('…');
        }
        return Some(t);
    }
    let mut n = node.child_by_field_name(field)?;
    // C/C++: the name hides inside nested declarators
    // (function_definition → pointer_declarator → function_declarator → identifier)
    while let Some(inner) = n.child_by_field_name("declarator") {
        n = inner;
    }
    let text = n.utf8_text(src.as_bytes()).ok()?;
    // strip generic params: "Foo<T>" → "Foo", so qualified names stay addressable
    let t = text.split('<').next().unwrap_or(text).trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

pub(crate) fn first_line_sig(node: Node, src: &str) -> String {
    let text = node.utf8_text(src.as_bytes()).unwrap_or("");
    let mut line = text.lines().next().unwrap_or("").trim();
    // Drop a trailing block opener — ` {` / bare `{` carries no signature info,
    // it just restates that a body follows. A dangling `(` (multi-line params)
    // stays, since it signals the arg list continues.
    line = line.strip_suffix('{').map(str::trim_end).unwrap_or(line);
    let mut s: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        s.push('…');
    }
    s
}
