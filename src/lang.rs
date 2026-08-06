use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone)]
pub struct Sym {
    pub name: String,
    pub qualified: String,
    pub kind: &'static str,
    pub parent: Option<String>,
    pub start_line: usize, // 1-based inclusive
    pub end_line: usize,   // 1-based inclusive
    pub signature: String,
}

/// Every JS/TS file extension `detect_lang` accepts — the one list import
/// resolution (deps.rs) probes against, so the two can't drift.
pub const JS_TS_EXTS: [&str; 8] = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];

pub fn detect_lang(path: &str) -> Option<&'static str> {
    // extension-less build files matched by basename
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match base {
        "Makefile" | "makefile" | "GNUmakefile" => return Some("make"),
        "Dockerfile" | "dockerfile" | "Containerfile" => return Some("dockerfile"),
        _ if base.ends_with(".mk") => return Some("make"),
        // Dockerfile.<flavour> / <name>.dockerfile
        _ if base.starts_with("Dockerfile.") || base.ends_with(".dockerfile") => {
            return Some("dockerfile");
        }
        _ => {}
    }
    let ext = path.rsplit('.').next()?;
    match ext {
        "rs" => Some("rust"),
        "py" | "pyi" => Some("python"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "ts" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "go" => Some("go"),
        "java" => Some("java"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some("cpp"),
        "css" => Some("css"),
        "rb" => Some("ruby"),
        "php" => Some("php"),
        "cs" => Some("csharp"),
        "kt" | "kts" => Some("kotlin"),
        "swift" => Some("swift"),
        "sh" | "bash" => Some("bash"),
        "lua" => Some("lua"),
        "scala" | "sc" => Some("scala"),
        "ex" | "exs" => Some("elixir"),
        "dart" => Some("dart"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "toml" => Some("toml"),
        "html" | "htm" => Some("html"),
        "md" | "markdown" => Some("markdown"),
        "zig" => Some("zig"),
        "nix" => Some("nix"),
        "hs" => Some("haskell"),
        "ml" | "mli" => Some("ocaml"),
        "svelte" => Some("svelte"),
        "vue" => Some("vue"),
        "pl" | "pm" => Some("perl"),
        "r" | "R" => Some("r"),
        "jl" => Some("julia"),
        "ps1" | "psm1" | "psd1" => Some("powershell"),
        "m" | "mm" => Some("objc"),
        "xml" | "xsd" | "xsl" | "xslt" | "svg" => Some("xml"),
        "graphql" | "gql" => Some("graphql"),
        "proto" => Some("proto"),
        "sql" => Some("sql"),
        "tf" | "tfvars" | "hcl" => Some("hcl"),
        _ => None,
    }
}

/// Does this language have named code units worth reading one at a time?
///
/// cona indexes prose, markup and data formats too — Markdown headings, CSS
/// rules and JSON/YAML keys are useful `outline`/`grep` targets — but "read one
/// function instead of the whole file" is meaningless advice for them: a README
/// is read as prose, a stylesheet and a config are read whole. The read-advisory
/// hook tier uses this to stay quiet on such files.
///
/// Deny-list rather than allow-list so a newly added *code* language is
/// advisable by default — the safe direction to be wrong in. Every entry must be
/// a string `detect_lang` can actually return, and the set is exactly those
/// reachable languages whose `classify` arms yield no function-like kind.
///
/// NOTE: adding a prose/markup/data language to `detect_lang` means adding it
/// here too (see CLAUDE.md "Adding a new language").
pub fn has_callable_symbols(lang: &str) -> bool {
    !matches!(
        lang,
        "markdown" | "json" | "yaml" | "toml" | "xml" | "html" | "css" | "graphql"
    )
}

pub fn language_for(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "java" => Some(tree_sitter_java::LANGUAGE.into()),
        "c" => Some(tree_sitter_c::LANGUAGE.into()),
        "cpp" => Some(tree_sitter_cpp::LANGUAGE.into()),
        "css" => Some(tree_sitter_css::LANGUAGE.into()),
        "ruby" => Some(tree_sitter_ruby::LANGUAGE.into()),
        "php" => Some(tree_sitter_php::LANGUAGE_PHP.into()),
        "csharp" => Some(tree_sitter_c_sharp::LANGUAGE.into()),
        "kotlin" => Some(tree_sitter_kotlin_ng::LANGUAGE.into()),
        "swift" => Some(tree_sitter_swift::LANGUAGE.into()),
        "bash" => Some(tree_sitter_bash::LANGUAGE.into()),
        "lua" => Some(tree_sitter_lua::LANGUAGE.into()),
        "scala" => Some(tree_sitter_scala::LANGUAGE.into()),
        "elixir" => Some(tree_sitter_elixir::LANGUAGE.into()),
        "dart" => Some(tree_sitter_dart::language()),
        "json" => Some(tree_sitter_json::LANGUAGE.into()),
        "yaml" => Some(tree_sitter_yaml::LANGUAGE.into()),
        "toml" => Some(tree_sitter_toml_ng::LANGUAGE.into()),
        "html" => Some(tree_sitter_html::LANGUAGE.into()),
        "markdown" => Some(tree_sitter_md::LANGUAGE.into()),
        "zig" => Some(tree_sitter_zig::LANGUAGE.into()),
        "nix" => Some(tree_sitter_nix::LANGUAGE.into()),
        "haskell" => Some(tree_sitter_haskell::LANGUAGE.into()),
        "ocaml" => Some(tree_sitter_ocaml::LANGUAGE_OCAML.into()),
        "svelte" => Some(tree_sitter_svelte_ng::LANGUAGE.into()),
        "perl" => Some(tree_sitter_perl::LANGUAGE.into()),
        "r" => Some(tree_sitter_r::LANGUAGE.into()),
        "julia" => Some(tree_sitter_julia::LANGUAGE.into()),
        "powershell" => Some(tree_sitter_powershell::LANGUAGE.into()),
        "objc" => Some(tree_sitter_objc::LANGUAGE.into()),
        "xml" => Some(tree_sitter_xml::LANGUAGE_XML.into()),
        "graphql" => Some(tree_sitter_graphql::LANGUAGE.into()),
        "proto" => Some(tree_sitter_proto::LANGUAGE.into()),
        "sql" => Some(tree_sitter_sequel::LANGUAGE.into()),
        "hcl" => Some(tree_sitter_hcl::LANGUAGE.into()),
        "make" => Some(tree_sitter_make::LANGUAGE.into()),
        // vue + dockerfile grammars are vendored + compiled in build.rs (their
        // crates.io crates pin an incompatible tree-sitter runtime). The C entry
        // points return an ABI-14 TSLanguage our 0.26 runtime accepts.
        "vue" => Some(unsafe { Language::from_raw(tree_sitter_vue()) }),
        "dockerfile" => Some(unsafe { Language::from_raw(tree_sitter_dockerfile()) }),
        _ => None,
    }
}

// Vendored grammar entry points (see build.rs / vendor/*).
unsafe extern "C" {
    fn tree_sitter_vue() -> *const tree_sitter::ffi::TSLanguage;
    fn tree_sitter_dockerfile() -> *const tree_sitter::ffi::TSLanguage;
}

/// Maps a tree-sitter node kind to (symbol label, is_container, name_field)
fn classify(lang: &str, node_kind: &str) -> Option<(&'static str, bool, &'static str)> {
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
fn needs_body(node_kind: &str) -> bool {
    matches!(
        node_kind,
        "struct_specifier" | "class_specifier" | "enum_specifier" | "union_specifier"
    )
}

/// Sentinel name_field for grammars without name fields (CSS): the symbol
/// name is the text of the node's first named child (selector list / query).
const FIRST_CHILD: &str = "\0first";
/// Elixir sentinel: every construct is a `call`; the name is the identifier
/// being defined (first argument of a def/defp/defmodule/defmacro call).
const DEF_CALL: &str = "\0defcall";
/// Markdown sentinel: heading text is the `inline` child (atx) or the paragraph
/// text before the underline (setext).
const HEADING: &str = "\0heading";
/// Generic sentinel: the name is a specific descendant node kind that isn't the
/// first child and has no reusable `name` field — resolved per-language by
/// `nested_name` (OCaml bindings, Julia signatures, SQL object refs, …).
const NESTED: &str = "\0nested";
/// Dockerfile sentinel: a `from_instruction`'s name is its stage alias
/// (`FROM x AS build` → `build`), falling back to the image spec when unnamed.
const DOCKER_FROM: &str = "\0dockerfrom";
/// HCL/Terraform sentinel: every construct is a `block`; the name is the block
/// type identifier plus its string labels joined by `.`
/// (`resource "aws_instance" "web"` → `resource.aws_instance.web`). Blocks
/// without labels (`locals {}`, `terraform {}`) yield just the type.
const HCL_BLOCK: &str = "\0hclblock";
/// Sentinel for anonymous declarations whose kind IS their name (Swift
/// `init`/`deinit`/`subscript`): the name is the node kind minus
/// `_declaration`, so `Foo.init` stays addressable without a name field.
const FIXED_NAME: &str = "\0fixedname";

/// Resolves names for the NESTED sentinel: grammars where the identifier is a
/// known child kind buried past keywords/wrappers. Returns None → symbol skipped.
fn nested_name(lang: &str, node: Node, src: &str) -> Option<String> {
    // first descendant (BFS-ish, shallow) whose kind is in `kinds`
    fn first_of<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
        let mut cur = node.walk();
        for ch in node.named_children(&mut cur) {
            if kinds.contains(&ch.kind()) {
                return Some(ch);
            }
        }
        // one level deeper (signatures/type_head wrap the identifier)
        let mut cur = node.walk();
        for ch in node.named_children(&mut cur) {
            if let Some(hit) = first_of(ch, kinds) {
                return Some(hit);
            }
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

fn node_name(node: Node, src: &str, field: &str, lang: &str) -> Option<String> {
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

fn first_line_sig(node: Node, src: &str) -> String {
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

/// Declared parameter count parsed from a signature's FIRST parenthesised
/// group — the cheap arity signal for `narrow_by_scope`. Counts top-level
/// commas inside the outermost `(…)`, ignoring commas nested in `<>`/`[]`/`{}`
/// (generics, type args, defaults) and inside string/char literals. Empty
/// parens → 0. `None` when no `(` is present (a param-less signature form we
/// can't compare, e.g. a bare Python `def` slice or a non-callable) so callers
/// treat it as "no arity signal" rather than "zero params".
///
/// Purely textual and language-agnostic on purpose: it feeds a single-survivor
/// tiebreak, never a silent pick, so an occasional miscount only leaves a case
/// `·ambiguous` — it can't resolve to the wrong def.
pub fn param_count(sig: &str) -> Option<usize> {
    let bytes = sig.as_bytes();
    let open = bytes.iter().position(|&b| b == b'(')?;
    let mut depth = 0i32; // nesting of () [] {} <>
    let mut in_str: Option<u8> = None; // active quote char
    let mut escaped = false;
    let mut commas = 0usize;
    let mut saw_content = false; // any non-space between the parens
    for &b in &bytes[open..] {
        if let Some(q) = in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                in_str = None;
            }
            continue;
        }
        match b {
            b'"' | b'\'' => in_str = Some(b),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    break; // matched the opening paren
                }
            }
            b'<' => depth += 1,
            // only unwind a `<` we opened; comparison operators keep depth ≥1
            b'>' if depth > 1 => depth -= 1,
            b',' if depth == 1 => commas += 1,
            b' ' | b'\t' => {}
            _ if depth >= 1 => saw_content = true,
            _ => {}
        }
    }
    if !saw_content {
        Some(0) // `()` — genuinely zero params
    } else {
        Some(commas + 1)
    }
}

/// Whether a signature's FIRST parameter is an implicit receiver (`self` /
/// `&self` / `&mut self` in Rust, `self` in Python, `this` in some langs) that
/// is NOT written at the call site. When true, a call's arg count is one less
/// than `param_count`. Method-vs-free-fn can't be told from the symbol kind
/// (Rust methods are kind `fn` too), so we read the signature directly.
pub fn first_param_is_receiver(sig: &str) -> bool {
    let Some(open) = sig.find('(') else {
        return false;
    };
    let rest = &sig[open + 1..];
    // first param = text up to the first top-level comma or the closing paren
    let end = rest.find([',', ')']).unwrap_or(rest.len());
    let first = rest[..end].trim();
    let first = first.trim_start_matches('&');
    let first = first.trim_start_matches("mut ").trim();
    // bare receiver or typed `self: …` / `this: …`
    matches!(first, "self" | "this")
        || first.starts_with("self:")
        || first.starts_with("this:")
        || first.starts_with("self ")
        || first.starts_with("this ")
}

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

/// Swift folds struct/enum/extension/actor into `class_declaration`; the real
/// kind is the declaration keyword, exposed so labels stay truthful.
fn swift_class_label(node: Node, src: &str) -> &'static str {
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

fn walk(node: Node, src: &str, lang: &str, parent: Option<&str>, out: &mut Vec<Sym>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
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
                let mut emitted = false;
                for decl in decls {
                    if let Some((name, value)) = fn_valued_declarator(decl, src) {
                        let label = if child.kind().ends_with("_definition") {
                            "method"
                        } else {
                            "fn"
                        };
                        let qualified = match parent {
                            Some(p) => format!("{}.{}", p, name),
                            None => name.clone(),
                        };
                        out.push(Sym {
                            name,
                            qualified: qualified.clone(),
                            kind: label,
                            parent: parent.map(|s| s.to_string()),
                            start_line: decl.start_position().row + 1,
                            end_line: decl.end_position().row + 1,
                            signature: first_line_sig(decl, src),
                        });
                        walk(value, src, lang, Some(&qualified), out);
                        emitted = true;
                    }
                }
                if emitted {
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
                walk(child, src, lang, parent, out);
                continue;
            }
            if let Some(name) = node_name(child, src, name_field, lang) {
                let qualified = match parent {
                    Some(p) => format!("{}.{}", p, name),
                    None => name.clone(),
                };
                out.push(Sym {
                    name: name.clone(),
                    qualified: qualified.clone(),
                    kind: label,
                    parent: parent.map(|s| s.to_string()),
                    start_line: child.start_position().row + 1,
                    end_line: child.end_position().row + 1,
                    signature: first_line_sig(child, src),
                });
                // Descend into every named symbol to catch nested defs
                // (methods in a class, closures with inner fns, …). Containers
                // and leaf defs are handled identically here.
                walk(child, src, lang, Some(&qualified), out);
                continue;
            }
        }
        walk(child, src, lang, parent, out);
    }
}

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

