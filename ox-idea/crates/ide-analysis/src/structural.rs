//! Search / Replace Structurally: a code pattern with placeholders, matched
//! against syntax trees rather than text, so formatting, comments and
//! nesting do not matter.
//!
//! Placeholders: IDEA's `$name$` (one node), ast-grep's `$NAME` (one node),
//! `$$$NAME` (any number of nodes, e.g. arguments) and `$_` (one node, not
//! captured). A placeholder used twice must match equal code. The replacement
//! template refers to the same placeholders.

use crate::lines::Lines;
use crate::{Loader, SourceFile};
use ast_grep_core::matcher::PatternBuilder;
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage};
use ast_grep_core::{Language, Pattern, PatternError};
use ide_core::{CancellationToken, IdeError, IdeResult, IpcPath, LanguageId, Range};
use ide_language::Syntax;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::time::Instant;

/// Matches reported at most (0 in the query).
const DEFAULT_MAX_MATCHES: u32 = 5_000;
/// Matched text sent per match (the editor shows the rest).
const MAX_MATCH_TEXT: usize = 4 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StructuralQuery {
    pub pattern: String,
    /// Only files of this language are searched.
    pub language: LanguageId,
    /// Replacement template; matches then carry their replacement text.
    pub replacement: Option<String>,
    /// Stop after this many matches (0 = 5 000).
    pub max_matches: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StructuralMatch {
    pub range: Range,
    /// The matched code (capped).
    pub text: String,
    /// The first line of the match, for result rows.
    pub line_text: String,
    pub replacement: Option<String>,
}

/// All matches in one file; streamed as files complete.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StructuralFile {
    pub path: IpcPath,
    pub rel_path: String,
    pub matches: Vec<StructuralMatch>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StructuralSummary {
    pub files: u32,
    pub matches: u32,
    pub limit_hit: bool,
    pub cancelled: bool,
    pub elapsed_ms: u32,
}

/// A bundled grammar as an ast-grep language.
#[derive(Clone)]
struct Lang {
    grammar: &'static str,
    ts: TSLanguage,
}

impl Lang {
    fn new(syntax: &Syntax, language: &LanguageId) -> IdeResult<Self> {
        let (grammar, ts) = syntax
            .ts_language(language)
            .ok_or_else(|| IdeError::invalid(format!("no syntax tree support for {language}")))?;
        Ok(Self { grammar, ts })
    }
}

impl Language for Lang {
    fn kind_to_id(&self, kind: &str) -> u16 {
        self.ts.id_for_node_kind(kind, true)
    }

    fn field_to_id(&self, field: &str) -> Option<u16> {
        self.ts.field_id_for_name(field).map(|f| f.get())
    }

    /// Placeholders are spelled with a character the grammar accepts in
    /// identifiers, as ast-grep does for its own languages.
    fn expando_char(&self) -> char {
        match self.grammar {
            "javascript" | "typescript" | "tsx" | "java" | "bash" | "json" | "lua" | "scala" | "yaml" => '$',
            "c" | "cpp" => '\u{10000}',
            "css" | "nix" => '_',
            _ => 'µ',
        }
    }

    fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
        let expando = self.expando_char();
        if expando == '$' {
            return Cow::Borrowed(query);
        }
        // `$A`, `$$$A`, `$_` and `$$$` become expando-spelled; other `$` stay.
        let mut out = String::with_capacity(query.len());
        let mut dollars = 0;
        for c in query.chars() {
            if c == '$' {
                dollars += 1;
                continue;
            }
            let sigil = if c.is_ascii_uppercase() || c == '_' || dollars == 3 { expando } else { '$' };
            out.extend(std::iter::repeat_n(sigil, dollars));
            dollars = 0;
            out.push(c);
        }
        out.extend(std::iter::repeat_n(if dollars == 3 { expando } else { '$' }, dollars));
        Cow::Owned(out)
    }

    fn build_pattern(&self, builder: &PatternBuilder) -> Result<Pattern, PatternError> {
        builder.build(|src| StrDoc::try_new(src, self.clone()))
    }
}

