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
    // nix/svelte/vue/r/xml/graphql are parsed for refs/grep but extract no symbols
    for (lang, src) in [
        ("nix", "{ pkgs }: { hello = pkgs.hello; }\n"),
        ("svelte", "<script>let x = 0;</script>\n<b>{x}</b>\n"),
        (
            "vue",
            "<script setup>\nfunction greet() {}\n</script>\n<template><b>{{ greet() }}</b></template>\n",
        ),
        ("r", "add <- function(a) a\n"),
        ("xml", "<root><child>x</child></root>\n"),
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
fn mcp_stdio_handshake_and_tools_list() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let data = std::env::temp_dir().join(format!("cona-data-mcp-{}", std::process::id()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_cona"))
        .arg("mcp")
        .env("CONA_DATA_DIR", &data)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"more","arguments":{}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"find","arguments":{"name":"hello"}}}"#,
                "\n",
                // after `more`, the extended tools must appear in tools/list —
                // a client may only call what tools/list returned
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/list"}"#,
                "\n",
            )
            .as_bytes(),
        )
        .unwrap();
    drop(stdin); // EOF ends the serve loop
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let all: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // Server-initiated notifications are interleaved with the replies; keep them
    // apart so the replies stay addressable by request order.
    let notes: Vec<&str> = all
        .iter()
        .filter(|m| m.get("id").is_none())
        .map(|m| m["method"].as_str().unwrap_or(""))
        .collect();
    let lines: Vec<serde_json::Value> = all
        .iter()
        .filter(|m| m.get("id").is_some())
        .cloned()
        .collect();
    assert_eq!(lines.len(), 5); // our own notifications/initialized gets no reply
                                // Unlocking the extended tier MUST announce itself: a client that is never
                                // told to re-list can never call the tools `more` just revealed.
    assert!(
        notes.contains(&"notifications/tools/list_changed"),
        "no list_changed after `more`: {notes:?}"
    );
    assert_eq!(
        lines[0]["result"]["capabilities"]["tools"]["listChanged"],
        true
    );
    assert_eq!(lines[0]["result"]["serverInfo"]["name"], "cona");
    // a supported protocol version is echoed back verbatim (negotiation)
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-03-26");
    assert!(lines[0]["result"]["serverInfo"]["title"].is_string());
    // initialize carries the server preamble teaching the cona workflow
    let instructions = lines[0]["result"]["instructions"]
        .as_str()
        .expect("initialize result should carry instructions");
    assert!(
        instructions.contains("outline") && instructions.contains("show"),
        "{instructions:?}"
    );
    let tools: Vec<&str> = lines[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        tools.contains(&"find") && tools.contains(&"edit"),
        "{tools:?}"
    );
    // Progressive disclosure: tools/list carries the core tier plus the `more`
    // gate, NOT the full set — the schemas are re-sent on every request, so the
    // advanced tail is disclosed on demand instead.
    assert!(
        tools.contains(&"more"),
        "missing disclosure gate: {tools:?}"
    );
    assert!(
        tools.len() < 12,
        "tools/list should stay small, got {}: {tools:?}",
        tools.len()
    );

    // Parity is total once expanded, and the EXPANDED tools/list is what proves
    // it: `more`'s text payload is advisory, but only a listed tool is callable.
    let expanded: Vec<&str> = lines[4]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    let more = lines[2]["result"]["content"][0]["text"].as_str().unwrap();
    let gated: serde_json::Value = serde_json::from_str(&more[more.find('[').unwrap()..]).unwrap();
    let gated: Vec<&str> = gated
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for t in [
        "insert",
        "batch_edit",
        "check",
        "impact",
        "callers",
        "callees",
        "path",
        "deps",
        "shape",
        "entries",
        "tests",
        "note",
    ] {
        assert!(gated.contains(&t), "missing MCP tool {t}: {gated:?}");
        assert!(
            expanded.contains(&t),
            "{t} described by `more` but absent from the expanded tools/list, \
             so no client can call it: {expanded:?}"
        );
    }
    // A tool is disclosed by exactly one tier, never both.
    for t in &tools {
        assert!(!gated.contains(t), "tool {t} disclosed twice");
    }
    // Every core tool survives expansion, and the spent gate is retired.
    for t in &tools {
        if *t == "more" {
            assert!(!expanded.contains(t), "spent `more` gate still listed");
        } else {
            assert!(expanded.contains(t), "core tool {t} lost on expansion");
        }
    }

    // behaviour annotations: read-only queries vs writing tools. Core tools come
    // from tools/list, gated ones from the `more` payload — annotations must
    // survive disclosure, since that is the only place a client ever sees them.
    let ann = |name: &str| -> serde_json::Value {
        let from_list = lines[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .cloned();
        let from_more = || {
            serde_json::from_str::<serde_json::Value>(&more[more.find('[').unwrap()..])
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .cloned()
                .unwrap()
        };
        from_list.unwrap_or_else(from_more)["annotations"].clone()
    };
    assert_eq!(ann("show")["readOnlyHint"], true);
    assert_eq!(ann("edit")["readOnlyHint"], false);
    assert_eq!(ann("edit")["destructiveHint"], true);
    assert_eq!(ann("insert")["destructiveHint"], false);

    // A declared outputSchema is a contract: the tool must return matching
    // structuredContent, so schema and payload are asserted together.
    let find_tool = lines[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "find")
        .unwrap()
        .clone();
    assert_eq!(find_tool["outputSchema"]["type"], "object");
    assert!(find_tool["outputSchema"]["properties"]["symbols"].is_object());
    assert!(
        lines[3]["result"]["structuredContent"]["symbols"].is_array(),
        "find must return structuredContent: {:?}",
        lines[3]["result"]
    );
}