/// One tree-sitter parse of `src` in `lang` — the single entry point every
/// semantic helper goes through.
fn parse(lang: &str, src: &str) -> anyhow::Result<tree_sitter::Tree> {
    let language =
        language_for(lang).ok_or_else(|| anyhow::anyhow!("unsupported language: {lang}"))?;
    let mut parser = Parser::new();
    parser.set_language(&language)?;
    parser
        .parse(src, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))
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

fn collect_idents(node: Node, src: &str, out: &mut Vec<(String, usize)>) {
    // Lean traversal for the flag-less callers (refs / tree --rank): pushes
    // (name, line) directly. Deliberately does NOT run call_node_of per ident —
    // that ancestor walk is pure waste when the call flag is thrown away, and
    // this path runs over every identifier of every file on hot commands.
    if node.child_count() == 0 && node.kind().ends_with("identifier") {
        if let Ok(text) = node.utf8_text(src.as_bytes()) {
            out.push((text.to_string(), node.start_position().row + 1));
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_idents(child, src, out);
    }
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
    if node.child_count() == 0 && node.kind().ends_with("identifier") {
        if let Ok(text) = node.utf8_text(src.as_bytes()) {
            let call = call_node_of(node);
            out.push((
                text.to_string(),
                node.start_position().row + 1,
                call.is_some(),
                call.and_then(arg_count_of),
            ));
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_idents_with_call(child, src, out);
    }
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
    if node.child_count() == 0 && node.kind().ends_with("identifier") {
        if node.utf8_text(src.as_bytes()) == Ok(name) {
            out.push((node.start_position().row + 1, node.start_position().column));
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_named_positions(child, src, name, out);
    }
}

fn ref_lines_textual(src: &str, name: &str) -> Vec<usize> {
    let mut lines: Vec<usize> = textual_positions(src, name)
        .into_iter()
        .map(|(ln, _)| ln)
        .collect();
    lines.dedup(); // one hit per line is enough
    lines
}

/// Kind taxonomy — the labels minted by `classify()`. Which kinds are types
/// and which are callable is language knowledge and lives HERE; commands
/// consume the predicates instead of hardcoding label lists.
pub const TYPE_KINDS: &[&str] = &[
    "struct",
    "enum",
    "trait",
    "type",
    "class",
    "interface",
    "record",
    "actor",
    "extension",
];

pub fn is_type_kind(kind: &str) -> bool {
    TYPE_KINDS.contains(&kind)
}

pub fn is_callable_kind(kind: &str) -> bool {
    matches!(kind, "fn" | "fn*" | "def" | "method")
}

/// Identifier validity for `rename` targets (conservative ASCII rule shared
/// by all bundled grammars).
pub fn is_valid_ident(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn extract_symbols(lang: &str, src: &str) -> anyhow::Result<Vec<Sym>> {
    let mut out = Vec::new();
    walk(parse(lang, src)?.root_node(), src, lang, None, &mut out);
    Ok(out)
}

/// Verify that source parses without syntax errors. Returns list of error lines.
pub fn syntax_errors(lang: &str, src: &str) -> anyhow::Result<Vec<usize>> {
    let mut errors = Vec::new();
    collect_errors(parse(lang, src)?.root_node(), &mut errors);
    errors.sort();
    errors.dedup();
    Ok(errors)
}

fn collect_errors(node: Node, out: &mut Vec<usize>) {
    if node.is_error() || node.is_missing() {
        out.push(node.start_position().row + 1);
        return;
    }
    if !node.has_error() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_errors(child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::param_count;

    #[test]
    fn param_count_basic_and_langs() {
        assert_eq!(param_count("fn low() {}"), Some(0));
        assert_eq!(param_count("fn f(a, b)"), Some(2));
        assert_eq!(param_count("def foo(a, b, c):"), Some(3));
        assert_eq!(param_count("function go(x)"), Some(1));
        // method with one explicit param — the real BudgetOut.finish case
        assert_eq!(
            param_count("fn finish(mut self, trailer: &str) -> String"),
            Some(2)
        );
        // no parens at all → no arity signal, not zero
        assert_eq!(param_count("const X: i32 = 3"), None);
    }

    #[test]
    fn param_count_ignores_nested_commas() {
        // generic type args must not inflate the count
        assert_eq!(
            param_count("fn f(m: HashMap<String, i64>, n: i32)"),
            Some(2)
        );
        // nested tuple / array in a type
        assert_eq!(param_count("fn g(a: (i32, i32), b: [u8; 4])"), Some(2));
        // nested call in a python default
        assert_eq!(param_count("def h(a, b=foo(1, 2)):"), Some(2));
        // string literal containing a comma
        assert_eq!(param_count(r#"fn s(a: &str = "x, y")"#), Some(1));
    }

    #[test]
    fn param_count_edge_cases() {
        // whitespace-only inside parens still reads as zero
        assert_eq!(param_count("fn f(   )"), Some(0));
        // comparison operator in a default doesn't unbalance angle brackets
        assert_eq!(param_count("fn f(a: bool = 1 > 0, b: i32)"), Some(2));
    }
}
