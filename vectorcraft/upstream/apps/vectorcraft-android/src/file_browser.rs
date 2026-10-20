//! In-app file browser for the Android port.
//!
//! The system (SAF) pickers cover arbitrary locations, but day-to-day work
//! lives in the suite's shared media tree (`/sdcard/CraftApps/Media`). This
//! egui browser navigates that tree without leaving the app: open (single or
//! multi), save (with a name field), and folder picking all return real
//! filesystem paths the engine can read and write directly.
//!
//! The browser is driven through [`FileBrowser`]: the port's `start_pick`
//! implementation opens it with a [`BrowserRequest`] and a reply channel;
//! [`FileBrowser::show`] draws it each frame until the user confirms,
//! cancels, or asks for the system picker (which the caller then launches).

use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// What the browser is picking.
#[derive(Clone, Debug)]
pub enum BrowserMode {
    /// Pick one file to open.
    Open,
    /// Pick several files (File → Place…).
    OpenMany,
    /// Pick a folder and a file name to save.
    Save,
    /// Pick a folder (Relink to Folder, Package…).
    Folder,
}

/// A pick request for the browser.
#[derive(Clone, Debug)]
pub struct BrowserRequest {
    pub mode: BrowserMode,
    /// Lowercase extensions without the dot, e.g. `["svg", "vectorcraft"]`.
    /// Empty = show everything.
    pub extensions: Vec<String>,
    /// Suggested file name (Save mode).
    pub suggested_name: String,
    /// Title for the browser window.
    pub title: String,
}

#[derive(Debug)]
struct Entry {
    path: PathBuf,
    is_dir: bool,
    name: String,
}

/// Outcome of one [`FileBrowser::show`] call.
#[derive(Debug)]
pub enum BrowserEvent {
    /// Still open; keep drawing.
    None,
    /// Browser finished with these paths (empty = cancelled).
    Done(Vec<String>),
    /// The user asked for the system picker: launch SAF and reply on this channel.
    SystemPicker(Sender<Vec<String>>, BrowserRequest),
}

pub struct FileBrowser {
    root: PathBuf,
    current: PathBuf,
    entries: Vec<Entry>,
    selected: Vec<PathBuf>,
    save_name: String,
    request: Option<(BrowserRequest, Sender<Vec<String>>)>,
    mode: BrowserMode,
}

impl FileBrowser {
    pub fn new(root: PathBuf) -> Self {
        let current = root.clone();
        let mut this = Self {
            root,
            current,
            entries: Vec::new(),
            selected: Vec::new(),
            save_name: String::new(),
            request: None,
            mode: BrowserMode::Open,
        };
        this.refresh();
        this
    }

    pub fn is_open(&self) -> bool {
        self.request.is_some()
    }

    /// Open the browser for `request`; the answer goes to `tx`.
    pub fn open(&mut self, request: BrowserRequest, tx: Sender<Vec<String>>) {
        self.mode = request.mode.clone();
        // Stay inside the media tree; start at the root each time.
        self.current = self.root.clone();
        self.selected.clear();
        self.save_name = request.suggested_name.clone();
        self.refresh();
        self.request = Some((request, tx));
    }