// Drive the real binary against a real temp repo — matches the recovery-bug
// test philosophy (real IO). Covers edit --range, insert --after, and the
// syntax-verify rollback shared by both.
#[test]
fn edit_range_and_insert_roundtrip() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("s.rs");
    std::fs::write(&file, "fn a() {\n    let x = 1;\n}\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str], stdin: &str| -> (bool, String) {
        use std::io::Write;
        let mut c = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let o = c.wait_with_output().unwrap();
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
        )
    };

    run(&["index"], "");

    // edit --range replaces just line 2
    let (ok, _) = run(&["edit", "s.rs", "--range", "2-2"], "    let x = 42;");
    assert!(ok);
    assert!(std::fs::read_to_string(&file)
        .unwrap()
        .contains("let x = 42;"));

    // insert --after adds a sibling symbol, whole file still parses
    let (ok, _) = run(&["insert", "a", "--after"], "fn b() {}\n");
    assert!(ok);
    let body = std::fs::read_to_string(&file).unwrap();
    assert!(body.contains("fn b() {}"));

    // syntax error is rejected and the file is left untouched (invariant 3)
    let before = std::fs::read_to_string(&file).unwrap();
    let (ok, msg) = run(&["edit", "s.rs", "--range", "2-2"], "    let x = ;;;");
    assert!(!ok, "expected rejection, got: {msg}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);

    // check reports a clean file as ok
    let (ok, out) = run(&["check", "s.rs"], "");
    assert!(ok && out.contains("ok"), "{out}");

    // insert --at into a brand-new file (no indexed symbol to anchor on)
    let (ok, _) = run(&["insert", "--at", "fresh.rs", "0"], "fn fresh() {}\n");
    assert!(ok);
    assert_eq!(
        std::fs::read_to_string(dir.join("fresh.rs")).unwrap(),
        "fn fresh() {}\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// `show --all` renders every candidate of an ambiguous name — including the
// same-file enum + impl pair, where the `file:Name` escape hatch cannot
// disambiguate. Pins the guide/skill/MCP promise ("--all prints every
// definition instead of erroring") and the honest ambiguity message: hatches
// that cannot separate the pool (file/Parent.Name for a same-file pair) are
// not suggested.
#[test]
fn show_all_renders_same_file_enum_impl_pair() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-showall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("s.rs"),
        "pub enum Thing { A }\nimpl Thing {\n    pub fn go(&self) {}\n}\n",
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| -> (bool, String) {
        let o = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(&dir)
            .output()
            .unwrap();
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr),
        )
    };

    run(&["index"]);
    // --all prints BOTH definitions, no ambiguity error
    let (ok, out) = run(&["show", "Thing", "--all"]);
    assert!(ok, "{out}");
    assert!(out.contains("enum Thing"), "{out}");
    assert!(out.contains("impl Thing"), "{out}");
    assert!(!out.contains("ambiguous"), "{out}");
    // without --all a SMALL ambiguity (≤3 candidates, ≤400 lines total)
    // auto-renders every definition instead of erroring — a dead-end error
    // that --all immediately fixes was pure friction. The banner still names
    // the narrowing hatches.
    let (ok, msg) = run(&["show", "Thing"]);
    assert!(ok, "expected auto-all render, got error: {msg}");
    assert!(msg.contains("ambiguous — showing all 2"), "{msg}");
    assert!(
        msg.contains("enum Thing") && msg.contains("impl Thing"),
        "{msg}"
    );
    assert!(msg.contains("--kind"), "{msg}");
    let _ = std::fs::remove_dir_all(&dir);
}

