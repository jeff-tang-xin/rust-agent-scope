//! Match iteration that never splits a UTF-8 sequence after an empty match
//! (`grep-matcher`'s iterators advance by a single byte).

use grep_matcher::{Captures, Match, Matcher};
use grep_regex::{RegexCaptures, RegexMatcher};

/// Calls `f` for each non-overlapping match in `haystack[at..]`; `haystack[..at]`
/// is only used as look-behind context.
pub(crate) fn for_each_match(matcher: &RegexMatcher, haystack: &[u8], at: usize, mut f: impl FnMut(Match)) {
    let mut at = at;
    let mut last_end = None;
    while at <= haystack.len() {
        let Ok(Some(m)) = matcher.find_at(haystack, at) else { break };
        at = advance(haystack, m);
        if m.is_empty() && last_end == Some(m.end()) {
            continue;
        }
        last_end = Some(m.end());
        f(m);
    }
}

/// Like [`for_each_match`] but with capture groups.
pub(crate) fn for_each_captures(
    matcher: &RegexMatcher,
    haystack: &[u8],
    caps: &mut RegexCaptures,
    mut f: impl FnMut(&RegexCaptures),
) {
    let mut at = 0;
    let mut last_end = None;
    while at <= haystack.len() {
        if !matches!(matcher.captures_at(haystack, at, caps), Ok(true)) {
            break;
        }
        let Some(m) = caps.get(0) else { break };
        at = advance(haystack, m);
        if m.is_empty() && last_end == Some(m.end()) {
            continue;
        }
        last_end = Some(m.end());
        f(caps);
    }
}

/// Where to resume after `m`: its end, or past the next char for empty matches.
fn advance(haystack: &[u8], m: Match) -> usize {
    if !m.is_empty() {
        return m.end();
    }
    let mut next = m.end() + 1;
    while next < haystack.len() && (haystack[next] & 0xC0) == 0x80 {
        next += 1;
    }
    next
}
