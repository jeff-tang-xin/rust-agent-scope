//! Byte offset -> 0-based line / UTF-16 column for one text.

use ide_core::{Pos, Range};

pub(crate) struct Lines<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.bytes().enumerate().filter(|&(_, b)| b == b'\n').map(|(i, _)| i + 1));
        Self { text, starts }
    }

    pub(crate) fn line_of(&self, byte: usize) -> usize {
        self.starts.partition_point(|&s| s <= byte.min(self.text.len())) - 1
    }

    pub(crate) fn pos(&self, byte: usize) -> Pos {
        let mut byte = byte.min(self.text.len());
        while !self.text.is_char_boundary(byte) {
            byte -= 1;
        }
        let line = self.line_of(byte);
        let col: usize = self.text[self.starts[line]..byte].chars().map(char::len_utf16).sum();
        Pos::new(line as u32, col as u32)
    }

    pub(crate) fn range(&self, start: usize, end: usize) -> Range {
        Range::new(self.pos(start), self.pos(end))
    }

    /// Text of the line containing `byte`, without the line break.
    pub(crate) fn line_text(&self, byte: usize) -> &'a str {
        let line = self.line_of(byte);
        let start = self.starts[line];
        let end = self.starts.get(line + 1).map_or(self.text.len(), |&s| s - 1);
        self.text[start..end].trim_end_matches('\r')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_columns_and_lines() {
        let l = Lines::new("ab\nx😀y\r\nz");
        assert_eq!(l.pos(0), Pos::new(0, 0));
        assert_eq!(l.pos(3), Pos::new(1, 0));
        assert_eq!(l.pos(8), Pos::new(1, 3));
        assert_eq!(l.line_text(4), "x😀y");
        assert_eq!(l.pos(100), Pos::new(2, 1));
    }
}