impl LanguageExt for Lang {
    fn get_ts_language(&self) -> TSLanguage {
        self.ts.clone()
    }
}

/// Rewrites IDEA-style `$name$` placeholders to ast-grep's `$NAME`.
pub fn normalize_placeholders(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && chars.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == '_') {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if chars.get(j) == Some(&'$') {
                out.push('$');
                out.extend(chars[i + 1..j].iter().map(char::to_ascii_uppercase));
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn compile(syntax: &Syntax, query: &StructuralQuery) -> IdeResult<(Lang, Pattern)> {
    let lang = Lang::new(syntax, &query.language)?;
    let source = normalize_placeholders(query.pattern.trim());
    if source.is_empty() {
        return Err(IdeError::invalid("the search template is empty"));
    }
    let pattern = Pattern::try_new(&source, lang.clone()).map_err(|e| IdeError::invalid(pattern_error(&e)))?;
    Ok((lang, pattern))
}

fn pattern_error(e: &PatternError) -> String {
    match e {
        PatternError::Parse(_) => "the search template is not valid code in this language".into(),
        PatternError::NoContent(_) => "the search template contains no code".into(),
        PatternError::MultipleNode(_) => {
            "the search template must be a single statement or expression (use $$$ for sequences)".into()
        }
        other => other.to_string(),
    }
}

/// Checks the template while the user types.
pub fn validate_structural(syntax: &Syntax, query: &StructuralQuery) -> IdeResult<()> {
    compile(syntax, query).map(|_| ())
}

/// Matches of `query` in one text.
fn search_text(lang: &Lang, pattern: &Pattern, replacement: Option<&str>, text: &str) -> Vec<StructuralMatch> {
    let root = lang.ast_grep(text);
    let lines = Lines::new(text);
    root.root()
        .find_all(pattern)
        .map(|m| {
            let range = m.range();
            let mut cap = range.end.min(range.start + MAX_MATCH_TEXT);
            while !text.is_char_boundary(cap) {
                cap -= 1;
            }
            let replacement = replacement.map(|r| {
                let edit = m.replace_by(r);
                String::from_utf8_lossy(&edit.inserted_text).into_owned()
            });
            StructuralMatch {
                range: lines.range(range.start, range.end),
                text: text[range.start..cap].to_owned(),
                line_text: lines.line_text(range.start).to_owned(),
                replacement,
            }
        })
        .collect()
}

/// Searches `files` (those of the query's language) in parallel, calling
/// `on_file` for each file with matches.
pub fn search_structurally(
    syntax: &Syntax,
    files: &[SourceFile],
    load: Loader<'_>,
    query: &StructuralQuery,
    cancel: &CancellationToken,
    on_file: &(dyn Fn(StructuralFile) + Sync),
) -> IdeResult<StructuralSummary> {
    let started = Instant::now();
    let (lang, pattern) = compile(syntax, query)?;
    let replacement = query.replacement.as_deref().map(normalize_placeholders);
    let max = if query.max_matches == 0 { DEFAULT_MAX_MATCHES } else { query.max_matches };
    let found = AtomicU32::new(0);
    let file_count = AtomicU32::new(0);
    let limit_hit = AtomicBool::new(false);
    files.par_iter().for_each(|file| {
        if cancel.is_cancelled() || limit_hit.load(Relaxed) {
            return;
        }
        let Some((language, text)) = load(&file.path) else { return };
        if syntax.grammar_name(&language) != Some(lang.grammar) {
            return;
        }
        let mut matches = search_text(&lang, &pattern, replacement.as_deref(), &text);
        if matches.is_empty() {
            return;
        }
        let before = found.fetch_add(matches.len() as u32, Relaxed);
        if before >= max {
            limit_hit.store(true, Relaxed);
            return;
        }
        if before + matches.len() as u32 > max {
            matches.truncate((max - before) as usize);
            limit_hit.store(true, Relaxed);
        }
        file_count.fetch_add(1, Relaxed);
        on_file(StructuralFile { path: IpcPath(file.path.clone()), rel_path: file.rel_path.clone(), matches });
    });
    Ok(StructuralSummary {
        files: file_count.load(Relaxed),
        matches: found.load(Relaxed).min(max),
        limit_hit: limit_hit.load(Relaxed),
        cancelled: cancel.is_cancelled(),
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    fn query(lang: &str, pattern: &str, replacement: Option<&str>) -> StructuralQuery {
        StructuralQuery {
            pattern: pattern.into(),
            language: LanguageId::new(lang),
            replacement: replacement.map(Into::into),
            max_matches: 0,
        }
    }

    fn matches(lang: &str, text: &str, pattern: &str, replacement: Option<&str>) -> Vec<StructuralMatch> {
        let syntax = Syntax::new();
        let (l, p) = compile(&syntax, &query(lang, pattern, replacement)).unwrap();
        let replacement = replacement.map(normalize_placeholders);
        search_text(&l, &p, replacement.as_deref(), text)
    }

    #[test]
    fn idea_placeholders_become_metavariables() {
        assert_eq!(normalize_placeholders("$a$.equals($b$)"), "$A.equals($B)");
        assert_eq!(normalize_placeholders("foo($$$ARGS)"), "foo($$$ARGS)");
        assert_eq!(normalize_placeholders("let $x = 1; \"$5\""), "let $x = 1; \"$5\"");
        assert_eq!(normalize_placeholders("$Expr$ + $_"), "$EXPR + $_");
    }

    #[test]
    fn matches_ignore_formatting_and_respect_repeated_placeholders() {
        let ts = "if (a == a) {}\nif (a ==\n  b) {}\nconst x = f(1, 2, 3);\n";
        let m = matches("typescript", ts, "$x$ == $x$", None);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].text, "a == a");
        assert_eq!(m[0].range.start.line, 0);
        let m = matches("typescript", ts, "$a$ == $b$", None);
        assert_eq!(m.len(), 2);
        assert_eq!(m[1].range.end.line, 2);
        assert_eq!(m[1].line_text, "if (a ==");
        let m = matches("typescript", ts, "f($$$ARGS)", None);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn languages_without_dollar_identifiers_work() {
        let rust = "fn main() {\n    let v = x.unwrap();\n    let w = y.unwrap();\n}\n";
        let m = matches("rust", rust, "$e$.unwrap()", Some("$e$.expect(\"todo\")"));
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].replacement.as_deref(), Some("x.expect(\"todo\")"));
        let py = "print(a)\nprint(b, c)\n";
        assert_eq!(matches("python", py, "print($A)", None).len(), 1);
    }

    #[test]
    fn invalid_templates_and_languages_are_reported() {
        let syntax = Syntax::new();
        assert!(validate_structural(&syntax, &query("typescript", "", None)).is_err());
        assert!(validate_structural(&syntax, &query("no-such-language", "a", None)).is_err());
        assert!(validate_structural(&syntax, &query("typescript", "foo($a$)", None)).is_ok());
    }

    #[test]
    fn searches_files_of_the_language_with_a_limit() {
        let syntax = Syntax::new();
        let files: Vec<SourceFile> = ["a.ts", "b.ts", "c.py"]
            .iter()
            .map(|n| SourceFile { path: PathBuf::from(n), rel_path: (*n).into() })
            .collect();
        let load = |p: &Path| {
            Some(match p.to_str().unwrap() {
                "c.py" => (LanguageId::new("python"), "log(1)\n".to_owned()),
                _ => (LanguageId::new("typescript"), "log(1);\nlog(2);\n".to_owned()),
            })
        };
        let out = Mutex::new(Vec::new());
        let q = StructuralQuery { max_matches: 3, ..query("typescript", "log($v$)", None) };
        let s = search_structurally(&syntax, &files, &load, &q, &CancellationToken::new(), &|f| {
            out.lock().unwrap().push(f)
        })
        .unwrap();
        assert_eq!(s.matches, 3);
        assert!(s.limit_hit);
        let out = out.into_inner().unwrap();
        assert!(out.iter().all(|f| f.rel_path.ends_with(".ts")));
        assert_eq!(out.iter().map(|f| f.matches.len()).sum::<usize>(), 3);
    }
}
