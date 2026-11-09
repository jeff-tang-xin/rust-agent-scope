//! JaCoCo XML reports:
//!
//! ```xml
//! <report name="app">
//!   <package name="com/example">
//!     <sourcefile name="Calc.java">
//!       <line nr="5" mi="0" ci="3" mb="1" cb="1"/>
//! ```
//!
//! Source files are found below the report's module in the conventional
//! source roots (`src/<set>/{java,kotlin,groovy,scala}`), falling back to
//! `src/<set>/java`.

use crate::Accumulator;
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::path::{Path, PathBuf};

const LANGS: &[&str] = &["java", "kotlin", "groovy", "scala"];
const MAX_DEPTH: usize = 6;

/// The module a report belongs to: `<module>/target/site/jacoco/jacoco.xml` (Maven) or
/// `<module>/build/reports/jacoco/<task>/<task>.xml` (Gradle).
pub(crate) fn module_dir(report: &Path) -> PathBuf {
    for dir in report.ancestors().skip(1) {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if (name == "target" || name == "build")
            && let Some(parent) = dir.parent()
        {
            return parent.to_path_buf();
        }
    }
    report.parent().unwrap_or(report).to_path_buf()
}

/// JaCoCo XML reports below `dir` (skipping VCS / dependency directories).
pub(crate) fn find_reports(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, 0, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if name.starts_with('.') || name == "node_modules" || name == "src" {
                continue;
            }
            // `…/target/site/jacoco/` and `…/build/reports/jacoco/<task>/`.
            if name == "jacoco"
                && path.parent().is_some_and(|p| p.ends_with("target/site") || p.ends_with("build/reports"))
            {
                collect_xml(&path, 0, out);
                continue;
            }
            if depth < MAX_DEPTH {
                walk(&path, depth + 1, out);
            }
        }
    }
}

fn collect_xml(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() && depth < 1 {
            collect_xml(&path, depth + 1, out);
        } else if path.extension().is_some_and(|x| x == "xml") {
            out.push(path);
        }
    }
}

fn attr(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes().flatten().find(|a| a.key.as_ref() == name).map(|a| a.value.into_owned())
}

fn num(e: &BytesStart, name: &str) -> u32 {
    attr(e, name).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Source file of `package/name` below `module`.
fn locate(module: &Path, package: &str, name: &str) -> PathBuf {
    let rel = if package.is_empty() { PathBuf::from(name) } else { Path::new(package).join(name) };
    let src = module.join("src");
    if let Ok(sets) = std::fs::read_dir(&src) {
        let mut sets: Vec<PathBuf> = sets.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        // `main` first, so a class compiled from main sources does not resolve into tests.
        sets.sort_by_key(|p| (!p.ends_with("main"), p.clone()));
        for set in sets {
            for lang in LANGS {
                let candidate = set.join(lang).join(&rel);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    src.join("main").join("java").join(rel)
}

/// Parses one report of the module at `module` into `acc`.
pub(crate) fn parse_into(xml: &str, module: &Path, acc: &mut Accumulator) -> Result<(), quick_xml::Error> {
    let mut reader = Reader::from_str(xml);
    let mut package = String::new();
    let mut file: Option<PathBuf> = None;
    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e) => match e.name().as_ref() {
                "package" => package = attr(&e, "name").unwrap_or_default(),
                "sourcefile" => {
                    let name = attr(&e, "name").unwrap_or_default();
                    let path = locate(module, &package, &name);
                    acc.file(&path);
                    file = Some(path);
                }
                "line" => {
                    let Some(f) = file.as_deref() else { continue };
                    let nr = num(&e, "nr");
                    if nr == 0 {
                        continue;
                    }
                    let (ci, mb, cb) = (num(&e, "ci"), num(&e, "mb"), num(&e, "cb"));
                    acc.line(f, nr - 1, u64::from(ci));
                    for i in 0..mb + cb {
                        acc.branch(f, nr - 1, i.to_string(), i < cb);
                    }
                }
                _ => {}
            },
            Event::End(e) => match e.name().as_ref() {
                "sourcefile" => file = None,
                "package" => package.clear(),
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
/// Parses a single report whose sources live below `module`.
pub(crate) fn parse(xml: &str, module: &Path) -> Result<crate::CoverageSuite, String> {
    let mut acc = Accumulator { has_branches: true, ..Default::default() };
    parse_into(xml, module, &mut acc).map_err(|e| e.to_string())?;
    Ok(acc.finish("JaCoCo"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><!DOCTYPE report PUBLIC "-//JACOCO//DTD Report 1.1//EN" "report.dtd"><report name="app"><sessioninfo id="x" start="1" dump="2"/><package name="com/example"><class name="com/example/Calc" sourcefilename="Calc.java"><method name="add" desc="(II)I" line="5"><counter type="LINE" missed="0" covered="1"/></method></class><sourcefile name="Calc.java"><line nr="5" mi="0" ci="3" mb="0" cb="0"/><line nr="6" mi="2" ci="2" mb="1" cb="1"/><line nr="9" mi="4" ci="0" mb="0" cb="0"/><counter type="LINE" missed="1" covered="2"/></sourcefile><sourcefile name="Util.kt"><line nr="1" mi="0" ci="1" mb="0" cb="0"/></sourcefile></package></report>"#;

    #[test]
    fn parses_lines_and_branches_and_locates_sources() {
        let dir = tempfile::tempdir().unwrap();
        let kt = dir.path().join("src/main/kotlin/com/example");
        std::fs::create_dir_all(&kt).unwrap();
        std::fs::write(kt.join("Util.kt"), "").unwrap();
        let suite = parse(REPORT, dir.path()).unwrap();
        assert!(suite.has_branches);
        assert_eq!(suite.files.len(), 2);
        let calc = &suite.files[0];
        assert_eq!(calc.path.as_ref() as &Path, dir.path().join("src/main/java/com/example/Calc.java"));
        assert_eq!((calc.lines_total, calc.lines_covered), (3, 2));
        assert_eq!((calc.branches_total, calc.branches_covered), (2, 1));
        assert!(calc.lines[1].partial());
        assert_eq!(calc.lines[2].hits, 0);
        assert_eq!(suite.files[1].path.as_ref() as &Path, kt.join("Util.kt"));
    }

    #[test]
    fn finds_maven_and_gradle_reports() {
        let dir = tempfile::tempdir().unwrap();
        let maven = dir.path().join("core/target/site/jacoco");
        let gradle = dir.path().join("app/build/reports/jacoco/test");
        std::fs::create_dir_all(&maven).unwrap();
        std::fs::create_dir_all(&gradle).unwrap();
        std::fs::write(maven.join("jacoco.xml"), REPORT).unwrap();
        std::fs::write(gradle.join("jacocoTestReport.xml"), REPORT).unwrap();
        std::fs::write(maven.join("index.html"), "").unwrap();
        let found = find_reports(dir.path());
        assert_eq!(found.len(), 2);
        assert_eq!(module_dir(&found[0]), dir.path().join("app"));
        assert_eq!(module_dir(&found[1]), dir.path().join("core"));
    }

    #[test]
    fn rejects_broken_xml() {
        assert!(parse("<report><package name=\"a\"></sourcefile></report>", Path::new("/m")).is_err());
        let suite = parse("<report></report>", Path::new("/m")).unwrap();
        assert!(suite.files.is_empty());
    }
}
