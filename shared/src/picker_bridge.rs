//! File-based picker bridge: avoids JNI method lookup entirely.
//!
//! The Storage Access Framework picker must be launched from the Android UI
//! thread via `Activity.startActivityForResult`. Instead of calling into Java
//! via JNI (which fails with "Method not found" on some devices), Rust writes
//! a JSON request file and the Activity polls for it on the UI thread.
//!
//! - Rust: `request_picker()` writes `<dir>/request.json`
//! - Java: Handler polls, launches picker, writes `<dir>/response.json`
//! - Rust: `poll_response()` reads and deletes the response
//!
//! ## Where the files live
//!
//! The port sets `CRAFTAPPS_BRIDGE_DIR` to the app's *external files dir*
//! (`<external files>/.picker`). That directory is always writable, needs no
//! storage permission, and is private to the app — so the bridge works even
//! when the user picked "internal storage" in the first-run gate. The old
//! shared-folder location (`/sdcard/CraftApps/.picker`) stays as a fallback for
//! builds that did not set the variable.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Directory for picker bridge files.
pub fn bridge_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CRAFTAPPS_BRIDGE_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    // Legacy: the suite's shared folder (needs "All files access").
    if let Ok(dir) = std::env::var("CRAFTAPPS_SETTINGS_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir).join(".picker");
    }
    PathBuf::from("/sdcard/CraftApps/.picker")
}

fn write_then_rename(path: &PathBuf, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// What the Java side should show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    /// `ACTION_OPEN_DOCUMENT`: pick existing files.
    Open,
    /// `ACTION_CREATE_DOCUMENT`: pick where to write.
    Save,
    /// `ACTION_OPEN_DOCUMENT_TREE`: pick a folder.
    Folder,
}

impl PickerKind {
    /// The name the Java side reads from the request file.
    pub fn as_str(self) -> &'static str {
        match self {
            PickerKind::Open => "open",
            PickerKind::Save => "save",
            PickerKind::Folder => "folder",
        }
    }
}

/// Request a file picker. Returns the request ID.
///
/// * `kind` — open or save.
/// * `mimes` — the MIME types the picker should offer (empty = all).
/// * `name` — the suggested file name (save).
/// * `multiple` — allow picking more than one file (open).
pub fn request_picker(
    kind: PickerKind,
    mimes: Vec<String>,
    name: String,
    multiple: bool,
) -> Result<u64, String> {
    let dir = bridge_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create dir: {e}"))?;

    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let req = serde_json::json!({
        "id": id,
        "kind": kind.as_str(),
        "mimes": mimes,
        "name": name,
        "multiple": multiple,
    });

    write_then_rename(
        &dir.join("request.json"),
        &serde_json::to_string(&req).map_err(|e| format!("json: {e}"))?,
    )?;
    Ok(id)
}

/// The Java side's answer to a picker request.
#[derive(Clone, Debug, Default)]
pub struct PickerResponse {
    /// The request this answers.
    pub id: u64,
    /// The user backed out.
    pub cancelled: bool,
    /// Local file paths (the Java side copies picked files into the bridge
    /// inbox, so the app can read them as ordinary files).
    pub paths: Vec<String>,
    /// The document URI to write to (a save). The app writes through SAF.
    pub uri: Option<String>,
    /// The picked document's display name (the format hint for a save).
    pub name: Option<String>,
}

/// Poll for a picker response. Returns `Some(response)` if a response is
/// available, `None` if not yet. Deletes the response file after reading.
pub fn poll_response() -> Option<PickerResponse> {
    let path = bridge_dir().join("response.json");
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    // Delete first to avoid re-reading
    let _ = std::fs::remove_file(&path);

    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let id = v.get("id")?.as_u64()?;
    let cancelled = v.get("cancelled").and_then(|c| c.as_bool()).unwrap_or(false);
    let paths: Vec<String> = v
        .get("paths")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let uri = v.get("uri").and_then(|u| u.as_str()).map(|s| s.to_string());
    let name = v.get("name").and_then(|n| n.as_str()).map(|s| s.to_string());

    Some(PickerResponse { id, cancelled, paths, uri, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_match_the_protocol() {
        assert_eq!(PickerKind::Open.as_str(), "open");
        assert_eq!(PickerKind::Save.as_str(), "save");
        assert_eq!(PickerKind::Folder.as_str(), "folder");
    }
}
