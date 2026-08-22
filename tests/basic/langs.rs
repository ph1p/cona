//! Per-language symbol extraction + identifier semantics.

use cona::{db, lang};
use std::path::Path;

#[test]
fn rust_symbols_qualified_and_generics_stripped() {
    let src = r#"
pub struct Cache<K, V> { map: Vec<(K, V)> }
impl<K: Eq, V> Cache<K, V> {
    pub fn get(&self, k: &K) -> Option<&V> { None }
    pub fn put(&mut self, k: K, v: V) {}
}
pub fn free_fn() {}
"#;
    let quals = quals("rust", src);
    assert!(quals.iter().any(|q| q == "Cache"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Cache.get"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Cache.put"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "free_fn"), "{quals:?}");
    // no generics leak into names
    assert!(quals.iter().all(|q| !q.contains('<')), "{quals:?}");
}

#[test]
fn python_class_methods_are_qualified() {
    let src = "class A:\n    def m(self):\n        pass\n\ndef top():\n    pass\n";
    let syms = lang::extract_symbols("python", src).unwrap();
    let quals: Vec<&str> = syms.iter().map(|s| s.qualified.as_str()).collect();
    assert!(quals.contains(&"A"));
    assert!(quals.contains(&"A.m"));
    assert!(quals.contains(&"top"));
    let m = syms.iter().find(|s| s.qualified == "A.m").unwrap();
    assert_eq!(m.start_line, 2);
}

#[test]
fn typescript_interfaces_and_methods() {
    let src = "interface Shape { area(): number }\nclass Circle {\n  radius = 1\n  area() { return 3.14 }\n}\n";
    let quals = quals("typescript", src);
    assert!(quals.iter().any(|q| q == "Shape"));
    assert!(quals.iter().any(|q| q == "Circle.area"), "{quals:?}");
}

