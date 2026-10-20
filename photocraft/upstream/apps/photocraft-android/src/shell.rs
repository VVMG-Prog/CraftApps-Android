//! The eframe shell: the shared [`PhotocraftApp`] plus the Android-only UI
//! (the file-browser dialogs, the updater, the pen settings panel, and the
//! first-run storage permission gate) drawn on top of it.
//!
//! Upstream crates are not modified for the port; everything Android-specific
//! lives in this crate and is composed here.

use crate::file_browser::FileBrowser;
use craftapps_shared::gestures::GestureDetector;
use craftapps_shared::modifiers::ModifierKeys;
use craftapps_shared::pen::PenSettings;
use craftapps_shared::updater::Updater;
use eframe::egui;
use photocraft_ui_egui::PhotocraftApp;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct AndroidShell {
    inner: PhotocraftApp,
    browser: Arc<Mutex<FileBrowser>>,
    updater: Arc<Mutex<Updater>>,
    /// First-run shared-storage permission screen (`None` = resolved).
    gate: Option<StorageGate>,
    /// Tool to restore when a momentary pen-button action (Pan) releases.
    pen_prev_tool: Option<photocraft_ui_egui::Tool>,
    /// Pen settings (shared with the JNI runtime) and where they persist.
    pen_settings: Arc<Mutex<PenSettings>>,
    pen_config_dir: PathBuf,
    /// Whether the stylus settings panel is open.
    stylus_open: bool,
    /// Tool wheel popup state: (x, y) in egui points, or None if closed.
    tool_wheel: Option<egui::Pos2>,
    /// Multi-finger tap recognition (the "stylus mode" gestures).
    gestures: GestureDetector,
    /// Which modifier keys are held (winit's Android backend sends no
    /// `ModifiersChanged`, so they are tracked from the key events).
    modifiers: ModifierKeys,
    /// The history length at the end of the last frame, used to tell whether a
    /// multi-finger tap painted something of its own.
    history_len: usize,
}

impl AndroidShell {
    pub fn new(
        app: PhotocraftApp,
        browser: Arc<Mutex<FileBrowser>>,
        updater: Arc<Mutex<Updater>>,
        gate: Option<StorageGate>,
        pen_settings: Arc<Mutex<PenSettings>>,
        pen_config_dir: PathBuf,
    ) -> Self {
        AndroidShell {
            inner: app,
            browser,
            updater,
            gate,
            pen_prev_tool: None,
            pen_settings,
            pen_config_dir,
            stylus_open: false,
            tool_wheel: None,
            gestures: GestureDetector::new(),
            modifiers: ModifierKeys::default(),
            history_len: 0,
        }
    }

    /// Feed the pen side-channel state into the app's stylus feed (eraser end
    /// detection; pressure arrives through winit touch events on its own).
    fn feed_stylus(&mut self) {
        use craftapps_shared::pen::PenTool;
        let tool = craftapps_shared::pen::current_tool();
        let eraser = matches!(tool, PenTool::Eraser);
        // Only push when the eraser state changes; pressure/tilt come from
        // the touch pipeline. A `None` sample means "no pen down".
        let want = if craftapps_shared::pen::stylus_down() {
            Some(photocraft_ui_egui::stylus::PenSample { eraser, ..Default::default() })
        } else {
            None
        };
        // Avoid spamming the feed when nothing changed.
        let current_eraser = self.inner.stylus.feed.get().map(|s| s.eraser);
        if want.map(|s| s.eraser) != current_eraser || want.is_some() != current_eraser.is_some() {
            self.inner.stylus.feed.set(want);
        }
    }

