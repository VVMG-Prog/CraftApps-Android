//! Android file dialogs for EffectCraft, behind the app's dialog hooks.
//!
//! The suite's one file method: the shared file bridge. Rust writes a
//! request file, `EffectCraftActivity` (polling on the Android main thread)
//! launches the system SAF picker, and the answer comes back as a response
//! file. Two hook shapes sit on top of it:
//!
//! - `file_dialog_async` serves the commands' dialogs (File ▸ Import, Open,
//!   Save As…): the pick runs on a worker thread and the answer arrives
//!   through the `DialogDone` callback, so no frame ever blocks.
//! - The synchronous hooks serve the panels that call them directly (Render
//!   Queue output, Settings paths, the shortcut editor): the same blocking
//!   pick runs on the calling thread; the system picker is in front
//!   meanwhile, so the paused frame behind it is never seen.
//!
//! Nothing here uses the shared crate's pending-request flow, so the shell
//! must NOT call `saf::poll_bridge()`: it would take bridge responses and
//! drop them for want of a pending request. The workers below own the
//! response file end to end (the FilmCraft arrangement).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use craftapps_shared::picker_bridge::{PickerKind, PickerResponse, poll_response, request_picker};
use effectcraft_ui_egui::{DialogDone, FileDialog};

/// How long a picker may stay open before the dialog is treated as
/// abandoned. Generous: importing can mean browsing a big library.
const PICK_TIMEOUT: Duration = Duration::from_secs(600);

/// How often a waiting thread polls the bridge for the answer.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// The non-blocking hook (commands' dialogs).

/// The `file_dialog_async` hook: run the dialog on a worker thread and hand
/// the app its answer through `done` (empty = cancelled).
pub fn run_dialog_async(dialog: FileDialog, done: DialogDone, media_dir: std::sync::Arc<PathBuf>) {
    std::thread::spawn(move || {
        let paths = pick_dialog(&dialog, &media_dir);
        done(paths);
    });
}

/// Run one async dialog to completion on the worker thread.
fn pick_dialog(dialog: &FileDialog, media_dir: &Path) -> Vec<String> {
    match dialog {
        FileDialog::Open { exts, many } => pick_open(&ext_refs(exts), *many),
        FileDialog::OpenProject => pick_open_project().into_iter().collect(),
        FileDialog::Folder => pick_folder().into_iter().collect(),
        FileDialog::Save { default, ext, project } => {
            let picked = if *project {
                pick_save_project(default, media_dir)
            } else {
                pick_save_file(default, ext, media_dir)
            };
            picked.into_iter().collect()
        }
    }
}

// ---------------------------------------------------------------------------
// The synchronous hooks (panels).

/// Files to open or import (several).
pub fn pick_files(exts: &[&str]) -> Vec<String> {
    pick_open(exts, true)
}

/// An EffectCraft project to open.
pub fn pick_open_project() -> Option<String> {
    // No MIME filter: .ecproj has no registered type, so a filter could
    // hide every project. The extension decides instead.
    let paths = pick_open(&[], false);
    paths
        .into_iter()
        .find(|p| {
            let lower = p.to_ascii_lowercase();
            lower.ends_with(".ecproj") || lower.ends_with(".ecprojx")
        })
}

/// Where to save the project (the name suggested to the picker).
pub fn pick_save_project(name: &str, media_dir: &Path) -> Option<String> {
    pick_save(name, media_dir)
}

/// Where to save another file kind (a name or full path, and its extension).
pub fn pick_save_file(name: &str, _ext: &str, media_dir: &Path) -> Option<String> {
    pick_save(name, media_dir)
}

