//! lcov tracefiles (`geninfo` format), as written by cargo-llvm-cov, istanbul
//! (vitest / jest / c8) and coverage.py:
//!
//! ```text
//! SF:src/lib.rs
//! DA:3,1
//! BRDA:4,0,0,1
//! BRDA:4,0,1,-
//! end_of_record
//! ```

use crate::Accumulator;
use std::path::{Path, PathBuf};

/// Parses `text`; relative `SF:` paths resolve against `base`.
pub(crate) fn parse(text: &str, base: &Path) -> Accumulator {
    let mut acc = Accumulator::default();
    let mut file: Option<PathBuf> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(sf) = line.strip_prefix("SF:") {
            let p = Path::new(sf.trim());
            let path = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
            acc.file(&path);
            file = Some(path);
            continue;
        }
        if line == "end_of_record" {
            file = None;
            continue;
        }
        let Some(f) = file.as_deref() else { continue };
        if let Some(da) = line.strip_prefix("DA:") {
            let mut parts = da.split(',');
            let (Some(n), Some(hits)) = (parts.next(), parts.next()) else { continue };
            let (Ok(n), Ok(hits)) = (n.trim().parse::<u32>(), hits.trim().parse::<f64>()) else { continue };
            if n == 0 {
                continue;
            }
            acc.line(f, n - 1, hits.max(0.0) as u64);
        } else if let Some(brda) = line.strip_prefix("BRDA:") {
            let parts: Vec<&str> = brda.split(',').map(str::trim).collect();
            let [n, block, branch, taken] = parts[..] else { continue };
            let Ok(n) = n.parse::<u32>() else { continue };
            if n == 0 {
                continue;
            }
            let taken = taken != "-" && taken.parse::<f64>().is_ok_and(|t| t > 0.0);
            acc.has_branches = true;
            acc.branch(f, n - 1, format!("{block}:{branch}"), taken);
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "TN:\nSF:/abs/src/lib.rs\nFN:1,add\nFNDA:3,add\nDA:1,3\nDA:2,3\nDA:5,0\nBRDA:2,0,0,3\nBRDA:2,0,1,-\nLF:3\nLH:2\nend_of_record\nSF:src/util.ts\nDA:10,1\nend_of_record\n";

    #[test]
    fn parses_lines_and_branches() {
        let suite = parse(SAMPLE, Path::new("/base")).finish("lcov");
        assert!(suite.has_branches);
        assert_eq!(suite.files.len(), 2);
        let lib = &suite.files[0];
        assert_eq!(lib.path.as_ref() as &Path, Path::new("/abs/src/lib.rs"));
        assert_eq!((lib.lines_total, lib.lines_covered), (3, 2));
        assert_eq!((lib.branches_total, lib.branches_covered), (2, 1));
        assert_eq!(lib.lines[0].line, 0);
        assert!(lib.lines[1].partial());
        assert_eq!(lib.lines[2].hits, 0);
        assert_eq!(suite.files[1].path.as_ref() as &Path, Path::new("/base/src/util.ts"));
    }

    #[test]
    fn merges_repeated_records_and_ignores_garbage() {
        let text = "SF:/a.py\nDA:1,0\nDA:x,1\nDA:0,4\nend_of_record\nDA:9,9\nSF:/a.py\nDA:1,2\nend_of_record\n";
        let suite = parse(text, Path::new("/")).finish("lcov");
        assert!(!suite.has_branches);
        assert_eq!(suite.files.len(), 1);
        assert_eq!(suite.files[0].lines.len(), 1);
        assert_eq!(suite.files[0].lines[0].hits, 2);
    }
}
