//! Locate Duplicates: token-based clone detection (CPD-style).
//!
//! 1. Every file is tokenised with its tree-sitter grammar: the leaves of the
//!    tree, comments dropped, literals kept as one token each. Identifiers and
//!    literals can be anonymised so renamed copies still match.
//! 2. A rolling hash over each window of `min_tokens` tokens indexes every
//!    position; windows sharing a hash are verified token by token.
//! 3. Each verified pair that cannot be extended backwards is extended
//!    forwards to its maximal length; equal token runs of equal length form one
//!    group of fragments.

use crate::lines::Lines;
use crate::{Loader, SourceFile};
use ide_core::{CancellationToken, IpcPath, LanguageId, Range};
use ide_language::Syntax;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Instant;

/// Fragments of one group whose text is sent with the report (each capped).
const MAX_FRAGMENT_TEXT: usize = 32 * 1024;
/// Window positions compared per hash bucket (pathologically repetitive code).
const MAX_BUCKET: usize = 64;
/// Groups reported at most.
const MAX_GROUPS: usize = 500;
/// Base of the polynomial rolling hash.
const BASE: u64 = 0x100_0000_01b3;

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateOptions {
    /// Shortest duplicate reported, in tokens.
    pub min_tokens: u32,
    /// Treat all identifiers as equal (finds copies with renamed variables).
    pub anonymize_identifiers: bool,
    /// Treat all literals as equal.
    pub anonymize_literals: bool,
}

impl Default for DuplicateOptions {
    fn default() -> Self {
        Self { min_tokens: 60, anonymize_identifiers: true, anonymize_literals: true }
    }
}

