//! Pen/stylus support for Android: settings, pressure processing, and pen
//! button remapping.
//!
//! ## What winit 0.30 actually delivers on Android (researched 2026-10-11)
//!
//! winit's Android backend (`platform_impl/android/mod.rs`, `handle_input_event`)
//! turns every `MotionEvent` into a generic [`winit::event::WindowEvent::Touch`]
//! with `force = Some(Force::Normalized(pressure))`. That is ALL it forwards:
//!
//! | Android data | winit 0.30 | reaches the app? |
//! |---|---|---|
//! | `pressure` | `Touch.force` | yes (via egui `Event::Touch { force }`) |
//! | `device_id` | `Touch.device_id` | yes (egui `TouchDeviceId`, hashed) |
//! | `tool_type` (Stylus/Finger/Eraser/Palm) | — | **NO, dropped** |
//! | `button_state` (barrel button) | — | **NO, dropped** |
//! | `ACTION_BUTTON_PRESS/RELEASE` | mapped to `None` ("TODO mouse events") | **NO, dropped** |
//! | hover (`ACTION_HOVER_MOVE`) | mapped to `None` | **NO, dropped** |
//! | tilt / orientation | — | **NO** |
//!
//! The NDK *does* expose all of it (`android-activity 0.6.1`:
//! `Pointer::tool_type()`, `MotionEvent::button_state()`,
//! `ButtonState::stylus_primary()`), but winit consumes the input queue
//! exclusively, so the app cannot read it through winit.
//!
//! ## How this module gets the missing data
//!
//! Each port's `<App>Activity.java` overrides `dispatchTouchEvent` — a Java
//! entry point that still fires for `NativeActivity` subclasses — and forwards
//! only pen-relevant state changes (tool type, barrel-button transitions) to
//! Rust via the JNI entry [`native_on_pen_event`] (same established pattern as
//! `nativeOnActivityResult`). Java throttles to state changes so JNI traffic is
//! a handful of calls per stroke, not per motion event.
//!
//! ## What works without the Java hook
//!
//! Pressure (`force`), pressure curve/sensitivity/smoothing, and the pen-test
//! area work with winit alone. Pen-vs-touch separation uses a hybrid: the Java
//! side-channel bootstraps stylus detection, then the learned
//! `device_id` (the S Pen is a separate input device from the touchscreen on
//! Samsung tablets) becomes the primary signal.
//!
//! ## Threading
//!
//! The JNI entry runs on Android's UI thread; the app reads state on the
//! winit/egui thread. All shared state is behind `Mutex`/`Atomic*`. Remapped
//! actions are pushed to a queue drained once per frame by the app.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Types

/// Which physical tool is touching, from the Java side-channel
/// (`MotionEvent.getToolType`). winit never reports this.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PenTool {
    Unknown,
    Finger,
    Stylus,
    Eraser,
    Palm,
}

/// The remappable pen inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PenButton {
    /// S Pen barrel button (`BUTTON_STYLUS_PRIMARY`).
    BarrelPrimary,
    /// Second barrel button, if the pen has one (`BUTTON_STYLUS_SECONDARY`).
    BarrelSecondary,
    /// Inverted pen / eraser end (`TOOL_TYPE_ERASER`).
    Eraser,
}

impl PenButton {
    pub fn label(self) -> &'static str {
        match self {
            PenButton::BarrelPrimary => "Pen button",
            PenButton::BarrelSecondary => "Pen button 2",
            PenButton::Eraser => "Eraser end",
        }
    }

    pub fn all() -> [PenButton; 3] {
        [PenButton::BarrelPrimary, PenButton::BarrelSecondary, PenButton::Eraser]
    }
}

/// App-agnostic actions a pen button or touch gesture can trigger. Each port
/// maps these to its own commands (see PhotoCraft's `shell.rs` for the
/// reference mapping).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PenAction {
    /// Do nothing.
    None,
    Undo,
    Redo,
    /// Toggle between brush and eraser.
    ToggleEraser,
    /// Eyedropper / color picker.
    Eyedropper,
    /// Pan / hand tool while held (fires press + release).
    PanTool,
    /// Right-click at the pen tip.
    RightClick,
    /// Open the radial tool wheel at the tap point.
    ToolWheel,
}

impl PenAction {
    pub fn label(self) -> &'static str {
        match self {
            PenAction::None => "Do nothing",
            PenAction::Undo => "Undo",
            PenAction::Redo => "Redo",
            PenAction::ToggleEraser => "Toggle eraser",
            PenAction::Eyedropper => "Eyedropper",
            PenAction::PanTool => "Pan tool (while held)",
            PenAction::RightClick => "Right-click",
            PenAction::ToolWheel => "Tool wheel",
        }
    }

    pub fn all() -> [PenAction; 8] {
        [
            PenAction::None,
            PenAction::Undo,
            PenAction::Redo,
            PenAction::ToggleEraser,
            PenAction::Eyedropper,
            PenAction::PanTool,
            PenAction::RightClick,
            PenAction::ToolWheel,
        ]
    }

    /// Momentary actions (like [`PenAction::PanTool`]) need a release event.
    pub fn is_momentary(self) -> bool {
        matches!(self, PenAction::PanTool)
    }
}

