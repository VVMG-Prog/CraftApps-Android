//! Keyboard modifier tracking.
//!
//! winit's Android backend never sends `WindowEvent::ModifiersChanged`, so
//! egui's modifier state is always empty and every ⌘/Ctrl/⇧/⌥ shortcut is
//! dead. The key events themselves do arrive, so the modifiers are tracked
//! from them and written into egui's input state every frame.
//!
//! The app's shortcuts are written as "Cmd+…", which the upstream shortcut
//! parser maps to ⌘ on macOS and Ctrl everywhere else — so on Android the
//! Ctrl key is published as egui's `command` (as egui asks: "on Windows and
//! Linux, set this to the same value as `ctrl`").

use eframe::egui::{Context, Event, Key, Modifiers};

/// Which modifier keys are held down.
#[derive(Clone, Copy, Debug, Default)]
pub struct ModifierKeys {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl ModifierKeys {
    /// Fold the frame's key events into the modifier state.
    pub fn update(&mut self, ctx: &Context) {
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            if let Event::Key { key, pressed, .. } = event {
                match key {
                    Key::ControlLeft | Key::ControlRight => self.ctrl = pressed,
                    Key::ShiftLeft | Key::ShiftRight => self.shift = pressed,
                    Key::AltLeft | Key::AltRight => self.alt = pressed,
                    _ => {}
                }
            }
        }
    }

    /// The modifiers as egui sees them.
    pub fn modifiers(&self) -> Modifiers {
        Modifiers {
            alt: self.alt,
            ctrl: self.ctrl,
            shift: self.shift,
            mac_cmd: false,
            // Not macOS: ⌘ is the Ctrl key.
            command: self.ctrl,
        }
    }

    /// Publish the tracked modifiers into egui's input state, so the app's
    /// shortcut matching and its ⌘-click gestures see them.
    pub fn publish(&self, ctx: &Context) {
        let mods = self.modifiers();
        ctx.input_mut(|i| {
            i.modifiers = mods;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_is_command_off_macos() {
        let keys = ModifierKeys { ctrl: true, ..Default::default() };
        let mods = keys.modifiers();
        assert!(mods.ctrl);
        assert!(mods.command, "the Ctrl key is the command key off macOS");
        assert!(!mods.mac_cmd);
    }

    #[test]
    fn no_modifiers_is_none() {
        let mods = ModifierKeys::default().modifiers();
        assert!(!mods.ctrl && !mods.shift && !mods.alt && !mods.command);
    }
}
