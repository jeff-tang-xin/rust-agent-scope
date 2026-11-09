//! Parser for IntelliJ-style `.http` / `.rest` request files.
//!
//! ```text
//! @host = example.com          # file variable
//! ### Get user                 # separator; the rest of the line names the request
//! # @name getUser              # explicit name
//! GET https://{{host}}/users/1
//! Accept: application/json
//!
//! { "body": true }
//! < ./payload.json             # body include
//! ```
//!
//! Response handlers (`> {% … %}`, `> file.js`), response redirects (`>> file`)
//! and references to previous responses (`<> file`) are recognised and skipped.

use serde::{Deserialize, Serialize};
use specta::Type;

pub const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE", "CONNECT"];

/// One request of a file, as shown by the gutter and the Run All list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequestBlock {
    /// 0-based index among the file's requests.
    pub index: u32,
    pub name: Option<String>,
    pub method: String,
    /// URL as written (variables not substituted).
    pub url: String,
    /// 0-based line of the request line.
    pub line: u32,
    /// First and last line (0-based, inclusive) of the block, separator excluded.
    pub start_line: u32,
    pub end_line: u32,
}

/// A body line: literal text or `< path` (file contents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyPart {
    Text(String),
    File(String),
}

/// A request with every part still unsubstituted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRequest {
    pub block: HttpRequestBlock,
    pub headers: Vec<(String, String)>,
    pub body: Vec<BodyPart>,
}

/// Everything parsed from one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpFile {
    /// `@name = value` file variables, in order (later wins).
    pub variables: Vec<(String, String)>,
    pub requests: Vec<RawRequest>,
}

fn is_separator(line: &str) -> bool {
    line.trim_start().starts_with("###")
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || t.starts_with("//")
}

/// `# @name foo` / `// @name foo` / `# @name=foo`.
fn name_tag(line: &str) -> Option<String> {
    let t = line.trim_start();
    let rest = t.strip_prefix('#').or_else(|| t.strip_prefix("//"))?.trim_start();
    let rest = rest.strip_prefix("@name")?;
    let name = rest.trim_start_matches([' ', '\t', '=']).trim();
    (!name.is_empty() && (rest.starts_with([' ', '\t', '=']))).then(|| name.to_owned())
}

/// `@name = value` (file variable).
fn file_variable(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix('@')?;
    let (name, value) = rest.split_once('=')?;
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.') {
        return None;
    }
    Some((name.to_owned(), value.trim().to_owned()))
}

/// `METHOD url [HTTP/x]` or a bare URL (GET).
fn request_line(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    let (first, rest) = match t.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim()),
        None => (t, ""),
    };
    let (method, target) = if METHODS.contains(&first) {
        if rest.is_empty() {
            return None;
        }
        (first.to_owned(), rest)
    } else if looks_like_url(t) {
        ("GET".to_owned(), t)
    } else {
        return None;
    };
    // Drop a trailing protocol version.
    let url = match target.rsplit_once(char::is_whitespace) {
        Some((u, v)) if v.starts_with("HTTP/") => u.trim(),
        _ => target,
    };
    Some((method, url.to_owned()))
}

fn looks_like_url(t: &str) -> bool {
    t.starts_with("http://") || t.starts_with("https://") || t.starts_with("{{") || t.starts_with('/')
}

fn header(line: &str) -> Option<(String, String)> {
    let (name, value) = line.split_once(':')?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some((name.to_owned(), value.trim().to_owned()))
}

/// Splits `text` into blocks separated by `###` lines: `(first line, lines)`.
fn blocks(text: &str) -> Vec<(u32, Option<String>, Vec<&str>)> {
    let mut out = Vec::new();
    let mut start = 0u32;
    let mut title: Option<String> = None;
    let mut cur: Vec<&str> = Vec::new();
    for (i, raw) in text.split('\n').enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if is_separator(line) {
            out.push((start, title.take(), std::mem::take(&mut cur)));
            start = i as u32 + 1;
            let t = line.trim_start().trim_start_matches('#').trim();
            title = (!t.is_empty()).then(|| t.to_owned());
        } else {
            cur.push(line);
        }
    }
    out.push((start, title, cur));
    out
}

