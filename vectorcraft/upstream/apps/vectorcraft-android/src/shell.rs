//! eframe shell: owns the [`VectorcraftApp`], draws the in-app file browser
//! overlay each frame, and forwards system-picker handoffs to SAF.

use std::sync::{Arc, Mutex};

use eframe::egui;
use vectorcraft_ui_egui::VectorcraftApp;

use crate::file_browser::{BrowserEvent, FileBrowser};
use crate::services::AndroidServices;

/// The host's `eframe::App`: the VectorCraft UI plus the Android browser overlay.
pub struct ShellApp {
    app: VectorcraftApp,
    browser: Arc<Mutex<FileBrowser>>,
    services: AndroidServices,
}

impl ShellApp {
    pub fn new(app: VectorcraftApp, services: AndroidServices) -> Self {
        let browser = services.browser.clone();
        Self { app, browser, services }
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
                // Other pen actions (Undo/Redo/tool switches) are not yet
                // mapped to VectorCraft commands; log and ignore.
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

        // Pen side-channel button actions (e.g. barrel-button right-click).
        // Runs after the app frame so dialogs/menus are settled.
        self.dispatch_pen_actions(&ctx);

        // In-app file browser overlay, when a pick is in flight.
        let event = if let Ok(mut b) = self.browser.lock() {
            b.show(&ctx)
        } else {
            BrowserEvent::None
        };
        if let BrowserEvent::SystemPicker(tx, request) = event {
            self.services.on_system_picker(tx, &request);
        }
    }
}
