//! The per-project config folder (`<root>/.openidea/`).

use std::path::{Path, PathBuf};

/// Folder name under the project root.
pub const DIR: &str = ".openidea";

/// Where `rel` (e.g. `settings.toml`) is read from and written to.
pub fn path(root: &Path, rel: impl AsRef<Path>) -> PathBuf {
    root.join(DIR).join(rel)
}