/// How pen and finger input are treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMode {
    /// Default: pen draws, finger does gestures (pan/zoom).
    PenDrawsTouchGestures,
    /// Only the pen draws; finger input never paints.
    PenOnly,
    /// Finger paints too (for finger-painting).
    TouchDrawsToo,
}

impl InputMode {
    pub fn label(self) -> &'static str {
        match self {
            InputMode::PenDrawsTouchGestures => "Pen draws, touch does gestures",
            InputMode::PenOnly => "Pen only",
            InputMode::TouchDrawsToo => "Touch draws too",
        }
    }

    pub fn all() -> [InputMode; 3] {
        [InputMode::PenDrawsTouchGestures, InputMode::PenOnly, InputMode::TouchDrawsToo]
    }
}

/// Pressure response curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PressureCurve {
    Linear,
    /// Reaches full strength with a light touch.
    Soft,
    /// Needs a hard press for full strength.
    Firm,
}

impl PressureCurve {
    pub fn label(self) -> &'static str {
        match self {
            PressureCurve::Linear => "Linear",
            PressureCurve::Soft => "Soft",
            PressureCurve::Firm => "Firm",
        }
    }

    pub fn all() -> [PressureCurve; 3] {
        [PressureCurve::Linear, PressureCurve::Soft, PressureCurve::Firm]
    }

    /// Map raw 0..1 pressure through the curve.
    pub fn apply(self, p: f32) -> f32 {
        let p = p.clamp(0.0, 1.0);
        match self {
            PressureCurve::Linear => p,
            // gamma < 1 boosts low end
            PressureCurve::Soft => p.powf(0.65),
            // gamma > 1 compresses low end
            PressureCurve::Firm => p.powf(1.6),
        }
    }
}

// ---------------------------------------------------------------------------
// Settings

/// Pen settings, persisted per-app as `pen.json` under the app's settings dir
/// (`/sdcard/CraftApps/Settings/<App>/`).
///
/// `#[serde(default)]`: a file written by an older build (before a setting
/// existed) must not lose the settings it does have — the missing field falls
/// back to its default instead of failing the whole parse.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PenSettings {
    pub pressure_enabled: bool,
    pub pressure_curve: PressureCurve,
    /// 0.25..=3.0 multiplier on top of the curve.
    pub sensitivity: f32,
    /// Moving-average window over recent pressure samples, 0 = off.
    pub smoothing: u8,
    pub input_mode: InputMode,
    /// Best-effort: ignore touches that start while a palm is down. Needs the
    /// Java side-channel (winit drops `TOOL_TYPE_PALM`).
    pub palm_rejection: bool,
    pub button_primary: PenAction,
    pub button_secondary: PenAction,
    pub eraser_action: PenAction,
    /// Stylus mode: multi-finger tap gestures for tool switching while drawing.
    /// When enabled, 3-finger and 4-finger taps trigger the configured actions
    /// instead of being treated as canvas gestures.
    pub stylus_mode: bool,
    /// Action for 3-finger tap (default: Undo).
    pub three_finger_tap: PenAction,
    /// Action for 4-finger tap (default: ToolWheel).
    pub four_finger_tap: PenAction,
}

impl Default for PenSettings {
    fn default() -> Self {
        PenSettings {
            pressure_enabled: true,
            pressure_curve: PressureCurve::Linear,
            sensitivity: 1.0,
            smoothing: 0,
            input_mode: InputMode::PenDrawsTouchGestures,
            palm_rejection: true,
            button_primary: PenAction::RightClick,
            button_secondary: PenAction::None,
            eraser_action: PenAction::ToggleEraser,
            stylus_mode: false,
            three_finger_tap: PenAction::Undo,
            four_finger_tap: PenAction::ToolWheel,
        }
    }
}

impl PenSettings {
    fn path(settings_dir: &Path) -> std::path::PathBuf {
        settings_dir.join("pen.json")
    }

    /// Load, falling back to defaults on any error (never fails the app).
    pub fn load(settings_dir: &Path) -> Self {
        let path = Self::path(settings_dir);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return PenSettings::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Save; errors are returned for the caller to log.
    pub fn save(&self, settings_dir: &Path) -> Result<(), String> {
        let path = Self::path(settings_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("pen settings dir: {e}"))?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("pen settings encode: {e}"))?;
        std::fs::write(&path, text).map_err(|e| format!("pen settings write: {e}"))?;
        Ok(())
    }

    pub fn action_for(&self, button: PenButton) -> PenAction {
        match button {
            PenButton::BarrelPrimary => self.button_primary,
            PenButton::BarrelSecondary => self.button_secondary,
            PenButton::Eraser => self.eraser_action,
        }
    }

    /// Full pressure pipeline: on/off → curve → sensitivity clamp.
    /// Smoothing is stateful; see [`PressureSmoother`].
    pub fn adjust_pressure(&self, raw: f32) -> f32 {
        if !self.pressure_enabled {
            return 1.0;
        }
        (self.pressure_curve.apply(raw) * self.sensitivity.clamp(0.25, 3.0)).clamp(0.0, 1.0)
    }
}

/// Stateful moving-average smoother for pressure. Feed raw 0..1 samples per
/// motion event; read the smoothed value for the stroke.
#[derive(Clone, Debug, Default)]
pub struct PressureSmoother {
    window: VecDeque<f32>,
}

