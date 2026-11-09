//! The few `pom.xml` settings that move roots: `<build><sourceDirectory>` /
//! `<testSourceDirectory>` and `build-helper-maven-plugin` `add-source` /
//! `add-test-source` executions. A plain text scan, not a full POM model:
//! values with properties other than `${project.basedir}` / `${basedir}` are skipped.

use super::RootKind;

/// `(module-relative path, kind)` of the extra roots declared in `pom`.
pub(super) fn extra_roots(pom: &str) -> Vec<(String, RootKind)> {
    let pom = strip_comments(pom);
    let mut out = Vec::new();
    if let Some(build) = section(&pom, "build") {
        // Only `<build>`'s own children, not plugin configuration.
        let own = plugins_removed(build);
        for (tag, kind) in [("sourceDirectory", RootKind::Source), ("testSourceDirectory", RootKind::Test)] {
            if let Some(dir) = section(&own, tag).and_then(path_value) {
                out.push((dir, kind));
            }
        }
    }
    let mut rest = pom.as_str();
    while let Some(at) = rest.find("build-helper-maven-plugin") {
        let plugin = &rest[at..];
        let end = plugin.find("</plugin>").unwrap_or(plugin.len());
        let body = &plugin[..end];
        for exec in body.split("<execution>").skip(1) {
            let kind = if exec.contains("add-test-source") {
                RootKind::Test
            } else if exec.contains("add-source") {
                RootKind::Source
            } else {
                continue;
            };
            let mut s = exec;
            while let Some(v) = section(s, "source") {
                if let Some(dir) = path_value(v) {
                    out.push((dir, kind));
                }
                let next = s.find("</source>").map_or(s.len(), |i| i + "</source>".len());
                s = &s[next..];
            }
        }
        rest = &plugin[end..];
    }
    out
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = rest[start..].find("-->").map_or("", |end| &rest[start + end + 3..]);
    }
    out.push_str(rest);
    out
}

/// Text between the first `<tag>` and its `</tag>`.
fn section<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{tag}>"))?;
    Some(&text[start..start + end])
}

fn plugins_removed(build: &str) -> String {
    let mut text = build.to_owned();
    for tag in ["plugins", "pluginManagement"] {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        let mut kept = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while let Some(start) = rest.find(&open) {
            kept.push_str(&rest[..start]);
            rest = rest[start..].find(&close).map_or("", |end| &rest[start + end + close.len()..]);
        }
        kept.push_str(rest);
        text = kept;
    }
    text
}

/// A module-relative directory from an element value, or `None` when it uses other properties.
fn path_value(raw: &str) -> Option<String> {
    let mut v = raw.trim();
    for prefix in ["${project.basedir}", "${basedir}"] {
        if let Some(rest) = v.strip_prefix(prefix) {
            v = rest;
        }
    }
    let v = super::normalize(v);
    (!v.is_empty() && !v.contains("${") && !v.contains("..")).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_directories_and_build_helper() {
        let pom = r#"<project>
          <build>
            <sourceDirectory>src/java</sourceDirectory>
            <testSourceDirectory>${project.basedir}/test</testSourceDirectory>
            <!-- <sourceDirectory>old</sourceDirectory> -->
            <plugins>
              <plugin>
                <artifactId>build-helper-maven-plugin</artifactId>
                <executions>
                  <execution><goals><goal>add-source</goal></goals>
                    <configuration><sources><source>gen/main</source><source>${build.dir}/x</source></sources></configuration>
                  </execution>
                  <execution><goals><goal>add-test-source</goal></goals>
                    <configuration><sources><source>gen/test</source></sources></configuration>
                  </execution>
                </executions>
              </plugin>
              <plugin><artifactId>other</artifactId><configuration><sourceDirectory>no</sourceDirectory></configuration></plugin>
            </plugins>
          </build>
        </project>"#;
        assert_eq!(
            extra_roots(pom),
            vec![
                ("src/java".to_owned(), RootKind::Source),
                ("test".to_owned(), RootKind::Test),
                ("gen/main".to_owned(), RootKind::Source),
                ("gen/test".to_owned(), RootKind::Test),
            ]
        );
    }

    #[test]
    fn plain_pom_has_no_extra_roots() {
        assert!(extra_roots("<project><build><plugins/></build></project>").is_empty());
        assert!(extra_roots("").is_empty());
    }
}
