//! Node-kind classification: maps tree-sitter node kinds to symbol labels,
//! plus the sentinel `name_field` values that tell `names::node_name` how to
//! resolve a name when the grammar has no reusable `name` field.

use tree_sitter::Node;

/// Maps a tree-sitter node kind to (symbol label, is_container, name_field)
pub(crate) fn classify(lang: &str, node_kind: &str) -> Option<(&'static str, bool, &'static str)> {
    match lang {
        "rust" => match node_kind {
            "function_item" => Some(("fn", false, "name")),
            "struct_item" => Some(("struct", false, "name")),
            "enum_item" => Some(("enum", false, "name")),
            "trait_item" => Some(("trait", true, "name")),
            "impl_item" => Some(("impl", true, "type")),
            "mod_item" => Some(("mod", true, "name")),
            "const_item" => Some(("const", false, "name")),
            "static_item" => Some(("static", false, "name")),
            "type_item" => Some(("type", false, "name")),
            "macro_definition" => Some(("macro", false, "name")),
            _ => None,
        },
        "python" => match node_kind {
            "function_definition" => Some(("def", false, "name")),
            "class_definition" => Some(("class", true, "name")),
            _ => None,
        },
        // NOTE: `const foo = () => …` (the dominant modern form) is handled
        // structurally in walk() — the symbol is the declarator, not a node
        // kind classify() can see.
        "javascript" | "typescript" | "tsx" => match node_kind {
            "function_declaration" => Some(("fn", false, "name")),
            "generator_function_declaration" => Some(("fn*", false, "name")),
            "class_declaration" | "abstract_class_declaration" => Some(("class", true, "name")),
            "method_definition" => Some(("method", false, "name")),
            "interface_declaration" => Some(("interface", true, "name")),
            "type_alias_declaration" => Some(("type", false, "name")),
            "enum_declaration" => Some(("enum", false, "name")),
            _ => None,
        },
        "go" => match node_kind {
            "function_declaration" => Some(("fn", false, "name")),
            "method_declaration" => Some(("method", false, "name")),
            "type_spec" => Some(("type", false, "name")),
            "const_spec" => Some(("const", false, "name")),
            _ => None,
        },
        "java" => match node_kind {
            "class_declaration" => Some(("class", true, "name")),
            "interface_declaration" => Some(("interface", true, "name")),
            "enum_declaration" => Some(("enum", true, "name")),
            "record_declaration" => Some(("record", true, "name")),
            "method_declaration" => Some(("method", false, "name")),
            "constructor_declaration" => Some(("ctor", false, "name")),
            _ => None,
        },
        "c" | "cpp" => match node_kind {
            // name lives inside nested declarators — node_name drills down
            "function_definition" => Some(("fn", false, "declarator")),
            "type_definition" => Some(("type", false, "declarator")),
            "struct_specifier" => Some(("struct", true, "name")),
            "enum_specifier" => Some(("enum", false, "name")),
            "union_specifier" => Some(("union", false, "name")),
            "preproc_function_def" | "preproc_def" => Some(("macro", false, "name")),
            "class_specifier" if lang == "cpp" => Some(("class", true, "name")),
            "namespace_definition" if lang == "cpp" => Some(("namespace", true, "name")),
            _ => None,
        },
        // XML/POM/csproj: every construct is an `element`; XML_ELEMENT builds the
        // name from the tag plus an identifying child. is_container so nested
        // elements (a <profile>'s <plugin>s) are walked too.
        "xml" => match node_kind {
            "element" => Some(("element", true, XML_ELEMENT)),
            _ => None,
        },
        // HTML: elements are symbols only when identified or structural (see
        // HTML_ELEMENT). is_container is irrelevant — the walk descends into
        // skipped elements anyway, so children of a plain <div> still surface.
        "html" => match node_kind {
            "element" | "script_element" | "style_element" => Some(("element", true, HTML_ELEMENT)),
            _ => None,
        },
        // CSS has no `name` fields — FIRST_CHILD tells node_name to use the
        // first named child's text (the selector list / at-rule query)
        "css" => match node_kind {
            "rule_set" => Some(("rule", true, FIRST_CHILD)),
            "media_statement" => Some(("media", true, FIRST_CHILD)),
            "keyframes_statement" => Some(("keyframes", true, FIRST_CHILD)),
            "supports_statement" => Some(("supports", true, FIRST_CHILD)),
            _ => None,
        },
        "ruby" => match node_kind {
            "method" => Some(("def", false, "name")),
            "singleton_method" => Some(("def", false, "name")),
            "class" => Some(("class", true, "name")),
            "module" => Some(("module", true, "name")),
            _ => None,
        },
        "php" => match node_kind {
            "function_definition" => Some(("fn", false, "name")),
            "method_declaration" => Some(("method", false, "name")),
            "class_declaration" => Some(("class", true, "name")),
            "interface_declaration" => Some(("interface", true, "name")),
            "trait_declaration" => Some(("trait", true, "name")),
            "enum_declaration" => Some(("enum", true, "name")),
            _ => None,
        },
        "csharp" => match node_kind {
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                Some(("namespace", true, "name"))
            }
            "class_declaration" => Some(("class", true, "name")),
            "interface_declaration" => Some(("interface", true, "name")),
            "struct_declaration" => Some(("struct", true, "name")),
            "enum_declaration" => Some(("enum", true, "name")),
            "record_declaration" => Some(("record", true, "name")),
            "method_declaration" => Some(("method", false, "name")),
            "constructor_declaration" => Some(("ctor", false, "name")),
            _ => None,
        },
        // interface/struct/enum all parse as class_declaration — labeled "class"
        "kotlin" => match node_kind {
            "class_declaration" => Some(("class", true, "name")),
            "object_declaration" => Some(("object", true, "name")),
            "function_declaration" => Some(("fn", false, "name")),
            _ => None,
        },
        // struct/enum/extension/actor all parse as class_declaration — walk()
        // relabels from the declaration keyword so `cona find --kind struct` works
        "swift" => match node_kind {
            "class_declaration" => Some(("class", true, "name")),
            "protocol_declaration" => Some(("protocol", true, "name")),
            "function_declaration" | "protocol_function_declaration" => {
                Some(("func", false, "name"))
            }
            "init_declaration" => Some(("init", false, FIXED_NAME)),
            "deinit_declaration" => Some(("deinit", false, FIXED_NAME)),
            "typealias_declaration" => Some(("type", false, "name")),
            "associatedtype_declaration" => Some(("type", false, "name")),
            "subscript_declaration" => Some(("subscript", false, FIXED_NAME)),
            _ => None,
        },
        // name is the first `word` child — FIRST_CHILD grabs it
        "bash" => match node_kind {
            "function_definition" => Some(("fn", false, FIRST_CHILD)),
            _ => None,
        },
        // dot/method forms (M.foo) have no `name` field → node_name yields None, skipped
        "lua" => match node_kind {
            "function_declaration" => Some(("fn", false, "name")),
            _ => None,
        },
        "scala" => match node_kind {
            "class_definition" => Some(("class", true, "name")),
            "object_definition" => Some(("object", true, "name")),
            "trait_definition" => Some(("trait", true, "name")),
            "function_definition" => Some(("def", false, "name")),
            _ => None,
        },
        // methods surface as function_signature nested in class_member_definition;
        // top-level fns are function_signature inside a lambda_expression
        "dart" => match node_kind {
            "class_definition" => Some(("class", true, "name")),
            "enum_declaration" => Some(("enum", false, "name")),
            "function_signature" => Some(("fn", false, "name")),
            _ => None,
        },
        // every construct is a `call`; DEF_CALL tells node_name to read the
        // def/defp/defmodule argument. is_container handled in walk via re-descent.
        "elixir" => match node_kind {
            "call" => Some(("def", true, DEF_CALL)),
            _ => None,
        },
        // TOML tables; the key sits in the first named child (bare_key/dotted_key)
        "toml" => match node_kind {
            "table" => Some(("table", true, FIRST_CHILD)),
            "table_array_element" => Some(("table[]", true, FIRST_CHILD)),
            _ => None,
        },
        // top-level (and nested) mapping keys
        "yaml" => match node_kind {
            "block_mapping_pair" => Some(("key", false, "key")),
            _ => None,
        },
        // headings; the title text is the trailing `inline` child (HEADING sentinel)
        "markdown" => match node_kind {
            "atx_heading" => Some(("heading", false, HEADING)),
            "setext_heading" => Some(("heading", false, HEADING)),
            _ => None,
        },
        // name is the `name` identifier child; struct/enum are variable_declaration
        // with a struct/enum initializer — kept simple: only fns + top-level consts
        "zig" => match node_kind {
            "function_declaration" => Some(("fn", false, "name")),
            _ => None,
        },
        // function/data/class carry a `name` field; type_synonym/newtype too
        "haskell" => match node_kind {
            "function" => Some(("fn", false, "name")),
            "data_type" => Some(("data", false, "name")),
            "newtype" => Some(("newtype", false, "name")),
            "type_synonym" => Some(("type", false, "name")),
            "class" => Some(("class", true, "name")),
            _ => None,
        },
        // bindings nest under *_definition; name via NESTED per-binding helper
        "ocaml" => match node_kind {
            "value_definition" => Some(("let", false, NESTED)),
            "type_definition" => Some(("type", false, NESTED)),
            "module_definition" => Some(("module", true, NESTED)),
            "class_definition" => Some(("class", true, NESTED)),
            _ => None,
        },
        // module has `name` field; struct/abstract keep name in type_head (NESTED);
        // function name lives inside the `signature` child (NESTED)
        "julia" => match node_kind {
            "function_definition" => Some(("fn", false, NESTED)),
            "struct_definition" => Some(("struct", false, NESTED)),
            "abstract_definition" => Some(("abstract", false, NESTED)),
            "module_definition" => Some(("module", true, "name")),
            _ => None,
        },
        // names in function_name / simple_name — first named child
        "powershell" => match node_kind {
            "function_statement" => Some(("fn", false, FIRST_CHILD)),
            "class_statement" => Some(("class", true, FIRST_CHILD)),
            _ => None,
        },
        // first identifier child is the class name; method name is NESTED
        "objc" => match node_kind {
            "class_interface" => Some(("interface", true, FIRST_CHILD)),
            "class_implementation" => Some(("impl", true, FIRST_CHILD)),
            "protocol_declaration" => Some(("protocol", true, FIRST_CHILD)),
            "method_declaration" => Some(("method", false, NESTED)),
            "method_definition" => Some(("method", false, NESTED)),
            "function_definition" => Some(("fn", false, "declarator")),
            _ => None,
        },
        // *_name is first named child
        "proto" => match node_kind {
            "message" => Some(("message", true, FIRST_CHILD)),
            "service" => Some(("service", true, FIRST_CHILD)),
            "enum" => Some(("enum", false, FIRST_CHILD)),
            "rpc" => Some(("rpc", false, FIRST_CHILD)),
            _ => None,
        },
        // name = the object_reference child (NESTED skips the CREATE/TABLE keywords)
        "sql" => match node_kind {
            "create_table" => Some(("table", false, NESTED)),
            "create_view" => Some(("view", false, NESTED)),
            "create_function" => Some(("fn", false, NESTED)),
            "create_index" => Some(("index", false, NESTED)),
            _ => None,
        },
        // perl subs carry `name`; packages via NESTED (package_name child)
        "perl" => match node_kind {
            "function_definition" => Some(("sub", false, "name")),
            "package_statement" => Some(("package", false, NESTED)),
            _ => None,
        },
        // targets → word (NESTED); variable_assignment → word (FIRST_CHILD)
        "make" => match node_kind {
            "rule" => Some(("rule", false, NESTED)),
            "variable_assignment" => Some(("var", false, FIRST_CHILD)),
            _ => None,
        },
        // build stages are the addressable units; name = `AS <alias>` or the
        // image if unnamed (DOCKER_FROM). Vue (.vue) stays parse-only like svelte.
        "dockerfile" => match node_kind {
            "from_instruction" => Some(("stage", true, DOCKER_FROM)),
            _ => None,
        },
        // every construct is a `block`; name = type + labels (HCL_BLOCK)
        "hcl" => match node_kind {
            "block" => Some(("block", false, HCL_BLOCK)),
            _ => None,
        },
        _ => None,
    }
}

