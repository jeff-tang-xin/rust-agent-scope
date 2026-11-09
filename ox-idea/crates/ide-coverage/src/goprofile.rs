//! Go cover profiles (`go test -coverprofile`):
//!
//! ```text
//! mode: set
//! example.com/m/pkg/calc.go:3.24,5.2 1 1
//! ```
//!
//! Each block `file:startLine.startCol,endLine.endCol statements count`
//! covers its lines; a line touched by covered and uncovered blocks is partial.

use crate::Accumulator;
use std::path::{Path, PathBuf};

/// File of an import path: below `module` → `module_dir`, absolute paths as they are.
fn resolve(file: &str, module: &str, module_dir: &Path) -> PathBuf {
    let p = Path::new(file);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    if !module.is_empty() {
        if file == module {
            return module_dir.to_path_buf();
        }
        if let Some(rest) = file.strip_prefix(module).and_then(|r| r.strip_prefix('/')) {
            return module_dir.join(rest);
        }
    }
    module_dir.join(file)
}

pub(crate) fn parse(text: &str, module: &str, module_dir: &Path) -> Accumulator {
    let mut acc = Accumulator::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("mode:") {
            continue;
        }
        // `name.go:3.24,5.2 1 1` — the file name may contain ':' only on Windows drives, so split from the right.
        let mut fields = line.rsplitn(3, ' ');
        let (Some(count), Some(_stmts), Some(loc)) = (fields.next(), fields.next(), fields.next()) else { continue };
        let Ok(count) = count.parse::<u64>() else { continue };
        let Some((file, range)) = loc.rsplit_once(':') else { continue };
        let Some((start, end)) = range.split_once(',') else { continue };
        let line_of = |s: &str| s.split('.').next().and_then(|l| l.parse::<u32>().ok());
        let (Some(first), Some(last)) = (line_of(start), line_of(end)) else { continue };
        if first == 0 || last < first {
            continue;
        }
        let path = resolve(file, module, module_dir);
        for n in first..=last {
            acc.line(&path, n - 1, count);
            acc.branch(&path, n - 1, range.to_owned(), count > 0);
        }
    }
    // Blocks are not branches: keep only the partial marks of lines with mixed blocks.
    acc.drop_unanimous_branches();
    acc
}

impl Accumulator {
    /// Removes the branch data of lines whose branches are all taken or all missed.
    fn drop_unanimous_branches(&mut self) {
        for lines in self.files.values_mut() {
            for acc in lines.values_mut() {
                let taken = acc.branches.values().filter(|t| **t).count();
                if taken == 0 || taken == acc.branches.len() {
                    acc.branches.clear();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_import_paths_and_marks_mixed_lines_partial() {
        let text = "mode: count\n\
                    example.com/m/calc/calc.go:3.24,5.2 1 4\n\
                    example.com/m/calc/calc.go:5.2,7.3 2 0\n\
                    example.com/m/calc/calc.go:9.1,9.20 1 0\n\
                    /abs/other.go:1.1,1.5 1 1\n";
        let suite = parse(text, "example.com/m", Path::new("/proj")).finish("go");
        assert!(!suite.has_branches);
        assert_eq!(suite.files.len(), 2);
        let other = &suite.files[0];
        assert_eq!(other.path.as_ref() as &Path, Path::new("/abs/other.go"));
        let calc = &suite.files[1];
        assert_eq!(calc.path.as_ref() as &Path, Path::new("/proj/calc/calc.go"));
        let at = |n: u32| calc.lines.iter().find(|l| l.line == n).unwrap();
        assert_eq!(at(2).hits, 4);
        assert!(!at(2).partial());
        assert!(at(4).partial(), "line 5 has a covered and an uncovered block");
        assert_eq!(at(5).hits, 0);
        assert_eq!(at(8).hits, 0);
        assert_eq!((calc.lines_total, calc.lines_covered), (6, 3));
    }

    #[test]
    fn skips_malformed_lines() {
        let suite = parse("mode: set\nnot a block\nx.go:a.1,2.1 1 1\n", "", Path::new("/p")).finish("go");
        assert!(suite.files.is_empty());
    }
}
