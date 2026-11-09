//! Prints the tree-sitter tree of a file with field names (for writing queries
//! and node-kind tables). Usage: `cargo run -p ide-syntax --example tree_dump -- <language-id> <file>`

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(lang), Some(file)) = (args.next(), args.next()) else {
        eprintln!("usage: tree_dump <language-id> <file>");
        std::process::exit(2);
    };
    let text = std::fs::read_to_string(&file).expect("read file");
    let syntax = ide_language::Syntax::new();
    let lang = ide_core::LanguageId::new(lang);
    let out = syntax.with_parsed(&lang, &text, &|| false, |p| {
        let mut out = String::new();
        dump(p.tree.root_node(), None, 0, p.text, &mut out);
        out
    });
    match out {
        Ok(s) => print!("{s}"),
        Err(e) => eprintln!("{e:?}"),
    }
}

fn dump(node: tree_sitter::Node, field: Option<&str>, depth: usize, text: &str, out: &mut String) {
    use std::fmt::Write;
    if !node.is_named() && !node.is_missing() {
        return;
    }
    let field = field.map(|f| format!("{f}: ")).unwrap_or_default();
    let leaf = if node.named_child_count() == 0 { format!(" {:?}", &text[node.byte_range()]) } else { String::new() };
    let flags = if node.is_missing() {
        " MISSING"
    } else if node.is_error() {
        " ERROR"
    } else {
        ""
    };
    let _ = writeln!(
        out,
        "{:w$}{field}{}{flags} [{}..{}]{leaf}",
        "",
        node.kind(),
        node.start_position().row,
        node.end_position().row,
        w = depth * 2
    );
    let mut cursor = node.walk();
    for (i, child) in node.children(&mut cursor).enumerate() {
        dump(child, node.field_name_for_child(i as u32), depth + 1, text, out);
    }
}
