//! `locals` per language: shadowing, nested functions, lambdas, member
//! positions and broken code (a missing `;`).

use super::*;

#[test]
fn java() {
    let src = r#"class A {
  int x;
  int f(int a) {
    int x = 1;
    { int y = x + a; }
    Runnable r = () -> foo(x);
    Function<Integer, Integer> g = (b) -> b + this.x + a;
    String s = "x" + obj.x + obj.x() + x;
    for (int i = 0; i < x; i++) { int a2 = i; }
    try { run(); } catch (Exception e) { log(e, x); }
    return x;
  }
  void h() { x(); y = x; }
}
"#;
    // `x` #2 is the local; #1 the field; #5 `this.x`, #6 `"x"`, #7/#8 members.
    assert_local("java", src, "x", 2, 2, &[3, 4, 9, 10, 11, 12]);
    assert_local("java", src, "x", 12, 2, &[3, 4, 9, 10, 11, 12]);
    assert_eq!(assert_not_local("java", src, "x", 5), NotLocalReason::Member);
    assert_eq!(assert_not_local("java", src, "x", 7), NotLocalReason::Member);
    assert_eq!(assert_not_local("java", src, "x", 8), NotLocalReason::Member);
    assert_eq!(assert_not_local("java", src, "x", 13), NotLocalReason::Member, "method name");
    assert_eq!(assert_not_local("java", src, "x", 1), NotLocalReason::Member, "field");
    assert_eq!(assert_not_local("java", src, "x", 14), NotLocalReason::Unresolved, "field read outside `f`");
    assert_local("java", src, "a", 2, 1, &[2, 3]);
    assert_local("java", src, "b", 2, 1, &[2]);
    assert_local("java", src, "i", 2, 1, &[2, 3, 4]);
    assert_local("java", src, "e", 1, 1, &[2]);
    assert!(matches!(locals_at("java", src, "f", 1), Locals::NotLocal { .. }));
    assert!(matches!(locals_at("java", "class A { }", "class", 1), Locals::NoIdentifier));

    // An anonymous class's field shadows the local inside the class only.
    let anon = "class A {\n  void f() {\n    int n = 1;\n    Object o = new Object() {\n      int n = 2;\n      int g() { return n; }\n    };\n    use(n);\n  }\n}\n";
    assert_local("java", anon, "n", 1, 1, &[4]);
    assert_eq!(assert_not_local("java", anon, "n", 3), NotLocalReason::Member);

    // A missing `;` must not break resolution of the surrounding locals.
    let broken = "class A {\n  void f() {\n    int x = 1\n    int y = x + 2;\n    use(x, y);\n  }\n}\n";
    assert_local("java", broken, "x", 3, 1, &[2, 3]);
    assert_local("java", broken, "y", 2, 1, &[2]);
}

#[test]
fn kotlin() {
    let src = r#"class A(val x: Int) {
  fun f(a: Int): Int {
    val x = 1
    if (a > 0) { val y = x + a }
    val g = { b: Int -> b + x }
    val h = listOf(1).map { it + x }
    val s = obj.x + f(a = x)
    fun inner(c: Int) = c + x
    return x
  }
}
"#;
    assert_local("kotlin", src, "x", 2, 2, &[3, 4, 5, 7, 8, 9]);
    assert_eq!(assert_not_local("kotlin", src, "x", 6), NotLocalReason::Member);
    assert_eq!(assert_not_local("kotlin", src, "x", 1), NotLocalReason::Member, "property");
    assert_eq!(assert_not_local("kotlin", src, "a", 4), NotLocalReason::Member, "named argument label");
    assert_local("kotlin", src, "a", 1, 1, &[2, 3]);
    assert_local("kotlin", src, "b", 1, 1, &[2]);
    assert_local("kotlin", src, "c", 2, 1, &[2]);
    assert_local("kotlin", src, "inner", 1, 1, &[]);
    assert!(matches!(locals_at("kotlin", src, "x", 2), Locals::Local { symbol } if symbol.certain));
    // tree-sitter-kotlin-ng keeps `"$x"` as plain string text: the rename would miss it.
    let tpl = "fun f() {\n    val x = 1\n    println(\"$x\")\n}\n";
    assert!(matches!(locals_at("kotlin", tpl, "x", 1), Locals::Local { symbol } if !symbol.certain));
}

