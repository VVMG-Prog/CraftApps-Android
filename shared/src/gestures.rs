//! Multi-finger tap gestures: the "stylus mode" shortcuts (3-finger tap =
//! Undo, 4-finger tap = tool wheel, both configurable).
//!
//! ## Why this is in Rust and not in Java
//!
//! The taps used to be detected in `PhotoCraftActivity.dispatchTouchEvent`.
//! That hook never runs: `NativeActivity.onCreate` calls
//! `getWindow().takeInputQueue(this)`, and AOSP documents `takeInputQueue` as
//! "the window will no longer read and dispatch input events from the queue;
//! it is your responsibility to do so". The window's input queue belongs to
//! the native side (winit), so the Java view hierarchy — and the Activity
//! itself — never sees a touch, key or generic-motion event.
//!
//! What *is* alive is egui's touch stream: winit turns every `MotionEvent`
//! pointer into `WindowEvent::Touch`, which egui-winit hands to egui as
//! `Event::Touch { device_id, id, phase, pos, force }`. That carries
//! everything the gesture needs: which pointer, which input device, where it
//! is and how hard it presses.
//!
//! ## Pen vs finger
//!
//! winit drops the tool type (`TOOL_TYPE_STYLUS` and friends), so the pen is
//! recognised by its pressure instead: Android reports a constant 1.0 for a
//! finger, while a stylus reports its real pressure. A pointer that presses
//! less than [`FINGER_FORCE`] is treated as a pen and is not counted as a
//! finger, so resting the S Pen on the screen while tapping with three
//! fingers still counts three.
//!
//! The finger count is also taken per input device, so a pen that reports as
//! its own device never inflates the count.

use eframe::egui::{self, Event, Pos2, TouchDeviceId, TouchId, TouchPhase};
use std::collections::BTreeMap;

/// A pointer that presses less than this is a stylus, not a finger.
const FINGER_FORCE: f32 = 0.98;
/// How far a pointer may travel (in egui points) and still count as a tap.
const TAP_SLOP: f32 = 24.0;
/// How long the whole tap may take (seconds).
const TAP_TIMEOUT: f64 = 0.6;

/// A recognised tap.
#[derive(Clone, Copy, Debug)]
pub struct Tap {
    /// How many fingers took part (3 or 4).
    pub fingers: usize,
    /// Where the tap happened, in egui points.
    pub pos: Pos2,
}

/// One pointer being tracked.
struct Tracked {
    /// The input device the pointer came from.
    device: TouchDeviceId,
    /// Where the pointer went down.
    start: Pos2,
    /// Has it travelled further than the tap slop?
    moved: bool,
    /// Does it look like a finger (and not a pressure-sensitive stylus)?
    finger: bool,
}

/// Recognises multi-finger taps in egui's touch stream.
pub struct GestureDetector {
    /// The pointers currently on the screen.
    active: BTreeMap<TouchId, Tracked>,
    /// The most fingers that were down at the same time.
    peak: usize,
    /// Did any pointer of the current gesture travel too far?
    moved: bool,
    /// Was the current gesture interrupted (a cancelled pointer)?
    cancelled: bool,
    /// When the current gesture started (egui time, seconds).
    started: Option<f64>,
    /// Where the pointer that lifted last was.
    last_up: Option<Pos2>,
    /// A tap that is ready to be delivered: it is held back one frame, so the
    /// app has committed whatever the tap painted by the time it is delivered.
    pending: Option<Tap>,
    /// The egui time of the current frame.
    now: f64,
}

impl Default for GestureDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl GestureDetector {
    pub fn new() -> Self {
        GestureDetector {
            active: BTreeMap::new(),
            peak: 0,
            moved: false,
            cancelled: false,
            started: None,
            last_up: None,
            pending: None,
            now: 0.0,
        }
    }