/// C/C++ struct/class/enum/union specifiers appear both as definitions and as
/// bare type references (`struct Foo x;`) — only definitions carry a body.
pub(crate) fn needs_body(node_kind: &str) -> bool {
    matches!(
        node_kind,
        "struct_specifier" | "class_specifier" | "enum_specifier" | "union_specifier"
    )
}

/// Sentinel name_field for grammars without name fields (CSS): the symbol
/// name is the text of the node's first named child (selector list / query).
pub(crate) const FIRST_CHILD: &str = "\0first";
/// Elixir sentinel: every construct is a `call`; the name is the identifier
/// being defined (first argument of a def/defp/defmodule/defmacro call).
pub(crate) const DEF_CALL: &str = "\0defcall";
/// Markdown sentinel: heading text is the `inline` child (atx) or the paragraph
/// text before the underline (setext).
pub(crate) const HEADING: &str = "\0heading";
/// Generic sentinel: the name is a specific descendant node kind that isn't the
/// first child and has no reusable `name` field — resolved per-language by
/// `nested_name` (OCaml bindings, Julia signatures, SQL object refs, …).
pub(crate) const NESTED: &str = "\0nested";
/// Dockerfile sentinel: a `from_instruction`'s name is its stage alias
/// (`FROM x AS build` → `build`), falling back to the image spec when unnamed.
pub(crate) const DOCKER_FROM: &str = "\0dockerfrom";
/// HCL/Terraform sentinel: every construct is a `block`; the name is the block
/// type identifier plus its string labels joined by `.`
/// (`resource "aws_instance" "web"` → `resource.aws_instance.web`). Blocks
/// without labels (`locals {}`, `terraform {}`) yield just the type.
pub(crate) const HCL_BLOCK: &str = "\0hclblock";
/// Sentinel for anonymous declarations whose kind IS their name (Swift
/// `init`/`deinit`/`subscript`): the name is the node kind minus
/// `_declaration`, so `Foo.init` stays addressable without a name field.
pub(crate) const FIXED_NAME: &str = "\0fixedname";

