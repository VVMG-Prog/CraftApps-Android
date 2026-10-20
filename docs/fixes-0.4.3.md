# Fix log — PhotoCraft 0.4.3 (2026-10-19)

The S Pen's side button **while the pen hovers**, plus the two missing
upstream patches and the dead code from the old JNI picker path.

> 0.4.2 was the first build of the hover work; it had the hovering indicator
> **reversed** (it latched on and never went off) and the button still needed
> a touch. 0.4.3 fixes both. The 0.4.2 build can be ignored.

---

## 1. The S Pen's button while the pen hovers

### What was wrong

Two separate things, found by testing on the tablet:

1. **The "Pen hovering" indicator was reversed.** It showed *hovering* when
   the pen was nowhere near the screen, and it went off when the screen was
   tapped.
2. **The button still needed a touch** to work: pressing it while the pen
   only hovered did nothing.

### Why the indicator was reversed

`ACTION_HOVER_EXIT` (10) means the pen **left** the screen — the code treated
it like the other hover actions and *set* the flag, so it latched on and
never went off. Only a touch (or an up/cancel) cleared it.

**Fixed** (`shared/src/pen.rs`): the hover actions are now distinguished —

```rust
let hovering = matches!(action, ACTION_HOVER_MOVE | ACTION_HOVER_ENTER | ACTION_HOVER_EXIT);
if hovering {
    // ACTION_HOVER_EXIT means the pen *left* the screen: the flag must go
    // off, not on.
    rt.hovering = action != ACTION_HOVER_EXIT;
}
```

### Why the button needed a touch

The button state is carried by every motion event while it is held
(`MotionEvent.getButtonState()`), so the *shared runtime* has to read it from
**every** action — including the hover events. It did not: it read the held
state only in the "not a button event" branch, and a button press or release
was handled **exclusively** through the action and `getActionButton()`.

**Fixed** (`shared/src/pen.rs`): the held state is now used for every event,
and the button press/release actions are an *additional* source:

```rust
// The button state is carried by *every* event while the button is held
// (`MotionEvent.getButtonState()`), so use it for every action — including
// the hover events, where the button press arrives while the pen is not
// touching the screen.
set_button(&mut rt, PenButton::BarrelPrimary, primary_held, x, y);
set_button(&mut rt, PenButton::BarrelSecondary, secondary_held, x, y);
if action == ACTION_BUTTON_PRESS { /* … the action as an extra source … */ }
```

### The third path: the button as a key event

Android 14 and later report the stylus' side buttons as **key events**
(`KEYCODE_STYLUS_BUTTON_PRIMARY` = 308, `..._SECONDARY` = 309) — and on
some devices that is the *only* report a button press produces while the
pen hovers (there is no touch event then).

egui drops unknown key codes (`egui::Key` has no `Unidentified` variant), so
the port cannot see them. The **vendored android-activity patch** can: the raw
key event is still there. The patch now translates such a key event into the
same protocol as the motion events — an `ACTION_BUTTON_PRESS` /
`ACTION_BUTTON_RELEASE` with the matching button — so the app reads the button
the same way either way.

**`patches/android-activity-0.6.1-stylus.patch`** (239 lines) grew a third
part: `record_stylus_button_key()`.

### The diagnostic

The Stylus Settings now show the last event the stylus sent:

```
Last stylus event: action 7, buttons 0x20, tool 2
```

* `action` — which event arrived (7 = hover move, 9 = hover enter,
  10 = hover exit, 0/1/2/3 = down/up/move/cancel, 11/12 = button
  press/release).
* `buttons` — the button bits (`0x20` = the stylus' primary button is held).
* `tool` — 2 = stylus, 4 = eraser, 1 = finger.

**That line is the answer to "does the button work while hovering":** hover
the pen, press the button, and read the line. If the buttons show `0x20` the
app has the press; if they show `0x00` the tablet did not report it.

### What to check on the tablet

1. **Edit ▸ Stylus Settings…** → the **"Pen hovering"** dot: it must light up
   when the pen is near the screen and go off when the pen leaves it or
   touches the screen.
2. The **"Last stylus event"** line: hovering the pen must show a hover
   action (7/9/10) and the tool type 2.
3. Hover the pen and press the side button: the mapped action must fire (the
   default is a right-click at the pen's position; assign *Undo* in **Pen
   buttons** to see it clearly).

---

## 2. The menu bar could disappear on Android

`docs/android-upstream-patches.md` listed an upstream patch that was **not
actually applied**:

```rust
// crates/ui-egui/src/view_cmds.rs
pub fn hides_chrome(&self) -> bool {
    self.screen_mode == "fullScreen"      // <- no Android gate
}
```

`hides_chrome()` decides whether the title bar, the options bar, the status
bar and the toolbar are drawn (`crates/ui-egui/src/lib.rs`). In the
**fullScreen** screen mode it hides them — and on Android that takes away the
**menu bar**, which is the only way to reach the Android menu entries
(*Edit ▸ Stylus Settings…*, *Help ▸ Check for Updates*) and the options bar
with the tool options. The screen mode is a persisted preference, so one
accidental switch to fullScreen made the Stylus Settings unreachable.

**Fixed** — the patch the documentation promised, applied:

```rust
pub fn hides_chrome(&self) -> bool {
    // ANDROID-PORT: the menu bar is the only way to the Android menu
    // entries (Edit ▸ Stylus Settings…, Help ▸ Check for Updates) and the
    // options bar carries the tool options, so the chrome stays visible.
    #[cfg(target_os = "android")]
    { false }
    #[cfg(not(target_os = "android"))]
    { self.screen_mode == "fullScreen" }
}
```

**`xtask/src/layers.rs`** — the other missing patch: the port crate was not
in the layer table, so the upstream's own `cargo xtask layers` check could
not classify it:

```rust
// ANDROID-PORT: the Android port crate (apps/photocraft-android). It is
// an app shell, like the others.
("android", Class::Exempt),
```

**Lesson:** a patch listed in the documentation but not applied is worse than
no patch — the next agent trusts the list.

---

## 3. Dead code from the old JNI picker path

The file bridge replaced the JNI picker launch, which left ten unused items
in `shared/src/saf.rs` (`ACTION_OPEN_DOCUMENT`, `ACTION_CREATE_DOCUMENT`,
`CATEGORY_OPENABLE`, `EXTRA_MIME_TYPES`, `EXTRA_ALLOW_MULTIPLE`,
`EXTRA_TITLE`, `add_category`, `put_extra_bool`, `put_extra_string`,
`put_extra_mime_types`) — a warning on every build.

They are removed. What stays is what the JNI *tree* picker still needs
(`new_intent`, `add_flags`, `ACTION_OPEN_DOCUMENT_TREE`) — the other four
apps still use it.

`shared/src/picker_bridge.rs`: `PickerKind::from_str` went with them (it was
only used by a helper that is now gone); the kind names are tested directly.

**The shared crate now builds warning-free** (Android target).

---

## 4. The audit (what else was found)

The whole suite was audited for the same class of bug — code that looks
alive but never runs, and documentation that promises patches which were
never applied. The findings are in `docs/audit-2026-10-19.md`; the headline:
**the other four apps have the picker-bridge bug, the missing stylus data
and the missing modifiers** (all of them are in the "Known Issues & TODO"
list of `AGENTS.md`).