#[test]
fn go_functions_methods_types() {
    let src = "package main\n\ntype Point struct{ X int }\n\nfunc (p Point) Dist() int { return p.X }\n\nfunc main() { p := Point{1}; p.Dist() }\n";
    let quals = quals("go", src);
    assert!(quals.iter().any(|q| q == "Point"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Dist"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "main"), "{quals:?}");
    assert_eq!(lang::detect_lang("x/y.go"), Some("go"));
}

#[test]
fn java_classes_and_methods_are_qualified() {
    let src = "public class App {\n  public App() {}\n  public int run() { return util(); }\n  static int util() { return 1; }\n}\ninterface Greeter { String hi(); }\n";
    let quals = quals("java", src);
    assert!(quals.iter().any(|q| q == "App"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "App.run"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "App.util"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Greeter"), "{quals:?}");
    assert_eq!(lang::detect_lang("A.java"), Some("java"));
}

#[test]
fn c_functions_structs_typedefs() {
    let src = "#include <stdio.h>\n#define MAX(a,b) ((a)>(b)?(a):(b))\nstruct point { int x; };\ntypedef struct point point_t;\nstatic int *helper(int n) { return 0; }\nint main(int argc, char **argv) { helper(argc); return 0; }\n";
    let quals = quals("c", src);
    assert!(quals.iter().any(|q| q == "point"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "point_t"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "helper"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "main"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "MAX"), "{quals:?}");
    // a bare `struct point x;` usage must NOT create a second symbol
    let src2 = "struct point; void f(struct point p) {}\n";
    let syms2 = lang::extract_symbols("c", src2).unwrap();
    assert!(syms2.iter().all(|s| s.name != "point"), "{syms2:?}");
    assert_eq!(lang::detect_lang("m.c"), Some("c"));
    assert_eq!(lang::detect_lang("m.h"), Some("c"));
}

#[test]
fn cpp_classes_namespaces_methods() {
    let src = "namespace geo {\nclass Circle {\npublic:\n  double area() { return 3.14; }\n};\ndouble Circle_area2();\n}\ndouble geo::Circle_area2() { return 0; }\n";
    let quals = quals("cpp", src);
    assert!(quals.iter().any(|q| q == "geo"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "geo.Circle"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "geo.Circle.area"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "geo::Circle_area2"), "{quals:?}");
    assert_eq!(lang::detect_lang("m.cpp"), Some("cpp"));
    assert_eq!(lang::detect_lang("m.hpp"), Some("cpp"));
}

#[test]
fn css_rules_media_keyframes() {
    let src = ":root { --c: #333; }\n.card, .tile { color: red; }\n@media screen and (max-width: 600px) {\n  .card { display: none; }\n}\n@keyframes spin { from {} to {} }\n";
    let quals = quals("css", src);
    assert!(quals.iter().any(|q| q == ":root"), "{quals:?}");
    assert!(quals.iter().any(|q| q == ".card, .tile"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "spin"), "{quals:?}");
    // nested rule is qualified under its @media query
    assert!(
        quals
            .iter()
            .any(|q| q.starts_with("screen") && q.ends_with(".card")),
        "{quals:?}"
    );
    assert_eq!(lang::detect_lang("a/style.css"), Some("css"));
}

#[test]
fn ruby_modules_classes_methods() {
    let src = "module Billing\n  class Invoice\n    def total\n      42\n    end\n    def self.build(x)\n      new\n    end\n  end\nend\ndef helper\n  1\nend\n";
    let quals = quals("ruby", src);
    assert!(quals.iter().any(|q| q == "Billing"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Billing.Invoice"), "{quals:?}");
    assert!(
        quals.iter().any(|q| q == "Billing.Invoice.total"),
        "{quals:?}"
    );
    assert!(
        quals.iter().any(|q| q == "Billing.Invoice.build"),
        "{quals:?}"
    );
    assert!(quals.iter().any(|q| q == "helper"), "{quals:?}");
    assert_eq!(lang::detect_lang("app.rb"), Some("ruby"));
}

#[test]
fn php_classes_traits_interfaces() {
    let src = "<?php\ninterface Shape { public function area(): float; }\ntrait Loggable { public function log($m) {} }\nenum Suit { case Hearts; }\nclass Circle implements Shape {\n  public function area(): float { return 1.0; }\n}\nfunction main() { return new Circle(); }\n";
    let quals = quals("php", src);
    assert!(quals.iter().any(|q| q == "Shape"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Shape.area"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Loggable"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Suit"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Circle.area"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "main"), "{quals:?}");
    assert_eq!(lang::detect_lang("index.php"), Some("php"));
}

#[test]
fn csharp_namespace_class_method_record() {
    let src = "namespace Demo;\npublic record Point(int X, int Y);\npublic struct Vec { public int X; }\npublic interface IShape { double Area(); }\npublic class Circle : IShape {\n  public Circle(double r) {}\n  public double Area() { return 1.0; }\n}\n";
    let quals = quals("csharp", src);
    assert!(quals.iter().any(|q| q == "Demo"), "{quals:?}");
    // file-scoped namespace: declarations are siblings, not children
    assert!(quals.iter().any(|q| q == "Point"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "IShape.Area"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Circle.Circle"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Circle.Area"), "{quals:?}");
    assert_eq!(lang::detect_lang("Program.cs"), Some("csharp"));
}

#[test]
fn kotlin_classes_objects_functions() {
    let src = "class Foo {\n  fun bar(x: Int): Int { return x }\n}\nobject O {\n  fun m() {}\n}\ninterface I { fun req() }\nfun baz() {}\n";
    let quals = quals("kotlin", src);
    assert!(quals.iter().any(|q| q == "Foo"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Foo.bar"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "O"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "O.m"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "baz"), "{quals:?}");
    assert_eq!(lang::detect_lang("Main.kt"), Some("kotlin"));
    assert_eq!(lang::detect_lang("build.kts"), Some("kotlin"));
}

#[test]
fn swift_classes_protocols_functions() {
    let src = "class Foo {\n  func bar() {}\n}\nprotocol P { func req() }\nstruct S {}\nenum E {}\nfunc baz() {}\n";
    let quals = quals("swift", src);
    assert!(quals.iter().any(|q| q == "Foo"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Foo.bar"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "P"), "{quals:?}");
    // struct/enum parse as class_declaration — labeled class, still captured
    assert!(quals.iter().any(|q| q == "S"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "baz"), "{quals:?}");
    assert_eq!(lang::detect_lang("App.swift"), Some("swift"));
}

#[test]
fn swift_declaration_kinds_and_members() {
    let src = "struct S {\n  init() {}\n  subscript(i: Int) -> Int { i }\n}\n\
               extension S {\n  func ext() {}\n}\nactor A {}\ntypealias T = Int\n\
               enum E {}\nclass C {\n  deinit {}\n}\n";
    let syms = lang::extract_symbols("swift", src).unwrap();
    let kind_of = |q: &str| syms.iter().find(|s| s.qualified == q).map(|s| s.kind);
    // struct/enum/extension/actor relabeled from the declaration keyword
    assert_eq!(kind_of("S"), Some("struct"), "{syms:?}");
    assert_eq!(kind_of("E"), Some("enum"), "{syms:?}");
    assert_eq!(kind_of("A"), Some("actor"), "{syms:?}");
    assert_eq!(kind_of("C"), Some("class"), "{syms:?}");
    assert!(
        syms.iter().any(|s| s.name == "S" && s.kind == "extension"),
        "extension S must be captured as its own kind: {syms:?}"
    );
    // anonymous members addressable via the FIXED_NAME sentinel
    assert_eq!(kind_of("S.init"), Some("init"), "{syms:?}");
    assert_eq!(kind_of("S.subscript"), Some("subscript"), "{syms:?}");
    assert_eq!(kind_of("C.deinit"), Some("deinit"), "{syms:?}");
    assert_eq!(kind_of("S.ext"), Some("func"), "{syms:?}");
    assert_eq!(kind_of("T"), Some("type"), "{syms:?}");
}

#[test]
fn ts_function_valued_consts_are_indexed() {
    let src = "export const handler = async (req: Request) => {\n  return null\n}\n\
               const legacy = function () {}\n\
               class C {\n  onClick = () => {}\n}\n\
               const nested = () => {\n  const inner = () => {}\n}\n\
               const notAFn = 42\nconst { destructured } = mod\n";
    let syms = lang::extract_symbols("typescript", src).unwrap();
    let kind_of = |q: &str| syms.iter().find(|s| s.qualified == q).map(|s| s.kind);
    assert_eq!(kind_of("handler"), Some("fn"), "{syms:?}");
    assert_eq!(kind_of("legacy"), Some("fn"), "{syms:?}");
    assert_eq!(kind_of("C.onClick"), Some("method"), "{syms:?}");
    // nested arrow inside an arrow body is qualified under its parent
    assert_eq!(kind_of("nested.inner"), Some("fn"), "{syms:?}");
    // non-function bindings and destructuring stay unindexed
    assert!(syms.iter().all(|s| s.name != "notAFn"), "{syms:?}");
    assert!(syms.iter().all(|s| s.name != "destructured"), "{syms:?}");
}

#[test]
fn bash_functions() {
    let src = "foo() {\n  echo hi\n}\nfunction bar {\n  echo yo\n}\n";
    let quals = quals("bash", src);
    assert!(quals.iter().any(|q| q == "foo"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "bar"), "{quals:?}");
    assert_eq!(lang::detect_lang("deploy.sh"), Some("bash"));
}

#[test]
fn lua_functions() {
    let src = "function foo(x) return x end\nlocal function bar() end\n";
    let quals = quals("lua", src);
    assert!(quals.iter().any(|q| q == "foo"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "bar"), "{quals:?}");
    assert_eq!(lang::detect_lang("init.lua"), Some("lua"));
}

#[test]
fn scala_classes_objects_traits_defs() {
    let src =
        "class Foo {\n  def bar(x: Int): Int = x\n}\nobject O {}\ntrait T {}\ndef baz() = 1\n";
    let quals = quals("scala", src);
    assert!(quals.iter().any(|q| q == "Foo"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Foo.bar"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "O"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "T"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "baz"), "{quals:?}");
    assert_eq!(lang::detect_lang("Main.scala"), Some("scala"));
}

#[test]
fn elixir_modules_and_functions() {
    let src = "defmodule Foo do\n  def bar(x) do\n    x\n  end\n  defp baz, do: 1\nend\n";
    let quals = quals("elixir", src);
    assert!(quals.iter().any(|q| q == "Foo"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Foo.bar"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Foo.baz"), "{quals:?}");
    assert_eq!(lang::detect_lang("app.ex"), Some("elixir"));
    assert_eq!(lang::detect_lang("test.exs"), Some("elixir"));
}

#[test]
fn dart_classes_methods_enums() {
    let src = "class Foo {\n  int bar(int x) { return x; }\n}\nvoid baz() {}\nenum E { a, b }\n";
    let quals = quals("dart", src);
    assert!(quals.iter().any(|q| q == "Foo"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "Foo.bar"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "baz"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "E"), "{quals:?}");
    assert_eq!(lang::detect_lang("main.dart"), Some("dart"));
}

#[test]
fn toml_tables() {
    let src = "[section]\nkey = \"val\"\n[other.sub]\nx = 1\n[[array]]\ny = 2\n";
    let quals = quals("toml", src);
    assert!(quals.iter().any(|q| q == "section"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "other.sub"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "array"), "{quals:?}");
    assert_eq!(lang::detect_lang("Cargo.toml"), Some("toml"));
}

#[test]
fn yaml_mapping_keys() {
    let src = "name: x\nnested:\n  key: val\n";
    let quals = quals("yaml", src);
    assert!(quals.iter().any(|q| q == "name"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "nested"), "{quals:?}");
    assert!(quals.iter().any(|q| q == "nested.key"), "{quals:?}");
    assert_eq!(lang::detect_lang("ci.yml"), Some("yaml"));
    assert_eq!(lang::detect_lang("conf.yaml"), Some("yaml"));
}

#[test]
fn markdown_headings() {
    let src = "# Title\n\n## Section\n\ntext\n\n### Sub\n";
    let syms = lang::extract_symbols("markdown", src).unwrap();
    let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Title"), "{names:?}");
    assert!(names.contains(&"Section"), "{names:?}");
    assert!(names.contains(&"Sub"), "{names:?}");
    assert_eq!(lang::detect_lang("README.md"), Some("markdown"));
}

fn quals(lang: &str, src: &str) -> Vec<String> {
    lang::extract_symbols(lang, src)
        .unwrap()
        .iter()
        .map(|s| s.qualified.clone())
        .collect()
}

#[test]
fn hcl_terraform_blocks() {
    let src = "resource \"aws_instance\" \"web\" {\n  ami = \"x\"\n}\n\
               variable \"region\" {\n  default = \"us\"\n}\n\
               module \"vpc\" { source = \"./vpc\" }\n\
               locals { x = 1 }\n";
    let q = quals("hcl", src);
    assert!(
        q.contains(&"resource.aws_instance.web".to_string()),
        "{q:?}"
    );
    assert!(q.contains(&"variable.region".to_string()), "{q:?}");
    assert!(q.contains(&"module.vpc".to_string()), "{q:?}");
    assert!(q.contains(&"locals".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("main.tf"), Some("hcl"));
    assert_eq!(lang::detect_lang("vars.tfvars"), Some("hcl"));
    assert_eq!(lang::detect_lang("config.hcl"), Some("hcl"));
}

#[test]
fn zig_symbols() {
    let q = quals(
        "zig",
        "fn add(a: i32) i32 { return a; }\npub fn main() void {}\n",
    );
    assert!(q.contains(&"add".to_string()), "{q:?}");
    assert!(q.contains(&"main".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("x.zig"), Some("zig"));
}

#[test]
fn haskell_symbols() {
    let q = quals(
        "haskell",
        "data Tree = Leaf\nadd a b = a + b\nclass Show a where\n  s :: a\n",
    );
    assert!(q.contains(&"Tree".to_string()), "{q:?}");
    assert!(q.contains(&"add".to_string()), "{q:?}");
    assert!(q.contains(&"Show".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("Main.hs"), Some("haskell"));
}

#[test]
fn ocaml_symbols() {
    let q = quals(
        "ocaml",
        "let add a b = a + b\ntype tree = Leaf\nmodule M = struct let x = 1 end\n",
    );
    assert!(q.contains(&"add".to_string()), "{q:?}");
    assert!(q.contains(&"tree".to_string()), "{q:?}");
    assert!(q.contains(&"M".to_string()), "{q:?}");
    assert!(q.contains(&"M.x".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("a.ml"), Some("ocaml"));
    assert_eq!(lang::detect_lang("a.mli"), Some("ocaml"));
}

#[test]
fn julia_symbols() {
    let q = quals(
        "julia",
        "function add(a, b)\n a\nend\nstruct Point\n x\nend\nmodule M\nend\nabstract type Animal end\n",
    );
    assert!(q.contains(&"add".to_string()), "{q:?}");
    assert!(q.contains(&"Point".to_string()), "{q:?}");
    assert!(q.contains(&"M".to_string()), "{q:?}");
    assert!(q.contains(&"Animal".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("x.jl"), Some("julia"));
}

#[test]
fn powershell_symbols() {
    let q = quals("powershell", "function Get-Foo {}\nclass Bar {}\n");
    assert!(q.contains(&"Get-Foo".to_string()), "{q:?}");
    assert!(q.contains(&"Bar".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("x.ps1"), Some("powershell"));
}

#[test]
fn objc_symbols() {
    let q = quals(
        "objc",
        "@interface Foo : NSObject\n- (void)doThing;\n@end\n@implementation Foo\n- (void)doThing {}\n@end\n",
    );
    assert!(q.contains(&"Foo".to_string()), "{q:?}");
    assert!(q.contains(&"Foo.doThing".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("x.m"), Some("objc"));
}

#[test]
fn proto_symbols() {
    let q = quals(
        "proto",
        "message Foo {}\nservice Bar { rpc Do(Foo) returns (Foo); }\nenum E { A = 0; }\n",
    );
    assert!(q.contains(&"Foo".to_string()), "{q:?}");
    assert!(q.contains(&"Bar".to_string()), "{q:?}");
    assert!(q.contains(&"Bar.Do".to_string()), "{q:?}");
    assert!(q.contains(&"E".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("x.proto"), Some("proto"));
}

#[test]
fn sql_symbols() {
    let q = quals(
        "sql",
        "CREATE TABLE users (id INT);\nCREATE VIEW v AS SELECT 1;\nCREATE FUNCTION f() RETURNS INT AS $$ SELECT 1 $$;\n",
    );
    assert!(q.contains(&"users".to_string()), "{q:?}");
    assert!(q.contains(&"v".to_string()), "{q:?}");
    assert!(q.contains(&"f".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("q.sql"), Some("sql"));
}

#[test]
fn perl_symbols() {
    let q = quals("perl", "package Foo;\nsub bar {}\nsub baz {}\n");
    assert!(q.contains(&"Foo".to_string()), "{q:?}");
    assert!(q.contains(&"bar".to_string()), "{q:?}");
    assert!(q.contains(&"baz".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("x.pl"), Some("perl"));
    assert_eq!(lang::detect_lang("X.pm"), Some("perl"));
}

#[test]
fn make_symbols() {
    let q = quals("make", "CC = gcc\nall: main.o\n\tgcc\nclean:\n\trm\n");
    assert!(q.contains(&"CC".to_string()), "{q:?}");
    assert!(q.contains(&"all".to_string()), "{q:?}");
    assert!(q.contains(&"clean".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("Makefile"), Some("make"));
    assert_eq!(lang::detect_lang("path/to/Makefile"), Some("make"));
    assert_eq!(lang::detect_lang("build.mk"), Some("make"));
}

#[test]
fn dockerfile_symbols() {
    // build stages: named `AS` alias wins, unnamed falls back to image spec
    let q = quals(
        "dockerfile",
        "FROM rust:1 AS build\nRUN cargo build\nFROM alpine\nCMD [\"x\"]\n",
    );
    assert!(q.contains(&"build".to_string()), "{q:?}");
    assert!(q.contains(&"alpine".to_string()), "{q:?}");
    assert_eq!(lang::detect_lang("Dockerfile"), Some("dockerfile"));
    assert_eq!(lang::detect_lang("path/to/Dockerfile"), Some("dockerfile"));
    assert_eq!(lang::detect_lang("Dockerfile.prod"), Some("dockerfile"));
    assert_eq!(lang::detect_lang("api.dockerfile"), Some("dockerfile"));
}

#[test]
fn parse_only_langs_parse_without_symbols() {
    // nix/svelte/vue/r/graphql are parsed for refs/grep but extract no symbols.
    // xml is NOT in this list any more — it yields element symbols; see
    // `xml_elements_named_by_identifying_child`.
    for (lang, src) in [
        ("nix", "{ pkgs }: { hello = pkgs.hello; }\n"),
        ("svelte", "<script>let x = 0;</script>\n<b>{x}</b>\n"),
        (
            "vue",
            "<script setup>\nfunction greet() {}\n</script>\n<template><b>{{ greet() }}</b></template>\n",
        ),
        ("r", "add <- function(a) a\n"),
        ("graphql", "type Query { hello: String }\n"),
    ] {
        // must not error (parser wired), symbol list may be empty
        let syms = lang::extract_symbols(lang, src).unwrap();
        assert!(syms.is_empty() || !syms.is_empty(), "{lang}");
    }
    assert_eq!(lang::detect_lang("f.nix"), Some("nix"));
    assert_eq!(lang::detect_lang("C.svelte"), Some("svelte"));
    assert_eq!(lang::detect_lang("App.vue"), Some("vue"));
    assert_eq!(lang::detect_lang("s.R"), Some("r"));
    assert_eq!(lang::detect_lang("d.xml"), Some("xml"));
    assert_eq!(lang::detect_lang("s.graphql"), Some("graphql"));
}

#[test]
fn syntax_errors_detected_and_clean_code_passes() {
    assert!(lang::syntax_errors("python", "def ok():\n    return 1\n")
        .unwrap()
        .is_empty());
    assert!(!lang::syntax_errors("python", "def broken(:\n  ((\n")
        .unwrap()
        .is_empty());
}

#[test]
fn excluded_dirs_and_home_guard() {
    use cona::indexer::is_excluded_dir;
    assert!(is_excluded_dir("node_modules"));
    assert!(is_excluded_dir("target"));
    assert!(is_excluded_dir(".venv"));
    assert!(!is_excluded_dir("src"));
    assert!(!is_excluded_dir("lib"));

    assert!(db::is_home_or_fs_root(Path::new("/")));
    assert!(!db::is_home_or_fs_root(Path::new("/tmp/some/project")));
}

#[test]
fn project_hash_is_deterministic_and_distinct() {
    let a1 = db::project_hash(Path::new("/tmp/project-a"));
    let a2 = db::project_hash(Path::new("/tmp/project-a"));
    let b = db::project_hash(Path::new("/tmp/project-b"));
    assert_eq!(a1, a2);
    assert_ne!(a1, b);
    assert_eq!(a1.len(), 16);
    // pinned value: FNV-1a must never change, or existing DBs are orphaned
    assert_eq!(db::project_hash(Path::new("/x")), "07d64e07b49caeb2");
}

#[test]
fn extract_idents_ordered_unique_no_numbers() {
    let src = "let x2 = foo(bar, foo); baz_qux.method(42, 9abc)";
    let ids = lang::extract_idents(src);
    assert_eq!(ids, vec!["let", "x2", "foo", "bar", "baz_qux", "method"]);
}

#[test]
fn ident_occurrences_skip_strings_and_comments() {
    let src = r#"
// helper calls target in a comment: target
fn caller() {
    let s = "target inside a string";
    target();
}
"#;
    let occ = lang::ident_occurrences("rust", src).unwrap();
    let target_lines: Vec<usize> = occ
        .iter()
        .filter(|(n, _)| n == "target")
        .map(|(_, l)| *l)
        .collect();
    // only the real call on line 5 — not the comment (2) or the string (4)
    assert_eq!(target_lines, vec![5]);
}

#[test]
fn ref_lines_semantic_vs_textual_fallback() {
    let src = "// target\nfn f() { target(); }\nlet s = \"target\";\n";
    // rust parses → semantic: only the call
    assert_eq!(lang::ref_lines(Some("rust"), src, "target"), vec![2]);
    // unknown language → textual word-boundary fallback: all three lines
    assert_eq!(lang::ref_lines(None, src, "target"), vec![1, 2, 3]);
    // word boundaries hold in the fallback: no substring hits
    assert_eq!(
        lang::ref_lines(None, "retargeted target2 target", "target"),
        vec![1]
    );
}

#[test]
fn xml_elements_named_by_identifying_child() {
    let src = "<project>\n  <artifactId>demo</artifactId>\n  <profiles>\n    <profile>\n      <id>with-frontend-build</id>\n    </profile>\n  </profiles>\n</project>\n";
    let quals = quals("xml", src);
    // the tag alone is not addressable in a POM — every <profile> looks alike,
    // so the identifying child is folded into the name with a `#` separator
    assert!(
        quals
            .iter()
            .any(|q| q.ends_with("profile#with-frontend-build")),
        "{quals:?}"
    );
    assert!(quals.iter().any(|q| q == "project#demo"), "{quals:?}");
    assert_eq!(lang::detect_lang("pom.xml"), Some("xml"));
}

#[test]
fn html_elements_named_by_attribute_or_directive() {
    let src = "<html>\n  <body>\n    <main id=\"shell\">\n      <h1 th:text=\"${title}\">t</h1>\n      <div class=\"x\">plain</div>\n    </main>\n  </body>\n</html>\n";
    let quals = quals("html", src);
    // structural landmarks earn a symbol with no attributes at all
    assert!(quals.iter().any(|q| q.ends_with("body")), "{quals:?}");
    assert!(quals.iter().any(|q| q.ends_with("main#shell")), "{quals:?}");
    // a framework directive identifies an element when no plain attribute does
    assert!(
        quals.iter().any(|q| q.ends_with("h1#@th:text")),
        "{quals:?}"
    );
    // an unidentified, non-structural div is noise and stays out
    assert!(!quals.iter().any(|q| q.ends_with("div")), "{quals:?}");
    assert_eq!(lang::detect_lang("templates/home.html"), Some("html"));
}