    /// Multi-finger taps (the "stylus mode" gestures) and the keyboard
    /// modifiers. Both run before the app's frame, so the app sees the
    /// modifiers and the gesture's side effects in the same frame.
    fn process_input(&mut self, ctx: &egui::Context) {
        // Pen input pump: collects stylus states on its own thread and
        // wakes the app when a pen-button event lands, so a button press
        // while hovering dispatches immediately instead of waiting for
        // the next touch to run a frame. Idempotent; first call starts it.
        craftapps_shared::pen::install_pen_pump(ctx.clone());
        // Keyboard modifiers: winit's Android backend never sends
        // `ModifiersChanged`, so egui's modifier state is empty and every
        // Ctrl/Shift/Alt shortcut is dead without this.
        self.modifiers.update(ctx);
        self.modifiers.publish(ctx);

        // The stylus data of the last motion event: the tool type (stylus,
        // eraser end, palm) and the side-button state. winit drops both, so
        // the vendored android-activity records them on the way past.
        self.feed_stylus_state();

        // The history length before this frame's work: a multi-finger tap
        // arrives at the canvas as a click, which can paint a dab of its own.
        let history_now = self.inner.session.active().map(|st| st.history.past_len()).unwrap_or(0);
        let history_before = self.history_len;
        self.history_len = history_now;

        if let Some(tap) = self.gestures.update(ctx) {
            // Undo whatever the tap itself painted, so the gesture is the only
            // thing the tap does.
            if history_now > history_before && self.tap_painted_a_stroke() {
                if let Err(e) = self.inner.session.execute("edit.undo", serde_json::Value::Null) {
                    log::warn!("undoing the tap's stroke: {e}");
                }
            }
            // The shared runtime maps the finger count to the configured
            // action (and ignores the gesture when stylus mode is off).
            let ppp = ctx.pixels_per_point();
            craftapps_shared::pen::on_gesture(
                tap.fingers as i32,
                tap.pos.x * ppp,
                tap.pos.y * ppp,
            );
        }
    }

    /// Hand the stylus data of the motion events to the shared pen runtime:
    /// the tool type (stylus / eraser end / palm) and the side button (the
    /// S Pen's barrel button).
    ///
    /// The runtime keeps the pressed state and only acts on the transitions,
    /// so the same state twice is harmless.
    fn feed_stylus_state(&mut self) {
        // Android tool types.
        const TOOL_FINGER: i32 = 1;
        const TOOL_MOUSE: i32 = 3;
        // The stylus' side buttons.
        const STYLUS_BUTTONS: u32 = (1 << 5) | (1 << 6);

        // States arrive via the shared pen pump when it is running (it
        // notes them for the diagnostics itself); otherwise drain the
        // Android queue directly, as before.
        let states = if craftapps_shared::pen::pen_pump_active() {
            craftapps_shared::pen::take_queued_states()
        } else {
            let raw = android_activity::input::drain_stylus_states();
            for state in &raw {
                craftapps_shared::pen::note_queue_state(
                    state.action,
                    state.buttons,
                    state.action_button as i32,
                    state.tool_type,
                );
            }
            raw
        };
        for state in states {
            // A finger's touch is not the pen's business: it must not clear
            // the pen's state (the pen may well still be down).
            if (state.tool_type == TOOL_FINGER || state.tool_type == TOOL_MOUSE)
                && state.buttons & STYLUS_BUTTONS == 0
            {
                continue;
            }
            // The pen runtime speaks the Java protocol: the Android action
            // codes (0 down, 1 up, 2 move, 3 cancel) and the `BUTTON_*` bits.
            craftapps_shared::pen::on_pen_event(
                state.action,
                state.tool_type,
                state.buttons as i32,
                state.action_button as i32,
                state.x,
                state.y,
            );
        }
    }

    /// Did the tap paint a stroke of its own? The history's last step names
    /// the tool that made it.
    fn tap_painted_a_stroke(&self) -> bool {
        let Some(state) = self.inner.session.active() else { return false };
        let Some(label) = state.history.undo_label() else { return false };
        let label = label.to_ascii_lowercase();
        label.contains("brush") || label.contains("pencil") || label.contains("eraser")
    }

