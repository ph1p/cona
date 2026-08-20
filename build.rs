// Compiles the vendored tree-sitter grammars whose crates.io crates pin an old
// tree-sitter runtime (vue, dockerfile). Vendoring just the grammar C
// (parser.c + scanner) avoids linking a second `tree-sitter` runtime, which
// would collide on the shared `ts_*` C symbols. Grammar sources live in
// vendor/<lang>/; each exposes a `tree_sitter_<lang>()` C entry point that
// lang.rs binds via `extern "C"`.
use std::path::Path;

fn main() {
    // vue: C parser + C++ scanner (the scanner #includes the bundled html scanner)
    let vue = Path::new("vendor/vue");
    cc::Build::new()
        .include(vue)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs")
        .file(vue.join("parser.c"))
        .compile("tree_sitter_vue_parser");
    cc::Build::new()
        .cpp(true)
        .include(vue)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .file(vue.join("scanner.cc"))
        .compile("tree_sitter_vue_scanner");

    // dockerfile: C parser + C scanner
    let dock = Path::new("vendor/dockerfile");
    cc::Build::new()
        .include(dock)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .file(dock.join("parser.c"))
        .file(dock.join("scanner.c"))
        .compile("tree_sitter_dockerfile");

    for f in [
        "vendor/vue/parser.c",
        "vendor/vue/scanner.cc",
        // #included by vue/scanner.cc — without it here, edits to the bundled
        // html scanner do not trigger a rebuild and the stale object silently wins.
        "vendor/vue/tree_sitter_html/scanner.cc",
        "vendor/dockerfile/parser.c",
        "vendor/dockerfile/scanner.c",
    ] {
        println!("cargo:rerun-if-changed={f}");
    }
}