pub fn parse(text: &str) -> HttpFile {
    let mut file = HttpFile::default();
    for (start, title, lines) in blocks(text) {
        let mut name = title;
        let mut i = 0;
        // Preamble: comments, names, variables, blank lines.
        let mut request = None;
        while i < lines.len() {
            let line = lines[i];
            if let Some(n) = name_tag(line) {
                name = Some(n);
            } else if let Some(v) = file_variable(line) {
                file.variables.push(v);
            } else if line.trim().is_empty() || is_comment(line) {
            } else if let Some(r) = request_line(line) {
                request = Some((i, r));
                i += 1;
                break;
            }
            i += 1;
        }
        let Some((req_idx, (method, mut url))) = request else { continue };
        // Multi-line query: indented continuation lines.
        while i < lines.len() {
            let l = lines[i];
            let t = l.trim();
            if l.starts_with([' ', '\t']) && (t.starts_with('?') || t.starts_with('&') || t.starts_with('/')) {
                url.push_str(t);
                i += 1;
            } else {
                break;
            }
        }
        let mut headers = Vec::new();
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty() {
                i += 1;
                break;
            }
            if !is_comment(l)
                && let Some(h) = header(l)
            {
                headers.push(h);
            }
            i += 1;
        }
        let body = body_parts(&lines[i.min(lines.len())..]);
        // Trailing blank lines are not part of the block's visible extent.
        let mut last = lines.len();
        while last > req_idx + 1 && lines[last - 1].trim().is_empty() {
            last -= 1;
        }
        let index = file.requests.len() as u32;
        file.requests.push(RawRequest {
            block: HttpRequestBlock {
                index,
                name,
                method,
                url,
                line: start + req_idx as u32,
                start_line: start,
                end_line: start + last.saturating_sub(1) as u32,
            },
            headers,
            body,
        });
    }
    file
}

fn body_parts(lines: &[&str]) -> Vec<BodyPart> {
    let mut out = Vec::new();
    let mut in_handler = false;
    for &l in lines {
        let t = l.trim_start();
        if in_handler {
            if t.trim_end().ends_with("%}") {
                in_handler = false;
            }
            continue;
        }
        if t.starts_with("> {%") {
            in_handler = !t.trim_end().ends_with("%}");
            continue;
        }
        if t.starts_with("<>") || t.starts_with(">>") || t.starts_with("> ") {
            continue;
        }
        if let Some(path) = t.strip_prefix("< ") {
            out.push(BodyPart::File(path.trim().to_owned()));
        } else {
            out.push(BodyPart::Text(l.to_owned()));
        }
    }
    while matches!(out.last(), Some(BodyPart::Text(t)) if t.trim().is_empty()) {
        out.pop();
    }
    while matches!(out.first(), Some(BodyPart::Text(t)) if t.trim().is_empty()) {
        out.remove(0);
    }
    out
}

/// The blocks only (gutter, Run All).
pub fn requests(text: &str) -> Vec<HttpRequestBlock> {
    parse(text).requests.into_iter().map(|r| r.block).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"@host = example.com
### Get user
# a comment
GET https://{{host}}/users/1 HTTP/1.1
Accept: application/json
# not a header
X-Trace: 1

###
# @name create
POST https://{{host}}/users
    ?page=1
    &size=2
Content-Type: application/json

{
  "a": 1
}
< ./extra.json

> {%
  client.log(1)
%}

### bare
https://example.com/health
"#;

    #[test]
    fn parses_requests_names_and_blocks() {
        let f = parse(SAMPLE);
        assert_eq!(f.variables, vec![("host".into(), "example.com".into())]);
        assert_eq!(f.requests.len(), 3);
        let a = &f.requests[0];
        assert_eq!(a.block.name.as_deref(), Some("Get user"));
        assert_eq!(a.block.method, "GET");
        assert_eq!(a.block.url, "https://{{host}}/users/1");
        assert_eq!(a.block.line, 3);
        assert_eq!(a.headers, vec![("Accept".into(), "application/json".into()), ("X-Trace".into(), "1".into())]);
        assert!(a.body.is_empty());

        let b = &f.requests[1];
        assert_eq!(b.block.name.as_deref(), Some("create"));
        assert_eq!(b.block.url, "https://{{host}}/users?page=1&size=2");
        assert_eq!(b.block.line, 10);
        assert_eq!(
            b.body,
            vec![
                BodyPart::Text("{".into()),
                BodyPart::Text("  \"a\": 1".into()),
                BodyPart::Text("}".into()),
                BodyPart::File("./extra.json".into()),
            ]
        );

        let c = &f.requests[2];
        assert_eq!(c.block.method, "GET");
        assert_eq!(c.block.name.as_deref(), Some("bare"));
        assert_eq!(c.block.url, "https://example.com/health");
        assert_eq!(c.block.start_line, 25);
        assert_eq!(c.block.end_line, 25);
    }

    #[test]
    fn ignores_blocks_without_a_request_and_crlf() {
        let f = parse("### only comments\r\n# nothing\r\n###\r\nDELETE /x\r\nA: b\r\n");
        assert_eq!(f.requests.len(), 1);
        assert_eq!(f.requests[0].block.method, "DELETE");
        assert_eq!(f.requests[0].block.index, 0);
        assert_eq!(f.requests[0].headers, vec![("A".into(), "b".into())]);
    }

    #[test]
    fn plain_text_is_not_a_request() {
        assert!(parse("hello world\n").requests.is_empty());
        assert!(parse("GET\n").requests.is_empty());
    }
}