    /// Shared [`craftapps_shared::pen`] actions → PhotoCraft tools. Runs after
    /// the app frame so dialogs/menus are settled.
    fn dispatch_pen_actions(&mut self, ctx: &egui::Context) {
        for qa in craftapps_shared::pen::drain_actions() {
            use craftapps_shared::pen::PenAction;
            match (qa.action, qa.pressed) {
                (PenAction::None, _) => {}
                (PenAction::Undo, true) => {
                    if let Err(e) = self.inner.session.execute("edit.undo", serde_json::Value::Null) {
                        log::warn!("pen undo: {e}");
                    }
                }
                (PenAction::Redo, true) => {
                    if let Err(e) = self.inner.session.execute("edit.redo", serde_json::Value::Null) {
                        log::warn!("pen redo: {e}");
                    }
                }
                (PenAction::ToggleEraser, true) => {
                    let cur = self.inner.ui.tool;
                    self.inner.ui.tool = if matches!(cur, photocraft_ui_egui::Tool::Eraser) {
                        photocraft_ui_egui::Tool::Brush
                    } else {
                        photocraft_ui_egui::Tool::Eraser
                    };
                }
                (PenAction::Eyedropper, true) => {
                    self.inner.ui.tool = photocraft_ui_egui::Tool::Eyedropper;
                }
                (PenAction::PanTool, true) => {
                    // Momentary: remember the current tool, switch to Hand.
                    if self.pen_prev_tool.is_none() {
                        self.pen_prev_tool = Some(self.inner.ui.tool);
                    }
                    self.inner.ui.tool = photocraft_ui_egui::Tool::Hand;
                }
                (PenAction::PanTool, false) => {
                    if let Some(prev) = self.pen_prev_tool.take() {
                        self.inner.ui.tool = prev;
                    }
                }
                (PenAction::RightClick, true) => {
                    // Synthesize a right-click (Secondary button) at the
                    // position from the pen event. The Java side passes the
                    // MotionEvent coordinates, which work for hover button
                    // presses where egui has no pointer position. Convert
                    // from Android pixels to egui points.
                    let ppp = ctx.pixels_per_point();
                    let pos = egui::pos2(qa.x / ppp, qa.y / ppp);
                    ctx.input_mut(|i| {
                        // Move the pointer to the press location first, so
                        // the right-click lands under the pen.
                        i.events.push(egui::Event::PointerMoved(pos));
                        i.events.push(egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Secondary,
                            pressed: true,
                            modifiers: egui::Modifiers::default(),
                        });
                        i.events.push(egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Secondary,
                            pressed: false,
                            modifiers: egui::Modifiers::default(),
                        });
                    });
                    // Ensure the next frame processes the injected events
                    // promptly.
                    ctx.request_repaint();
                }
                (PenAction::ToolWheel, true) => {
                    // Open the radial tool wheel at the tap position.
                    let ppp = ctx.pixels_per_point();
                    let pos = egui::pos2(qa.x / ppp, qa.y / ppp);
                    self.tool_wheel = Some(pos);
                    ctx.request_repaint();
                }
                // Release events for one-shot actions are ignored.
                (_, false) => {}
            }
        }
    }

    /// Small Android toolbar: Stylus settings and Check for Updates. Drawn in
    /// the top-right corner, out of the canvas's way.
    /// Stylus settings window (opened via Edit > Stylus Settings menu).
    fn stylus_settings_window(&mut self, ctx: &egui::Context) {
        if self.stylus_open {
            let mut open = true;
            egui::Window::new("Stylus Settings")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_size([380.0, 420.0])
                .show(ctx, |ui| {
                    let mut settings = self
                        .pen_settings
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .clone();
                    let mut changed = false;
                    craftapps_shared::pen::pen_settings_ui(ui, &mut settings, &mut || changed = true);
                    if changed {
                        craftapps_shared::pen::sync_settings(&settings);
                        if let Ok(mut guard) = self.pen_settings.lock() {
                            *guard = settings.clone();
                        }
                        if let Err(e) = settings.save(&self.pen_config_dir) {
                            log::error!("pen settings save: {e}");
                        }
                    }
                });
            if !open {
                self.stylus_open = false;
            }
        }
    }

    /// Radial tool wheel popup, opened by 4-finger tap in stylus mode.
    /// Shows quick tool buttons around the tap point; tapping a tool selects
    /// it and closes the wheel. Tapping outside closes without changing.
    fn tool_wheel_popup(&mut self, ctx: &egui::Context) {
        let Some(center) = self.tool_wheel else { return };
        // Close on any pointer press outside the wheel buttons.
        // The wheel itself is a small panel; use a Window without title.
        let mut keep_open = true;
        egui::Window::new("tool_wheel")
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .fixed_pos(center - egui::vec2(90.0, 90.0))
            .fixed_size([180.0, 180.0])
            .show(ctx, |ui| {
                ui.label("Tools");
                ui.add_space(4.0);
                // Grid of tool buttons
                let tools: [(photocraft_ui_egui::Tool, &str); 6] = [
                    (photocraft_ui_egui::Tool::Brush, "Brush"),
                    (photocraft_ui_egui::Tool::Eraser, "Eraser"),
                    (photocraft_ui_egui::Tool::Eyedropper, "Picker"),
                    (photocraft_ui_egui::Tool::Hand, "Hand"),
                    (photocraft_ui_egui::Tool::Lasso, "Lasso"),
                    (photocraft_ui_egui::Tool::PaintBucket, "Fill"),
                ];
                egui::Grid::new("tool_wheel_grid")
                    .num_columns(2)
                    .show(ui, |ui| {
                        for (i, (tool, label)) in tools.iter().enumerate() {
                            let is_current = self.inner.ui.tool == *tool;
                            let btn = ui.selectable_label(is_current, *label);
                            if btn.clicked() {
                                self.inner.ui.tool = *tool;
                                keep_open = false;
                            }
                            if i % 2 == 1 {
                                ui.end_row();
                            }
                        }
                    });
                ui.add_space(4.0);
                if ui.button("Close").clicked() {
                    keep_open = false;
                }
            });
        if !keep_open {
            self.tool_wheel = None;
        }
        // Also close if the user taps elsewhere (pointer pressed outside).
        // We check this by seeing if the window lost focus, but a simpler
        // approach: close on next frame if a pointer press happened outside.
        // For now, the Close button and tool selection handle dismissal.
    }
}

