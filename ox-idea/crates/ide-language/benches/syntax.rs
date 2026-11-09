//! `cargo bench -p ide-language`: outline / folding on a 2 MB Rust file,
//! uncached (full parse per call, the old behaviour) vs the per-document tree
//! cache (hit, and incremental re-parse after a keystroke).

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use ide_core::{ChangeSet, DocId, LanguageId, TextChange};
use ide_language::{Syntax, SyntaxCache};
use ide_text::Document;
use std::sync::Arc;

pub fn rust_source(target_bytes: usize) -> String {
    let mut s = String::with_capacity(target_bytes + 1024);
    let mut i = 0usize;
    while s.len() < target_bytes {
        s.push_str(&format!(
            "/// Docs for item {i}.\npub struct Item{i} {{\n    id: u64,\n    name: String,\n}}\n\n\
             impl Item{i} {{\n    pub fn new(id: u64) -> Self {{\n        Self {{ id, name: format!(\"item-{{id}}\") }}\n    }}\n\n\
             \x20   pub fn score(&self, other: &Self) -> u64 {{\n        let mut acc = 0;\n        for k in 0..self.id {{\n            if k % 3 == 0 {{ acc += k * other.id; }}\n        }}\n        acc\n    }}\n}}\n\n"
        ));
        i += 1;
    }
    s
}

fn bench(c: &mut Criterion) {
    let syntax = Arc::new(Syntax::new());
    let rust = LanguageId::new("rust");
    let text = rust_source(2 << 20);

    let mut g = c.benchmark_group("syntax_2mb");
    g.sample_size(10);
    g.bench_function("outline_uncached", |b| b.iter(|| black_box(syntax.outline(&rust, &text).len())));
    g.bench_function("folding_uncached", |b| b.iter(|| black_box(syntax.folding(&rust, &text).len())));

    let cache = SyntaxCache::new(syntax.clone());
    let id = DocId::next();
    cache.open(id, &rust, 0);
    cache.outline(id, &rust, 0, &text);
    g.bench_function("outline_cached_hit", |b| b.iter(|| black_box(cache.outline(id, &rust, 0, &text).len())));
    g.bench_function("folding_cached_hit", |b| b.iter(|| black_box(cache.folding(id, &rust, 0, &text).len())));
    g.bench_function("selection_cached_hit", |b| {
        b.iter(|| black_box(cache.selection_ranges(id, &rust, 0, &text, 1_000_000).len()))
    });

    // Keystroke in the middle of the file, then outline / folding: incremental
    // re-parse plus the walk (results are memoized per version, so a cache hit
    // above is a copy of the previous answer).
    let mut doc = Document::new(&text);
    let mid = doc.pos_to_offset(doc.offset_to_pos(doc.len_utf16() / 2));
    let id2 = DocId::next();
    cache.open(id2, &rust, 0);
    cache.folding(id2, &rust, 0, &text);
    let mut toggle = false;
    let mut keystroke = |doc: &mut Document| {
        // Alternate insert / delete so the document does not grow.
        let change = if toggle {
            TextChange { from: mid, to: mid + 1, insert: String::new() }
        } else {
            TextChange { from: mid, to: mid, insert: "x".into() }
        };
        toggle = !toggle;
        let (_, edits) = doc.apply_detailed(doc.version(), &ChangeSet { changes: vec![change] }).unwrap();
        cache.edit(id2, doc.version(), &edits);
        doc.text()
    };
    g.bench_function("folding_after_keystroke_incremental", |b| {
        b.iter(|| {
            let t = keystroke(&mut doc);
            black_box(cache.folding(id2, &rust, doc.version(), &t).len())
        })
    });
    g.bench_function("outline_after_keystroke_incremental", |b| {
        b.iter(|| {
            let t = keystroke(&mut doc);
            black_box(cache.outline(id2, &rust, doc.version(), &t).len())
        })
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