impl PressureSmoother {
    pub fn new() -> Self {
        PressureSmoother { window: VecDeque::new() }
    }

    /// Push a sample; returns the smoothed value. `smoothing` is the window
    /// size from [`PenSettings::smoothing`]; 0 disables smoothing.
    pub fn push(&mut self, sample: f32, smoothing: u8) -> f32 {
        if smoothing == 0 {
            self.window.clear();
            return sample;
        }
        let n = smoothing.clamp(1, 16) as usize;
        self.window.push_back(sample);
        while self.window.len() > n {
            self.window.pop_front();
        }
        let sum: f32 = self.window.iter().sum();
        sum / self.window.len() as f32
    }

    pub fn reset(&mut self) {
        self.window.clear();
    }
}

// ---------------------------------------------------------------------------
// Runtime state (UI thread JNI writes, egui thread reads)

/// A pen-button press/release delivered from Java, queued for the app thread.
#[derive(Clone, Copy, Debug)]
pub struct PenButtonEvent {
    pub button: PenButton,
    pub pressed: bool,
    /// Position (in pixels) where the event occurred, from the MotionEvent.
    /// Used for hover button presses where egui has no pointer position.
    pub x: f32,
    pub y: f32,
}

struct PenRuntime {
    /// Last tool reported by the Java side-channel.
    tool: PenTool,
    /// Currently pressed buttons (from Java transitions).
    pressed: [bool; 3],
    /// Palm currently down (Java side-channel).
    palm_down: bool,
    /// Stylus currently touching (Java side-channel).
    stylus_down: bool,
    /// The stylus is hovering over the screen but not touching it (the
    /// hover events: `ACTION_HOVER_ENTER` / `_MOVE` / `_EXIT`). A stylus'
    /// side button is reported on those events too, so the button works
    /// while the pen hovers.
    hovering: bool,
    /// The last position the stylus reported (used by the key-event path).
    x: f32,
    y: f32,
    /// The last motion event seen: (action, button bits, tool type).
    last_event: Option<(i32, i32, i32)>,
    /// Learned winit device id hash for the pen (S Pen is a separate input
    /// device on Samsung tablets). `None` until observed.
    pen_device: Option<u64>,
    /// Remap capture in progress: which button's row is being assigned.
    capture: Option<PenButton>,
    /// Completed capture: (row being assigned, physical button pressed).
    captured: Option<(PenButton, PenButton)>,
    /// Queued button events for the app thread to drain.
    queue: VecDeque<PenButtonEvent>,
    /// Queued actions for the app thread to drain (already remapped).
    actions: VecDeque<QueuedAction>,
}

/// A remapped action waiting for the app thread, with press/release for
/// momentary actions.
#[derive(Clone, Copy, Debug)]
pub struct QueuedAction {
    pub action: PenAction,
    pub pressed: bool,
    /// Position (in pixels) where the action occurred. Used for RightClick
    /// synthesis when the pen is hovering.
    pub x: f32,
    pub y: f32,
}

static RUNTIME: OnceLock<Mutex<PenRuntime>> = OnceLock::new();

fn runtime() -> &'static Mutex<PenRuntime> {
    RUNTIME.get_or_init(|| {
        Mutex::new(PenRuntime {
            tool: PenTool::Unknown,
            pressed: [false; 3],
            palm_down: false,
            stylus_down: false,
            hovering: false,
            x: 0.0,
            y: 0.0,
            last_event: None,
            pen_device: None,
            capture: None,
            captured: None,
            queue: VecDeque::new(),
            actions: VecDeque::new(),
        })
    })
}

/// Settings snapshot shared with the JNI thread for remap lookup. The app
/// calls [`sync_settings`] whenever settings change (and once at startup).
static SETTINGS_SNAP: OnceLock<Mutex<PenSettings>> = OnceLock::new();

fn settings_snap() -> &'static Mutex<PenSettings> {
    SETTINGS_SNAP.get_or_init(|| Mutex::new(PenSettings::default()))
}

/// Publish current settings to the runtime (call after load and on change).
pub fn sync_settings(settings: &PenSettings) {
    if let Ok(mut snap) = settings_snap().lock() {
        *snap = settings.clone();
    }
}

fn button_index(b: PenButton) -> usize {
    match b {
        PenButton::BarrelPrimary => 0,
        PenButton::BarrelSecondary => 1,
        PenButton::Eraser => 2,
    }
}