    /// Feed the frame's events. Returns a tap that is ready to act on.
    ///
    /// The tap is delivered one frame after the fingers lifted: the frame the
    /// fingers lifted in is the frame the app commits whatever the tap
    /// painted, and the caller needs that to be over before it acts.
    pub fn update(&mut self, ctx: &egui::Context) -> Option<Tap> {
        // The tap detected in the previous frame is the one to deliver now.
        let ready = self.pending.take();
        self.now = ctx.input(|i| i.time);
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            if let Event::Touch { device_id, id, phase, pos, force } = event {
                self.on_touch(device_id, id, phase, pos, force);
            }
        }
        if self.pending.is_some() {
            // A tap waits one frame (so the app has committed whatever it
            // painted): make sure that frame comes.
            ctx.request_repaint();
        }
        ready
    }

    fn on_touch(
        &mut self,
        device: TouchDeviceId,
        id: TouchId,
        phase: TouchPhase,
        pos: Pos2,
        force: Option<f32>,
    ) {
        match phase {
            TouchPhase::Start => {
                if self.active.is_empty() {
                    // A new gesture: forget the previous one's bookkeeping.
                    self.peak = 0;
                    self.moved = false;
                    self.cancelled = false;
                    self.started = Some(self.now);
                    self.last_up = None;
                }
                self.active.insert(
                    id,
                    Tracked { device, start: pos, moved: false, finger: is_finger(force) },
                );
                self.update_peak();
            }
            TouchPhase::Move => {
                if let Some(tracked) = self.active.get_mut(&id)
                    && !tracked.moved
                    && pos.distance(tracked.start) > TAP_SLOP
                {
                    tracked.moved = true;
                    self.moved = true;
                }
            }
            TouchPhase::End | TouchPhase::Cancel => {
                if phase == TouchPhase::Cancel {
                    // An interrupted gesture is not a tap.
                    self.cancelled = true;
                }
                self.last_up = Some(pos);
                self.active.remove(&id);
                if self.active.is_empty() {
                    self.finish();
                }
            }
        }
    }

    /// The most fingers down at the same time, counted per input device.
    fn update_peak(&mut self) {
        let mut counts: BTreeMap<TouchDeviceId, usize> = BTreeMap::new();
        for tracked in self.active.values() {
            if tracked.finger {
                *counts.entry(tracked.device).or_insert(0) += 1;
            }
        }
        if let Some(max) = counts.values().copied().max() {
            self.peak = self.peak.max(max);
        }
    }

    /// The gesture is over: is it a tap?
    fn finish(&mut self) {
        let fingers = self.peak;
        let duration = self.started.map(|s| self.now - s).unwrap_or(0.0);
        let is_tap =
            (fingers == 3 || fingers == 4) && !self.moved && !self.cancelled && duration <= TAP_TIMEOUT;
        if is_tap {
            let pos = self.last_up.unwrap_or(Pos2::ZERO);
            self.pending = Some(Tap { fingers, pos });
        }
        self.active.clear();
        self.peak = 0;
        self.moved = false;
        self.cancelled = false;
        self.started = None;
        self.last_up = None;
    }
}