#[test]
fn python() {
    let src = r#"class A:
    x = 1
    def f(self, a, *args, k=2):
        x = 1
        def inner(y):
            x = y
            return x
        g = lambda b: b + x
        s = f"x {x}" + obj.x + call(x=x)
        x = x + 1
        for i in range(x):
            print(i, a, k, args)
        return [x for x in range(3)]
"#;
    // `x` #2 is the local of `f`; inner's `x` (#3, #4) shadows it; the comprehension (#14, #15) too.
    assert_local("python", src, "x", 2, 2, &[5, 7, 10, 11, 12, 13]);
    assert_local("python", src, "x", 12, 2, &[5, 7, 10, 11, 12, 13]);
    assert_local("python", src, "x", 4, 3, &[4]);
    assert_local("python", src, "x", 14, 15, &[14]);
    assert_eq!(assert_not_local("python", src, "x", 8), NotLocalReason::Member);
    assert_eq!(assert_not_local("python", src, "x", 9), NotLocalReason::Member, "keyword argument");
    assert_eq!(assert_not_local("python", src, "x", 1), NotLocalReason::TopLevel, "class attribute");
    assert_local("python", src, "args", 1, 1, &[2]);
    assert_local("python", src, "k", 1, 1, &[2]);
    assert_local("python", src, "inner", 1, 1, &[]);
    assert!(matches!(
        locals_at("python", src, "print", 1),
        Locals::NotLocal { reason: NotLocalReason::Unresolved, .. }
    ));
}

#[test]
fn javascript_and_typescript() {
    let src = r#"function f(a, {b, c: d}, ...rest) {
  let x = 1;
  { const x = 2; use(x); }
  const g = (y) => y + x;
  const h = z => z;
  const s = `x ${x}` + obj.x + {x}.x;
  class K { m(p) { return p + x + this.x; } }
  function inner() { return x + a + b + d + rest; }
  let w = x
  return x;
}
"#;
    for lang in ["javascript", "typescript", "typescriptreact"] {
        assert_local(lang, src, "x", 1, 1, &[4, 6, 8, 10, 12, 13, 14]);
        assert_local(lang, src, "x", 3, 2, &[3]);
        assert_eq!(assert_not_local(lang, src, "x", 7), NotLocalReason::Member);
        assert_eq!(assert_not_local(lang, src, "x", 9), NotLocalReason::Member);
        assert_eq!(assert_not_local(lang, src, "x", 11), NotLocalReason::Member);
        assert_local(lang, src, "a", 2, 1, &[2]);
        assert_local(lang, src, "b", 1, 1, &[2]);
        assert_local(lang, src, "d", 2, 1, &[2]);
        assert_local(lang, src, "rest", 1, 1, &[2]);
        assert_local(lang, src, "y", 1, 1, &[2]);
        assert_local(lang, src, "z", 2, 1, &[2]);
        assert_local(lang, src, "p", 1, 1, &[2]);
        assert_local(lang, src, "inner", 1, 1, &[]);
        assert_eq!(assert_not_local(lang, src, "f", 1), NotLocalReason::TopLevel);
    }
    // Class members never shadow locals in JS: they are only reachable through `this`.
    let cls = "function f() {\n  let m = 1;\n  class K { m = 2; m2() { return m; } }\n  return m;\n}\n";
    assert_local("javascript", cls, "m", 1, 1, &[3, 4]);
    let ts = "function t<T>(a: T, b?: number): T {\n  const x: number = b ?? 1;\n  return a ?? x;\n}\n";
    assert_local("typescript", ts, "a", 1, 1, &[2]);
    assert_local("typescript", ts, "b", 2, 1, &[2]);
    assert_local("typescript", ts, "x", 2, 1, &[2]);
}

