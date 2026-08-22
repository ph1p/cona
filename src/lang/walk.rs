//! The symbol-extraction walk: iterative pre-order over the AST, emitting a
//! `Sym` for every classified node (plus JS/TS function-valued bindings).

use super::classify::{classify, needs_body, swift_class_label};
use super::names::{first_line_sig, node_name};
use super::Sym;
use tree_sitter::Node;

/// JS/TS: a `variable_declarator` (or class field) whose value is a function —
/// `const foo = () => …`, `bar = function …` — is a function definition in all
/// but node kind. Returns the value node so walk() can descend for nested defs.
fn fn_valued_declarator<'a>(decl: Node<'a>, src: &str) -> Option<(String, Node<'a>)> {
    let value = decl.child_by_field_name("value")?;
    if !matches!(
        value.kind(),
        "arrow_function" | "function_expression" | "function" | "generator_function"
    ) {
        return None;
    }
    // `name` (TS fields) or `property` (JS field_definition); destructuring
    // patterns are skipped — there is no single name to index.
    let name_node = decl
        .child_by_field_name("name")
        .or_else(|| decl.child_by_field_name("property"))?;
    if !name_node.kind().ends_with("identifier") && name_node.kind() != "property_identifier" {
        return None;
    }
    let name = name_node.utf8_text(src.as_bytes()).ok()?.trim().to_string();
    (!name.is_empty()).then_some((name, value))
}

pub(crate) fn walk(node: Node, src: &str, lang: &str, parent: Option<&str>, out: &mut Vec<Sym>) {
    use std::rc::Rc;
    // Explicit worklist, NOT recursion: recursion depth would equal AST depth,
    // and generated/minified files nest arbitrarily deep — a recursive walk
    // overflows the parse threads' stack and aborts the whole process.
    enum Job<'t> {
        /// Classify this node as a child of `parent` (the loop body below).
        Visit(Node<'t>, Option<Rc<str>>),
        /// Emit a js/ts function-valued binding, then descend into its value.
        FnDecl {
            name: String,
            site: Node<'t>,
            value: Node<'t>,
            label: &'static str,
            parent: Option<Rc<str>>,
        },
    }
    // Pre-order = pop order, so children go on the stack reversed.
    fn push_children<'t>(stack: &mut Vec<Job<'t>>, node: Node<'t>, parent: Option<Rc<str>>) {
        let base = stack.len();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(Job::Visit(child, parent.clone()));
        }
        stack[base..].reverse();
    }
    let mut stack: Vec<Job> = Vec::new();
    push_children(&mut stack, node, parent.map(Rc::from));
    while let Some(job) = stack.pop() {
        let (child, parent) = match job {
            Job::FnDecl {
                name,
                site,
                value,
                label,
                parent,
            } => {
                let qualified = match parent.as_deref() {
                    Some(p) => format!("{}.{}", p, name),
                    None => name.clone(),
                };
                out.push(Sym {
                    name,
                    qualified: qualified.clone(),
                    kind: label,
                    parent: parent.as_deref().map(|s| s.to_string()),
                    start_line: site.start_position().row + 1,
                    end_line: site.end_position().row + 1,
                    signature: first_line_sig(site, src),
                });
                push_children(&mut stack, value, Some(Rc::from(qualified)));
                continue;
            }
            Job::Visit(child, parent) => (child, parent),
        };
        // js/ts: function-valued bindings — the declarator, not a classifiable
        // statement kind, carries the symbol
        if matches!(lang, "javascript" | "typescript" | "tsx") {
            let declish = matches!(
                child.kind(),
                "lexical_declaration"
                    | "variable_declaration"
                    | "public_field_definition"
                    | "field_definition"
            );
            if declish {
                let mut c2 = child.walk();
                let decls: Vec<Node> = if child.kind().ends_with("_definition") {
                    vec![child]
                } else {
                    child
                        .named_children(&mut c2)
                        .filter(|d| d.kind() == "variable_declarator")
                        .collect()
                };
                let label = if child.kind().ends_with("_definition") {
                    "method"
                } else {
                    "fn"
                };
                let base = stack.len();
                for decl in decls {
                    if let Some((name, value)) = fn_valued_declarator(decl, src) {
                        stack.push(Job::FnDecl {
                            name,
                            site: decl,
                            value,
                            label,
                            parent: parent.clone(),
                        });
                    }
                }
                if stack.len() > base {
                    stack[base..].reverse();
                    continue;
                }
            }
        }
        if let Some((label, _is_container, name_field)) = classify(lang, child.kind()) {
            let label = if lang == "swift" && child.kind() == "class_declaration" {
                swift_class_label(child, src)
            } else {
                label
            };
            if needs_body(child.kind()) && child.child_by_field_name("body").is_none() {
                push_children(&mut stack, child, parent);
                continue;
            }
            if let Some(name) = node_name(child, src, name_field, lang) {
                let qualified = match parent.as_deref() {
                    Some(p) => format!("{}.{}", p, name),
                    None => name.clone(),
                };
                out.push(Sym {
                    name: name.clone(),
                    qualified: qualified.clone(),
                    kind: label,
                    parent: parent.as_deref().map(|s| s.to_string()),
                    start_line: child.start_position().row + 1,
                    end_line: child.end_position().row + 1,
                    signature: first_line_sig(child, src),
                });
                // Descend into every named symbol to catch nested defs
                // (methods in a class, closures with inner fns, …). Containers
                // and leaf defs are handled identically here.
                push_children(&mut stack, child, Some(Rc::from(qualified)));
                continue;
            }
        }
        push_children(&mut stack, child, parent);
    }
}
