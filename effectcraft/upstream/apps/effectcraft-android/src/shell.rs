//! eframe shell: owns the [`EffectcraftApp`], running the full shared stack
//! around it — keyboard modifiers, the S Pen side-channel, stylus-mode
//! multi-finger gestures and pen button remap dispatch.
//!
//! The shell deliberately does NOT poll the shared crate's SAF pending
//! flow (`saf::poll_bridge`): EffectCraft's dialogs all go through the
//! file bridge's worker threads (see `picks.rs`), which own the bridge
//! response file; `poll_bridge` would take those responses and drop them
//! for want of a pending request.

use eframe::egui;
use effectcraft_ui_egui::EffectcraftApp;

use craftapps_shared::gestures::GestureDetector;
use craftapps_shared::modifiers::ModifierKeys;

/// The host's `eframe::App`: the EffectCraft UI plus the Android input stack.
pub struct ShellApp {
    app: EffectcraftApp,
    /// Multi-finger tap recognition (the "stylus mode" gestures).
    gestures: GestureDetector,
    /// Which modifier keys are held (winit's Android backend sends no
    /// `ModifiersChanged`, so they are tracked from the key events).
    modifiers: ModifierKeys,
    /// The undo history length at the end of the last frame, used to tell
    /// whether a multi-finger tap did something of its own.
    history_len: usize,
}

impl ShellApp {
    pub fn new(app: EffectcraftApp) -> Self {
        Self {
            app,
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

        // The stylus data of the last motion event: the tool type (stylus,
        // eraser end, palm) and the side-button state. winit drops both, so
        // the vendored android-activity records them on the way past.
        self.feed_stylus_state();

        // The history length before this frame's work: a multi-finger tap
        // arrives at the viewer as a click, which could do something of
        // its own.
        let history_now = self.app.session.history.undo.len();
        let history_before = self.history_len;
        self.history_len = history_now;

        if let Some(tap) = self.gestures.update(ctx) {
            // Undo whatever the tap itself did, so the gesture is the only
            // thing the tap does. A stray tap in a compositor can add or
            // create things (a click is a click).
            if history_now > history_before && self.tap_did_something() {
                if let Err(e) = self.app.session.execute("edit.undo", serde_json::json!({})) {
                    log::warn!("undoing the tap's edit: {e}");
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

    /// Did the tap do something undoable of its own? The history's last step
    /// names the command: match the labels a stray click can produce in a
    /// compositor (adding/creating layers or items, pasting, dropping).
    fn tap_did_something(&self) -> bool {
        self.app
            .session
            .history
            .undo
            .last()
            .map(|(label, _)| {
                let label = label.to_ascii_lowercase();
                label.contains("add")
                    || label.contains("new")
                    || label.contains("create")
                    || label.contains("insert")
                    || label.contains("paste")
                    || label.contains("drop")
            })
            .unwrap_or(false)
    }

    /// Shared [`craftapps_shared::pen`] actions → EffectCraft. Runs after
    /// the app frame so dialogs/menus are settled.
    fn dispatch_pen_actions(&mut self, ctx: &egui::Context) {
        for qa in craftapps_shared::pen::drain_actions() {
            use craftapps_shared::pen::PenAction;
            match (qa.action, qa.pressed) {
                (PenAction::None, _) => {}
                (PenAction::RightClick, true) => {
                    // Synthesize a right-click at the pen event position.
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
                    if let Err(e) = self.app.session.execute("edit.undo", serde_json::json!({})) {
                        log::warn!("pen undo: {e}");
                    }
                }
                (PenAction::Redo, true) => {
                    if let Err(e) = self.app.session.execute("edit.redo", serde_json::json!({})) {
                        log::warn!("pen redo: {e}");
                    }
                }
                // EffectCraft has no tool-wheel UI and the remaining
                // remappable actions have no command equivalent here: log
                // and ignore.
                (action, pressed) => {
                    log::debug!("pen action ignored: {action:?} pressed={pressed}");
                }
            }
        }
    }
}

impl eframe::App for ShellApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Modifiers and gestures first: the app must see the modifier state
        // and a cleaned-up tap in the frame they happen.
        self.process_input(ctx);
        // The app's per-frame logic (its `logic` picks up the file dialogs'
        // answers first thing, so panels draw what the user chose).
        self.app.logic(ctx, _frame);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.app.ui(ui, frame);
        // Pen side-channel button actions (e.g. barrel-button right-click).
        // Runs after the app frame so dialogs/menus are settled.
        let ctx = ui.ctx().clone();
        self.dispatch_pen_actions(&ctx);
    }
}
