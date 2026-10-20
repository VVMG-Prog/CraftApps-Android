//! Android services for the PdfCraft port: shared suite storage.
//!
//! The port keeps documents under the suite's shared media tree
//! (`/sdcard/CraftApps/Media/PdfCraft`) when "All files access" is granted,
//! falling back to the app-private files dir otherwise. Settings live in
//! `/sdcard/CraftApps/Settings/PdfCraft`.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use craftapps_shared::storage::{StorageAppInfo, StorageManager};
use std::path::PathBuf;
use std::sync::Arc;

const APP_INFO: StorageAppInfo = StorageAppInfo {
    app_name: "PdfCraft",
    settings_folder: "PdfCraft",
};

/// Media subdirectory for this app's documents.
const MEDIA_SUBDIR: &str = "PdfCraft";

/// Build the shared [`StorageManager`]. `cache_dir` comes from the JNI bridge
/// (the app's cache dir).
pub fn init_storage(cache_dir: &PathBuf) -> Result<Arc<StorageManager>, String> {
    let storage = Arc::new(StorageManager::new(cache_dir, &APP_INFO)?);
    storage.ensure_dirs()?;
    // Tell the portable UI where output-folder picks resolve
    // (`/sdcard/CraftApps/Media/PdfCraft`).
    let out_base = app_media_dir(&storage);
    pdfcraft_ui_egui::android::set_output_base(out_base);
    Ok(storage)
}

/// This app's media directory: `<CraftApps>/Media/PdfCraft`.
pub fn app_media_dir(storage: &StorageManager) -> PathBuf {
    // Per-app subdir keeps the suite's apps from colliding in shared storage.
    let dir = storage.tablet_media_dir().join(MEDIA_SUBDIR);
    let _ = std::fs::create_dir_all(&dir);
    dir
}