#[test]
fn rust() {
    let src = r#"fn f(a: u32, (p, q): (u32, u32)) -> u32 {
    let x = 1;
    let x = x + a;
    { let y = x; }
    let g = |b: u32| b + x;
    let s = S { x, y: x };
    let t = obj.x + obj.x();
    match a { Some(v) => v + x, _ => 0 }
    println!("{}", x);
    let z = x
    x + p + q
}
"#;
    // The second `let x` shadows the first from the next statement on.
    assert_local("rust", src, "x", 1, 1, &[3]);
    assert_local("rust", src, "x", 3, 1, &[3]);
    assert_local("rust", src, "x", 2, 2, &[4, 5, 6, 7, 10, 11, 12, 13]);
    assert_local("rust", src, "x", 13, 2, &[4, 5, 6, 7, 10, 11, 12, 13]);
    assert_eq!(assert_not_local("rust", src, "x", 8), NotLocalReason::Member);
    assert_eq!(assert_not_local("rust", src, "x", 9), NotLocalReason::Member);
    assert_local("rust", src, "v", 1, 1, &[2]);
    assert_local("rust", src, "b", 2, 1, &[2]);
    assert_local("rust", src, "p", 2, 1, &[2]);
    assert_local("rust", src, "a", 1, 1, &[2, 3]);
    match locals_at("rust", src, "x", 2) {
        Locals::Local { symbol } => {
            assert!(symbol.certain);
            assert_eq!(symbol.kind, LocalKind::Var);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn go() {
    let src = r#"package main
func f(a int) int {
	x := 1
	if y := x + a; y > 0 { x := 2; use(x) }
	g := func(b int) int { return b + x }
	s := obj.x + P{x: x}.x
	for i, v := range xs { use(i, v) }
	var z int = x
	return x
}
"#;
    assert_local("go", src, "x", 1, 1, &[2, 5, 8, 10, 11]);
    assert_local("go", src, "x", 4, 3, &[4]);
    assert_eq!(assert_not_local("go", src, "x", 6), NotLocalReason::Member);
    assert_eq!(assert_not_local("go", src, "x", 7), NotLocalReason::Member, "composite literal key");
    assert_local("go", src, "y", 1, 1, &[2]);
    assert_local("go", src, "b", 2, 1, &[2]);
    assert_local("go", src, "v", 1, 1, &[2]);
    assert_eq!(assert_not_local("go", src, "f", 1), NotLocalReason::TopLevel);
}

#[test]
fn c_and_cpp() {
    let src = r#"int f(int a) {
  int x = 1;
  { int x = 2; use(x); }
  for (int i = 0; i < x; i++) { use(i); }
  int s = obj.x + p->x;
  int z = x
  return x + a;
}
"#;
    for lang in ["c", "cpp"] {
        assert_local(lang, src, "x", 1, 1, &[4, 7, 8]);
        assert_local(lang, src, "x", 3, 2, &[3]);
        assert_eq!(assert_not_local(lang, src, "x", 5), NotLocalReason::Member);
        assert_eq!(assert_not_local(lang, src, "x", 6), NotLocalReason::Member);
        assert_local(lang, src, "i", 1, 1, &[2, 3, 4]);
        assert_local(lang, src, "a", 1, 1, &[2]);
    }
    let cpp = "int f(int a) {\n  int x = 1;\n  auto g = [x](int b) { return b + x; };\n  for (auto& v : xs) { use(v); }\n  return ns::x + x;\n}\n";
    assert_local("cpp", cpp, "b", 1, 1, &[2]);
    assert_local("cpp", cpp, "v", 1, 1, &[2]);
    assert_eq!(assert_not_local("cpp", cpp, "x", 4), NotLocalReason::Member);
}

#[test]
fn csharp() {
    let src = r#"class A {
  int x;
  int F(int a) {
    var x = 1;
    { var y = x + a; }
    Func<int, int> g = b => b + x;
    foreach (var v in xs) { Use(v); }
    var s = obj.x + this.x + Call(x: x);
    var z = x
    return x;
  }
}
"#;
    // `var z = x` lacks its `;`: `return x` ends up inside an ERROR node.
    assert_local("csharp", src, "x", 2, 2, &[3, 4, 8, 9]);
    assert!(matches!(locals_at("csharp", src, "x", 2), Locals::Local { symbol } if !symbol.certain));
    assert_eq!(assert_not_local("csharp", src, "x", 5), NotLocalReason::Member);
    assert_eq!(assert_not_local("csharp", src, "x", 6), NotLocalReason::Member);
    assert_eq!(assert_not_local("csharp", src, "x", 7), NotLocalReason::Member, "named argument");
    assert_local("csharp", src, "b", 1, 1, &[2]);
    assert_local("csharp", src, "v", 1, 1, &[2]);
    assert_local("csharp", src, "a", 1, 1, &[2]);
}