/// Called by the JNI entry when Java reports a pen state change.
/// `action`: MotionEvent action masked; `tool`: `getToolType`;
/// `buttons`: `getButtonState` bits; `action_button`: `getActionButton`;
/// `x`, `y`: position in pixels from the MotionEvent.
pub fn on_pen_event(action: i32, tool: i32, buttons: i32, action_button: i32, x: f32, y: f32) {
    // Android constants (stable API literals, mirroring MotionEvent).
    const ACTION_BUTTON_PRESS: i32 = 11;
    const ACTION_BUTTON_RELEASE: i32 = 12;
    const ACTION_DOWN: i32 = 0;
    const ACTION_UP: i32 = 1;
    const ACTION_CANCEL: i32 = 3;
    const ACTION_MOVE: i32 = 2;
    // The hover actions. Android's numbering changed over the releases
    // (hover move is 7, hover enter 9, hover exit 10 today; 7/8/9 in the
    // older ones), so all of them count as "hovering".
    const ACTION_HOVER_MOVE: i32 = 7;
    const ACTION_HOVER_ENTER: i32 = 9;
    const ACTION_HOVER_EXIT: i32 = 10;
    const TOOL_STYLUS: i32 = 2;
    const TOOL_ERASER: i32 = 4;
    const TOOL_PALM: i32 = 5;
    const BUTTON_STYLUS_PRIMARY: i32 = 1 << 5;
    const BUTTON_STYLUS_SECONDARY: i32 = 1 << 6;

    let Ok(mut rt) = runtime().lock() else { return };

    // Remember where the stylus is and what the last event was: the key-event
    // path needs the position, and the Stylus Settings show the event.
    rt.x = x;
    rt.y = y;
    rt.last_event = Some((action, buttons, tool));

    // Tool tracking.
    rt.tool = match tool {
        TOOL_STYLUS => PenTool::Stylus,
        TOOL_ERASER => PenTool::Eraser,
        TOOL_PALM => PenTool::Palm,
        1 => PenTool::Finger,
        _ => PenTool::Unknown,
    };

    // Hovering: the stylus is over the screen but not touching it. Its side
    // button is reported on the hover events as well, so the button works
    // while the pen hovers.
    let hovering = matches!(action, ACTION_HOVER_MOVE | ACTION_HOVER_ENTER | ACTION_HOVER_EXIT);
    if hovering {
        // ACTION_HOVER_EXIT means the pen *left* the screen: the flag must go
        // off, not on.
        rt.hovering = action != ACTION_HOVER_EXIT;
    }

    rt.palm_down = tool == TOOL_PALM && (action == ACTION_DOWN || action == ACTION_MOVE);
    if action == ACTION_UP || action == ACTION_CANCEL {
        rt.palm_down = false;
        rt.hovering = false;
        if tool == TOOL_STYLUS || tool == TOOL_ERASER {
            rt.stylus_down = false;
        }
    }
    if action == ACTION_DOWN && (tool == TOOL_STYLUS || tool == TOOL_ERASER) {
        rt.stylus_down = true;
        // Touching the screen ends the hover.
        rt.hovering = false;
    }

    // Eraser end: tool-type transition acts like a button.
    let eraser_now = tool == TOOL_ERASER && (action == ACTION_DOWN || action == ACTION_MOVE);
    set_button(&mut rt, PenButton::Eraser, eraser_now, x, y);

    // Barrel buttons: the held state is carried by *every* event while the
    // button is held (`MotionEvent.getButtonState()`), so use it for every
    // action — including the hover events, where the button press arrives
    // while the pen is not touching the screen.
    let primary_held = buttons & BUTTON_STYLUS_PRIMARY != 0;
    let secondary_held = buttons & BUTTON_STYLUS_SECONDARY != 0;
    set_button(&mut rt, PenButton::BarrelPrimary, primary_held, x, y);
    set_button(&mut rt, PenButton::BarrelSecondary, secondary_held, x, y);
    if action == ACTION_BUTTON_PRESS {
        // A button press that does not set the button bits (some devices
        // report the button only through the action).
        if action_button == BUTTON_STYLUS_PRIMARY {
            set_button(&mut rt, PenButton::BarrelPrimary, true, x, y);
        } else if action_button == BUTTON_STYLUS_SECONDARY {
            set_button(&mut rt, PenButton::BarrelSecondary, true, x, y);
        }
    } else if action == ACTION_BUTTON_RELEASE {
        if action_button == BUTTON_STYLUS_PRIMARY {
            set_button(&mut rt, PenButton::BarrelPrimary, false, x, y);
        } else if action_button == BUTTON_STYLUS_SECONDARY {
            set_button(&mut rt, PenButton::BarrelSecondary, false, x, y);
        }
    }

    // Lift/cancel clears everything: a pen that leaves the screen cannot be
    // holding a button. Prevents stuck "pressed" states.
    if action == ACTION_UP || action == ACTION_CANCEL {
        set_button(&mut rt, PenButton::BarrelPrimary, false, x, y);
        set_button(&mut rt, PenButton::BarrelSecondary, false, x, y);
        set_button(&mut rt, PenButton::Eraser, false, x, y);
    }
}

/// Record a button transition; on change, queue the event and the remapped
/// action (or complete a capture).
fn set_button(rt: &mut PenRuntime, button: PenButton, pressed: bool, x: f32, y: f32) {
    let i = button_index(button);
    if rt.pressed[i] == pressed {
        return;
    }
    rt.pressed[i] = pressed;

    // Capture mode: the user pressed a pen button to assign it. Record the
    // (row, physical button) pair; the UI reassigns the row to the pressed
    // button. The press itself is swallowed, not dispatched.
    if pressed {
        if let Some(target) = rt.capture.take() {
            rt.captured = Some((target, button));
            return;
        }
    }

    rt.queue.push_back(PenButtonEvent { button, pressed, x, y });

    // Remap to the configured action.
    let action = settings_snap()
        .lock()
        .map(|s| s.action_for(button))
        .unwrap_or(PenAction::None);
    if action == PenAction::None {
        return;
    }
    if !action.is_momentary() && !pressed {
        return; // fire one-shot actions on press only
    }
    rt.actions.push_back(QueuedAction { action, pressed, x, y });
}