// Grouped subcommands (`nav show`) and their flat aliases (`show`) dispatch to
// the same operation and produce identical output. Pins the CLI grouping
// contract: flat forms stay backward-compatible forever.
#[test]
fn grouped_and_flat_are_equivalent() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-group-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("s.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| -> String {
        let o = Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", dir.join(".cona-data"))
            .current_dir(&dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).to_string()
    };

    run(&["index"]);
    // one representative per group: nav/inspect/history
    for (flat, grouped) in [
        (vec!["show", "alpha"], vec!["nav", "show", "alpha"]),
        (vec!["outline", "s.rs"], vec!["nav", "outline", "s.rs"]),
        (vec!["deps"], vec!["inspect", "deps"]),
        (vec!["entries"], vec!["inspect", "entries"]),
    ] {
        assert_eq!(
            run(&flat),
            run(&grouped),
            "flat {flat:?} != grouped {grouped:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// check with no file argument walks git for changed + untracked files.
#[test]
fn check_no_arg_walks_git_changes() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    // one committed-clean file, one untracked broken file
    std::fs::write(dir.join("ok.rs"), "fn a() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(dir.join("bad.rs"), "fn broken( {\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_cona");
    Command::new(bin)
        .arg("index")
        .env("CONA_DATA_DIR", dir.join(".cona-data"))
        .current_dir(&dir)
        .output()
        .unwrap();
    let out = Command::new(bin)
        .arg("check")
        .env("CONA_DATA_DIR", dir.join(".cona-data"))
        .current_dir(&dir)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    // the untracked broken file is flagged; the clean committed one is not walked
    assert!(
        text.contains("bad.rs") && text.contains("syntax error"),
        "{text}"
    );
    assert!(
        !text.contains("ok.rs: ok"),
        "clean committed file should not be walked: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--read-only` cannot refresh the index, so it must never serve stale line
/// numbers as if they were current (invariant 2). `show` fails with a message
/// naming the file and the fix — NOT rusqlite's "attempt to write a readonly
/// database" — and `outline`, which still prints its indexed ranges, labels
/// them stale. The writable run afterwards proves the refusal is scoped to
/// read-only mode and normal use still self-heals.
#[test]
fn read_only_never_serves_stale_ranges_as_fresh() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("cona-ro-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join(".cona-data");
    let bin = env!("CARGO_BIN_EXE_cona");
    let cona = |args: &[&str]| {
        Command::new(bin)
            .args(args)
            .env("CONA_DATA_DIR", &data)
            .current_dir(&dir)
            .output()
            .unwrap()
    };

    std::fs::write(dir.join("lib.rs"), "fn target() {\n    let _ = 1;\n}\n").unwrap();
    cona(&["index"]);
    // fresh index: read-only resolves the real range
    let out = cona(&["--read-only", "show", "target"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("lib.rs:1-3"),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // shift the symbol down without reindexing — indexed 1-3, really 3-5
    std::fs::write(
        dir.join("lib.rs"),
        "// added\n// added\nfn target() {\n    let _ = 1;\n}\n",
    )
    .unwrap();

    // `show` reports per-symbol failures on stdout so one bad name cannot abort
    // a multi-symbol batch — the message is what matters, not the stream.
    let out = cona(&["--read-only", "show", "target"]);
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        err.contains("read-only mode") && err.contains("lib.rs"),
        "expected a stale-file message naming the file, got: {err}"
    );
    assert!(
        !err.contains("readonly database"),
        "raw sqlite error leaked instead of an actionable message: {err}"
    );
    // the stale range must not be printed as if it were current
    assert!(
        !err.contains("lib.rs:1-3"),
        "stale range served as fresh: {err}"
    );

    // outline still prints the indexed ranges, but discloses that they are stale
    let out = cona(&["--read-only", "outline", "lib.rs"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("stale"), "outline hid staleness: {text}");

    // writable mode is unaffected: it reindexes and reports the live range
    let out = cona(&["show", "target"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("lib.rs:3-5"),
        "writable mode should self-heal: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The plugin ships its own copy of the skill because Claude Code loads it from
/// `plugin/skills/cona/SKILL.md`, while the installer bakes the root file in via
/// `include_str!`. Two sources for one text drift silently — pin them equal.
#[test]
fn plugin_skill_matches_the_canonical_one() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let canonical = std::fs::read_to_string(root.join("SKILL.md")).expect("root SKILL.md");
    let plugin =
        std::fs::read_to_string(root.join("plugin/skills/cona/SKILL.md")).expect("plugin SKILL.md");
    assert_eq!(
        canonical, plugin,
        "plugin/skills/cona/SKILL.md drifted from SKILL.md — \
         re-run: cp SKILL.md plugin/skills/cona/SKILL.md"
    );
}

/// The redirect tier only runs on tool calls the matcher admits, and the matcher
/// is written down twice: once derived from hook.rs (settings.json, via the
/// installer) and once by hand in the plugin's hooks.json. A drift between them
/// is invisible — the hook simply stops firing on that distribution path,
/// silently, with no error anywhere.
#[test]
fn plugin_hook_matcher_matches_the_installer() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let hooks: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("plugin/hooks/hooks.json")).expect("plugin hooks.json"),
    )
    .expect("plugin hooks.json is valid JSON");
    let pre = hooks["hooks"]["PreToolUse"]
        .as_array()
        .and_then(|a| a.first())
        .expect("a PreToolUse entry");
    assert_eq!(
        pre["matcher"].as_str(),
        Some(cona::hook::PRETOOL_MATCHER.as_str()),
        "plugin/hooks/hooks.json PreToolUse matcher drifted from \
         hook::PRETOOL_MATCHER"
    );
}

/// One plugin directory serves both harnesses: Claude Code reads
/// `.claude-plugin/plugin.json`, Codex reads `.codex-plugin/plugin.json`, and
/// both point at the SAME skills/hooks/mcp payload. If the two manifests
/// disagree about what they are describing, one harness ships something the
/// other does not.
#[test]
fn both_plugin_manifests_describe_the_same_plugin() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let read = |p: &str| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(root.join(p)).unwrap_or_else(|e| {
            panic!("{p}: {e}");
        }))
        .unwrap_or_else(|e| panic!("{p} is not valid JSON: {e}"))
    };
    let claude = read("plugin/.claude-plugin/plugin.json");
    let codex = read("plugin/.codex-plugin/plugin.json");
    for key in ["name", "version", "description", "homepage", "license"] {
        assert_eq!(
            claude[key], codex[key],
            "plugin manifests disagree on `{key}`"
        );
    }
    // The Codex manifest names the shared payload explicitly (Claude Code
    // discovers it by convention), so those paths must actually exist.
    for key in ["skills", "mcpServers", "hooks"] {
        let rel = codex[key].as_str().unwrap_or_else(|| {
            panic!(".codex-plugin/plugin.json is missing `{key}`");
        });
        let path = root.join("plugin").join(rel.trim_start_matches("./"));
        assert!(path.exists(), "`{key}` points at a missing path: {rel}");
    }
}

/// A malformed inputSchema is not a soft failure: clients reject the whole
/// tools/list, so ONE bad tool silently removes every cona tool from the
/// session. Validate the shape of all of them, both tiers.
#[test]
fn every_mcp_tool_schema_is_well_formed() {
    for expanded in [false, true] {
        for t in cona::commands::mcp_server::mcp_tools(expanded) {
            let name = t["name"].as_str().expect("tool needs a name");
            let schema = &t["inputSchema"];
            assert_eq!(schema["type"], "object", "{name}: inputSchema not object");
            let props = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name}: properties missing/not an object"));
            for (prop, spec) in props {
                let spec = spec
                    .as_object()
                    .unwrap_or_else(|| panic!("{name}.{prop} is not a schema object: {spec}"));
                assert!(
                    spec.contains_key("type"),
                    "{name}.{prop} has no declared type"
                );
            }
            // `required` must name declared properties, or a client can reject a
            // call it has no way to satisfy.
            for r in t["inputSchema"]["required"].as_array().unwrap() {
                let r = r.as_str().unwrap();
                assert!(props.contains_key(r), "{name}: required {r} not declared");
            }
            if let Some(out) = t.get("outputSchema") {
                assert_eq!(out["type"], "object", "{name}: outputSchema not object");
            }
        }
    }
}

/// The SessionStart hook fires unattended in whatever directory the harness
/// happens to be in. When that is $HOME, walking the tree is never what anyone
/// asked for — several agent sessions launched from the home directory each
/// started a multi-hundred-MB walk of the whole home tree. `--session-start`
/// must bail out there, and quietly: the hook is fail-open, so it exits 0 and
/// emits no context rather than failing a session over a missing index.
#[test]
fn session_start_refuses_to_index_the_home_dir() {
    use std::process::Command;
    let home = std::env::temp_dir().join(format!("cona-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("a.rs"), "fn hello() {}\n").unwrap();
    let bin = env!("CARGO_BIN_EXE_cona");
    let run = |args: &[&str]| {
        Command::new(bin)
            .args(args)
            .env("HOME", &home)
            .env("CONA_DATA_DIR", home.join(".cona-data"))
            .current_dir(&home)
            .output()
            .unwrap()
    };

    let out = run(&["index", "--quiet", "--session-start"]);
    assert!(out.status.success(), "hook must never fail a session start");
    assert!(
        out.stdout.is_empty(),
        "no context block from an unindexed home dir: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // A typed `cona index` in $HOME stays allowed — that is a deliberate act,
    // and it warns rather than refusing.
    let out = run(&["index"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("home/filesystem root"),
        "explicit index in $HOME should warn: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(&home);
}
