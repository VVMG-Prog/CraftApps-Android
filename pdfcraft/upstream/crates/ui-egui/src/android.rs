//! Android shell integration for `pdfcraft-ui-egui`.
//!
//! The Android port (`pdfcraft-android` crate) owns the JNI bridge, the
//! Storage Access Framework pickers and the shared storage layout. This
//! module is the narrow contract between the portable UI and that shell:
//!
//! - [`set_output_base`] — the directory the shell wants output-folder picks
//!   (`Ask::FolderOut`) to resolve to. The shell sets it once at startup
//!   from its `StorageManager`.
//! - [`output_dir`] — resolve a per-purpose output directory under the base,
//!   creating it.
//! - [`PickRequest`], [`PickKind`] and [`set_pick_handler`] are re-exported
//!   from the Android `rfd` shim so the shell can implement picks without a
//!   direct dependency on the shim crate.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use crate::rfd::{PickKind, PickRequest, set_pick_handler};

/// Base directory for output-folder picks, installed by the shell at startup
/// (e.g. `/sdcard/CraftApps/Media/PdfCraft`).
static OUTPUT_BASE: OnceLock<PathBuf> = OnceLock::new();

/// Install the base directory for [`output_dir`]. Called once by the shell.
pub fn set_output_base(dir: PathBuf) {
    let _ = OUTPUT_BASE.set(dir);
}

/// Resolve (and create) the output directory for `purpose` under the base set
/// by [`set_output_base`]. Falls back to the system temp dir when the shell
/// never called [`set_output_base`] (tests).
pub fn output_dir(purpose: &str) -> PathBuf {
    let base = OUTPUT_BASE
        .get()
        .cloned()
        .unwrap_or_else(std::env::temp_dir);
    // Keep the directory name filesystem-safe.
    let safe: String = purpose
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let dir = base.join(if safe.is_empty() { "output".to_string() } else { safe });
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The output base as a lossy string, for status messages.
pub fn output_base_display() -> String {
    OUTPUT_BASE
        .get()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| std::env::temp_dir().display().to_string())
}

/// True on Android: whether `path` is a SAF `content://` URI rather than a
/// filesystem path.
pub fn is_content_uri(path: &Path) -> bool {
    path.to_string_lossy().starts_with("content://")
}