/// Called by the JNI entry when Java detects a multi-finger tap gesture.
/// `fingers`: 3 or 4 (number of fingers in the tap); `x`, `y`: tap position
/// in pixels. Only fires when stylus mode is enabled in settings.
pub fn on_gesture(fingers: i32, x: f32, y: f32) {
    let Ok(mut rt) = runtime().lock() else { return };
    let action = settings_snap()
        .lock()
        .map(|s| {
            if !s.stylus_mode {
                return PenAction::None;
            }
            match fingers {
                3 => s.three_finger_tap,
                4 => s.four_finger_tap,
                _ => PenAction::None,
            }
        })
        .unwrap_or(PenAction::None);
    if action == PenAction::None {
        return;
    }
    // Gestures are one-shot: fire on the tap (pressed=true).
    rt.actions.push_back(QueuedAction { action, pressed: true, x, y });
}

// ---------------------------------------------------------------------------
// App-thread API

/// Drain queued pen-button events (raw). Usually you want [`drain_actions`].
pub fn drain_button_events() -> Vec<PenButtonEvent> {
    runtime()
        .lock()
        .map(|mut rt| rt.queue.drain(..).collect())
        .unwrap_or_default()
}

/// Current pressed state of each physical pen button, for diagnostics UI.
/// Returns `(barrel_primary, barrel_secondary, eraser)`.
pub fn button_state() -> (bool, bool, bool) {
    runtime()
        .lock()
        .map(|rt| {
            (
                rt.pressed[button_index(PenButton::BarrelPrimary)],
                rt.pressed[button_index(PenButton::BarrelSecondary)],
                rt.pressed[button_index(PenButton::Eraser)],
            )
        })
        .unwrap_or((false, false, false))
}

/// Drain remapped actions. Call once per frame; dispatch each to the app's
/// command system. Returns `(action, pressed)`; one-shot actions only appear
/// with `pressed == true`.
pub fn drain_actions() -> Vec<QueuedAction> {
    runtime()
        .lock()
        .map(|mut rt| rt.actions.drain(..).collect())
        .unwrap_or_default()
}

/// A stylus' side button press or release that arrived as a **key event**.
///
/// Android 14 and later also report the stylus' buttons as key events
/// (`KEYCODE_STYLUS_BUTTON_PRIMARY` = 308, `..._SECONDARY` = 309); winit maps
/// those to `Key::Unidentified(308)`, so the port can pick them up there.
/// The position is the last one the stylus reported.
pub fn on_pen_button(button: PenButton, pressed: bool) {
    let Ok(mut rt) = runtime().lock() else { return };
    let (x, y) = (rt.x, rt.y);
    set_button(&mut rt, button, pressed, x, y);
}

/// The last motion event the runtime saw: (action, button bits, tool type).
/// A diagnostic for the Stylus Settings.
pub fn last_event() -> Option<(i32, i32, i32)> {
    runtime().lock().ok().and_then(|rt| rt.last_event)
}

/// The last raw stylus state the shell drained from the Android input
/// queue, BEFORE any filtering: (action, button bits, action button,
/// tool type). A diagnostic for the Stylus Settings: it separates "the OS
/// never delivered the event" from "the app received and dropped it".
static LAST_QUEUE_STATE: std::sync::Mutex<Option<(i32, u32, i32, i32)>> =
    std::sync::Mutex::new(None);

/// Record a raw drained stylus state. The shell calls this for every
/// state it drains, before deciding whether the pen runtime should see
/// it.
pub fn note_queue_state(action: i32, buttons: u32, action_button: i32, tool: i32) {
    if let Ok(mut slot) = LAST_QUEUE_STATE.lock() {
        *slot = Some((action, buttons, action_button, tool));
    }
}

/// The last raw stylus state the shell drained (see [`note_queue_state`]).
pub fn last_queue_state() -> Option<(i32, u32, i32, i32)> {
    LAST_QUEUE_STATE.lock().ok().and_then(|slot| *slot)
}

// ---------------------------------------------------------------------------
// Input pump: keep pen-button events from starving.
//
// The vendored android-activity pump RECORDS stylus states as events pass
// through winit's input iterator -- including barrel-button presses while
// hovering. But eframe only runs a frame when egui has something to react
// to, and a stylus button press produces no egui event: over the canvas
// the app sits idle, the recorded press is never drained, and nothing
// happens until the next touch wakes the app and the stale press fires
// then. (In the Stylus Settings the capture polling repaints every frame,
// which is why presses are seen there.)
//
// The pump thread moves recorded states into a holding queue and, when a
// state carries a button transition, asks egui for a repaint so the
// shell drains and dispatches it immediately.

static PUMP_CTX: std::sync::Mutex<Option<egui::Context>> = std::sync::Mutex::new(None);
static PUMP_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static QUEUED_STATES: std::sync::Mutex<
    std::collections::VecDeque<android_activity::input::StylusState>,
> = std::sync::Mutex::new(std::collections::VecDeque::new());

/// Max states buffered while the app is not draining (e.g. backgrounded);
/// the oldest are dropped beyond this, mirroring the input queue's cap.
const QUEUED_STATES_CAP: usize = 512;

