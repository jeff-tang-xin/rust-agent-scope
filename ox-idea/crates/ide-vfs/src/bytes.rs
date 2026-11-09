//! Raw byte access for non-text viewers (image viewer, hex view): a cheap
//! probe deciding whether a file is text, and bounded reads encoded as base64
//! so they travel over JSON IPC compactly.

use crate::encoding::looks_binary;
use crate::{READ_ONLY_THRESHOLD, SNIFF_LEN};
use ide_core::{IdeError, IdeResult};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Largest chunk one [`read_range`] call returns (the hex view pages through files).
pub const MAX_RANGE_LEN: u32 = 1024 * 1024;
/// Largest image [`read_data_url`] inlines.
pub const MAX_IMAGE_SIZE: u64 = 64 * 1024 * 1024;

/// What the editor needs to know before choosing a viewer for a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FileProbe {
    pub size: u64,
    /// Content looks binary (same heuristic as text loading).
    pub binary: bool,
    /// Above the read-only threshold: the text editor would open it read-only.
    pub too_large: bool,
}

/// A slice of a file: `data` is base64; `total` is the file size.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ByteRange {
    pub offset: u64,
    pub data: String,
    pub total: u64,
}

/// Entry bytes of an archive path (`<archive>!/<entry>`), `None` for ordinary paths.
fn archive_bytes(path: &Path, limit: u64) -> IdeResult<Option<Vec<u8>>> {
    let Some((archive, entry)) = crate::split_archive_path(path) else { return Ok(None) };
    let (size, _) = crate::archive::entry_size(&archive, &entry)?;
    if size > limit {
        return Err(IdeError::invalid(format!("{} is too large ({} MiB)", path.display(), size / (1024 * 1024))));
    }
    Ok(Some(crate::read_archive_entry(&archive, &entry)?.0))
}

/// Reads only the head of the file to classify it.
pub fn probe(path: &Path) -> IdeResult<FileProbe> {
    if let Some((archive, entry)) = crate::split_archive_path(path) {
        let (size, _) = crate::archive::entry_size(&archive, &entry)?;
        let head =
            if size > crate::MAX_OPEN_SIZE { Vec::new() } else { crate::read_archive_entry(&archive, &entry)?.0 };
        return Ok(FileProbe { size, binary: looks_binary(&head), too_large: size > READ_ONLY_THRESHOLD });
    }
    let meta = fs::metadata(path)?;
    if meta.is_dir() {
        return Err(IdeError::invalid(format!("{} is a directory", path.display())));
    }
    let mut head = Vec::with_capacity(SNIFF_LEN);
    File::open(path)?.take(SNIFF_LEN as u64).read_to_end(&mut head)?;
    Ok(FileProbe { size: meta.len(), binary: looks_binary(&head), too_large: meta.len() > READ_ONLY_THRESHOLD })
}

/// Reads up to `len` bytes (capped at [`MAX_RANGE_LEN`]) from `offset`.
pub fn read_range(path: &Path, offset: u64, len: u32) -> IdeResult<ByteRange> {
    if let Some(bytes) = archive_bytes(path, crate::MAX_OPEN_SIZE)? {
        let total = bytes.len() as u64;
        let offset = offset.min(total);
        let end = (offset + u64::from(len.min(MAX_RANGE_LEN))).min(total);
        return Ok(ByteRange { offset, data: base64(&bytes[offset as usize..end as usize]), total });
    }
    let mut file = File::open(path)?;
    let total = file.metadata()?.len();
    let offset = offset.min(total);
    let len = u64::from(len.min(MAX_RANGE_LEN)).min(total - offset);
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity(len as usize);
    file.take(len).read_to_end(&mut buf)?;
    Ok(ByteRange { offset, data: base64(&buf), total })
}

/// MIME type of the image formats the viewer shows, by extension.
pub fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        _ => return None,
    })
}

/// The whole image as a `data:` URL (allowed by the webview's CSP, unlike file URLs).
pub fn read_data_url(path: &Path) -> IdeResult<String> {
    let mime =
        image_mime(path).ok_or_else(|| IdeError::invalid(format!("{} is not a supported image", path.display())))?;
    if let Some(bytes) = archive_bytes(path, MAX_IMAGE_SIZE)? {
        return Ok(format!("data:{mime};base64,{}", base64(&bytes)));
    }
    let meta = fs::metadata(path)?;
    if meta.len() > MAX_IMAGE_SIZE {
        return Err(IdeError::invalid(format!(
            "{} is too large to preview ({} MiB)",
            path.display(),
            meta.len() / (1024 * 1024)
        )));
    }
    let bytes = fs::read(path)?;
    Ok(format!("data:{mime};base64,{}", base64(&bytes)))
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        let sextet = |shift: u32| ALPHABET[((n >> shift) & 63) as usize] as char;
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn probe_classifies_text_and_binary() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("a.txt");
        fs::write(&text, "hello\nworld\n").unwrap();
        assert_eq!(probe(&text).unwrap(), FileProbe { size: 12, binary: false, too_large: false });
        let bin = dir.path().join("a.bin");
        fs::write(&bin, [0u8, 1, 2, 3, 0, 0, 0xff, 0x10]).unwrap();
        assert!(probe(&bin).unwrap().binary);
        // BOM-less UTF-16 has NULs but is text.
        let utf16 = dir.path().join("u.txt");
        let bytes: Vec<u8> = "hello world, this is text".encode_utf16().flat_map(u16::to_le_bytes).collect();
        fs::write(&utf16, bytes).unwrap();
        assert!(!probe(&utf16).unwrap().binary);
        assert!(probe(dir.path()).is_err());
    }

    #[test]
    fn read_range_is_bounded_by_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("data");
        fs::write(&f, b"0123456789").unwrap();
        let r = read_range(&f, 4, 3).unwrap();
        assert_eq!((r.offset, r.data.as_str(), r.total), (4, "NDU2", 10));
        let tail = read_range(&f, 8, 100).unwrap();
        assert_eq!(tail.data, base64(b"89"));
        let past = read_range(&f, 50, 10).unwrap();
        assert_eq!((past.offset, past.data.as_str()), (10, ""));
    }

    #[test]
    fn data_url_for_images_only() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("x.PNG");
        fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        assert_eq!(
            read_data_url(&png).unwrap(),
            format!("data:image/png;base64,{}", base64(&[0x89, b'P', b'N', b'G']))
        );
        let txt = dir.path().join("x.txt");
        fs::write(&txt, "x").unwrap();
        assert!(read_data_url(&txt).is_err());
        assert_eq!(image_mime(Path::new("a/b.svg")), Some("image/svg+xml"));
    }
}
