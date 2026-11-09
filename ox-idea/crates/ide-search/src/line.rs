//! Turns a matched line plus byte ranges into a [`LineMatch`] with UTF-16
//! columns, truncating very long lines to a window around the first match.

use crate::LineMatch;

/// Lines longer than this (in UTF-16 units) are truncated.
pub(crate) const MAX_LINE_LEN: usize = 500;
/// How much text before the first match a truncated line keeps.
const CONTEXT_BEFORE: usize = 100;

/// `line` excludes the line terminator; `ranges` are sorted, non-overlapping
/// byte ranges within `line`.
pub(crate) fn line_match(line_no: u32, line: &[u8], ranges: &[(usize, usize)]) -> LineMatch {
    match std::str::from_utf8(line) {
        Ok(s) => from_str(line_no, s, ranges),
        Err(_) => {
            let (s, mapped) = lossy(line, ranges);
            from_str(line_no, &s, &mapped)
        }
    }
}

fn from_str(line_no: u32, s: &str, ranges: &[(usize, usize)]) -> LineMatch {
    let (start, end) = if s.len() <= MAX_LINE_LEN || utf16_len(s.as_bytes()) <= MAX_LINE_LEN {
        (0, s.len())
    } else {
        window(s, floor_boundary(s, ranges.first().map_or(0, |r| r.0.min(s.len()))))
    };
    let text = &s[start..end];
    let text_offset = if start == 0 { 0 } else { utf16_len(&s.as_bytes()[..start]) };

    let mut out = Vec::with_capacity(ranges.len());
    let (mut pos, mut col) = (start, 0usize);
    for &(a, b) in ranges {
        if a > end || (a == end && b > a) {
            break;
        }
        let (a, b) = (floor_boundary(s, a.max(start)), ceil_boundary(s, b.min(end)));
        if a < pos {
            continue;
        }
        col += utf16_len(&s.as_bytes()[pos..a]);
        let col_a = col;
        col += utf16_len(&s.as_bytes()[a..b]);
        pos = b;
        out.push((col_a as u32, col as u32));
    }
    LineMatch { line: line_no, text: text.to_owned(), ranges: out, text_offset: text_offset as u32 }
}

/// Byte window `[start, end)` of at most `MAX_LINE_LEN` UTF-16 units that
/// starts a little before `first_match`, on char boundaries.
fn window(s: &str, first_match: usize) -> (usize, usize) {
    let mut start = first_match;
    let mut units = 0;
    for (i, ch) in s[..first_match].char_indices().rev() {
        units += ch.len_utf16();
        if units > CONTEXT_BEFORE {
            break;
        }
        start = i;
    }
    let mut end = start;
    let mut units = 0;
    for ch in s[start..].chars() {
        let w = ch.len_utf16();
        if units + w > MAX_LINE_LEN {
            break;
        }
        units += w;
        end += ch.len_utf8();
    }
    // Near the end of the line: use the spare budget for more leading context.
    if end == s.len() {
        for (i, ch) in s[..start].char_indices().rev() {
            let w = ch.len_utf16();
            if units + w > MAX_LINE_LEN {
                break;
            }
            units += w;
            start = i;
        }
    }
    (start, end)
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// UTF-16 length of valid UTF-8: one unit per char, two for 4-byte chars.
#[inline]
pub(crate) fn utf16_len(bytes: &[u8]) -> usize {
    bytes.iter().map(|&b| usize::from((b & 0xC0) != 0x80) + usize::from(b >= 0xF0)).sum()
}

/// Lossy UTF-8 conversion (as `String::from_utf8_lossy`) that remaps byte
/// ranges into the converted string.
fn lossy(line: &[u8], ranges: &[(usize, usize)]) -> (String, Vec<(usize, usize)>) {
    // (orig start, new start, valid len, invalid len) per chunk.
    let mut chunks = Vec::new();
    let mut out = String::with_capacity(line.len() + 8);
    let mut orig = 0;
    for chunk in line.utf8_chunks() {
        let (valid, invalid) = (chunk.valid(), chunk.invalid());
        chunks.push((orig, out.len(), valid.len(), invalid.len()));
        out.push_str(valid);
        if !invalid.is_empty() {
            out.push(char::REPLACEMENT_CHARACTER);
        }
        orig += valid.len() + invalid.len();
    }
    let map = |off: usize, is_end: bool| -> usize {
        for &(o, n, v, inv) in &chunks {
            if off <= o + v {
                return n + off.saturating_sub(o);
            }
            if off < o + v + inv {
                let replacement = char::REPLACEMENT_CHARACTER.len_utf8();
                return n + v + if is_end { replacement } else { 0 };
            }
        }
        out.len()
    };
    let mapped = ranges.iter().map(|&(a, b)| (map(a, false), map(b, true).max(map(a, false)))).collect();
    (out, mapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_columns() {
        let s = "中文 😀 foo";
        let a = s.find("foo").unwrap();
        let m = line_match(3, s.as_bytes(), &[(a, a + 3)]);
        // 中 文 space (3) + emoji (2) + space (1) = 6
        assert_eq!(m.ranges, vec![(6, 9)]);
        assert_eq!(m.text_offset, 0);
        assert_eq!(m.text, s);
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let line = b"ab\xFFcd foo";
        let m = line_match(0, line, &[(6, 9)]);
        assert_eq!(m.text, "ab\u{FFFD}cd foo");
        assert_eq!(m.ranges, vec![(6, 9)]);
    }

    #[test]
    fn long_line_window() {
        let s = format!("{}needle{}", "x".repeat(1000), "y".repeat(1000));
        let m = line_match(0, s.as_bytes(), &[(1000, 1006)]);
        assert_eq!(m.text_offset, 900);
        assert_eq!(m.ranges, vec![(100, 106)]);
        assert_eq!(utf16_len(m.text.as_bytes()), MAX_LINE_LEN);
        assert_eq!(&m.text[100..106], "needle");
    }

    #[test]
    fn long_line_window_at_end() {
        let s = format!("{}needle", "x".repeat(1000));
        let m = line_match(0, s.as_bytes(), &[(1000, 1006)]);
        assert_eq!(m.text_offset, 506);
        assert_eq!(m.ranges, vec![(494, 500)]);
    }

    #[test]
    fn long_line_window_never_splits_surrogates() {
        let s = format!("{}needle{}", "😀".repeat(400), "😀".repeat(400));
        let a = "😀".len() * 400;
        let m = line_match(0, s.as_bytes(), &[(a, a + 6)]);
        assert_eq!(m.text_offset, 700);
        assert_eq!(m.ranges, vec![(100, 106)]);
        assert!(utf16_len(m.text.as_bytes()) <= MAX_LINE_LEN);
    }
}
