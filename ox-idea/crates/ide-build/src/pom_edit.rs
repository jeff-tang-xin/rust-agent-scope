//! Edits of `pom.xml` text for the dependency analyzer: finding where a direct
//! dependency is declared and adding an `<exclusion>` to it, in the style
//! (indentation, line endings, one-line elements) of the surrounding POM.

use crate::maven::{self, MavenModel};
use crate::xml::{self, Element};
use ide_core::{Pos, Range, TextEdit};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::{HashMap, HashSet};

/// `groupId:artifactId` of a dependency.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactKey {
    pub group_id: String,
    pub artifact_id: String,
}

impl ArtifactKey {
    pub fn new(group_id: impl Into<String>, artifact_id: impl Into<String>) -> Self {
        Self { group_id: group_id.into(), artifact_id: artifact_id.into() }
    }
}

impl std::fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.group_id, self.artifact_id)
    }
}

/// What adding an exclusion does to a POM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exclusion {
    /// The edit that adds it.
    Edit(TextEdit),
    /// The dependency already excludes it (or everything, `*:*`).
    Present,
}

/// Properties the coordinates of a POM's dependencies may refer to.
fn properties(project: &Element) -> HashMap<String, String> {
    let mut props: HashMap<String, String> = project
        .child("properties")
        .map(|p| p.children.iter().map(|c| (c.name.clone(), c.text.trim().to_owned())).collect())
        .unwrap_or_default();
    let own_or_parent = |name: &str| project.text_of(name).or_else(|| project.child("parent")?.text_of(name));
    for (key, value) in [
        ("project.groupId", own_or_parent("groupId")),
        ("project.version", own_or_parent("version")),
        ("project.artifactId", project.text_of("artifactId")),
    ] {
        if let Some(v) = value {
            props.insert(key.into(), v);
        }
    }
    props
}

fn matches(e: &Element, key: &ArtifactKey, props: &HashMap<String, String>) -> bool {
    let text = |name: &str| e.text_of(name).map(|t| maven::interpolate(&t, props));
    text("groupId").as_deref() == Some(key.group_id.as_str())
        && text("artifactId").as_deref() == Some(key.artifact_id.as_str())
}

/// The `<dependency>` declaring `key` in the project's `<dependencies>`, else in a
/// profile's (never `<dependencyManagement>`, which declares no dependency).
fn find_dependency<'a>(project: &'a Element, key: &ArtifactKey) -> Option<&'a Element> {
    let props = properties(project);
    let own = project.list("dependencies", "dependency");
    let profiles = project.list("profiles", "profile").into_iter().flat_map(|p| p.list("dependencies", "dependency"));
    own.into_iter().chain(profiles).find(|d| matches(d, key, &props))
}

/// 0-based line / UTF-16 column of byte offset `at`.
fn pos_at(text: &str, at: usize) -> Pos {
    let before = &text[..at];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Pos::new(before.matches('\n').count() as u32, before[line_start..].encode_utf16().count() as u32)
}

fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// The whitespace before `at` on its line, or `None` when other text precedes it there.
fn indent_before(text: &str, at: usize) -> Option<&str> {
    let ws = &text[line_start(text, at)..at];
    ws.chars().all(|c| c == ' ' || c == '\t').then_some(ws)
}