/// One occurrence of a duplicated code fragment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Fragment {
    pub path: IpcPath,
    pub rel_path: String,
    pub range: Range,
    /// The fragment's source, from the start of its first line (capped).
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateGroup {
    pub tokens: u32,
    /// Lines of the first fragment.
    pub lines: u32,
    pub language: LanguageId,
    pub fragments: Vec<Fragment>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DuplicatesReport {
    pub groups: Vec<DuplicateGroup>,
    /// Files tokenised (files without a grammar are skipped).
    pub files: u32,
    pub tokens: u32,
    /// More groups were found than reported.
    pub truncated: bool,
    pub cancelled: bool,
    pub elapsed_ms: u32,
}

#[derive(Clone, Copy, Debug)]
struct Token {
    hash: u64,
    start: u32,
    end: u32,
}

struct Tokenized {
    file: SourceFile,
    language: LanguageId,
    text: String,
    tokens: Vec<Token>,
    /// `prefix[i]` = rolling hash of `tokens[..i]`.
    prefix: Vec<u64>,
}

impl Tokenized {
    fn window(&self, at: usize, len: usize, pow: &Pow) -> u64 {
        self.prefix[at + len].wrapping_sub(self.prefix[at].wrapping_mul(pow.get(len)))
    }
}

/// Powers of [`BASE`], computed lazily up to the longest run needed.
struct Pow(Vec<u64>);

impl Pow {
    fn new(max: usize) -> Self {
        let mut v = Vec::with_capacity(max + 1);
        let mut p = 1u64;
        for _ in 0..=max {
            v.push(p);
            p = p.wrapping_mul(BASE);
        }
        Self(v)
    }
    fn get(&self, n: usize) -> u64 {
        self.0[n]
    }
}

/// Finds duplicated code among `files`. Files of different languages never
/// match each other.
pub fn find_duplicates(
    syntax: &Syntax,
    files: &[SourceFile],
    load: Loader<'_>,
    options: &DuplicateOptions,
    cancel: &CancellationToken,
) -> DuplicatesReport {
    let started = Instant::now();
    let k = options.min_tokens.max(5) as usize;
    let docs: Vec<Tokenized> = files
        .par_iter()
        .filter_map(|file| {
            if cancel.is_cancelled() {
                return None;
            }
            let (language, text) = load(&file.path)?;
            let tokens = tokenize(syntax, &language, &text, options, cancel)?;
            (tokens.len() >= k).then(|| {
                let mut prefix = Vec::with_capacity(tokens.len() + 1);
                let mut h = 0u64;
                prefix.push(h);
                for t in &tokens {
                    h = h.wrapping_mul(BASE).wrapping_add(t.hash);
                    prefix.push(h);
                }
                Tokenized { file: file.clone(), language, text, tokens, prefix }
            })
        })
        .collect();
    let mut report = DuplicatesReport {
        files: docs.len() as u32,
        tokens: docs.iter().map(|d| d.tokens.len() as u32).sum(),
        ..Default::default()
    };
    if cancel.is_cancelled() {
        report.cancelled = true;
        return report;
    }
    let pow = Pow::new(docs.iter().map(|d| d.tokens.len()).max().unwrap_or(0));

    // Window hash -> positions, per language (mixed in so languages never collide).
    let mut index: HashMap<u64, Vec<(u32, u32)>> = HashMap::new();
    for (fi, d) in docs.iter().enumerate() {
        let salt = hash_str(d.language.as_str());
        for at in 0..=d.tokens.len() - k {
            index.entry(d.window(at, k, &pow) ^ salt).or_default().push((fi as u32, at as u32));
        }
        if cancel.is_cancelled() {
            report.cancelled = true;
            return report;
        }
    }

    // (length, content hash) -> occurrences.
    let mut groups: HashMap<(usize, u64), Vec<(u32, u32)>> = HashMap::new();
    for bucket in index.values().filter(|b| b.len() > 1) {
        if cancel.is_cancelled() {
            report.cancelled = true;
            return report;
        }
        let bucket = &bucket[..bucket.len().min(MAX_BUCKET)];
        for (i, &(fa, pa)) in bucket.iter().enumerate() {
            for &(fb, pb) in &bucket[i + 1..] {
                let (a, b) = (&docs[fa as usize], &docs[fb as usize]);
                let (pa, pb) = (pa as usize, pb as usize);
                if a.language != b.language || !same(a, pa, b, pb, k) {
                    continue;
                }
                // Not maximal: the pair also matches one token earlier.
                if pa > 0 && pb > 0 && a.tokens[pa - 1].hash == b.tokens[pb - 1].hash {
                    continue;
                }
                let mut len = k;
                while pa + len < a.tokens.len()
                    && pb + len < b.tokens.len()
                    && a.tokens[pa + len].hash == b.tokens[pb + len].hash
                {
                    len += 1;
                }
                if fa == fb {
                    // Overlapping copies within one file (repetitive code): keep the disjoint part.
                    let (lo, hi) = (pa.min(pb), pa.max(pb));
                    len = len.min(hi - lo);
                    if len < k {
                        continue;
                    }
                }
                let key = (len, a.window(pa, len, &pow));
                let occ = groups.entry(key).or_default();
                for o in [(fa, pa as u32), (fb, pb as u32)] {
                    if !occ.contains(&o) {
                        occ.push(o);
                    }
                }
            }
        }
    }

    // Largest duplication (length × copies) first; ties in source order, so runs are stable.
    let mut ranked: Vec<(usize, Vec<(u32, u32)>)> = groups
        .into_iter()
        .map(|((len, _), mut occ)| {
            occ.sort_unstable();
            (len, occ)
        })
        .collect();
    ranked.sort_by(|a, b| (b.0 * b.1.len()).cmp(&(a.0 * a.1.len())).then_with(|| a.1.cmp(&b.1)));
    report.truncated = ranked.len() > MAX_GROUPS;
    report.groups = ranked
        .into_iter()
        .take(MAX_GROUPS)
        .map(|(len, occ)| {
            let fragments: Vec<Fragment> =
                occ.iter().map(|&(f, p)| fragment(&docs[f as usize], p as usize, len)).collect();
            let lines = fragments.first().map_or(0, |f| f.range.end.line - f.range.start.line + 1);
            DuplicateGroup { tokens: len as u32, lines, language: docs[occ[0].0 as usize].language.clone(), fragments }
        })
        .collect();
    report.elapsed_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
    report
}

fn same(a: &Tokenized, pa: usize, b: &Tokenized, pb: usize, k: usize) -> bool {
    a.tokens[pa..pa + k].iter().zip(&b.tokens[pb..pb + k]).all(|(x, y)| x.hash == y.hash)
}

fn fragment(d: &Tokenized, at: usize, len: usize) -> Fragment {
    let start = d.tokens[at].start as usize;
    let end = d.tokens[at + len - 1].end as usize;
    let lines = Lines::new(&d.text);
    let from = d.text[..start].rfind('\n').map_or(0, |i| i + 1);
    let mut to = end.min(from + MAX_FRAGMENT_TEXT);
    while !d.text.is_char_boundary(to) {
        to -= 1;
    }
    Fragment {
        path: IpcPath(d.file.path.clone()),
        rel_path: d.file.rel_path.clone(),
        range: lines.range(start, end),
        text: d.text[from..to].to_owned(),
    }
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn is_comment(kind: &str) -> bool {
    kind.contains("comment")
}

/// Literal nodes are one token each (their inner structure — quotes, escapes,
/// string fragments — does not matter for duplication).
fn is_literal(kind: &str) -> bool {
    !kind.contains("content")
        && ["string", "char", "number", "integer", "float", "literal", "decimal", "rune"]
            .iter()
            .any(|w| kind.contains(w))
}

fn is_identifier(kind: &str) -> bool {
    kind.contains("identifier") || matches!(kind, "name" | "constant" | "variable_name" | "word")
}

/// The token stream of `text`, or `None` without a grammar.
fn tokenize(
    syntax: &Syntax,
    language: &LanguageId,
    text: &str,
    options: &DuplicateOptions,
    cancel: &CancellationToken,
) -> Option<Vec<Token>> {
    let anon_id = hash_str("\0id");
    let anon_lit = hash_str("\0lit");
    syntax
        .with_parsed(language, text, &|| cancel.is_cancelled(), |parsed| {
            let mut out = Vec::new();
            let mut cursor = parsed.tree.walk();
            'walk: loop {
                let node = cursor.node();
                let kind = node.kind();
                let literal = node.is_named() && is_literal(kind);
                if is_comment(kind) {
                    // Skipped with its children.
                } else if literal || node.child_count() == 0 {
                    let (start, end) = (node.start_byte(), node.end_byte());
                    if end > start {
                        let hash = if literal && options.anonymize_literals {
                            anon_lit
                        } else if !literal && options.anonymize_identifiers && is_identifier(kind) {
                            anon_id
                        } else {
                            let mut h = DefaultHasher::new();
                            kind.hash(&mut h);
                            text[start..end].hash(&mut h);
                            h.finish()
                        };
                        out.push(Token { hash, start: start as u32, end: end as u32 });
                    }
                } else if cursor.goto_first_child() {
                    continue;
                }
                while !cursor.goto_next_sibling() {
                    if !cursor.goto_parent() {
                        break 'walk;
                    }
                }
            }
            out
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    const A: &str = r#"
fn total(items: &[Item]) -> u64 {
    let mut sum = 0;
    for item in items {
        if item.enabled && item.price > 10 {
            sum += item.price * item.count;
        } else {
            sum += 1;
        }
    }
    println!("total {}", sum);
    sum
}
"#;

    /// The same body with other names and literals.
    const B: &str = r#"
// a copy
fn grand_total(entries: &[Entry]) -> u64 {
    let mut acc = 0;
    for e in entries {
        if e.enabled && e.price > 99 {
            acc += e.price * e.count;
        } else {
            acc += 2;
        }
    }
    println!("sum {}", acc);
    acc
}
"#;

    const C: &str = "fn other() { let x = vec![1, 2, 3]; x.len() }\n";

    fn run(files: &[(&str, &str)], options: DuplicateOptions) -> DuplicatesReport {
        let syntax = Syntax::new();
        let sources: Vec<SourceFile> =
            files.iter().map(|(name, _)| SourceFile { path: PathBuf::from(name), rel_path: (*name).into() }).collect();
        let map: HashMap<PathBuf, String> = files.iter().map(|(n, t)| (PathBuf::from(n), (*t).to_owned())).collect();
        let load = |p: &Path| map.get(p).map(|t| (LanguageId::new("rust"), t.clone()));
        find_duplicates(&syntax, &sources, &load, &options, &CancellationToken::new())
    }

    #[test]
    fn finds_renamed_copies_when_anonymised() {
        let r =
            run(&[("a.rs", A), ("b.rs", B), ("c.rs", C)], DuplicateOptions { min_tokens: 20, ..Default::default() });
        assert_eq!(r.files, 3);
        assert_eq!(r.groups.len(), 1, "{:#?}", r.groups);
        let g = &r.groups[0];
        assert_eq!(g.fragments.len(), 2);
        assert_eq!(g.fragments[0].rel_path, "a.rs");
        assert_eq!(g.fragments[1].rel_path, "b.rs");
        // The whole function matches, from `fn` to the closing brace.
        assert_eq!(g.fragments[0].range.start.line, 1);
        assert_eq!(g.fragments[0].range.end.line, 12);
        assert_eq!(g.fragments[1].range.start.line, 2, "the comment is not part of the clone");
        assert!(g.fragments[1].text.starts_with("fn grand_total"));
        assert_eq!(g.lines, 12);
    }

    #[test]
    fn exact_mode_needs_equal_names() {
        let r = run(
            &[("a.rs", A), ("b.rs", B)],
            DuplicateOptions { min_tokens: 20, anonymize_identifiers: false, anonymize_literals: false },
        );
        assert!(r.groups.is_empty(), "{:#?}", r.groups);
        let r = run(
            &[("a.rs", A), ("a2.rs", A)],
            DuplicateOptions { min_tokens: 20, anonymize_identifiers: false, anonymize_literals: false },
        );
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].fragments.len(), 2);
    }

    #[test]
    fn three_copies_form_one_group_and_min_size_applies() {
        let r =
            run(&[("a.rs", A), ("b.rs", B), ("c.rs", A)], DuplicateOptions { min_tokens: 20, ..Default::default() });
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].fragments.len(), 3);
        let r = run(&[("a.rs", A), ("b.rs", B)], DuplicateOptions { min_tokens: 500, ..Default::default() });
        assert!(r.groups.is_empty());
    }

    #[test]
    fn repeats_within_one_file_do_not_overlap() {
        let twice = format!("{A}\n{A}");
        let r = run(&[("a.rs", &twice)], DuplicateOptions { min_tokens: 20, ..Default::default() });
        assert_eq!(r.groups.len(), 1);
        let f = &r.groups[0].fragments;
        assert_eq!(f.len(), 2);
        assert!(f[0].range.end.line < f[1].range.start.line);
    }

    #[test]
    fn cancelled_runs_report_it() {
        let syntax = Syntax::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let files = vec![SourceFile { path: PathBuf::from("a.rs"), rel_path: "a.rs".into() }];
        let load = |_: &Path| Some((LanguageId::new("rust"), A.to_owned()));
        let r = find_duplicates(&syntax, &files, &load, &DuplicateOptions::default(), &cancel);
        assert!(r.cancelled);
        assert!(r.groups.is_empty());
    }
}