impl eframe::App for AndroidShell {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Modifiers and gestures first: the app must see the modifier state
        // and a cleaned-up tap in the frame they happen.
        self.process_input(ctx);
        self.inner.logic(ctx, frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // The storage gate blocks the whole app until the user grants shared
        // storage or explicitly chooses internal storage. Nothing behind it is
        // reachable, so there is no dead end.
        if let Some(gate) = self.gate.as_mut() {
            if gate.draw(ui) {
                self.gate = None;
            }
            return;
        }
        self.inner.ui(ui, frame);
        // Pen side-channel → stylus feed, then button remap dispatch. Runs
        // after the app frame so dialogs/menus are settled.
        self.feed_stylus();
        let ctx2 = ui.ctx().clone();
        self.dispatch_pen_actions(&ctx2);
        // Android menu actions (from Help/Settings menus, replacing the old
        // floating toolbar). Drain the queue and act on them.
        for action in photocraft_ui_egui::menus::drain_android_menu_actions() {
            match action {
                photocraft_ui_egui::menus::AndroidMenuAction::OpenStylusSettings => {
                    self.stylus_open = true;
                }
                photocraft_ui_egui::menus::AndroidMenuAction::CheckForUpdates => {
                    if let Ok(mut u) = self.updater.lock() {
                        u.open();
                    }
                }
            }
        }
        // Overlays drawn after the app so they sit on top.
        let ctx = ui.ctx().clone();
        // Stylus settings window (opened via Edit > Stylus Settings menu)
        self.stylus_settings_window(&ctx);
        // Tool wheel popup (opened via 4-finger tap in stylus mode)
        self.tool_wheel_popup(&ctx);
        if let Ok(mut browser) = self.browser.try_lock() {
            browser.show(&ctx);
        }
        let mut updater = self.updater.lock().unwrap_or_else(|e| e.into_inner());
        updater.poll();
        updater.show(&ctx);
    }
}

