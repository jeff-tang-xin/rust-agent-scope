//! IDEA-style Extend Selection: word -> token -> bracket contents -> node -> ... -> file.

use super::folding::{bracket_pair, is_comment};
use super::lines::LineIndex;
use ide_core::Range;
use tree_sitter::{Node, Tree};

pub(crate) fn selection_ranges(tree: &Tree, text: &str, offset_utf16: u32) -> Vec<Range> {
    let idx = LineIndex::new(text);
    let b = idx.byte_of_utf16(offset_utf16);
    let mut spans: Vec<(usize, usize)> = Vec::new();

    let root = tree.root_node();
    let probe = if !is_word_at(text, b) && b > 0 && is_word_before(text, b) { prev_char(text, b) } else { b };
    let Some(mut node) = root.descendant_for_byte_range(probe, probe) else {
        return plain_selection_ranges(text, offset_utf16);
    };

    // Comments, strings and other wide leaves: select the word under the caret first.
    if let Some(word) = word_at(text, b)
        && (word.0 > node.start_byte() || word.1 < node.end_byte())
        && node.start_byte() <= word.0
        && word.1 <= node.end_byte()
    {
        spans.push(word);
    }
    loop {
        if let Some(inner) = inner_span(&node, text) {
            spans.push(inner);
        } else if (is_comment(node.kind()) || node.kind().contains("string"))
            && let Some(inner) = quoted_inner(&node, text)
        {
            spans.push(inner);
        }
        spans.push((node.start_byte(), node.end_byte()));
        match node.parent() {
            Some(p) => node = p,
            None => break,
        }
    }
    spans.push((0, text.len()));
    finish(&idx, spans)
}

/// Fallback without a grammar: word, line, whole text.
pub(crate) fn plain_selection_ranges(text: &str, offset_utf16: u32) -> Vec<Range> {
    let idx = LineIndex::new(text);
    let b = idx.byte_of_utf16(offset_utf16);
    let mut spans = Vec::new();
    if let Some(w) = word_at(text, b) {
        spans.push(w);
    }
    let line_start = text[..b].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[b..].find('\n').map_or(text.len(), |i| b + i);
    let line = &text[line_start..line_end];
    let lead = line.len() - line.trim_start().len();
    let trail = line.len() - line.trim_end().len();
    if line_start + lead <= line_end - trail {
        spans.push((line_start + lead, line_end - trail));
    }
    spans.push((line_start, line_end));
    spans.push((0, text.len()));
    finish(&idx, spans)
}

/// Keeps a strictly growing chain of spans that all contain the first one.
fn finish(idx: &LineIndex, spans: Vec<(usize, usize)>) -> Vec<Range> {
    let mut chain: Vec<(usize, usize)> = Vec::new();
    for s in spans {
        if s.0 > s.1 {
            continue;
        }
        match chain.last() {
            Some(&last) if s.0 <= last.0 && last.1 <= s.1 && s != last => chain.push(s),
            Some(_) => {}
            None => chain.push(s),
        }
    }
    chain.into_iter().map(|(s, e)| idx.range(s, e)).collect()
}

/// Contents of a bracketed node, without the brackets and surrounding whitespace.
fn inner_span(node: &Node, text: &str) -> Option<(usize, usize)> {
    let (open, close) = bracket_pair(node)?;
    trimmed(text, open.end_byte(), close.start_byte())
}

fn quoted_inner(node: &Node, text: &str) -> Option<(usize, usize)> {
    let s = text.get(node.start_byte()..node.end_byte())?;
    let quote = s.chars().next()?;
    if !matches!(quote, '"' | '\'' | '`') || s.len() < 2 || !s.ends_with(quote) {
        return None;
    }
    let q = quote.len_utf8();
    let (start, end) = (node.start_byte() + q, node.end_byte() - q);
    (start <= end).then_some((start, end))
}

fn trimmed(text: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let s = text.get(start..end)?;
    let lead = s.len() - s.trim_start().len();
    let trail = s.len() - s.trim_end().len();
    let (a, b) = (start + lead, end - trail);
    (a < b).then_some((a, b))
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn is_word_at(text: &str, b: usize) -> bool {
    text.get(b..).and_then(|s| s.chars().next()).is_some_and(is_word_char)
}

fn is_word_before(text: &str, b: usize) -> bool {
    text.get(..b).and_then(|s| s.chars().next_back()).is_some_and(is_word_char)
}

fn prev_char(text: &str, b: usize) -> usize {
    text[..b].char_indices().next_back().map_or(0, |(i, _)| i)
}

fn word_at(text: &str, b: usize) -> Option<(usize, usize)> {
    if !is_word_at(text, b) && !is_word_before(text, b) {
        return None;
    }
    let start = text[..b].char_indices().rev().take_while(|&(_, c)| is_word_char(c)).last().map_or(b, |(i, _)| i);
    let end = text[b..].char_indices().find(|&(_, c)| !is_word_char(c)).map_or(text.len(), |(i, _)| b + i);
    (start < end).then_some((start, end))
}
