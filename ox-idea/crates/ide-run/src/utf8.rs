//! Incremental UTF-8 decoding for byte streams that may split code points
//! across reads.

/// Mirrors `ide-terminal`'s decoder; kept local so the crates stay independent.
/// Decodes a byte stream chunk by chunk. Incomplete trailing sequences are held
/// back until the next chunk; invalid bytes become U+FFFD.
#[derive(Debug, Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes `bytes`, appending the complete text to `out`.
    pub fn decode_into(&mut self, bytes: &[u8], out: &mut String) {
        let owned;
        let mut input: &[u8] = if self.pending.is_empty() {
            bytes
        } else {
            self.pending.extend_from_slice(bytes);
            owned = std::mem::take(&mut self.pending);
            &owned
        };
        loop {
            match std::str::from_utf8(input) {
                Ok(s) => {
                    out.push_str(s);
                    return;
                }
                Err(e) => {
                    let (valid, rest) = input.split_at(e.valid_up_to());
                    // `valid` is UTF-8 by construction.
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match e.error_len() {
                        Some(len) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            input = &rest[len..];
                        }
                        None => {
                            self.pending.extend_from_slice(rest);
                            return;
                        }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    /// Convenience wrapper around [`Self::decode_into`].
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        let mut out = String::new();
        self.decode_into(bytes, &mut out);
        out
    }

    /// Flushes held-back bytes at end of stream (a truncated sequence → U+FFFD).
    pub fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            String::new()
        } else {
            self.pending.clear();
            char::REPLACEMENT_CHARACTER.to_string()
        }
    }

    #[cfg(test)]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passthrough() {
        let mut d = Utf8Decoder::new();
        assert_eq!(d.decode(b"hello"), "hello");
        assert_eq!(d.finish(), "");
    }

    #[test]
    fn multibyte_split_at_every_position() {
        let text = "aé中😀z";
        let bytes = text.as_bytes();
        for split in 0..=bytes.len() {
            let mut d = Utf8Decoder::new();
            let mut out = d.decode(&bytes[..split]);
            out.push_str(&d.decode(&bytes[split..]));
            out.push_str(&d.finish());
            assert_eq!(out, text, "split at {split}");
        }
    }

    #[test]
    fn byte_by_byte() {
        let text = "😀中é";
        let mut d = Utf8Decoder::new();
        let mut out = String::new();
        for b in text.as_bytes() {
            d.decode_into(std::slice::from_ref(b), &mut out);
        }
        assert!(!d.has_pending());
        assert_eq!(out, text);
    }

    #[test]
    fn invalid_bytes_are_replaced() {
        let mut d = Utf8Decoder::new();
        assert_eq!(d.decode(b"a\xffb\xc3(c"), "a\u{FFFD}b\u{FFFD}(c");
    }

    #[test]
    fn truncated_sequence_at_end_of_stream() {
        let mut d = Utf8Decoder::new();
        assert_eq!(d.decode(b"ok\xe4\xb8"), "ok");
        assert!(d.has_pending());
        assert_eq!(d.finish(), "\u{FFFD}");
        assert_eq!(d.decode(b"x"), "x");
    }

    #[test]
    fn invalid_continuation_after_pending() {
        let mut d = Utf8Decoder::new();
        assert_eq!(d.decode(b"\xe4"), "");
        assert_eq!(d.decode(b"A"), "\u{FFFD}A");
    }
}

/// Output decoder for the console's configured encoding (Settings › Console).
pub enum OutputDecoder {
    Utf8(Utf8Decoder),
    Other(Box<encoding_rs::Decoder>),
}

impl OutputDecoder {
    /// Unknown labels fall back to UTF-8.
    pub fn for_label(label: Option<&str>) -> Self {
        match label.and_then(|l| encoding_rs::Encoding::for_label(l.trim().as_bytes())) {
            Some(enc) if enc != encoding_rs::UTF_8 => Self::Other(Box::new(enc.new_decoder())),
            _ => Self::Utf8(Utf8Decoder::new()),
        }
    }

    pub fn decode_into(&mut self, bytes: &[u8], out: &mut String) {
        match self {
            Self::Utf8(d) => d.decode_into(bytes, out),
            Self::Other(d) => decode_with(d, bytes, out, false),
        }
    }

    pub fn finish(&mut self) -> String {
        match self {
            Self::Utf8(d) => d.finish(),
            Self::Other(d) => {
                let mut out = String::new();
                decode_with(d, &[], &mut out, true);
                out
            }
        }
    }
}

fn decode_with(d: &mut encoding_rs::Decoder, bytes: &[u8], out: &mut String, last: bool) {
    let mut src = bytes;
    loop {
        out.reserve(d.max_utf8_buffer_length(src.len()).unwrap_or(src.len() * 3 + 16));
        let (result, read, _) = d.decode_to_string(src, out, last);
        src = &src[read..];
        if result == encoding_rs::CoderResult::InputEmpty {
            break;
        }
    }
}

#[cfg(test)]
mod encoding_tests {
    use super::OutputDecoder;

    #[test]
    fn gbk_split_across_chunks() {
        // "中文" in GBK: D6 D0 CE C4
        let mut d = OutputDecoder::for_label(Some("GBK"));
        let mut out = String::new();
        d.decode_into(&[0xD6], &mut out);
        d.decode_into(&[0xD0, 0xCE], &mut out);
        d.decode_into(&[0xC4], &mut out);
        out.push_str(&d.finish());
        assert_eq!(out, "中文");
    }

    #[test]
    fn unknown_label_is_utf8() {
        let mut d = OutputDecoder::for_label(Some("no-such-encoding"));
        let mut out = String::new();
        d.decode_into("héllo".as_bytes(), &mut out);
        assert_eq!(out, "héllo");
        assert!(matches!(d, OutputDecoder::Utf8(_)));
    }
}
