//! Android platform services for DesignCraft.
//!
//! File open goes through the async path (`open_async` + `inbox`): the SAF
//! system picker is asynchronous, so instead of the desktop's synchronous
//! `pick_open` we launch `ACTION_OPEN_DOCUMENT` and deliver the picked file's
//! bytes into the inbox, which `DesignApp::logic` drains every frame. Save
//! uses the system create-document picker through the shared file bridge --
//! the same file method as PhotoCraft and FilmCraft: `pick_save` launches
//! the picker, waits for the bridge's answer, and maps the picked SAF URI
//! back to a filesystem path for the engine (which writes with `std::fs`).
//! `read`/`write` accept both plain filesystem paths and `content://` SAF
//! URIs. Clipboard text comes from Android's `ClipboardManager` via JNI.

use craftapps_shared::{jni_bridge, saf, storage::StorageManager};
use designcraft_ui_egui::{ImportRequest, ImportedFile, Inbox, Services};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// MIME types offered by the SAF open picker, keyed by DesignCraft's purpose
/// strings (`open`, `place`, `relink`, `styles`, `swatches`, …).
fn mime_types_for_purpose(purpose: &str) -> Vec<String> {
    match purpose {
        "place" | "relink" => vec!["image/*".to_string(), "application/pdf".to_string()],
        _ => vec!["*/*".to_string()],
    }
}

/// Read bytes for a path: a `content://` SAF URI goes through the content
/// resolver, anything else is a plain filesystem read.
fn read_bytes(path: &str) -> Result<Vec<u8>, String> {
    if path.starts_with("content://") {
        return saf::saf_read_uri(path).map(|(_, bytes)| bytes);
    }
    std::fs::read(path).map_err(|e| format!("couldn't read {path}: {e}"))
}

/// Write bytes: a `content://` SAF URI goes through the content resolver,
/// anything else is a crash-safe atomic filesystem write.
fn write_bytes(path: &str, bytes: &[u8]) -> Result<(), String> {
    if path.starts_with("content://") {
        return saf::saf_write_uri(path, bytes);
    }
    let target = std::path::Path::new(path);
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("couldn't create {}: {e}", parent.display()));
        }
    }
    craftapps_shared::services::write_atomic(target, bytes)
}

/// The system clipboard's text via Android's `ClipboardManager`.
///
/// Best effort: any failure (no clipboard, no primary clip, JNI error)
/// yields `None` rather than an error — the caller treats it as "no text".
fn android_clipboard_text() -> Option<String> {
    use jni::objects::JObject;
    // SAFETY: ndk-context hands us the JVM pointer the platform gave this
    // process; `from_raw` only wraps it.
    let vm = unsafe { jni::JavaVM::from_raw(ndk_context::android_context().vm().cast()) };
    // jni 0.22 attaches via a callback that receives the Env; the callback's
    // error type must convert from jni::errors::Error.
    let attached: Result<Option<String>, jni::errors::Error> =
        vm.attach_current_thread(|env: &mut jni::Env| {
            // SAFETY: the context pointer is the ANativeActivity* the platform
            // passed to android_main; `from_raw` only wraps it, and the env
            // outlives the object within this callback.
            let activity = unsafe { JObject::from_raw(env, ndk_context::android_context().context().cast()) };
            Ok(clipboard_text_with_env(env, &activity))
        });
    match attached {
        Ok(text) => text,
        Err(_) => None,
    }
}

/// Clipboard read given an attached JNI env. Every step is fallible;
/// any failure means "no text".
fn clipboard_text_with_env(env: &mut jni::Env, activity: &jni::objects::JObject) -> Option<String> {
    use jni::objects::JValue;
    let name = env.new_string("clipboard").ok()?;
    let cm = env
        .call_method(
            &activity,
            jni::jni_str!("getSystemService"),
            jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
            &[JValue::from(&name)],
        )
        .ok()?
        .l()
        .ok()?;
    if cm.is_null() {
        return None;
    }
    let clip = env
        .call_method(&cm, jni::jni_str!("getPrimaryClip"), jni::jni_sig!("()Landroid/content/ClipData;"), &[])
        .ok()?
        .l()
        .ok()?;
    if clip.is_null() {
        return None;
    }
    let count = env
        .call_method(&clip, jni::jni_str!("getItemCount"), jni::jni_sig!("()I"), &[])
        .ok()?
        .i()
        .ok()?;
    if count <= 0 {
        return None;
    }
    let item = env
        .call_method(
            &clip,
            jni::jni_str!("getItemAt"),
            jni::jni_sig!("(I)Landroid/content/ClipData$Item;"),
            &[JValue::from(0)],
        )
        .ok()?
        .l()
        .ok()?;
    if item.is_null() {
        return None;
    }
    let text = env
        .call_method(&item, jni::jni_str!("getText"), jni::jni_sig!("()Ljava/lang/CharSequence;"), &[])
        .ok()?
        .l()
        .ok()?;
    if text.is_null() {
        return None;
    }
    let as_string = env
        .call_method(&text, jni::jni_str!("toString"), jni::jni_sig!("()Ljava/lang/String;"), &[])
        .ok()?
        .l()
        .ok()?;
    if as_string.is_null() {
        return None;
    }
    // SAFETY: `toString()` on a CharSequence returns a java.lang.String, so
    // the raw jobject is a valid jstring; `from_raw` only wraps the pointer.
    let jstring = unsafe { jni::objects::JString::from_raw(env, as_string.as_raw().cast()) };
    let out: String = jstring.try_to_string(env).ok()?;
    if out.is_empty() { None } else { Some(out) }
}