/// One level of indentation: what the first child of `e` adds to `e`'s, else two spaces.
fn indent_unit(text: &str, e: &Element) -> String {
    let outer = indent_before(text, e.start);
    let inner = e.children.first().and_then(|c| indent_before(text, c.start));
    match (outer, inner) {
        (Some(o), Some(i)) if i.len() > o.len() && i.starts_with(o) => i[o.len()..].to_owned(),
        (_, Some(i)) if i.starts_with('\t') => "\t".into(),
        _ => "  ".into(),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

/// Range of the `<artifactId>` of the dependency declaring `key` in `text` (a POM), or of
/// the whole `<dependency>` when it has none; `None` when the POM does not declare it.
pub fn declaration(text: &str, key: &ArtifactKey) -> Option<Range> {
    let project = xml::parse(text)?;
    let dep = find_dependency(&project, key)?;
    let target = dep.child("artifactId").unwrap_or(dep);
    Some(Range::new(pos_at(text, target.start), pos_at(text, target.end)))
}

/// Adds `exclude` to the `<exclusions>` of the dependency declaring `dependency` in `text`
/// (a POM); `None` when the POM does not declare it.
pub fn add_exclusion(text: &str, dependency: &ArtifactKey, exclude: &ArtifactKey) -> Option<Exclusion> {
    let project = xml::parse(text)?;
    let dep = find_dependency(&project, dependency)?;
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let unit = indent_unit(text, dep);
    let (g, a) = (escape(&exclude.group_id), escape(&exclude.artifact_id));

    // Insert `block` (lines relative to `parent`'s indentation + one level) before `parent`'s
    // end tag; on one line (`compact`) when `parent` is not laid out one element per line.
    let insert = |parent: &Element, lines: &[(usize, String)], compact: String| {
        let close_on_own_line = indent_before(text, parent.inner_end).is_some() && parent.inner_end < parent.end;
        let (at, new_text) = match indent_before(text, parent.start) {
            Some(base) if close_on_own_line => {
                let mut out = String::new();
                for (depth, line) in lines {
                    out.push_str(base);
                    out.push_str(&unit.repeat(depth + 1));
                    out.push_str(line);
                    out.push_str(nl);
                }
                (line_start(text, parent.inner_end), out)
            }
            _ => (parent.inner_end, compact),
        };
        let p = pos_at(text, at);
        Exclusion::Edit(TextEdit { range: Range::point(p), new_text })
    };
    let exclusion_lines = |depth: usize| {
        vec![
            (depth, "<exclusion>".to_owned()),
            (depth + 1, format!("<groupId>{g}</groupId>")),
            (depth + 1, format!("<artifactId>{a}</artifactId>")),
            (depth, "</exclusion>".to_owned()),
        ]
    };
    let compact = format!("<exclusion><groupId>{g}</groupId><artifactId>{a}</artifactId></exclusion>");

    if let Some(exclusions) = dep.child("exclusions") {
        let props = properties(&project);
        let wildcard = ArtifactKey::new("*", "*");
        let any_artifact = ArtifactKey::new(exclude.group_id.clone(), "*");
        if exclusions
            .children_named("exclusion")
            .any(|e| [exclude, &wildcard, &any_artifact].iter().any(|k| matches(e, k, &props)))
        {
            return Some(Exclusion::Present);
        }
        return Some(insert(exclusions, &exclusion_lines(0), compact));
    }
    let mut lines = vec![(0, "<exclusions>".to_owned())];
    lines.extend(exclusion_lines(1));
    lines.push((0, "</exclusions>".to_owned()));
    Some(insert(dep, &lines, format!("<exclusions>{compact}</exclusions>")))
}

/// The POMs that may declare a dependency of the module `pom` (project-relative), nearest
/// first: the module's own, then its parents that are part of the project.
pub fn declaring_poms(model: &MavenModel, pom: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = model.projects.iter().find(|p| p.pom == pom);
    while let Some(p) = current {
        if !seen.insert(p.pom.clone()) {
            break;
        }
        out.push(p.pom.clone());
        current = p.parent.as_deref().and_then(|coords| {
            model.projects.iter().find(|q| format!("{}:{}:{}", q.group_id, q.artifact_id, q.version) == coords)
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, edit: &TextEdit) -> String {
        let offset = |p: Pos| {
            let start: usize = text.split_inclusive('\n').take(p.line as usize).map(str::len).sum();
            let line = &text[start..];
            start + line.char_indices().nth(p.col as usize).map_or(line.len(), |(i, _)| i)
        };
        let (from, to) = (offset(edit.range.start), offset(edit.range.end));
        format!("{}{}{}", &text[..from], edit.new_text, &text[to..])
    }

    fn exclude(text: &str, dep: (&str, &str), excl: (&str, &str)) -> Option<String> {
        match add_exclusion(text, &ArtifactKey::new(dep.0, dep.1), &ArtifactKey::new(excl.0, excl.1))? {
            Exclusion::Edit(e) => Some(apply(text, &e)),
            Exclusion::Present => Some(text.to_owned()),
        }
    }

    const POM: &str = "<project>
    <groupId>demo</groupId>
    <properties>
        <guice.group>com.google.inject</guice.group>
    </properties>
    <dependencyManagement>
        <dependencies>
            <dependency>
                <groupId>com.google.inject</groupId>
                <artifactId>guice</artifactId>
                <version>5.1.0</version>
            </dependency>
        </dependencies>
    </dependencyManagement>
    <dependencies>
        <dependency>
            <groupId>${guice.group}</groupId>
            <artifactId>guice</artifactId>
        </dependency>
    </dependencies>
</project>
";

    #[test]
    fn adds_exclusions_to_the_declaration_not_the_managed_entry() {
        let out = exclude(POM, ("com.google.inject", "guice"), ("com.google.guava", "guava")).unwrap();
        let expected = POM.replace(
            "            <artifactId>guice</artifactId>\n        </dependency>\n    </dependencies>",
            "            <artifactId>guice</artifactId>
            <exclusions>
                <exclusion>
                    <groupId>com.google.guava</groupId>
                    <artifactId>guava</artifactId>
                </exclusion>
            </exclusions>
        </dependency>
    </dependencies>",
        );
        assert_eq!(out, expected);
        // A second exclusion joins the first; excluding it again changes nothing.
        let twice = exclude(&out, ("com.google.inject", "guice"), ("aopalliance", "aopalliance")).unwrap();
        assert!(twice.contains(
            "                </exclusion>
                <exclusion>
                    <groupId>aopalliance</groupId>
                    <artifactId>aopalliance</artifactId>
                </exclusion>
            </exclusions>"
        ));
        assert_eq!(exclude(&twice, ("com.google.inject", "guice"), ("aopalliance", "aopalliance")).unwrap(), twice);
    }

    #[test]
    fn keeps_tabs_crlf_and_one_line_dependencies() {
        let tabs = "<project>\r\n\t<dependencies>\r\n\t\t<dependency>\r\n\t\t\t<groupId>g</groupId>\r\n\t\t\t<artifactId>a</artifactId>\r\n\t\t</dependency>\r\n\t</dependencies>\r\n</project>\r\n";
        let out = exclude(tabs, ("g", "a"), ("x", "y")).unwrap();
        assert!(out.contains("<artifactId>a</artifactId>\r\n\t\t\t<exclusions>\r\n\t\t\t\t<exclusion>\r\n\t\t\t\t\t<groupId>x</groupId>\r\n"));
        assert!(out.contains("\t\t\t</exclusions>\r\n\t\t</dependency>"));

        let inline = "<project><dependencies><dependency><groupId>g</groupId><artifactId>a</artifactId></dependency></dependencies></project>";
        assert_eq!(
            exclude(inline, ("g", "a"), ("x", "y")).unwrap(),
            "<project><dependencies><dependency><groupId>g</groupId><artifactId>a</artifactId><exclusions><exclusion><groupId>x</groupId><artifactId>y</artifactId></exclusion></exclusions></dependency></dependencies></project>"
        );
    }

    #[test]
    fn wildcards_profiles_and_missing_declarations() {
        let pom = "<project>
  <profiles>
    <profile>
      <dependencies>
        <dependency>
          <groupId>g</groupId><artifactId>a</artifactId>
          <exclusions><exclusion><groupId>x</groupId><artifactId>*</artifactId></exclusion></exclusions>
        </dependency>
      </dependencies>
    </profile>
  </profiles>
</project>";
        let key = ArtifactKey::new("g", "a");
        assert_eq!(add_exclusion(pom, &key, &ArtifactKey::new("x", "y")), Some(Exclusion::Present));
        let out = exclude(pom, ("g", "a"), ("z", "w")).unwrap();
        assert!(out.contains(
            "</exclusion><exclusion><groupId>z</groupId><artifactId>w</artifactId></exclusion></exclusions>"
        ));
        assert_eq!(add_exclusion(pom, &ArtifactKey::new("g", "other"), &key), None);
        assert_eq!(add_exclusion("not xml", &key, &key), None);
    }

    #[test]
    fn declaration_ranges_use_utf16_columns() {
        let pom = "<project><!-- é😀 --><dependencies>\n  <dependency><groupId>g</groupId><artifactId>a</artifactId></dependency>\n</dependencies></project>";
        let r = declaration(pom, &ArtifactKey::new("g", "a")).unwrap();
        assert_eq!(r, Range::new(Pos::new(1, 34), Pos::new(1, 60)));
        let first = "<project><!-- é😀 --><dependencies><dependency><groupId>g</groupId><artifactId>a</artifactId>";
        let r = declaration(&format!("{first}</dependency></dependencies></project>"), &ArtifactKey::new("g", "a"))
            .unwrap();
        let start = first.find("<artifactId>").unwrap();
        assert_eq!(r.start.col as usize, first[..start].encode_utf16().count());
    }

    #[test]
    fn declaring_poms_follow_parents_inside_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let write = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        write(
            "pom.xml",
            "<project><groupId>d</groupId><artifactId>parent</artifactId><version>1</version><packaging>pom</packaging><modules><module>app</module></modules></project>",
        );
        write(
            "app/pom.xml",
            "<project><parent><groupId>d</groupId><artifactId>parent</artifactId><version>1</version></parent><artifactId>app</artifactId></project>",
        );
        let model = maven::load_with_repo(root, None);
        assert_eq!(declaring_poms(&model, "app/pom.xml"), ["app/pom.xml", "pom.xml"]);
        assert_eq!(declaring_poms(&model, "pom.xml"), ["pom.xml"]);
        assert!(declaring_poms(&model, "missing/pom.xml").is_empty());
    }
}