/// Android reports a constant 1.0 for a finger; a stylus reports real
/// pressure, so a lighter touch is a pen.
fn is_finger(force: Option<f32>) -> bool {
    match force {
        None => true,
        Some(force) => force >= FINGER_FORCE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(device: u64, id: u64, phase: TouchPhase, pos: Pos2, force: Option<f32>) -> Event {
        Event::Touch {
            device_id: TouchDeviceId(device),
            id: TouchId(id),
            phase,
            pos,
            force,
        }
    }

    /// A detector fed with a list of events, the way `update` does.
    fn feed(detector: &mut GestureDetector, events: &[Event]) {
        for event in events {
            if let Event::Touch { device_id, id, phase, pos, force } = *event {
                detector.on_touch(device_id, id, phase, pos, force);
            }
        }
    }

    #[test]
    fn three_finger_tap_is_a_tap() {
        let mut d = GestureDetector::new();
        d.now = 1.0;
        feed(
            &mut d,
            &[
                touch(1, 0, TouchPhase::Start, Pos2::new(10.0, 10.0), Some(1.0)),
                touch(1, 1, TouchPhase::Start, Pos2::new(20.0, 10.0), Some(1.0)),
                touch(1, 2, TouchPhase::Start, Pos2::new(30.0, 10.0), Some(1.0)),
            ],
        );
        d.now = 1.2;
        feed(
            &mut d,
            &[
                touch(1, 0, TouchPhase::End, Pos2::new(10.0, 10.0), Some(1.0)),
                touch(1, 1, TouchPhase::End, Pos2::new(20.0, 10.0), Some(1.0)),
                touch(1, 2, TouchPhase::End, Pos2::new(30.0, 10.0), Some(1.0)),
            ],
        );
        let tap = d.pending.expect("a three-finger tap is a tap");
        assert_eq!(tap.fingers, 3);
    }

    #[test]
    fn a_drag_is_not_a_tap() {
        let mut d = GestureDetector::new();
        d.now = 1.0;
        feed(
            &mut d,
            &[
                touch(1, 0, TouchPhase::Start, Pos2::new(10.0, 10.0), Some(1.0)),
                touch(1, 1, TouchPhase::Start, Pos2::new(20.0, 10.0), Some(1.0)),
                touch(1, 2, TouchPhase::Start, Pos2::new(30.0, 10.0), Some(1.0)),
            ],
        );
        d.now = 1.1;
        feed(&mut d, &[touch(1, 0, TouchPhase::Move, Pos2::new(200.0, 10.0), Some(1.0))]);
        d.now = 1.2;
        feed(
            &mut d,
            &[
                touch(1, 0, TouchPhase::End, Pos2::new(200.0, 10.0), Some(1.0)),
                touch(1, 1, TouchPhase::End, Pos2::new(20.0, 10.0), Some(1.0)),
                touch(1, 2, TouchPhase::End, Pos2::new(30.0, 10.0), Some(1.0)),
            ],
        );
        assert!(d.pending.is_none(), "a drag is not a tap");
    }

    #[test]
    fn a_pen_is_not_a_finger() {
        let mut d = GestureDetector::new();
        d.now = 1.0;
        // The pen rests on the screen (light pressure) while three fingers tap.
        feed(&mut d, &[touch(2, 0, TouchPhase::Start, Pos2::new(5.0, 5.0), Some(0.2))]);
        feed(
            &mut d,
            &[
                touch(1, 1, TouchPhase::Start, Pos2::new(10.0, 10.0), Some(1.0)),
                touch(1, 2, TouchPhase::Start, Pos2::new(20.0, 10.0), Some(1.0)),
                touch(1, 3, TouchPhase::Start, Pos2::new(30.0, 10.0), Some(1.0)),
            ],
        );
        d.now = 1.1;
        feed(
            &mut d,
            &[
                touch(2, 0, TouchPhase::End, Pos2::new(5.0, 5.0), Some(0.2)),
                touch(1, 1, TouchPhase::End, Pos2::new(10.0, 10.0), Some(1.0)),
                touch(1, 2, TouchPhase::End, Pos2::new(20.0, 10.0), Some(1.0)),
                touch(1, 3, TouchPhase::End, Pos2::new(30.0, 10.0), Some(1.0)),
            ],
        );
        let tap = d.pending.expect("three fingers still count as three");
        assert_eq!(tap.fingers, 3);
    }

    #[test]
    fn a_slow_tap_is_not_a_tap() {
        let mut d = GestureDetector::new();
        d.now = 1.0;
        feed(
            &mut d,
            &[
                touch(1, 0, TouchPhase::Start, Pos2::new(10.0, 10.0), Some(1.0)),
                touch(1, 1, TouchPhase::Start, Pos2::new(20.0, 10.0), Some(1.0)),
                touch(1, 2, TouchPhase::Start, Pos2::new(30.0, 10.0), Some(1.0)),
            ],
        );
        d.now = 1.0 + TAP_TIMEOUT + 0.1;
        feed(
            &mut d,
            &[
                touch(1, 0, TouchPhase::End, Pos2::new(10.0, 10.0), Some(1.0)),
                touch(1, 1, TouchPhase::End, Pos2::new(20.0, 10.0), Some(1.0)),
                touch(1, 2, TouchPhase::End, Pos2::new(30.0, 10.0), Some(1.0)),
            ],
        );
        assert!(d.pending.is_none(), "a slow touch is not a tap");
    }

    #[test]
    fn an_interrupted_gesture_is_not_a_tap() {
        let mut d = GestureDetector::new();
        d.now = 1.0;
        feed(
            &mut d,
            &[
                touch(1, 0, TouchPhase::Start, Pos2::new(10.0, 10.0), Some(1.0)),
                touch(1, 1, TouchPhase::Start, Pos2::new(20.0, 10.0), Some(1.0)),
                touch(1, 2, TouchPhase::Start, Pos2::new(30.0, 10.0), Some(1.0)),
            ],
        );
        d.now = 1.1;
        feed(
            &mut d,
            &[
                touch(1, 0, TouchPhase::Cancel, Pos2::new(10.0, 10.0), Some(1.0)),
                touch(1, 1, TouchPhase::End, Pos2::new(20.0, 10.0), Some(1.0)),
                touch(1, 2, TouchPhase::End, Pos2::new(30.0, 10.0), Some(1.0)),
            ],
        );
        assert!(d.pending.is_none(), "an interrupted gesture is not a tap");
    }
}