/// XML element sentinel: the tag name, qualified by an identifying child's text
/// when the element has one (`<profile><id>x</id>` → `profile.x`). Build files
/// repeat the same tag hundreds of times (`dependency`, `plugin`, `execution`),
/// so the bare tag is not an addressable name.
pub(crate) const XML_ELEMENT: &str = "\0xmlelement";

/// HTML element sentinel: only elements that carry an identity (`id`, a framework
/// directive, a `name`) or structural meaning (landmarks, `script`/`style`,
/// `template`) become symbols. A template is mostly `<div>`/`<span>` scaffolding;
/// indexing all of it would bury the handful of elements worth navigating to.
pub(crate) const HTML_ELEMENT: &str = "\0htmlelement";

/// Swift folds struct/enum/extension/actor into `class_declaration`; the real
/// kind is the declaration keyword, exposed so labels stay truthful.
pub(crate) fn swift_class_label(node: Node, src: &str) -> &'static str {
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        match c.kind() {
            "struct" => return "struct",
            "enum" => return "enum",
            "extension" => return "extension",
            "actor" => return "actor",
            "class" => return "class",
            _ => {}
        }
        // stop scanning once we're past the intro keywords
        if c.kind() == "type_identifier" || c.kind() == "user_type" {
            break;
        }
    }
    let _ = src;
    "class"
}
