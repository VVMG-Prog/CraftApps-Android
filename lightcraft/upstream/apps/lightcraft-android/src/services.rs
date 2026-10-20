//! Platform services for LightCraft on Android.
//!
//! Builds the [`Services`](lightcraft_ui_egui::Services) the UI crate needs,
//! backed by Android APIs:
//! - async file dialogs → SAF, via [`crate::file_browser`]
//! - writes → `content://` URIs through SAF, plain paths atomically
//! - links → the system browser through JNI

use crate::file_browser;
use craftapps_shared::{jni_bridge, saf, services as shared_services, storage};
use lightcraft_ui_egui::Services;
use std::sync::Arc;

/// Build the [`Services`] the UI crate needs, backed by Android APIs.
pub fn android_services(storage: Arc<storage::StorageManager>) -> Services {
    let folder_storage = storage.clone();
    Services {
        // Async file dialogs via SAF (see `file_browser`).
        picker: Some(file_browser::build_picker(storage.clone())),
        // Synchronous fallback: the async picker above serves file picks.
        pick_files: Some(Box::new(Vec::new)),
        // Folder picks (Open/New Library, Add Folder, copy destination):
        // the system folder picker via the shared file bridge -- the same
        // file method as PhotoCraft and FilmCraft. The hook is synchronous,
        // so wait for the bridge's answer here; the system picker is in
        // front meanwhile, so the paused frame behind it is never seen.
        // The library engine works with real paths, so the picked tree
        // URI is mapped back to a filesystem path (primary-volume picks;
        // the common case).
        pick_folder: Some(Box::new(move || {
            let id = match craftapps_shared::picker_bridge::request_picker(
                craftapps_shared::picker_bridge::PickerKind::Folder,
                Vec::new(),
                String::new(),
                false,
            ) {
                Ok(id) => id,
                Err(e) => {
                    log::error!("couldn't launch the folder picker: {e}");
                    return None;
                }
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
            loop {
                if let Some(resp) = craftapps_shared::picker_bridge::poll_response() {
                    if resp.id == id {
                        if resp.cancelled {
                            return None;
                        }
                        let Some(uri) = resp.uri else {
                            log::error!("folder picker answered without a URI");
                            return None;
                        };
                        // Remember the grant: parts of the app also read
                        // through SAF tree URIs, not just paths.
                        if let Err(e) = folder_storage.set_sd_tree_uri(&uri) {
                            log::warn!("couldn't remember the picked folder: {e}");
                        }
                        return match external_storage_path(&uri) {
                            Some(path) => Some(path),
                            None => {
                                log::error!("picked folder {uri} is not a primary-volume path; cannot use it as a library location");
                                None
                            }
                        };
                    }
                    log::warn!("folder picker: ignoring a response for request {}", resp.id);
                }
                if std::time::Instant::now() >= deadline {
                    log::error!("folder picker timed out");
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
        })),
        // Writes: content:// URIs go through SAF, plain paths are atomic.
        write: Some(Box::new(move |path: &str, bytes: &[u8]| {
            if path.starts_with("content://") {
                saf::saf_write_uri(path, bytes)
            } else {
                shared_services::write_atomic(std::path::Path::new(path), bytes)
            }
        })),
        open_url: Some(Box::new(|url: &str| {
            if !url.starts_with("https://") {
                return Err("only https links are opened".to_string());
            }
            jni_bridge::open_url(url)
        })),
        ..Default::default()
    }
}

/// `content://com.android.externalstorage.documents/...` -> a filesystem
/// path on the primary volume, or `None` for any other provider/volume.
/// Same mapping the FilmCraft and DesignCraft ports use.
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
