//! `workspace.fileOperations` filters (LSP 3.16): which renames a server wants
//! to hear about.

use globset::{Glob, GlobBuilder, GlobMatcher};
use serde_json::Value;
use std::path::Path;

/// One `FileOperationFilter`.
pub struct FileFilter {
    matcher: GlobMatcher,
    /// `Some(true)`: folders only; `Some(false)`: files only.
    folders: Option<bool>,
}

impl FileFilter {
    fn parse(v: &Value) -> Option<Self> {
        if v.get("scheme").and_then(Value::as_str).is_some_and(|s| s != "file") {
            return None;
        }
        let pattern = v.get("pattern")?;
        let glob = pattern.get("glob")?.as_str()?;
        let ignore_case = pattern.pointer("/options/ignoreCase").and_then(Value::as_bool).unwrap_or(false);
        let built = GlobBuilder::new(glob)
            .literal_separator(true)
            .case_insensitive(ignore_case)
            .build()
            .or_else(|_| Glob::new(glob))
            .ok()?;
        let folders = match pattern.get("matches").and_then(Value::as_str) {
            Some("folder") => Some(true),
            Some("file") => Some(false),
            _ => None,
        };
        Some(Self { matcher: built.compile_matcher(), folders })
    }

    pub fn matches(&self, path: &Path, is_dir: bool) -> bool {
        if self.folders.is_some_and(|f| f != is_dir) {
            return false;
        }
        let text = path.to_string_lossy().replace('\\', "/");
        self.matcher.is_match(&text)
            || text.rsplit('/').next().is_some_and(|name| !name.is_empty() && self.matcher.is_match(name))
    }
}

/// Filters of `registration` (`{ filters: [...] }`); `None` when the server does not register the operation.
pub fn parse_filters(registration: Option<&Value>) -> Option<Vec<FileFilter>> {
    let filters = registration?.get("filters")?.as_array()?;
    Some(filters.iter().filter_map(FileFilter::parse).collect())
}

pub fn any_match(filters: &[FileFilter], path: &Path, is_dir: bool) -> bool {
    filters.iter().any(|f| f.matches(path, is_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typescript_and_rust_analyzer_filters() {
        let ts = parse_filters(Some(&json!({ "filters": [
            { "scheme": "file", "pattern": { "glob": "**/*.{ts,js,jsx,tsx,mjs,mts,cjs,cts}", "matches": "file" } },
            { "scheme": "file", "pattern": { "glob": "**", "matches": "folder" } }
        ]})))
        .unwrap();
        assert!(any_match(&ts, Path::new("/p/src/a.ts"), false));
        assert!(!any_match(&ts, Path::new("/p/src/a.py"), false));
        assert!(any_match(&ts, Path::new("/p/src/util"), true));
        assert!(!any_match(&ts, Path::new("/p/src/util.py"), false));

        let ra = parse_filters(Some(&json!({ "filters": [
            { "scheme": "file", "pattern": { "glob": "**/*.rs", "matches": "file" } },
            { "scheme": "untitled", "pattern": { "glob": "**" } }
        ]})))
        .unwrap();
        assert_eq!(ra.len(), 1);
        assert!(any_match(&ra, Path::new("/p/src/lib.rs"), false));

        let ci = parse_filters(Some(&json!({ "filters": [
            { "pattern": { "glob": "**/*.JAVA", "options": { "ignoreCase": true } } }
        ]})))
        .unwrap();
        assert!(any_match(&ci, Path::new("/p/Foo.java"), false));
        assert!(parse_filters(None).is_none());
    }
}
