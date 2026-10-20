//! eframe shell: owns the [`VectorcraftApp`], draws the in-app file browser
//! overlay each frame, and forwards system-picker handoffs to SAF.

use std::sync::{Arc, Mutex};

use eframe::egui;
use vectorcraft_ui_egui::VectorcraftApp;

use crate::file_browser::{BrowserEvent, FileBrowser};
use crate::services::AndroidServices;

use craftapps_shared::gestures::GestureDetector;
use craftapps_shared::modifiers::ModifierKeys;

/// The host's `eframe::App`: the VectorCraft UI plus the Android browser overlay.
pub struct ShellApp {
    app: VectorcraftApp,
    browser: Arc<Mutex<FileBrowser>>,
    services: AndroidServices,
    /// Multi-finger tap recognition (the "stylus mode" gestures).
    gestures: GestureDetector,
    /// Which modifier keys are held (winit's Android backend sends no
    /// `ModifiersChanged`, so they are tracked from the key events).
    modifiers: ModifierKeys,
    /// The undo history length at the end of the last frame, used to tell
    /// whether a multi-finger tap painted something of its own.
    history_len: usize,
}

impl ShellApp {
    pub fn new(app: VectorcraftApp, services: AndroidServices) -> Self {
        let browser = services.browser.clone();
        Self {
            app,
            browser,
            services,
            gestures: GestureDetector::new(),
            modifiers: ModifierKeys::default(),
            history_len: 0,
        }
    }

    /// Multi-finger taps (the "stylus mode" gestures) and the keyboard
    /// modifiers. Both run before the app's frame, so the app sees the
    /// modifiers and the gesture's side effects in the same frame.
    fn process_input(&mut self, ctx: &egui::Context) {
        // Pen input pump: wakes the app on pen-button transitions so a
        // hover press dispatches immediately instead of waiting for the
        // next touch to run a frame (the PhotoCraft 0.4.5 fix).
        craftapps_shared::pen::install_pen_pump(ctx.clone());
        // Keyboard modifiers: winit's Android backend never sends
        // `ModifiersChanged`, so egui's modifier state is empty and every
        // Ctrl/Shift/Alt shortcut is dead without this.
        self.modifiers.update(ctx);
        self.modifiers.publish(ctx);

        // The file bridge's answer to a system picker, if one came back.
        craftapps_shared::saf::poll_bridge();

        // The stylus data of the last motion event: the tool type (stylus,
        // eraser end, palm) and the side-button state. winit drops both, so
        // the vendored android-activity records them on the way past.
        self.feed_stylus_state();

        // The history length before this frame's work: a multi-finger tap
        // arrives at the canvas as a click, which can paint a dab of its own.
        let history_now = self.app.session.active().map(|d| d.history.undo.len()).unwrap_or(0);
        let history_before = self.history_len;
        self.history_len = history_now;

        if let Some(tap) = self.gestures.update(ctx) {
            // Undo whatever the tap itself painted, so the gesture is the only
            // thing the tap does.
            if history_now > history_before && self.tap_painted_a_stroke() {
                let _ = self.app.run("edit.undo", serde_json::json!({}));
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

        // Stylus states via the shared pen pump when it is running (it
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
        let Some(doc) = self.app.session.active() else { return false };
        doc.history
            .undo
            .last()
            .map(|e| e.label.to_ascii_lowercase())
            .map(|label| {
                label.contains("brush")
                    || label.contains("pencil")
                    || label.contains("eraser")
                    || label.contains("pen")
            })
            .unwrap_or(false)
    }

    /// Shared [`craftapps_shared::pen`] actions → VectorCraft. Runs after the
    /// app frame so dialogs/menus are settled.
    fn dispatch_pen_actions(&mut self, ctx: &egui::Context) {
        for qa in craftapps_shared::pen::drain_actions() {
            use craftapps_shared::pen::PenAction;
            match (qa.action, qa.pressed) {
                (PenAction::None, _) => {}
                (PenAction::RightClick, true) => {
                    // Synthesize a right-click at the pen event position.
                    // The Java side passes MotionEvent coordinates, which work
                    // for hover button presses where egui has no pointer pos.
                    let ppp = ctx.pixels_per_point();
                    let pos = egui::pos2(qa.x / ppp, qa.y / ppp);
                    ctx.input_mut(|i| {
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
                    ctx.request_repaint();
                }
                (PenAction::Undo, true) => {
                    if let Err(e) = self.app.run("edit.undo", serde_json::json!({})) {
                        log::warn!("pen undo: {e}");
                    }
                }
                (PenAction::Redo, true) => {
                    if let Err(e) = self.app.run("edit.redo", serde_json::json!({})) {
                        log::warn!("pen redo: {e}");
                    }
                }
                // Other pen actions (tool switches) are not yet mapped to
                // VectorCraft commands; log and ignore.
                (action, pressed) => {
                    log::debug!("pen action ignored: {action:?} pressed={pressed}");
                }
            }
        }
    }
}

impl eframe::App for ShellApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The app's per-frame logic (menus, canvas, picks polling, …).
        self.app.logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Modifiers, stylus side-channel and gestures first: the app must see
        // the modifier state and a cleaned-up tap in the frame they happen.
        self.process_input(&ctx);
        // The app itself: toolbar, canvas, panels. (Was missing: the shell
        // ran input/browser/pen but never drew the app -> black screen.)
        self.app.ui(ui);
        // In-app file browser overlay, when a pick is in flight.
        let event = if let Ok(mut b) = self.browser.lock() {
            b.show(&ctx)
        } else {
            BrowserEvent::None
        };
        if let BrowserEvent::SystemPicker(tx, request) = event {
            self.services.on_system_picker(tx, &request);
        }

        // Pen side-channel button actions (e.g. barrel-button right-click).
        // Runs after the app frame so dialogs/menus are settled.
        self.dispatch_pen_actions(&ctx);
    }
}
