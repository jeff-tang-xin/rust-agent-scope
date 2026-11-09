//! Parse + query timings of the structure service on generated sources:
//! a 5k-line Java file and a 20k-line TypeScript file, as open documents
//! (cached tree) — the path editor features take.
//!
//! `cargo run --release -p ide-syntax --example structure_perf`

use ide_core::{CancellationToken, DocId, LanguageId, Pos, Range};
use ide_language::{Syntax, SyntaxCache};
use ide_syntax::{Snapshot, StructureService};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn java_source(lines: usize) -> String {
    let mut s = String::from("package demo;\n\nimport java.util.*;\n\npublic class Big {\n");
    let mut i = 0;
    while s.lines().count() < lines - 1 {
        s.push_str(&format!(
            "  private int field{i} = {i};\n\n  public int method{i}(int a, List<String> items) {{\n    int total = a + field{i};\n    for (String item : items) {{\n      if (item.length() > total) {{\n        total += item.length() * 2;\n      }}\n    }}\n    Runnable r = () -> System.out.println(total + a);\n    return compute(total, a) + items.size();\n  }}\n\n"
        ));
        i += 1;
    }
    s.push_str("}\n");
    s
}

fn ts_source(lines: usize) -> String {
    let mut s = String::from("import { thing } from './thing';\n\n");
    let mut i = 0;
    while s.lines().count() < lines {
        s.push_str(&format!(
            "export function handler{i}(req: Request, opts: Options = {{}}): number {{\n  const total = req.items.length + {i};\n  let acc = 0;\n  for (const item of req.items) {{\n    acc += item.weight * total;\n  }}\n  const f = (x: number) => x + acc;\n  return f(total) + thing(opts);\n}}\n\n"
        ));
        i += 1;
    }
    s
}

fn time<R>(label: &str, runs: u32, mut f: impl FnMut() -> R) -> Duration {
    f(); // warm-up
    let start = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(f());
    }
    let per = start.elapsed() / runs;
    println!("  {label:<34} {:>8.3} ms", per.as_secs_f64() * 1000.0);
    per
}

fn bench(service: &StructureService, cache: &SyntaxCache, lang: &str, text: &str, ident: &str, line: u32) {
    let language = LanguageId::new(lang);
    let doc = DocId::next();
    let none = CancellationToken::new();
    println!("{lang}: {} lines, {} KB", text.lines().count(), text.len() / 1024);
    cache.open(doc, &language, 0);
    let snap = Snapshot::doc(doc, 0, &language, text);
    let start = Instant::now();
    service.syntax_errors(&snap, &none).unwrap();
    println!("  {:<34} {:>8.3} ms", "first query (full parse)", start.elapsed().as_secs_f64() * 1000.0);
    let line_text = text.lines().nth(line as usize).unwrap();
    let col = line_text.find(ident).unwrap() as u32;
    let pos = Pos::new(line, col);
    let range = Range::new(pos, Pos::new(line, col + ident.len() as u32));
    time("snapshot copy (String clone)", 50, || text.to_owned());
    time("node_at", 200, || service.node_at(&snap, range, &none).unwrap());
    time("expression_at", 200, || service.expression_at(&snap, range, &none).unwrap());
    time("syntax_errors", 200, || service.syntax_errors(&snap, &none).unwrap());
    time("locals", 200, || service.locals(&snap, pos, &none).unwrap());
    time("identifiers_named (common name)", 50, || service.identifiers_named(&snap, "total", &none).unwrap());
    time("structural_ranges", 200, || service.structural_ranges(&snap, pos, &none).unwrap());
    let free = Snapshot::text(&language, text);
    time("locals on a closed file (parses)", 5, || service.locals(&free, pos, &none).unwrap());
    cache.close(doc);
}

fn main() {
    let syntax = Arc::new(Syntax::new());
    let cache = Arc::new(SyntaxCache::new(syntax));
    let service = StructureService::new(cache.clone());
    let java = java_source(5_000);
    // A use of `total` at the end of the file.
    let last = |text: &str, needle: &str| {
        text.lines().enumerate().filter(|(_, l)| l.contains(needle)).last().unwrap().0 as u32
    };
    let line = last(&java, "return compute(total");
    bench(&service, &cache, "java", &java, "total", line);
    let ts = ts_source(20_000);
    let line = last(&ts, "return f(total)");
    bench(&service, &cache, "typescript", &ts, "total", line);
}
