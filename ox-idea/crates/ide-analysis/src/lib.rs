//! Project-wide analyses over syntax trees.
//!
//! * [`duplicates`]: Locate Duplicates — token-based clone detection in the
//!   style of PMD's CPD (tree-sitter leaves as tokens, optional anonymisation
//!   of identifiers and literals, rolling hashes over token windows).
//! * [`structural`]: Search / Replace Structurally — code patterns with
//!   `$VAR$` placeholders matched against syntax trees (ast-grep's matcher on
//!   the bundled grammars).
//!
//! Both work on whole files supplied by the caller (unsaved buffers first),
//! run on rayon and honour a [`CancellationToken`]: call them from a blocking
//! thread. Positions are 0-based lines and UTF-16 columns.

pub mod duplicates;
mod lines;
pub mod structural;

pub use duplicates::{DuplicateGroup, DuplicateOptions, DuplicatesReport, Fragment, find_duplicates};
pub use structural::{
    StructuralFile, StructuralMatch, StructuralQuery, StructuralSummary, normalize_placeholders, search_structurally,
    validate_structural,
};

use ide_core::LanguageId;
use std::path::{Path, PathBuf};

/// A file to analyse: its project-relative path and a way to read it.
#[derive(Clone, Debug)]
pub struct SourceFile {
    pub path: PathBuf,
    pub rel_path: String,
}

/// Reads a file for analysis: its language and current text (the unsaved
/// buffer when open). `None` skips the file (binary, unreadable, too large).
pub type Loader<'a> = &'a (dyn Fn(&Path) -> Option<(LanguageId, String)> + Sync);
