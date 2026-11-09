//! IDEA-style file masks (`*.kt, *.java, !*Test.kt`).

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ide_core::{IdeError, IdeResult};

/// Masks without a `/` match the file name; masks with one match the path
/// relative to the project root (`src/**/*.ts`).
#[derive(Debug)]
pub struct FileMask {
    include_name: GlobSet,
    include_path: GlobSet,
    exclude_name: GlobSet,
    exclude_path: GlobSet,
    has_include: bool,
}

impl FileMask {
    /// Every entry may hold several comma-separated masks; a leading `!`
    /// moves a mask to the opposite list.
    pub fn new(include: &[String], exclude: &[String]) -> IdeResult<Self> {
        let mut inc = Lists::default();
        let mut exc = Lists::default();
        for (entries, is_include_list) in [(include, true), (exclude, false)] {
            for mask in entries.iter().flat_map(|e| e.split(',')) {
                let mask = mask.trim();
                let (negated, mask) = match mask.strip_prefix('!') {
                    Some(rest) => (true, rest.trim()),
                    None => (false, mask),
                };
                if mask.is_empty() {
                    continue;
                }
                let target = if is_include_list != negated { &mut inc } else { &mut exc };
                target.push(mask)?;
            }
        }
        Ok(Self {
            has_include: inc.count > 0,
            include_name: inc.name.build().map_err(glob_error)?,
            include_path: inc.path.build().map_err(glob_error)?,
            exclude_name: exc.name.build().map_err(glob_error)?,
            exclude_path: exc.path.build().map_err(glob_error)?,
        })
    }

    /// `rel_path` uses `/` separators.
    pub fn matches(&self, rel_path: &str) -> bool {
        let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
        if self.has_include && !self.include_name.is_match(name) && !self.include_path.is_match(rel_path) {
            return false;
        }
        !(self.exclude_name.is_match(name) || self.exclude_path.is_match(rel_path))
    }
}

struct Lists {
    name: GlobSetBuilder,
    path: GlobSetBuilder,
    count: usize,
}

impl Default for Lists {
    fn default() -> Self {
        Self { name: GlobSetBuilder::new(), path: GlobSetBuilder::new(), count: 0 }
    }
}

impl Lists {
    fn push(&mut self, mask: &str) -> IdeResult<()> {
        let mask = mask.trim_start_matches("./");
        let (set, mask) =
            if mask.contains('/') { (&mut self.path, mask.trim_start_matches('/')) } else { (&mut self.name, mask) };
        let glob = GlobBuilder::new(mask)
            .literal_separator(true)
            .backslash_escape(true)
            .case_insensitive(cfg!(any(target_os = "macos", target_os = "windows")))
            .build()
            .map_err(|e| IdeError::invalid(format!("Invalid file mask `{mask}`: {}", e.kind())))?;
        set.add(glob);
        self.count += 1;
        Ok(())
    }
}

fn glob_error(e: globset::Error) -> IdeError {
    IdeError::invalid(format!("Invalid file mask: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(include: &[&str], exclude: &[&str]) -> FileMask {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        FileMask::new(&s(include), &s(exclude)).unwrap()
    }

    #[test]
    fn empty_matches_everything() {
        assert!(mask(&[], &[]).matches("a/b.rs"));
    }

    #[test]
    fn name_masks() {
        let m = mask(&["*.kt, *.java, !*Test.kt"], &[]);
        assert!(m.matches("src/Main.kt"));
        assert!(m.matches("Main.java"));
        assert!(!m.matches("src/MainTest.kt"));
        assert!(!m.matches("src/main.rs"));
    }

    #[test]
    fn path_masks() {
        let m = mask(&["src/**/*.ts"], &["src/gen/**"]);
        assert!(m.matches("src/a.ts"));
        assert!(m.matches("src/x/y/a.ts"));
        assert!(!m.matches("lib/a.ts"));
        assert!(!m.matches("src/gen/a.ts"));
    }

    #[test]
    fn negation_in_exclude_list_includes() {
        let m = mask(&[], &["*.md", "!*.rs"]);
        assert!(m.matches("a.rs"));
        assert!(!m.matches("a.md"));
        assert!(!m.matches("a.txt"));
    }

    #[test]
    fn only_negated_includes() {
        let m = mask(&["!*.lock"], &[]);
        assert!(m.matches("a.rs"));
        assert!(!m.matches("Cargo.lock"));
    }

    #[test]
    fn invalid_glob() {
        let err = FileMask::new(&["a[".into()], &[]).unwrap_err();
        assert_eq!(err.kind, ide_core::error::ErrorKind::InvalidInput);
    }
}
