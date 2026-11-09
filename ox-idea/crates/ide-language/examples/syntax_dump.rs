//! Prints the tree-sitter tree, outline, tags and folds of a file.
//! Usage: `cargo run -p ide-language --example syntax_dump -- <language-id> <file>`

use ide_core::LanguageId;
use ide_language::Syntax;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(lang), Some(file)) = (args.next(), args.next()) else {
        eprintln!("usage: syntax_dump <language-id> <file>");
        std::process::exit(2);
    };
    let text = std::fs::read_to_string(&file).expect("read file");
    let syntax = Syntax::new();
    let lang = LanguageId::new(lang);
    let Some(tree) = syntax.debug_tree(&lang, &text) else {
        eprintln!("no grammar for {lang}");
        std::process::exit(1);
    };
    if std::env::var_os("DUMP_TREE").is_some() {
        println!("{tree}\n");
    }
    fn print(items: &[ide_core::OutlineItem], depth: usize) {
        for i in items {
            let d = i.detail.as_deref().unwrap_or("");
            println!(
                "{:indent$}{:?} {} {d} [{}:{}-{}:{}]",
                "",
                i.kind,
                i.name,
                i.range.start.line,
                i.range.start.col,
                i.range.end.line,
                i.range.end.col,
                indent = depth * 2
            );
            print(&i.children, depth + 1);
        }
    }
    print(&syntax.outline(&lang, &text), 0);
    for item in syntax.tags(&lang, &text) {
        println!(
            "tag {:?} {} in {:?} @{}:{}",
            item.kind, item.name, item.container, item.range.start.line, item.range.start.col
        );
    }
    for f in syntax.folding(&lang, &text) {
        println!("fold {}..{} {:?}", f.start_line, f.end_line, f.kind);
    }
}
