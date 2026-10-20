//! In-app file browser plumbing for LightCraft on Android.
//!
//! Android has no synchronous file dialog that eframe can call: the Storage
//! Access Framework is activity-based and answers later through
//! `onActivityResult`. This module builds the async
//! [`Services::picker`](lightcraft_ui_egui::Services::picker) the UI crate
//! expects: it launches the system picker and hands back a channel receiver.
//! `pick::poll` (in the UI crate) resumes the issuing command when the dialog
//! closes.
//!
//! Picked single files are staged into the suite's SAF inbox directory so the
//! engine always gets a plain path it can read; folder and save picks answer
//! with the `content://` URI the `write` service knows how to write to.

use craftapps_shared::{saf, services as shared_services, storage};
use lightcraft_ui_egui::pick::{PickKind, PickRequest};
use std::sync::Arc;
use std::sync::mpsc::Receiver;

/// Build the async SAF picker bound to the suite storage manager.
pub fn build_picker(
    storage: Arc<storage::StorageManager>,
) -> Box<dyn FnMut(PickRequest) -> Result<Receiver<Vec<String>>, String>> {
    let storage_for_picker = storage.clone();
    Box::new(move |req: PickRequest| {
        let (tx, rx) = std::sync::mpsc::channel();
        let storage = storage_for_picker.clone();
        match req.kind {
            PickKind::Files | PickKind::File => {
                let multiple = req.kind == PickKind::Files;
                let mimes: Vec<String> = req
                    .filter
                    .as_ref()
                    .map(|(_, exts)| {
                        exts.iter().filter_map(|e| saf::mime_for_extension(e)).map(|m| m.to_string()).collect()
                    })
                    .unwrap_or_default();
                let mimes = if mimes.is_empty() { vec!["*/*".to_string()] } else { mimes };
                struct OpenReply {
                    tx: std::sync::mpsc::Sender<Vec<String>>,
                    storage: Arc<storage::StorageManager>,
                }
                impl saf::DialogReply for OpenReply {
                    fn send(self: Box<Self>, answer: Option<saf::DialogAnswer>) {
                        let paths = match answer {
                            // Single file: stage the bytes into the SAF inbox so the
                            // engine gets a real path it can read.
                            Some(saf::DialogAnswer::Contents(name, bytes)) => {
                                let dest = self.storage.saf_inbox_dir().join(safe_name(&name));
                                match shared_services::write_atomic(&dest, &bytes) {
                                    Ok(()) => vec![dest.to_string_lossy().to_string()],
                                    Err(e) => {
                                        log::error!("staging picked file: {e}");
                                        vec![]
                                    }
                                }
                            }
                            // Multi-pick: already staged to paths by the SAF layer.
                            Some(saf::DialogAnswer::Paths(paths)) => paths,
                            _ => vec![],
                        };
                        let _ = self.tx.send(paths);
                    }
                }
                let reply: Box<dyn saf::DialogReply> = Box::new(OpenReply { tx: tx.clone(), storage });
                saf::set_pending_open(reply, multiple, storage_for_picker.clone());
                if let Err(e) = saf::launch_open_document(multiple, &mimes) {
                    log::error!("couldn't open the file picker: {e}");
                    let _ = tx.send(vec![]);
                }
            }
            PickKind::Folder => {
                struct FolderReply {
                    tx: std::sync::mpsc::Sender<Vec<String>>,
                }
                impl saf::DialogReply for FolderReply {
                    fn send(self: Box<Self>, answer: Option<saf::DialogAnswer>) {
                        let paths = match answer {
                            Some(saf::DialogAnswer::Folder(uri)) => vec![uri],
                            _ => vec![],
                        };
                        let _ = self.tx.send(paths);
                    }
                }
                let reply: Box<dyn saf::DialogReply> = Box::new(FolderReply { tx: tx.clone() });
                saf::set_pending_tree(Some(reply), storage_for_picker.clone());
                if let Err(e) = saf::launch_open_document_tree_bridge() {
                    log::error!("couldn't open the folder picker: {e}");
                    let _ = tx.send(vec![]);
                }
            }
            PickKind::Save => {
                let name = req.file_name.clone().unwrap_or_else(|| "untitled".to_string());
                let mime = name
                    .rsplit('.')
                    .next()
                    .and_then(saf::mime_for_extension)
                    .unwrap_or("application/octet-stream")
                    .to_string();
                struct SaveReply {
                    tx: std::sync::mpsc::Sender<Vec<String>>,
                }
                impl saf::DialogReply for SaveReply {
                    fn send(self: Box<Self>, answer: Option<saf::DialogAnswer>) {
                        let paths = match answer {
                            // The create-document answer is a content:// URI the
                            // `write` service below knows how to write to.
                            Some(saf::DialogAnswer::SaveTo(uri)) => vec![uri],
                            _ => vec![],
                        };
                        let _ = self.tx.send(paths);
                    }
                }
                let reply: Box<dyn saf::DialogReply> = Box::new(SaveReply { tx: tx.clone() });
                saf::set_pending_create(reply, name.clone(), storage_for_picker.clone());
                if let Err(e) = saf::launch_create_document(&name, &mime) {
                    log::error!("couldn't open the save picker: {e}");
                    let _ = tx.send(vec![]);
                }
            }
        }
        Ok(rx)
    })
}

/// Sanitize a picked file name for the staging directory.
fn safe_name(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "picked".to_string());
    base.chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
