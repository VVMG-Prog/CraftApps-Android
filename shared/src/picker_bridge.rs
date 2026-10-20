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

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Directory for picker bridge files.
pub fn bridge_dir() -> PathBuf {
    let base = std::env::var("CRAFTAPPS_SETTINGS_DIR")
        .unwrap_or_else(|_| "/sdcard/CraftApps".to_string());
    PathBuf::from(base).join(".picker")
}

fn write_then_rename(path: &PathBuf, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("write tmp: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// Request a file picker. Returns the request ID.
/// `kind` is "open" or "save". For open, `mimes` filters by MIME type.
/// For save, `name` is the suggested filename.
pub fn request_picker(kind: &str, mimes: Vec<String>, name: String) -> Result<u64, String> {
    let dir = bridge_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create dir: {e}"))?;
    
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let req = serde_json::json!({
        "id": id,
        "kind": kind,
        "mimes": mimes,
        "name": name,
    });
    
    write_then_rename(
        &dir.join("request.json"),
        &serde_json::to_string(&req).map_err(|e| format!("json: {e}"))?,
    )?;
    Ok(id)
}

/// Poll for a picker response. Returns `Some((id, paths))` if a response is
/// available, `None` if not yet. `paths` are local file paths (copied by Java).
/// Deletes the response file after reading.
pub fn poll_response() -> Option<(u64, Vec<String>, bool)> {
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
    
    Some((id, paths, cancelled))
}