/// A folder.
pub fn pick_folder() -> Option<String> {
    let response = await_response(PickerKind::Folder, Vec::new(), String::new(), false)?;
    if response.cancelled {
        return None;
    }
    match response.uri.as_deref().and_then(external_storage_path) {
        Some(path) => Some(path),
        None => {
            log::warn!(
                "folder URI {:?} is not a primary-volume path; the pick is unusable",
                response.uri
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge plumbing.

/// Ask for files through the bridge and wait for the answer. The Java side
/// copies the picks into the bridge inbox as plain files.
fn pick_open(exts: &[&str], multiple: bool) -> Vec<String> {
    let Some(response) = await_response(PickerKind::Open, mimes_for_extensions(exts), String::new(), multiple) else {
        return Vec::new();
    };
    if response.cancelled {
        return Vec::new();
    }
    response.paths
}

/// Ask where to write and answer with a filesystem path the engine can use.
///
/// The bridge returns a document URI for a save; the engine writes with
/// `std::fs`, so the URI is resolved to a real path when it names a file on
/// the primary external-storage volume (the common case, and the one "All
/// files access" covers). Anything else falls back to the suite media
/// folder under the chosen display name, so the save still lands somewhere
/// the user can reach. `media_dir` is the suite's shared media root.
fn pick_save(name: &str, media_dir: &Path) -> Option<String> {
    // The suggestion may be a full path; the picker takes a name.
    let base = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let response = await_response(PickerKind::Save, Vec::new(), base.clone(), false)?;
    if response.cancelled {
        return None;
    }
    if let Some(path) = response.uri.as_deref().and_then(external_storage_path) {
        log::info!("save target resolved: {path}");
        return Some(path);
    }
    let display = response
        .name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(&base);
    let fallback = media_dir.join("EffectCraft").join(sanitize_name(display));
    log::warn!(
        "save URI {:?} is not a primary-volume path; saving to {} instead",
        response.uri,
        fallback.display()
    );
    // Make sure the engine can write there.
    if let Some(dir) = fallback.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    Some(fallback.to_string_lossy().into_owned())
}

/// Write a bridge request and poll until its answer (or the timeout).
fn await_response(
    kind: PickerKind,
    mimes: Vec<String>,
    name: String,
    multiple: bool,
) -> Option<PickerResponse> {
    let id = match request_picker(kind, mimes, name, multiple) {
        Ok(id) => id,
        Err(e) => {
            log::error!("picker request failed: {e}");
            return None;
        }
    };
    let deadline = Instant::now() + PICK_TIMEOUT;
    loop {
        if let Some(response) = poll_response() {
            if response.id == id {
                return Some(response);
            }
            // A stale answer to an earlier request: discard and keep waiting.
            log::warn!("discarding stale picker response (id {})", response.id);
        }
        if Instant::now() >= deadline {
            log::warn!("picker request {id} timed out");
            return None;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// `content://com.android.externalstorage.documents/...` → a filesystem
/// path on the primary volume, or `None` for any other provider/volume.
///
/// The document id is `primary:<relative/path>`, URL-encoded, carried
/// either as the `document/<id>` or `tree/<id>` segment.
fn external_storage_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("content://com.android.externalstorage.documents/")?;
    let doc_id = rest
        .strip_prefix("document/")
        .or_else(|| rest.strip_prefix("tree/"))
        .unwrap_or(rest);
    let doc_id = percent_decode(doc_id);
    let relative = doc_id.strip_prefix("primary:")?;
    if relative.is_empty() {
        Some("/sdcard".to_string())
    } else {
        Some(format!("/sdcard/{relative}"))
    }
}

/// Minimal percent-decoding for a URI document id.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A display name safe to use as a plain file name.
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "Untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

fn ext_refs(exts: &[String]) -> Vec<&str> {
    exts.iter().map(String::as_str).collect()
}

/// File extensions → MIME types for the open picker, all or nothing: when
/// any extension has no known type the pick is left unfiltered, because a
/// partial `EXTRA_MIME_TYPES` list would hide the files whose types are
/// missing (the importer itself rejects anything it cannot read). An empty
/// extension list also means "any file".
fn mimes_for_extensions(exts: &[&str]) -> Vec<String> {
    if exts.is_empty() {
        return Vec::new();
    }
    let mut mimes = Vec::with_capacity(exts.len());
    for ext in exts {
        match mime_for_extension(ext) {
            Some(mime) => mimes.push(mime.to_string()),
            None => return Vec::new(),
        }
    }
    mimes
}

/// File extension → MIME type for the pickers. The shared crate's map only
/// covers still images (it was written for the photo apps); EffectCraft
/// picks are mostly media, so the map here also covers video, audio and
/// the app's own file kinds. `None`: unfiltered is safer than wrong.
fn mime_for_extension(ext: &str) -> Option<&'static str> {
    if let Some(mime) = craftapps_shared::saf::mime_for_extension(ext) {
        return Some(mime);
    }
    Some(match ext.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mxf" => "video/mxf",
        "wav" => "audio/x-wav",
        "aif" | "aiff" => "audio/x-aiff",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "psd" | "psb" => "image/vnd.adobe.photoshop",
        "json" => "application/json",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "txt" => "text/plain",
        "js" | "jsx" => "text/javascript",
        "wasm" => "application/wasm",
        "gltf" => "model/gltf+json",
        "glb" => "model/gltf-binary",
        _ => return None,
    })
}

