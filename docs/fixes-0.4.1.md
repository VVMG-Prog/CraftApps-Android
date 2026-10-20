# Fix log — PhotoCraft 0.4.1 (2026-10-19): the S Pen's side button

**Correction to the 0.4.0 log.** 0.4.0 concluded that "the Samsung S Pen
button does not work on the Tab S6 Lite". **That was wrong**, and the reason
it was wrong matters:

> The old test ("`BUTTON_STYLUS_PRIMARY`, `ACTION_BUTTON_PRESS`,
> `KEYCODE_STYLUS_BUTTON_PRIMARY`, Samsung codes 211-214 — none fire") was run
> through the Java `dispatchTouchEvent`, **which never runs** on a
> `NativeActivity` (see `docs/fixes-0.4.0.md` §1). It proved nothing.

The user pointed out that drawing apps (Sketchbook) do use the S Pen button on
this tablet, and that is right: **the button is delivered to apps the standard
Android way** — `MotionEvent.getButtonState()` with `BUTTON_STYLUS_PRIMARY`,
plus `MotionEvent.getToolType()`. The NDK exposes both
(`AMotionEvent_getButtonState`, `AMotionEvent_getToolType`); winit is what
throws them away.

---

## 1. What the Tab S6 Lite actually sends

The Tab S6 Lite's S Pen is a passive Wacom EMR stylus: no battery, no
Bluetooth. So Samsung's **S Pen Remote SDK** cannot help — per Samsung's own
documentation that SDK "depends on internal Android framework modules" and
"requires devices of the Note series", and it is a Java API besides (dead in a
`NativeActivity`).

But the *button* is not a Bluetooth feature. The digitizer reports it, and on
this tablet it does so by **replacing the motion action** with a proprietary
variant:

| action | meaning                        |
|--------|--------------------------------|
| 211    | down, with the side button held |
| 213    | move, with the side button held |
| 212    | up, with the side button held   |
| 214    | cancel, with the side button held |

(The same table is what the Wacom One / S Pen write-ups describe; the S8+ uses
`getButtonState()` instead, so both signals are handled.)

The NDK has no name for those actions, so they arrive as
`MotionAction::__Unknown(211)`. **winit maps only the standard actions to a
touch phase and drops everything else** — so while the S Pen's button was
held, *every* touch event was dropped: the stroke stopped, and the button was
invisible. That is the bug.

## 2. The fix

The data has to be taken where the `MotionEvent` still exists: the crate that
owns the input queue, **android-activity**.

### The vendored patch

`patches/android-activity-0.6.1-stylus.patch` (181 lines) changes
android-activity 0.6.1:

1. **`MotionEvent::action()`** maps 211/213/212/214 back to down/move/up/cancel,
   so the events keep flowing while the button is held.
2. **`MotionEvent::record_stylus_state()`** records the stylus data of every
   motion event as it passes through: the button bits, the tool type, the
   action button and the position. The app never sees the `MotionEvent`
   itself (winit owns the queue), so this is the only way out.
   * A Samsung "with the button" action also sets
     `BUTTON_STYLUS_PRIMARY` in the recorded bits, so the app reads the
     button the standard way on every tablet.
   * A button-only event (`ACTION_BUTTON_PRESS`, no pointer) is recorded too.
3. **`input::drain_stylus_states()`** hands the recorded events over as a
   queue, not a sample: a quick press and release cannot slip through
   between two frames.

The patch is applied by `scripts/vendor-android-activity.sh`, which copies the
crate out of the cargo registry into `vendor/` and applies the patch. The
vendored copy is a **build artefact** (git-ignored): the repository keeps only
*our* patch, no third-party code.

The upstream workspace points at the vendored copy:

```toml
# photocraft/upstream/Cargo.toml
[patch.crates-io]
android-activity = { path = "../../vendor/android-activity" }
```

(`[patch.crates-io]` has to live in the workspace root, so this is one of the
patches that must be re-applied when re-pinning upstream — it is listed in
`docs/android-upstream-patches.md`.)

`scripts/build-apk.sh` runs the vendoring script before the cargo build, so a
clean checkout builds without a manual step.

### The port

`AndroidShell::feed_stylus_state` (in
`photocraft/upstream/apps/photocraft-android/src/shell.rs`) drains the queue
each frame and hands every event to the shared pen runtime in the protocol it
already speaks (the Android action codes and the `BUTTON_*` bits):

```rust
craftapps_shared::pen::on_pen_event(
    state.action, state.tool_type, state.buttons as i32,
    state.action_button as i32, state.x, state.y,
);
```

The shared runtime (`shared/src/pen.rs`) then does the rest — it was written
for exactly this protocol and had simply never been fed:

* the **barrel button** press/release → the remapped action (the default is
  right-click; Stylus Settings → Pen buttons can make it Undo, Eyedropper,
  the tool wheel, …),
* the **eraser end** (`TOOL_TYPE_ERASER`) → the eraser action,
* **palm rejection** (`TOOL_TYPE_PALM`),
* the pen's pressure (unchanged: it comes through winit's `Touch.force`).

## 3. What to check on the tablet

1. **Hold the S Pen's side button while drawing.** The stroke must keep
   flowing (before: it stopped) — that alone proves the action codes are
   arriving and are being normalised.
2. **Stylus Settings → the button-state dots** must light up while the button
   is held.
3. **Stylus Settings → Pen buttons**: assign the primary button to *Undo*,
   then press it — the last action must be undone.
4. **Flip the pen to the eraser end** and draw: it must erase (the eraser
   action), and the "Eraser" dot in Stylus Settings must light up.
5. Rest a palm on the screen: with palm rejection on, the palm must not
   paint.

## 4. Still open

* The pen's **hover** is not delivered (winit drops hover events), so a
  button pressed while the pen hovers (not touching) may not be seen. The
  Tab S6 Lite has no Air Command anyway.
* The other four apps need the same three lines in their shell
  (`feed_stylus_state`) and the same `[patch.crates-io]` in their workspace.
