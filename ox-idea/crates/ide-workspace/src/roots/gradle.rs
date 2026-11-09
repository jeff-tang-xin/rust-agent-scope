//! Obvious `sourceSets` `srcDir(s)` declarations in `build.gradle(.kts)`:
//! `sourceSets.main.java.srcDirs = ['gen']`, `sourceSets { test { kotlin.srcDir("x") } }`,
//! `named("integrationTest") { java { srcDirs("it") } }`, `val commonMain by getting { … }`.
//! Only string literals count; anything computed is ignored.

use super::RootKind;

/// `test`, `integrationTest`, `androidTest`, `commonTest`, … are test source sets.
pub(super) fn is_test_set(name: &str) -> bool {
    name == "test" || name.ends_with("Test") || name.ends_with("Tests")
}

#[derive(Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Punct(char),
}

fn tokenize(text: &str) -> Vec<Tok> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == '"' || c == '\'' {
            let mut s = String::new();
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' {
                    i += 1;
                }
                if let Some(&ch) = chars.get(i) {
                    s.push(ch);
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(s));
        } else if c.is_alphanumeric() || c == '_' {
            let mut s = String::new();
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                s.push(chars[i]);
                i += 1;
            }
            out.push(Tok::Ident(s));
        } else {
            if !c.is_whitespace() {
                out.push(Tok::Punct(c));
            }
            i += 1;
        }
    }
    out
}

fn ident(t: Option<&Tok>) -> Option<&str> {
    match t {
        Some(Tok::Ident(s)) => Some(s),
        _ => None,
    }
}

/// Name of the block opened by the `{` at `at`.
fn block_name(toks: &[Tok], at: usize) -> String {
    let prev = at.checked_sub(1).map(|i| &toks[i]);
    match prev {
        // `named("x") {`, `getByName("x") {`, `create("x") {`
        Some(Tok::Punct(')')) => {
            let mut depth = 0;
            let mut j = at - 1;
            loop {
                match &toks[j] {
                    Tok::Punct(')') => depth += 1,
                    Tok::Punct('(') => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                if j == 0 {
                    return String::new();
                }
                j -= 1;
            }
            match (j.checked_sub(1).and_then(|k| ident(toks.get(k))), toks.get(j + 1)) {
                (Some("named" | "getByName" | "create" | "register" | "maybeCreate"), Some(Tok::Str(s))) => s.clone(),
                (Some(f), _) => f.to_owned(),
                _ => String::new(),
            }
        }
        // `val commonMain by getting {`
        Some(Tok::Ident(s))
            if (s == "getting" || s == "creating") && ident(toks.get(at.wrapping_sub(2))) == Some("by") =>
        {
            ident(toks.get(at.wrapping_sub(3))).unwrap_or_default().to_owned()
        }
        Some(Tok::Ident(s)) => s.clone(),
        _ => String::new(),
    }
}

const LANGS: &[&str] = &["java", "kotlin", "groovy", "scala", "resources"];

/// `(module-relative path, kind)` of the source set directories declared in `script`.
pub(super) fn extra_roots(script: &str) -> Vec<(String, RootKind)> {
    let toks = tokenize(script);
    let mut stack: Vec<String> = Vec::new();
    let mut out: Vec<(String, RootKind)> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Punct('{') => stack.push(block_name(&toks, i)),
            Tok::Punct('}') => {
                stack.pop();
            }
            Tok::Ident(name) if matches!(name.as_str(), "srcDir" | "srcDirs" | "setSrcDirs") => {
                // Dotted chain before the call: `sourceSets.main.java.srcDirs`.
                let mut chain: Vec<&str> = Vec::new();
                let mut j = i;
                while j >= 2 && toks[j - 1] == Tok::Punct('.') {
                    match ident(toks.get(j - 2)) {
                        Some(id) => chain.insert(0, id),
                        None => break,
                    }
                    j -= 2;
                }
                let path: Vec<&str> = stack.iter().map(String::as_str).chain(chain).collect();
                if !path.contains(&"sourceSets") {
                    continue;
                }
                let Some(at) = path.iter().rposition(|p| LANGS.contains(p)) else { continue };
                let Some(&set) = at.checked_sub(1).and_then(|k| path.get(k)) else { continue };
                if set == "sourceSets" {
                    continue;
                }
                let resources = path[at] == "resources";
                let kind = match (resources, is_test_set(set)) {
                    (true, true) => RootKind::TestResources,
                    (true, false) => RootKind::Resources,
                    (false, true) => RootKind::Test,
                    (false, false) => RootKind::Source,
                };
                // String arguments up to the end of the call / assignment.
                for t in &toks[i + 1..] {
                    match t {
                        Tok::Str(s) => {
                            let p = super::normalize(s);
                            if !p.is_empty() && !p.contains('$') && !p.contains("..") {
                                out.push((p, kind));
                            }
                        }
                        Tok::Punct('(' | ')' | '[' | ']' | ',' | '=' | '+') => {}
                        Tok::Ident(f) if matches!(f.as_str(), "listOf" | "setOf" | "files" | "file") => {}
                        _ => break,
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groovy_and_kotlin_dsl_src_dirs() {
        let groovy = r#"
            sourceSets {
                main {
                    java { srcDirs = ['src/gen', "src/extra"] }
                    resources.srcDir 'conf'
                }
                integrationTest { java.srcDir(file('it/java')) }
            }
            sourceSets.test.java.srcDirs += 'test-gen'
            // sourceSets.main.java.srcDirs = ['commented']
            task foo { srcDir 'not-a-source-set' }
        "#;
        assert_eq!(
            extra_roots(groovy),
            vec![
                ("src/gen".to_owned(), RootKind::Source),
                ("src/extra".to_owned(), RootKind::Source),
                ("conf".to_owned(), RootKind::Resources),
                ("it/java".to_owned(), RootKind::Test),
                ("test-gen".to_owned(), RootKind::Test),
            ]
        );
        let kts = r#"
            sourceSets {
                named("main") { kotlin.srcDirs("gen/kotlin", "$buildDir/x") }
                getByName("test").resources.srcDir("fixtures")
            }
            kotlin { sourceSets { val commonTest by getting { kotlin.srcDir("shared-test") } } }
        "#;
        assert_eq!(
            extra_roots(kts),
            vec![("gen/kotlin".to_owned(), RootKind::Source), ("shared-test".to_owned(), RootKind::Test)]
        );
    }

    #[test]
    fn test_set_names() {
        assert!(is_test_set("test") && is_test_set("integrationTest") && is_test_set("androidTest"));
        assert!(!is_test_set("main") && !is_test_set("testFixtures") && !is_test_set("contest"));
    }
}