    fn refresh(&mut self) {
        self.entries.clear();
        let Ok(read) = std::fs::read_dir(&self.current) else { return };
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let is_dir = path.is_dir();
            let e = Entry { path, is_dir, name };
            if is_dir {
                dirs.push(e);
            } else {
                files.push(e);
            }
        }
        dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        // Extension filter applies to files only; folders always show.
        let exts: Vec<String> = self
            .request
            .as_ref()
            .map(|(r, _)| r.extensions.clone())
            .unwrap_or_default();
        self.entries.extend(dirs);
        for f in files {
            if exts.is_empty() || has_extension(&f.name, &exts) {
                self.entries.push(f);
            }
        }
    }

    fn finish(&mut self, event: BrowserEvent) -> BrowserEvent {
        match event {
            BrowserEvent::None => BrowserEvent::None,
            BrowserEvent::Done(paths) => {
                if let Some((_, tx)) = self.request.take() {
                    let _ = tx.send(paths);
                }
                BrowserEvent::None
            }
            BrowserEvent::SystemPicker(_, _) => {
                // Rebuilt by show() with the live request; never constructed here.
                BrowserEvent::None
            }
        }
    }

    /// Draw the browser window. Call every frame while [`Self::is_open`].
    /// Returns an event for the caller to act on.
    pub fn show(&mut self, ctx: &egui::Context) -> BrowserEvent {
        if self.request.is_none() {
            return BrowserEvent::None;
        }
        let title = self
            .request
            .as_ref()
            .map(|(r, _)| r.title.clone())
            .unwrap_or_else(|| "Browse".to_string());
        let mut outcome = BrowserEvent::None;
        let mut navigate_to: Option<PathBuf> = None;
        let mut system_picker = false;

        egui::Window::new(title)
            .collapsible(false)
            .resizable(true)
            .default_size([420.0, 480.0])
            .show(ctx, |ui| {
                // Current folder + Up.
                ui.horizontal(|ui| {
                    if ui.button("⬆ Up").clicked() {
                        if let Some(parent) = self.current.parent() {
                            if parent.starts_with(&self.root) {
                                navigate_to = Some(parent.to_path_buf());
                            }
                        }
                    }
                    ui.label(self.current.to_string_lossy().to_string());
                });
                ui.separator();

                // Save mode: name field.
                if matches!(self.mode, BrowserMode::Save) {
                    ui.horizontal(|ui| {
                        ui.label("Name:");
                        ui.text_edit_singleline(&mut self.save_name);
                    });
                    ui.separator();
                }

                // Entries.
                let multi = matches!(self.mode, BrowserMode::OpenMany);
                let mut toggled: Vec<PathBuf> = Vec::new();
                let mut opened_dir: Option<PathBuf> = None;
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for e in &self.entries {
                        let label = if e.is_dir {
                            format!("📁 {}", e.name)
                        } else {
                            format!("📄 {}", e.name)
                        };
                        if e.is_dir {
                            if ui.button(label).clicked() {
                                opened_dir = Some(e.path.clone());
                            }
                        } else if multi {
                            let mut sel = self.selected.contains(&e.path);
                            if ui.checkbox(&mut sel, label).changed() {
                                toggled.push(e.path.clone());
                            }
                        } else if matches!(self.mode, BrowserMode::Folder) {
                            ui.label(label);
                        } else if ui.button(label).clicked() {
                            // Single-open: confirm at once.
                            self.selected = vec![e.path.clone()];
                            outcome = BrowserEvent::Done(vec![e.path.to_string_lossy().to_string()]);
                        }
                    }
                });
                for p in toggled {
                    if self.selected.contains(&p) {
                        self.selected.retain(|x| x != &p);
                    } else {
                        self.selected.push(p);
                    }
                }
                if let Some(d) = opened_dir {
                    navigate_to = Some(d);
                }

                ui.separator();
                // Actions.
                ui.horizontal(|ui| {
                    if ui.button("System files…").clicked() {
                        system_picker = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            outcome = BrowserEvent::Done(vec![]);
                        }
                        let confirm_label = match self.mode {
                            BrowserMode::Open => "Open",
                            BrowserMode::OpenMany => "Place",
                            BrowserMode::Save => "Save",
                            BrowserMode::Folder => "Select Folder",
                        };
                        let can_confirm = match self.mode {
                            BrowserMode::OpenMany => !self.selected.is_empty(),
                            BrowserMode::Save => !self.save_name.trim().is_empty(),
                            BrowserMode::Folder => true,
                            BrowserMode::Open => false, // single-open confirms on click
                        };
                        if can_confirm && ui.button(confirm_label).clicked() {
                            let paths = match self.mode {
                                BrowserMode::Save => {
                                    let mut p = self.current.join(self.save_name.trim());
                                    if p.extension().is_none() {
                                        if let Some(ext) = self
                                            .request
                                            .as_ref()
                                            .and_then(|(r, _)| r.extensions.first())
                                        {
                                            p.set_extension(ext);
                                        }
                                    }
                                    vec![p.to_string_lossy().to_string()]
                                }
                                BrowserMode::Folder => {
                                    vec![self.current.to_string_lossy().to_string()]
                                }
                                _ => self
                                    .selected
                                    .iter()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .collect(),
                            };
                            outcome = BrowserEvent::Done(paths);
                        }
                    });
                });
            });

        if let Some(d) = navigate_to {
            self.current = d;
            self.selected.clear();
            self.refresh();
        }
        if system_picker {
            // Convert to the handoff event with the live channel and request.
            if let Some((req, tx)) = self.request.take() {
                return BrowserEvent::SystemPicker(tx, req);
            }
            return BrowserEvent::None;
        }
        self.finish(outcome)
    }
}

fn has_extension(name: &str, exts: &[String]) -> bool {
    let lower = name.to_lowercase();
    exts.iter().any(|e| {
        let e = e.to_lowercase();
        lower.ends_with(&format!(".{e}")) || lower == e
    })
}