/// First-run screen shown when "All files access" isn't granted yet.
/// Either path resolves: grant (then restart into shared-storage mode) or a
/// deliberate, persisted choice of internal storage.
pub struct StorageGate {
    /// `<internal>/storage-choice.json`; `{"mode":"internal"}` stops the nag.
    choice_file: PathBuf,
    stage: GateStage,
    error: Option<String>,
}

enum GateStage {
    /// Explain shared storage: [Grant access] [Use internal storage].
    Ask,
    /// System settings were opened: [I've granted it — continue] [Use internal storage].
    WaitingGrant,
    /// Grant confirmed: [Restart now].
    Restart,
}

impl StorageGate {
    pub fn new(choice_file: PathBuf) -> Self {
        StorageGate { choice_file, stage: GateStage::Ask, error: None }
    }

    fn choose_internal(&self) {
        // Best effort: if the write fails the gate simply shows again next
        // launch, which is the safe direction.
        let _ = std::fs::write(&self.choice_file, "{\"mode\":\"internal\"}");
    }

    /// Draw the gate. Returns true once the user has resolved it.
    fn draw(&mut self, ui: &mut egui::Ui) -> bool {
        let mut resolved = false;
        ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.heading("Where should PhotoCraft keep your work?");
                ui.add_space(12.0);
                match self.stage {
                    GateStage::Ask => {
                        ui.label(
                            "PhotoCraft and its sibling craft apps share two folders on your tablet:\n\
                             /CraftApps/Media for your documents, and /CraftApps/Settings for \
                             preferences, presets and recovery data.\n\n\
                             Grant \"All files access\" so the app can use them.",
                        );
                        ui.add_space(16.0);
                        if ui.button("Grant access").clicked() {
                            match craftapps_shared::jni_bridge::open_all_files_settings() {
                                Ok(()) => self.stage = GateStage::WaitingGrant,
                                Err(e) => self.error = Some(e),
                            }
                        }
                        ui.add_space(8.0);
                        if ui.button("Use internal storage instead").clicked() {
                            self.choose_internal();
                            resolved = true;
                        }
                        ui.add_space(8.0);
                        ui.label(
                            "Internal storage keeps everything inside the app's private folder. \
                             It works, but the other craft apps can't see your files.",
                        );
                    }
                    GateStage::WaitingGrant => {
                        ui.label(
                            "The system settings should now be open. Turn on \"Allow all files \
                             access\" for PhotoCraft, then come back here.",
                        );
                        ui.add_space(16.0);
                        if ui.button("I've granted it — continue").clicked() {
                            match craftapps_shared::jni_bridge::is_external_storage_manager() {
                                Ok(true) => self.stage = GateStage::Restart,
                                Ok(false) => {
                                    self.error = Some(
                                        "Still not granted — flip the switch in the system \
                                         settings, then try again."
                                            .to_string(),
                                    )
                                }
                                Err(e) => self.error = Some(e),
                            }
                        }
                        ui.add_space(8.0);
                        if ui.button("Use internal storage instead").clicked() {
                            self.choose_internal();
                            resolved = true;
                        }
                    }
                    GateStage::Restart => {
                        ui.label(
                            "Shared storage is on. Restart PhotoCraft once to switch to the \
                             shared folders.",
                        );
                        ui.add_space(16.0);
                        if ui.button("Restart now").clicked() {
                            // Never returns; the system restarts the activity.
                            let _ = craftapps_shared::jni_bridge::restart_process();
                        }
                    }
                }
                if let Some(e) = self.error.as_deref() {
                    ui.add_space(8.0);
                    ui.colored_label(egui::Color32::from_rgb(220, 90, 90), e);
                }
            });
        resolved
    }
}