/// Start the pen input pump (idempotent) and give it the egui context to
/// wake when a pen-button event lands. Call from the shell each frame;
/// only the first call spawns the thread.
pub fn install_pen_pump(ctx: egui::Context) {
    if let Ok(mut slot) = PUMP_CTX.lock() {
        *slot = Some(ctx);
    }
    if PUMP_STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("pen-input-pump".into())
        .spawn(pump_loop);
    if let Err(e) = spawned {
        log::error!("pen input pump failed to start: {e}");
        PUMP_STARTED.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Is the pen input pump running? Shells drain [`take_queued_states`]
/// when it is, and the android-activity queue directly when it is not.
pub fn pen_pump_active() -> bool {
    PUMP_STARTED.load(std::sync::atomic::Ordering::SeqCst)
}

fn pump_loop() {
    loop {
        let states = android_activity::input::drain_stylus_states();
        if !states.is_empty() {
            let mut wake = false;
            if let Ok(mut queue) = QUEUED_STATES.lock() {
                for state in states {
                    note_queue_state(
                        state.action,
                        state.buttons,
                        state.action_button as i32,
                        state.tool_type,
                    );
                    // A button transition (press/release action, or any
                    // state carrying button bits) must not wait for the
                    // next touch: wake the app so it dispatches it now.
                    if state.action == 11 || state.action == 12 || state.buttons != 0 {
                        wake = true;
                    }
                    if queue.len() >= QUEUED_STATES_CAP {
                        queue.pop_front();
                    }
                    queue.push_back(state);
                }
            }
            if wake {
                if let Ok(slot) = PUMP_CTX.lock() {
                    if let Some(ctx) = slot.as_ref() {
                        ctx.request_repaint();
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Take the stylus states the pump collected, oldest first. Shells that
/// installed the pump drain THIS instead of the android-activity queue.
pub fn take_queued_states() -> Vec<android_activity::input::StylusState> {
    QUEUED_STATES
        .lock()
        .map(|mut queue| queue.drain(..).collect())
        .unwrap_or_default()
}

/// Is the stylus currently touching? (Java side-channel.)
pub fn stylus_down() -> bool {
    runtime().lock().map(|rt| rt.stylus_down).unwrap_or(false)
}

/// Is the stylus hovering over the screen (near it, not touching)?
///
/// The stylus' side button is reported on the hover events as well, so the
/// button works while the pen hovers.
pub fn hovering() -> bool {
    runtime().lock().map(|rt| rt.hovering).unwrap_or(false)
}

/// Is a palm currently down? (Java side-channel; winit drops this.)
pub fn palm_down() -> bool {
    runtime().lock().map(|rt| rt.palm_down).unwrap_or(false)
}

/// Last tool type reported by the Java side-channel.
pub fn current_tool() -> PenTool {
    runtime().lock().map(|rt| rt.tool).unwrap_or(PenTool::Unknown)
}

/// Learn the pen's winit device id. Call from the app's touch handling: when
/// [`stylus_down`] is true and a `Touch{Started}` arrives, its `device_id`
/// hash is the pen. Returns true if this device is (now) known as the pen.
pub fn note_touch_device(device_hash: u64, is_start: bool) -> bool {
    let Ok(mut rt) = runtime().lock() else { return false };
    if is_start && rt.stylus_down {
        rt.pen_device = Some(device_hash);
    }
    rt.pen_device == Some(device_hash)
}

/// Forget the learned pen device (e.g. on settings reset).
pub fn forget_pen_device() {
    if let Ok(mut rt) = runtime().lock() {
        rt.pen_device = None;
    }
}

/// Begin "press pen button to assign" capture for `button`'s settings row.
/// The next physical pen-button press completes it; see [`take_capture`].
pub fn begin_capture(button: PenButton) {
    if let Ok(mut rt) = runtime().lock() {
        rt.capture = Some(button);
    }
}

/// Cancel an in-progress capture.
pub fn cancel_capture() {
    if let Ok(mut rt) = runtime().lock() {
        rt.capture = None;
    }
}

/// Which button's row is capturing, if any.
pub fn capturing() -> Option<PenButton> {
    runtime().lock().ok().and_then(|rt| rt.capture)
}

/// Take a completed capture: (row being assigned, physical button pressed).
/// The UI should move the row's action to the pressed button.
pub fn take_captured() -> Option<(PenButton, PenButton)> {
    runtime().lock().ok().and_then(|mut rt| rt.captured.take())
}

// ---------------------------------------------------------------------------
// JNI entry: <App>Activity.nativeOnPenEvent (mangled name per app; the shared
// crate exposes this generic handler and each port re-exports it under its
// own mangled symbol — see PhotoCraft's saf.rs pattern).

/// Handle a pen state change from Java. See module docs for the protocol.
pub fn handle_pen_event(action: i32, tool: i32, buttons: i32, action_button: i32, x: f32, y: f32) {
    // Never unwind into the JVM.
    let _ = std::panic::catch_unwind(|| on_pen_event(action, tool, buttons, action_button, x, y));
}

// ---------------------------------------------------------------------------
// egui settings UI

/// Draw the pen settings section. `save` persists (called on change);
/// `action_rows` lists the remappable buttons. App-agnostic: the port passes
/// its settings dir and a save closure.
pub fn pen_settings_ui(
    ui: &mut egui::Ui,
    settings: &mut PenSettings,
    on_change: &mut dyn FnMut(),
) {
    let mut changed = false;

    // Live button state indicator (diagnostic): shows which physical buttons
    // the app is currently seeing as pressed. If pressing the S Pen button
    // doesn't light these up, the events aren't reaching the app.
    let (primary, secondary, eraser) = button_state();
    ui.horizontal(|ui| {
        ui.label("Button state:");
        let dot = |ui: &mut egui::Ui, label: &str, active: bool| {
            let color = if active {
                egui::Color32::GREEN
            } else {
                egui::Color32::DARK_GRAY
            };
            ui.colored_label(color, format!("● {label}"));
        };
        dot(ui, "Primary", primary);
        dot(ui, "Secondary", secondary);
        dot(ui, "Eraser", eraser);
    });
    // Hover: the pen is near the screen without touching it. The side
    // button is reported on the hover events too, so it works there as well.
    {
        let hovering = hovering();
        let color = if hovering { egui::Color32::GREEN } else { egui::Color32::DARK_GRAY };
        ui.colored_label(color, format!("● Pen hovering ({hovering})"));
    }
    // The last event the stylus sent: (action, button bits, tool type).
    // The diagnostic for "the button does nothing": the action shows which
    // event arrived, the bits whether the button was held with it.
    match last_event() {
        Some((action, buttons, tool)) => {
            ui.label(format!(
                "Last stylus event: action {action}, buttons 0x{buttons:02x}, tool {tool}"
            ));
        }
        None => {
            ui.label("Last stylus event: none yet");
        }
    }
    // The last raw state the Android input queue delivered, before the
    // shell's filtering: if a hover button press shows up here but the
    // dots above stay dark, the app is dropping it; if nothing shows up
    // here either, the OS never delivered it.
    match last_queue_state() {
        Some((action, buttons, action_button, tool)) => {
            ui.label(format!(
                "Last input from Android: action {action}, buttons 0x{buttons:02x}, action button {action_button}, tool {tool}"
            ));
        }
        None => {
            ui.label("Last input from Android: none yet");
        }
    }
    ui.add_space(4.0);

    ui.label("Pressure");
    let mut enabled = settings.pressure_enabled;
    if ui.checkbox(&mut enabled, "Use pen pressure").changed() {
        settings.pressure_enabled = enabled;
        changed = true;
    }
    if !settings.pressure_enabled {
        ui.label("Off: the pen paints like a mouse, at full pressure.");
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Response curve");
        egui::ComboBox::from_id_salt("pen-curve")
            .selected_text(settings.pressure_curve.label())
            .show_ui(ui, |ui| {
                for c in PressureCurve::all() {
                    if ui.selectable_value(&mut settings.pressure_curve, c, c.label()).changed() {
                        changed = true;
                    }
                }
            });
    });
    ui.label("Soft reaches full strength with a light touch; Firm needs a hard press.");

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Sensitivity");
        let mut s = settings.sensitivity;
        if ui.add(egui::Slider::new(&mut s, 0.25..=3.0).step_by(0.05)).changed() {
            settings.sensitivity = s;
            changed = true;
        }
    });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Smoothing");
        let mut s = settings.smoothing;
        if ui.add(egui::Slider::new(&mut s, 0..=10)).changed() {
            settings.smoothing = s;
            changed = true;
        }
    });
    ui.label("Averages recent pressure samples; higher is steadier but laggier. 0 = off.");

    ui.add_space(10.0);
    ui.label("Input");
    ui.horizontal(|ui| {
        ui.label("Pen / touch mode");
        egui::ComboBox::from_id_salt("pen-input-mode")
            .selected_text(settings.input_mode.label())
            .show_ui(ui, |ui| {
                for m in InputMode::all() {
                    if ui.selectable_value(&mut settings.input_mode, m, m.label()).changed() {
                        changed = true;
                    }
                }
            });
    });

    ui.add_space(4.0);
    let mut palm = settings.palm_rejection;
    if ui.checkbox(&mut palm, "Palm rejection").changed() {
        settings.palm_rejection = palm;
        changed = true;
    }
    ui.label("Ignores touches that start while your palm is on the screen. Needs the pen data channel (works on this build).");

    ui.add_space(10.0);
    ui.label("Pen buttons");
    ui.label("Tap “Assign…”, then press the pen button (or flip to the eraser end).");
    for button in PenButton::all() {
        ui.horizontal(|ui| {
            ui.label(button.label());
            let current = settings.action_for(button);
            egui::ComboBox::from_id_salt(format!("pen-btn-{:?}", button))
                .selected_text(current.label())
                .show_ui(ui, |ui| {
                    for a in PenAction::all() {
                        let mut sel = current;
                        if ui.selectable_value(&mut sel, a, a.label()).changed() {
                            match button {
                                PenButton::BarrelPrimary => settings.button_primary = sel,
                                PenButton::BarrelSecondary => settings.button_secondary = sel,
                                PenButton::Eraser => settings.eraser_action = sel,
                            }
                            changed = true;
                        }
                    }
                });
            let cap = capturing() == Some(button);
            let btn_label = if cap { "Press pen button… (tap to cancel)" } else { "Assign…" };
            if ui.button(btn_label).clicked() {
                if cap {
                    cancel_capture();
                } else {
                    begin_capture(button);
                }
                ui.ctx().request_repaint();
            }
        });
    }
    // Capture completion: the pressed button takes over the capturing row —
    // swap the two rows' actions so nothing is lost. Polled every frame while
    // the section is open (capture may complete on the UI thread at any time).
    if let Some((target, pressed)) = take_captured() {
        swap_button_actions(settings, target, pressed);
        changed = true;
    }
    if capturing().is_some() {
        ui.ctx().request_repaint();
    }

    ui.add_space(10.0);
    ui.label("Stylus mode");
    ui.label("Multi-finger tap gestures for fast tool switching while drawing with the pen.");
    let mut stylus_mode = settings.stylus_mode;
    if ui.checkbox(&mut stylus_mode, "Enable stylus mode gestures").changed() {
        settings.stylus_mode = stylus_mode;
        changed = true;
    }
    if settings.stylus_mode {
        ui.add_space(4.0);
        // 3-finger tap configuration
        ui.horizontal(|ui| {
            ui.label("3-finger tap");
            egui::ComboBox::from_id_salt("pen-gesture-3")
                .selected_text(settings.three_finger_tap.label())
                .show_ui(ui, |ui| {
                    for a in PenAction::all() {
                        if ui.selectable_value(&mut settings.three_finger_tap, a, a.label()).changed() {
                            changed = true;
                        }
                    }
                });
        });
        // 4-finger tap configuration
        ui.horizontal(|ui| {
            ui.label("4-finger tap");
            egui::ComboBox::from_id_salt("pen-gesture-4")
                .selected_text(settings.four_finger_tap.label())
                .show_ui(ui, |ui| {
                    for a in PenAction::all() {
                        if ui.selectable_value(&mut settings.four_finger_tap, a, a.label()).changed() {
                            changed = true;
                        }
                    }
                });
        });
        ui.label("Tip: 3 fingers for Undo, 4 fingers for the tool wheel.");
    }

    if changed {
        sync_settings(settings);
        on_change();
    }
}

fn swap_button_actions(settings: &mut PenSettings, a: PenButton, b: PenButton) {
    if a == b {
        return;
    }
    let get = |s: &PenSettings, btn: PenButton| s.action_for(btn);
    let (aa, ab) = (get(settings, a), get(settings, b));
    let set = |s: &mut PenSettings, btn: PenButton, act: PenAction| match btn {
        PenButton::BarrelPrimary => s.button_primary = act,
        PenButton::BarrelSecondary => s.button_secondary = act,
        PenButton::Eraser => s.eraser_action = act,
    };
    set(settings, a, ab);
    set(settings, b, aa);
}

/// Which [`PenAction`]s the app actually supports, for greying out rows.
/// (Kept simple: all actions are offered; the port's dispatch maps them.)
pub fn supported_actions() -> [PenAction; 8] {
    PenAction::all()
}

// ---------------------------------------------------------------------------
// Tests (host)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_curve_endpoints() {
        assert_eq!(PressureCurve::Linear.apply(0.0), 0.0);
        assert_eq!(PressureCurve::Linear.apply(1.0), 1.0);
        assert!(PressureCurve::Soft.apply(0.25) > 0.25);
        assert!(PressureCurve::Firm.apply(0.25) < 0.25);
    }

    #[test]
    fn sensitivity_clamped() {
        let mut s = PenSettings::default();
        s.sensitivity = 99.0;
        assert_eq!(s.adjust_pressure(0.5), 1.0);
        s.sensitivity = -1.0;
        assert!(s.adjust_pressure(0.5) >= 0.0);
    }

    #[test]
    fn pressure_off_is_full() {
        let mut s = PenSettings::default();
        s.pressure_enabled = false;
        assert_eq!(s.adjust_pressure(0.1), 1.0);
    }

    #[test]
    fn smoother_averages() {
        let mut sm = PressureSmoother::new();
        assert_eq!(sm.push(1.0, 0), 1.0); // off
        sm.push(0.0, 2);
        let v = sm.push(1.0, 2);
        assert!((v - 0.5).abs() < 1e-6);
    }

    #[test]
    fn settings_round_trip() {
        let s = PenSettings::default();
        let json = serde_json::to_string(&s).expect("encode");
        let back: PenSettings = serde_json::from_str(&json).expect("decode");
        assert_eq!(back.sensitivity, s.sensitivity);
        assert_eq!(back.button_primary, s.button_primary);
    }

    #[test]
    fn the_side_button_works_while_the_pen_hovers() {
        // A hover event (ACTION_HOVER_MOVE = 7) with the stylus' button held
        // (BUTTON_STYLUS_PRIMARY = 1 << 5).
        const ACTION_HOVER_MOVE: i32 = 7;
        const TOOL_STYLUS: i32 = 2;
        const BUTTON_STYLUS_PRIMARY: i32 = 1 << 5;

        on_pen_event(ACTION_HOVER_MOVE, TOOL_STYLUS, BUTTON_STYLUS_PRIMARY, 0, 10.0, 20.0);
        assert!(hovering(), "the pen is hovering");
        assert!(button_state().0, "the button is held while hovering");
        assert!(!stylus_down(), "the pen is not touching");

        // Released: the button is let go.
        on_pen_event(ACTION_HOVER_MOVE, TOOL_STYLUS, 0, 0, 10.0, 20.0);
        assert!(!button_state().0, "the button is released");
    }
}
