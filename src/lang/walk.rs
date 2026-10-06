//! The symbol-extraction walk: iterative pre-order over the AST, emitting a
//! `Sym` for every classified node (plus JS/TS function-valued bindings).

use super::classify::{classify, needs_body, swift_class_label};
use super::names::{first_line_sig, node_name};
use super::Sym;
use tree_sitter::Node;

/// JS/TS: a declarator or class field whose value is a function
/// (`const foo = () => …`) is a function definition in all but node kind.
/// Returns the value node so walk() can descend for nested defs.
fn fn_valued_declarator<'a>(decl: Node<'a>, src: &str) -> Option<(String, Node<'a>)> {
    let value = decl.child_by_field_name("value")?;
    if !matches!(
        value.kind(),
        "arrow_function" | "function_expression" | "function" | "generator_function"
    ) {
        return None;
    }
    // `name` (TS) or `property` (JS field_definition); destructuring has no
    // single name, so it is skipped
    let name_node = decl
        .child_by_field_name("name")
        .or_else(|| decl.child_by_field_name("property"))?;
    if !name_node.kind().ends_with("identifier") && name_node.kind() != "property_identifier" {
        return None;
    }
    let name = name_node.utf8_text(src.as_bytes()).ok()?.trim().to_string();
    (!name.is_empty()).then_some((name, value))
}

/// Go: the receiver's type with generics and pointers peeled
/// (`func (f *Food) Per()` → `Food`), so the method is addressable as `Food.Per`.
fn go_receiver_type(method: Node, src: &str) -> Option<String> {
    let param = method
        .child_by_field_name("receiver")?
        .named_children(&mut method.walk())
        .find(|c| c.kind() == "parameter_declaration")?;
    let mut stack = vec![param.child_by_field_name("type")?];
    while let Some(n) = stack.pop() {
        if n.kind() == "type_identifier" {
            return n.utf8_text(src.as_bytes()).ok().map(str::to_string);
        }
        // generic_type's own name comes first; its type arguments after
        let mut c = n.walk();
        let kids: Vec<Node> = n.named_children(&mut c).collect();
        stack.extend(kids.into_iter().rev());
    }
    None
}

/// A declaration that sits at file scope: only declaration wrappers (and an
/// `export`) between it and the root — never a function body or block.
fn at_file_scope(node: Node) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        match n.kind() {
            "source_file" | "program" => return true,
            "var_declaration"
            | "var_spec_list"
            | "const_declaration"
            | "export_statement"
            | "lexical_declaration" => cur = n.parent(),
            _ => return false,
        }
    }
    false
}

/// JS/TS: file-scope `const X = <non-function>` (config tables, schemas,
/// object-literal clients) — agents ask for these by name as often as for fns.
fn top_level_consts<'a>(decl: Node<'a>, src: &str) -> Vec<(String, Node<'a>)> {
    if decl.kind() != "lexical_declaration"
        || !decl
            .utf8_text(src.as_bytes())
            .is_ok_and(|t| t.starts_with("const"))
        || !at_file_scope(decl)
    {
        return Vec::new();
    }
    let mut c = decl.walk();
    decl.named_children(&mut c)
        .filter(|d| d.kind() == "variable_declarator" && d.child_by_field_name("value").is_some())
        .filter_map(|d| {
            let n = d.child_by_field_name("name")?;
            (n.kind() == "identifier")
                .then(|| n.utf8_text(src.as_bytes()).ok())
                .flatten()
                .map(|t| (t.to_string(), d))
        })
        .collect()
}

pub(crate) fn walk(node: Node, src: &str, lang: &str, parent: Option<&str>, out: &mut Vec<Sym>) {
    use std::rc::Rc;
    // Explicit worklist, NOT recursion: generated/minified files nest
    // arbitrarily deep, and overflowing a parse thread's stack aborts the process.
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
        // js/ts: function-valued bindings — the declarator carries the symbol
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
                let consts = top_level_consts(child, src);
                if !consts.is_empty() {
                    for (name, decl) in consts {
                        out.push(Sym {
                            name: name.clone(),
                            qualified: name.clone(),
                            kind: "const",
                            parent: None,
                            start_line: decl.start_position().row + 1,
                            end_line: decl.end_position().row + 1,
                            signature: first_line_sig(child, src),
                        });
                        // object-literal methods become `client.send`
                        push_children(&mut stack, decl, Some(Rc::from(name)));
                    }
                    continue;
                }
            }
        }
        if lang == "go" && child.kind() == "var_spec" && !at_file_scope(child) {
            push_children(&mut stack, child, parent);
            continue;
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
            let owner = match &parent {
                None if lang == "go" && child.kind() == "method_declaration" => {
                    go_receiver_type(child, src).map(Rc::from)
                }
                p => p.clone(),
            };
            if let Some(name) = node_name(child, src, name_field, lang) {
                let qualified = match owner.as_deref() {
                    Some(p) => format!("{}.{}", p, name),
                    None => name.clone(),
                };
                out.push(Sym {
                    name: name.clone(),
                    qualified: qualified.clone(),
                    kind: label,
                    parent: owner.as_deref().map(|s| s.to_string()),
                    start_line: child.start_position().row + 1,
                    end_line: child.end_position().row + 1,
                    signature: first_line_sig(child, src),
                });
                // Descend into every named symbol (containers and leaf defs
                // alike) to catch nested defs: class methods, inner fns, …
                push_children(&mut stack, child, Some(Rc::from(qualified)));
                continue;
            }
        }
        push_children(&mut stack, child, parent);
    }
}
