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
        "markdown" | "json" | "yaml" | "toml" | "css" | "graphql"
            // parse-only code languages: reachable from detect_lang and
            // parseable (refs/grep work), but with NO classify arms — they
            // index zero symbols, so `show <Symbol>` advice is a dead end
            | "nix" | "svelte" | "vue" | "r"
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

mod classify;
mod idents;
mod names;
#[cfg(test)]
mod tests;
mod walk;

pub use idents::{
    extract_idents, ident_counts, ident_occurrences, ident_occurrences_failopen, ident_positions,
    idents_in_range, ref_lines,
};

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
    walk::walk(parse(lang, src)?.root_node(), src, lang, None, &mut out);
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
    idents::for_each_node(node, |n| {
        if n.is_error() || n.is_missing() {
            out.push(n.start_position().row + 1);
            return false;
        }
        n.has_error() // subtrees without errors are pruned wholesale
    });
}