#[test]
fn ruby() {
    let src = r#"class A
  def f(a, *rest, k: 1, &blk)
    x = 1
    [1].each { |y| puts y + x }
    g = ->(b) { b + x }
    s = "x #{x}" + obj.x + obj.x(1)
    x = x + a
    x
  end
end
"#;
    // Assignment declares: both `x =` lines are the same variable.
    assert_local("ruby", src, "x", 1, 1, &[2, 3, 5, 8, 9, 10]);
    assert_eq!(assert_not_local("ruby", src, "x", 6), NotLocalReason::Member);
    assert_local("ruby", src, "y", 1, 1, &[2]);
    assert_local("ruby", src, "b", 1, 1, &[2]);
    assert_local("ruby", src, "a", 1, 1, &[2]);
}

#[test]
fn php() {
    let src = r#"<?php
function f($a) {
  $x = 1;
  $g = function($b) use ($x) { return $b + $x; };
  $h = fn($c) => $c + $x;
  foreach ($xs as $k => $v) { echo $v; }
  $s = "x $x" . $obj->x . $obj->x();
  $x = $x + $a;
  return $x;
}
"#;
    // Names are the `name` inside `$x`; the `use ($x)` clause declares a closure-local copy.
    assert_local("php", src, "x", 1, 1, &[4, 6, 9, 10, 11]);
    assert_local("php", src, "x", 3, 2, &[3]);
    assert_eq!(assert_not_local("php", src, "x", 7), NotLocalReason::Member);
    assert_eq!(assert_not_local("php", src, "x", 8), NotLocalReason::Member);
    assert_local("php", src, "b", 2, 1, &[2]);
    assert_local("php", src, "c", 2, 1, &[2]);
    assert_local("php", src, "v", 1, 1, &[2]);
}

#[test]
fn swift() {
    let src = r#"func f(a: Int, _ b: Int) -> Int {
  let x = 1
  if a > 0 { let x = 2; use(x) }
  let g = { (c: Int) -> Int in c + x }
  let s = "x \(x)" + obj.x
  for v in xs { use(v) }
  return x + b
}
"#;
    assert_local("swift", src, "x", 1, 1, &[4, 6, 8]);
    assert_local("swift", src, "x", 3, 2, &[3]);
    assert_eq!(assert_not_local("swift", src, "x", 7), NotLocalReason::Member);
    assert_local("swift", src, "v", 1, 1, &[2]);
    assert_local("swift", src, "a", 1, 1, &[2]);
}

#[test]
fn scala() {
    let src = r#"object O {
  def f(a: Int): Int = {
    val x = 1
    if (a > 0) { val x = 2; use(x) }
    val g = (b: Int) => b + x
    val s = obj.x
    for (v <- xs) use(v)
    x
  }
}
"#;
    assert_local("scala", src, "x", 1, 1, &[4, 6]);
    assert_local("scala", src, "x", 3, 2, &[3]);
    assert_eq!(assert_not_local("scala", src, "x", 5), NotLocalReason::Member);
    assert_local("scala", src, "b", 1, 1, &[2]);
    assert_local("scala", src, "a", 1, 1, &[2]);
}

