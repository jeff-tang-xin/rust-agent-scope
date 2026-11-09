//! `.env` files ("Paths to .env files" / EnvFile): `KEY=value` lines, `export`
//! prefixes, `#` comments, single- and double-quoted values (escapes in double
//! quotes) and `${VAR}` references to earlier variables or the base environment.

use std::collections::BTreeMap;

/// Parses `text`; `lookup` resolves `${VAR}` / `$VAR` not defined earlier in the file.
pub fn parse(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut lines = text.lines();
    while let Some(raw) = lines.next() {
        let line = raw.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some((key, rest)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            continue;
        }
        let rest = rest.trim_start();
        let value = if let Some(body) = rest.strip_prefix('\'') {
            quoted(body, '\'', &mut lines).unwrap_or_else(|| rest.to_owned())
        } else if let Some(body) = rest.strip_prefix('"') {
            let v = quoted(body, '"', &mut lines).unwrap_or_else(|| rest.to_owned());
            expand(&unescape(&v), &out, lookup)
        } else {
            let v = match rest.find(" #") {
                Some(i) => &rest[..i],
                None => rest,
            };
            expand(v.trim_end(), &out, lookup)
        };
        out.retain(|(k, _)| k != key);
        out.push((key.to_owned(), value));
    }
    out
}

/// The body of a quoted value up to the closing `quote`, which may be on a later line.
fn quoted<'a>(first: &str, quote: char, lines: &mut impl Iterator<Item = &'a str>) -> Option<String> {
    let mut text = first.to_owned();
    loop {
        let mut escaped = false;
        for (i, c) in text.char_indices() {
            if quote == '"' && c == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if c == quote && !escaped {
                return Some(text[..i].to_owned());
            }
            escaped = false;
        }
        text.push('\n');
        text.push_str(lines.next()?);
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn expand(value: &str, earlier: &[(String, String)], lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let resolve = |name: &str| {
        earlier
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| lookup(name))
            .unwrap_or_default()
    };
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < value.len() {
        if bytes[i] == b'$' {
            if bytes.get(i + 1) == Some(&b'{')
                && let Some(end) = value[i + 2..].find('}')
            {
                out.push_str(&resolve(&value[i + 2..i + 2 + end]));
                i += end + 3;
                continue;
            }
            let name_len = value[i + 1..].bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
            if name_len > 0 {
                out.push_str(&resolve(&value[i + 1..i + 1 + name_len]));
                i += name_len + 1;
                continue;
            }
        }
        let c = value[i..].chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Variables of every file in order (later files win); unreadable files are an error.
pub fn load(
    paths: &[std::path::PathBuf],
    base: &BTreeMap<String, String>,
) -> std::io::Result<BTreeMap<String, String>> {
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    for path in paths {
        let text = std::fs::read_to_string(path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        let lookup = |name: &str| vars.get(name).or_else(|| base.get(name)).cloned();
        let parsed = parse(&text, &lookup);
        vars.extend(parsed);
    }
    Ok(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn parses_common_forms() {
        let text = "# comment\nA=1\nexport B = two words # trailing\nC='lit $A'\nD=\"x\\ny $A ${B}\"\nE=\"multi\nline\"\n\nbad line\nF=\n";
        let v = parse(text, &none);
        let get = |k: &str| v.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("A"), Some("1"));
        assert_eq!(get("B"), Some("two words"));
        assert_eq!(get("C"), Some("lit $A"));
        assert_eq!(get("D"), Some("x\ny 1 two words"));
        assert_eq!(get("E"), Some("multi\nline"));
        assert_eq!(get("F"), Some(""));
        assert_eq!(v.len(), 6);
    }

    #[test]
    fn references_fall_back_to_base() {
        let v = parse("P=$HOME/bin:${MISSING}x", &|n| (n == "HOME").then(|| "/h".to_owned()));
        assert_eq!(v, [("P".to_owned(), "/h/bin:x".to_owned())]);
    }
}
