//! IDEA-style scope patterns for custom (named) scopes:
//! `file:src//*.ts&&!file:*test*`, `file:*.rs||file:docs//*`, `!(file:gen//*)`.
//!
//! A term is `file:<glob>` (the `file:` prefix and a `[module]` qualifier are
//! optional); `//` means "any depth" (`src//*` = `src/**/*`), a trailing `/`
//! means everything below. Globs without a `/` match the file name, others the
//! project-relative path, like [`crate::FileMask`]. Terms combine with `!`,
//! `&&`, `||` (in that precedence) and parentheses.

use globset::{GlobBuilder, GlobMatcher};
use ide_core::{IdeError, IdeResult};

#[derive(Debug)]
pub struct ScopePattern(Node);

#[derive(Debug)]
enum Node {
    Glob { glob: GlobMatcher, by_name: bool },
    Not(Box<Node>),
    And(Vec<Node>),
    Or(Vec<Node>),
}

impl ScopePattern {
    pub fn parse(source: &str) -> IdeResult<Self> {
        let mut p = Parser { s: source, pos: 0 };
        p.skip_ws();
        if p.at_end() {
            return Err(IdeError::invalid("Empty scope pattern"));
        }
        let node = p.or()?;
        p.skip_ws();
        if !p.at_end() {
            return Err(p.error("Unexpected input"));
        }
        Ok(Self(node))
    }

    /// `rel_path` is project-relative and `/`-separated.
    pub fn matches(&self, rel_path: &str) -> bool {
        self.0.matches(rel_path, rel_path.rsplit('/').next().unwrap_or(rel_path))
    }
}

impl Node {
    fn matches(&self, rel: &str, name: &str) -> bool {
        match self {
            Self::Glob { glob, by_name } => glob.is_match(if *by_name { name } else { rel }),
            Self::Not(n) => !n.matches(rel, name),
            Self::And(all) => all.iter().all(|n| n.matches(rel, name)),
            Self::Or(any) => any.iter().any(|n| n.matches(rel, name)),
        }
    }
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.s[self.pos..]
    }

    fn at_end(&self) -> bool {
        self.pos >= self.s.len()
    }

    fn skip_ws(&mut self) {
        let rest = self.rest();
        self.pos += rest.len() - rest.trim_start().len();
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_ws();
        if self.rest().starts_with(token) {
            self.pos += token.len();
            true
        } else {
            false
        }
    }

    fn error(&self, what: &str) -> IdeError {
        IdeError::invalid(format!("{what} at column {} of the scope pattern", self.s[..self.pos].chars().count() + 1))
    }

    fn or(&mut self) -> IdeResult<Node> {
        let mut items = vec![self.and()?];
        while self.eat("||") {
            items.push(self.and()?);
        }
        Ok(if items.len() == 1 { items.remove(0) } else { Node::Or(items) })
    }

    fn and(&mut self) -> IdeResult<Node> {
        let mut items = vec![self.unary()?];
        while self.eat("&&") {
            items.push(self.unary()?);
        }
        Ok(if items.len() == 1 { items.remove(0) } else { Node::And(items) })
    }

    fn unary(&mut self) -> IdeResult<Node> {
        if self.eat("!") {
            return Ok(Node::Not(Box::new(self.unary()?)));
        }
        if self.eat("(") {
            let node = self.or()?;
            if !self.eat(")") {
                return Err(self.error("Missing `)`"));
            }
            return Ok(node);
        }
        self.term()
    }

    fn term(&mut self) -> IdeResult<Node> {
        self.skip_ws();
        let rest = self.rest();
        let end = ["&&", "||", ")"].iter().filter_map(|t| rest.find(t)).min().unwrap_or(rest.len());
        let raw = rest[..end].trim();
        if raw.is_empty() {
            return Err(self.error("Expected a file pattern"));
        }
        let glob = strip_prefix(raw);
        if glob.is_empty() {
            return Err(self.error("Expected a file pattern"));
        }
        let node = glob_node(glob)?;
        self.pos += end;
        Ok(node)
    }
}

/// `file:x`, `file[module]:x` → `x`.
fn strip_prefix(term: &str) -> &str {
    let Some(rest) = term.strip_prefix("file") else { return term };
    let rest = match rest.strip_prefix('[') {
        Some(r) => match r.find(']') {
            Some(i) => &r[i + 1..],
            None => return term,
        },
        None => rest,
    };
    rest.strip_prefix(':').map_or(term, str::trim)
}

fn glob_node(pattern: &str) -> IdeResult<Node> {
    let mut glob = pattern.trim_start_matches("./").replace("//", "/**/");
    if glob.ends_with('/') {
        glob.push_str("**");
    }
    let by_name = !glob.contains('/');
    let glob = if by_name { glob } else { glob.trim_start_matches('/').to_owned() };
    let matcher = GlobBuilder::new(&glob)
        .literal_separator(true)
        .backslash_escape(true)
        .case_insensitive(cfg!(any(target_os = "macos", target_os = "windows")))
        .build()
        .map_err(|e| IdeError::invalid(format!("Invalid file pattern `{pattern}`: {}", e.kind())))?
        .compile_matcher();
    Ok(Node::Glob { glob: matcher, by_name })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ScopePattern {
        ScopePattern::parse(s).unwrap()
    }

    #[test]
    fn idea_recursive_directory_globs() {
        let s = p("file:src//*.ts");
        assert!(s.matches("src/a.ts"));
        assert!(s.matches("src/x/y/a.ts"));
        assert!(!s.matches("lib/a.ts"));
        assert!(!s.matches("src/a.rs"));
        assert!(p("src/").matches("src/deep/a.rs"));
        assert!(!p("file:src/*").matches("src/deep/a.rs"));
    }

    #[test]
    fn names_operators_and_grouping() {
        let s = p("file:src//*&&!file:*test*");
        assert!(s.matches("src/main.rs"));
        assert!(!s.matches("src/main_test.rs"));
        let s = p("*.md || file[app]:docs//*");
        assert!(s.matches("README.md"));
        assert!(s.matches("docs/x/y.txt"));
        assert!(!s.matches("src/a.rs"));
        let s = p("!(file:gen//* || *.lock) && *.*");
        assert!(s.matches("src/a.rs"));
        assert!(!s.matches("gen/a.rs"));
        assert!(!s.matches("Cargo.lock"));
    }

    #[test]
    fn errors() {
        for bad in ["", "  ", "file:", "a &&", "(a", "a)", "file:a["] {
            assert!(ScopePattern::parse(bad).is_err(), "{bad:?}");
        }
    }
}