#[test]
fn lua() {
    let src = r#"local function f(a, ...)
  local x = 1
  do local x = 2; use(x) end
  local g = function(b) return b + x end
  local s = obj.x .. obj:x() .. "x"
  for i, v in ipairs(t) do use(i, v) end
  local x = x + a
  return x
end
"#;
    // `local x = x + a` reads the outer `x` (#9) and declares a new one (#8).
    assert_local("lua", src, "x", 1, 1, &[4, 9]);
    assert_local("lua", src, "x", 3, 2, &[3]);
    assert_local("lua", src, "x", 10, 8, &[10]);
    assert_eq!(assert_not_local("lua", src, "x", 5), NotLocalReason::Member);
    assert_eq!(assert_not_local("lua", src, "x", 6), NotLocalReason::Member);
    assert_local("lua", src, "b", 1, 1, &[2]);
    assert_local("lua", src, "v", 1, 1, &[2]);
}

#[test]
fn bash() {
    let src = "f() {\n  local x=1\n  y=\"$x and ${x}\"\n  echo $x $y\n  for i in 1 2; do echo $i; done\n}\n";
    assert_local("shellscript", src, "x", 1, 1, &[2, 3, 4]);
    // Loop variables and plain assignments are global in Bash.
    assert_eq!(assert_not_local("shellscript", src, "i", 2), NotLocalReason::Unresolved);
    assert_eq!(assert_not_local("shellscript", src, "y", 2), NotLocalReason::Unresolved);
}

#[test]
fn dart() {
    let src = r#"class A {
  int x = 0;
  int f(int a, {int k = 1}) {
    var x = 1;
    if (a > 0) { var x = 2; use(x); }
    var g = (int b) => b + x;
    var s = obj.x;
    for (var v in xs) { use(v); }
    var z = x
    return x + k;
  }
}
"#;
    // `var z = x` lacks its `;`: the parser swallows `return x + k` into an ERROR token.
    assert_local("dart", src, "x", 2, 2, &[5, 7]);
    assert!(matches!(locals_at("dart", src, "x", 2), Locals::Local { symbol } if !symbol.certain));
    assert_local("dart", src, "x", 4, 3, &[4]);
    assert_eq!(assert_not_local("dart", src, "x", 6), NotLocalReason::Member);
    assert_eq!(assert_not_local("dart", src, "x", 1), NotLocalReason::Member, "field");
    assert_local("dart", src, "b", 1, 1, &[2]);
    assert_local("dart", src, "v", 1, 1, &[2]);
    assert_local("dart", src, "k", 1, 1, &[2]);
}

#[test]
fn elixir() {
    let src = r#"defmodule M do
  def f(a, %{k: v}) do
    x = 1
    g = fn b -> b + x end
    s = obj.x
    Enum.map(xs, fn y -> y + x end)
    x + a + v
  end
end
"#;
    assert_local("elixir", src, "x", 1, 1, &[2, 4, 5]);
    assert_eq!(assert_not_local("elixir", src, "x", 3), NotLocalReason::Member);
    assert_local("elixir", src, "b", 1, 1, &[2]);
    assert_local("elixir", src, "y", 1, 1, &[2]);
    assert_local("elixir", src, "a", 1, 1, &[2]);
}

#[test]
fn zig() {
    let src = r#"fn f(a: u32) u32 {
    const x: u32 = 1;
    {
        const y = x + a;
        _ = y;
    }
    for (xs) |v| { use(v); }
    const s = obj.x + x;
    var z = x
    return x;
}
"#;
    assert_local("zig", src, "x", 1, 1, &[2, 4, 5, 6]);
    assert_eq!(assert_not_local("zig", src, "x", 3), NotLocalReason::Member);
    assert_local("zig", src, "y", 1, 1, &[2]);
    assert_local("zig", src, "v", 1, 1, &[2]);
    assert_local("zig", src, "a", 1, 1, &[2]);
}

#[test]
fn haskell() {
    let src =
        "module M where\nf :: Int -> Int\nf a = let x = 1 in x + a + g a\n  where g b = b + 1\nh c = \\d -> c + d\n";
    assert_local("haskell", src, "x", 1, 1, &[2]);
    assert_local("haskell", src, "a", 1, 1, &[2, 3]);
    assert_local("haskell", src, "d", 1, 1, &[2]);
}
