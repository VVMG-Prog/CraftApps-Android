//! Android file dialogs for FilmCraft, behind [`HostHooks::run_dialog`].
//!
//! FilmCraft's dialog layer is asynchronous: the host writes a request, the
//! frame loop keeps running, and the answer arrives on a channel
//! ([`FilmcraftApp::pick`] answers `{"dialog": "open"}` meanwhile, and
//! `poll_native_dialog` runs the continuation on the frame the answer
//! arrives). That matches Android's model exactly — the Storage Access
//! Framework picker can only be launched from the UI thread, so the port's
//! shared file bridge carries the request: this module's worker thread writes
//! `request.json`, `FilmCraftActivity` (polling on the main thread) launches
//! the system picker, and the answer comes back as `response.json`.
//!
//! The synchronous pick hooks (`pick_files`, `pick_save`, …) are left unset:
//! with `run_dialog` present the app never calls them, and blocking the
//! render thread on a picker would deadlock the bridge poll.

use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use craftapps_shared::picker_bridge::{PickerKind, poll_response, request_picker};
use filmcraft_ui_egui::native_dialogs::FileDialog;

/// How long a picker may stay open before the dialog is treated as
/// abandoned. Generous: importing can mean browsing a big library.
const PICK_TIMEOUT: Duration = Duration::from_secs(600);

/// How often the worker thread polls the bridge for the answer.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The `run_dialog` hook: spawn the worker thread and hand the app its
/// answer channel at once. `media_dir` is the suite shared-media root used
/// when a save URI cannot be resolved to a filesystem path.
pub fn run_dialog(dialog: FileDialog, media_dir: std::path::PathBuf) -> Receiver<Vec<String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let paths = pick_blocking(dialog, &media_dir);
        // A dropped receiver means the app went away: nothing to answer.
        let _ = tx.send(paths);
    });
    rx
}

/// Run one dialog to completion on the worker thread.
fn pick_blocking(dialog: FileDialog, media_dir: &Path) -> Vec<String> {
    match dialog {
        FileDialog::OpenFiles { exts } => {
            pick_open(&ext_refs(&exts), true)
        }
        FileDialog::OpenFile { exts, .. } => take_one(pick_open(&ext_refs(&exts), false)),
        FileDialog::OpenProject => take_project(pick_open(&[], false)),
        FileDialog::Relink { exts, .. } => {
            // With `run_dialog` the continuation runs the relink command
            // itself with the chosen path.
            take_one(pick_open(&ext_refs(&exts), false))
        }
        FileDialog::SaveProject { name } => pick_save(&name, media_dir),
        FileDialog::SaveAs { name, .. } => {
            // The suggestion may be a full path; the picker takes a name.
            let base = Path::new(&name)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(name);
            pick_save(&base, media_dir)
        }
        FileDialog::Folder { .. } => pick_folder(),
    }
}

/// Ask for files through the bridge and wait for the answer.
fn pick_open(exts: &[&str], multiple: bool) -> Vec<String> {
    let mimes: Vec<String> = exts
        .iter()
        .filter_map(|e| mime_for_extension(e))
        .map(str::to_string)
        .collect();
    let Some(response) = await_response(PickerKind::Open, mimes, String::new(), multiple) else {
        return Vec::new();
    };
    if response.cancelled {
        return Vec::new();
    }
    // The Java side copies the picks into the bridge inbox as plain files.
    response.paths
}

/// Ask where to write and answer with a filesystem path the engine can use.
///
/// The bridge returns a document URI for a save; the engine writes with
/// `std::fs`, so the URI is resolved to a real path when it names a file on
/// the primary external-storage volume (the common case, and the one \"All
/// files access\" covers). Anything else falls back to the suite media
/// folder under the chosen display name, so the save still lands somewhere
/// the user can reach. `media_dir` is the suite's shared media root.
fn pick_save(name: &str, media_dir: &Path) -> Vec<String> {
    let Some(response) = await_response(PickerKind::Save, Vec::new(), name.to_string(), false) else {
        return Vec::new();
    };
    if response.cancelled {
        return Vec::new();
    }
    if let Some(path) = response.uri.as_deref().and_then(external_storage_path) {
        log::info!("save target resolved: {path}");
        return vec![path];
    }
    let display = response
        .name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(name);
    let fallback = media_dir.join("FilmCraft").join(sanitize_name(display));
    log::warn!(
        "save URI {:?} is not a primary-volume path; saving to {} instead",
        response.uri,
        fallback.display()
    );
    // Make sure the engine can write there.
    if let Some(dir) = fallback.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    vec![fallback.to_string_lossy().into_owned()]
}

/// Ask for a folder and answer with its filesystem path.
fn pick_folder() -> Vec<String> {
    let Some(response) = await_response(PickerKind::Folder, Vec::new(), String::new(), false) else {
        return Vec::new();
    };
    if response.cancelled {
        return Vec::new();
    }
    match response.uri.as_deref().and_then(external_storage_path) {
        Some(path) => vec![path],
        None => {
            log::warn!(
                "folder URI {:?} is not a primary-volume path; the pick is unusable",
                response.uri
            );
            Vec::new()
        }
    }
}

/// Write a bridge request and poll until its answer (or the timeout).
fn await_response(
    kind: PickerKind,
    mimes: Vec<String>,
    name: String,
    multiple: bool,
) -> Option<craftapps_shared::picker_bridge::PickerResponse> {
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

/// `content://com.android.externalstorage.documents/...` → a filesystem path
/// on the primary volume, or `None` for any other provider/volume.
///
/// The document id is `primary:<relative/path>`, URL-encoded, carried either
/// as the `document/<id>` or `tree/<id>` segment.
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

fn take_one(paths: Vec<String>) -> Vec<String> {
    paths.into_iter().take(1).collect()
}

/// Keep only project files from a free-form pick.
fn take_project(paths: Vec<String>) -> Vec<String> {
    let projects: Vec<String> = paths
        .into_iter()
        .filter(|p| p.to_ascii_lowercase().ends_with(".fcproj"))
        .collect();
    take_one(projects)
}

fn ext_refs(exts: &[String]) -> Vec<&str> {
    exts.iter().map(String::as_str).collect()
}

/// File extension → MIME type for the pickers. The shared crate's map only
/// covers still images (it was written for the photo apps); FilmCraft picks
/// are mostly media, so the map here also covers video and audio. Unknown
/// extensions are skipped — an empty list means "any file".
fn mime_for_extension(ext: &str) -> Option<&'static str> {
    if let Some(mime) = craftapps_shared::saf::mime_for_extension(ext) {
        return Some(mime);
    }
    Some(match ext.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/x-wav",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "ogg" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "fcproj" => "application/zip",
        _ => return None,
    })
}