/// A SAF open-dialog reply that delivers the picked file into DesignCraft's
/// async inbox with its original [`ImportRequest`] context.
struct InboxReply {
    inbox: Inbox,
    pending: Arc<Mutex<Option<ImportRequest>>>,
}

impl saf::DialogReply for InboxReply {
    fn send(self: Box<Self>, answer: Option<saf::DialogAnswer>) {
        let request = self.pending.lock().map(|mut s| s.take()).unwrap_or_else(|e| e.into_inner().take());
        let Some(request) = request else { return };
        let (name, bytes) = match answer {
            Some(saf::DialogAnswer::Contents(name, bytes)) => (name, bytes),
            _ => return,
        };
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.push(ImportedFile { request, name, bytes });
        }
    }
}

/// Builds the [`Services`] DesignCraft's UI needs on Android.
pub struct AndroidServices {
    storage: Arc<StorageManager>,
    inbox: Inbox,
    pending_request: Arc<Mutex<Option<ImportRequest>>>,
    /// Fallback save folder, used only when the save picker can't run or a
    /// pick can't be mapped to a path: `<suite media>/DesignCraft/Documents/`.
    documents_dir: PathBuf,
}

impl AndroidServices {
    pub fn new(storage: Arc<StorageManager>, documents_dir: PathBuf) -> Self {
        Self { storage, inbox: Arc::new(Mutex::new(Vec::new())), pending_request: Arc::new(Mutex::new(None)), documents_dir }
    }

    pub fn build(&self) -> Services {
        let storage = self.storage.clone();
        let inbox = self.inbox.clone();
        let pending = self.pending_request.clone();
        let documents_dir = self.documents_dir.clone();
        let save_storage = self.storage.clone();
        Services {
            // No synchronous pick_open: DesignCraft falls back to open_async +
            // inbox when it is unset, which fits Android's async SAF pickers.
            pick_open: None,
            // Save: the system create-document picker, via the shared file
            // bridge -- the same file method as PhotoCraft and FilmCraft.
            // `pick_save` is synchronous, so wait for the bridge's answer
            // here; the system picker is in front meanwhile, so the paused
            // frame behind it is never seen. The engine saves with
            // `std::fs::write`, so the picked SAF URI is mapped back to a
            // filesystem path (primary-volume picks; the common case). If
            // the picker can't run or the pick can't be mapped, fall back
            // to the deterministic Documents folder so work isn't lost.
            pick_save: Some(Box::new(move |name: &str| {
                let fallback = |file_name: &str| {
                    let safe_name = PathBuf::from(file_name).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Untitled.designcraft".to_string());
                    Some(documents_dir.join(safe_name).to_string_lossy().into_owned())
                };
                let ext = name.rsplit('.').next().unwrap_or("");
                let mime = saf::mime_for_extension(ext).unwrap_or("application/octet-stream").to_string();
                let id = match craftapps_shared::picker_bridge::request_picker(
                    craftapps_shared::picker_bridge::PickerKind::Save,
                    vec![mime],
                    name.to_string(),
                    false,
                ) {
                    Ok(id) => id,
                    Err(e) => {
                        log::error!("couldn't launch the save picker ({e}); using the fallback folder");
                        return fallback(name);
                    }
                };
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
                loop {
                    if let Some(resp) = craftapps_shared::picker_bridge::poll_response() {
                        if resp.id == id {
                            if resp.cancelled {
                                return None;
                            }
                            let picked_name = resp.name.clone().unwrap_or_else(|| name.to_string());
                            let Some(uri) = resp.uri else {
                                log::error!("save picker answered without a URI; using the fallback folder");
                                return fallback(&picked_name);
                            };
                            save_storage.record_uri_name(&uri, &picked_name);
                            return match external_storage_path(&uri) {
                                Some(path) => Some(path),
                                None => {
                                    log::error!("save pick {uri} is not a primary-volume path; using the fallback folder");
                                    fallback(&picked_name)
                                }
                            };
                        }
                        log::warn!("save picker: ignoring a response for request {}", resp.id);
                    }
                    if std::time::Instant::now() >= deadline {
                        log::error!("save picker timed out; using the fallback folder");
                        return fallback(name);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
            })),
            read: Some(Box::new(|p: &str| read_bytes(p))),
            write: Some(Box::new(move |p: &str, b: &[u8]| write_bytes(p, b))),
            open_async: Some(Box::new(move |request: ImportRequest| {
                if let Ok(mut slot) = pending.lock() {
                    *slot = Some(request.clone());
                }
                let reply: Box<dyn saf::DialogReply> =
                    Box::new(InboxReply { inbox: inbox.clone(), pending: pending.clone() });
                saf::set_pending_open(reply, false, storage.clone());
                let mimes = mime_types_for_purpose(&request.purpose);
                if let Err(e) = saf::launch_open_document(false, &mimes) {
                    log::error!("couldn't open the file picker: {e}");
                    if let Ok(mut slot) = pending.lock() {
                        *slot = None;
                    }
                }
            })),
            download: None,
            inbox: Some(self.inbox.clone()),
            clipboard_text: Some(Box::new(|| android_clipboard_text())),
        }
    }
}

/// Best-effort reachability check used at startup logging.
pub fn storage_state_line(storage: &StorageManager) -> String {
    format!("media: {}", storage.location_label())
}

/// Re-export so lib.rs can reference the JNI bridge version in logs.
pub fn bridge_version_note() -> String {
    match jni_bridge::android_sdk_int() {
        Ok(v) => format!("Android SDK {v}"),
        Err(e) => format!("Android SDK unknown ({e})"),
    }
}

/// `content://com.android.externalstorage.documents/...` -> a filesystem
/// path on the primary volume, or `None` for any other provider/volume.
/// Same mapping FilmCraft's port uses for its save dialog.
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
