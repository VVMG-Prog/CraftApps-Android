# Fix log — PhotoCraft 0.4.0 (2026-10-19)

The three problems reported against PhotoCraft 0.3.9 on the Tab S6 Lite:

1. The S Pen's barrel button does nothing.
2. The multi-finger "stylus mode" gestures (3-finger = Undo, 4-finger = tool
   wheel) never fire.
3. The file browser does not work.

All three turned out to share one root cause, found by reading the AOSP
sources. The fixes are below, file by file.

**Status: built (0.4.0, versionCode 100400), awaiting hardware test.**

---

## 1. The root cause: a `NativeActivity` never sees input in Java

`android.app.NativeActivity.onCreate` does this (AOSP
`core/java/android/app/NativeActivity.java`):

```java
getWindow().takeSurface(this);
getWindow().takeInputQueue(this);
```

and AOSP's contract for `takeInputQueue` (`core/java/android/view/Window.java`)
is:

> Take ownership of this window's InputQueue. The window will no longer read
> and dispatch input events from the queue; it is your responsibility to do so.

The native side (winit) owns the queue, so the Java view hierarchy — and the
Activity itself — receive **no touch, key or generic-motion events**. Every
input hook in `PhotoCraftActivity` was therefore dead code:

| Hook | Was supposed to | Reality |
|---|---|---|
| `dispatchTouchEvent` | forward the pen's tool type + barrel button | never called |
| `dispatchGenericMotionEvent` | forward a hover button press | never called |
| `dispatchKeyEvent` | forward Ctrl/Shift/Alt from a Bluetooth keyboard | never called |
| the 3-/4-finger tap detector | fire the stylus-mode gestures | never called |

This also explains the symptom recorded in the old AGENTS.md — "the button-state
indicator never lights up = events aren't arriving". The events were never
arriving *in Java*, and the S Pen's button is not reported by the Tab S6 Lite's
passive pen in the first place (see §4).

**Consequence:** anything that needs input data that winit drops (the tool
type, the barrel-button state) can only be fixed by replacing the input
backend, not by patching Java. Everything else can be done in Rust, on top of
egui's event stream.

---

## 2. The gestures (issue 2)

### What was done

The tap recognition moved from Java to Rust, fed by egui's touch events
(winit → egui-winit → egui). `Event::Touch` carries the pointer id, the input
device id, the position and the pressure — everything a tap needs.

**New: `shared/src/gestures.rs`**

* 3 or 4 pointers down and up within 600 ms, none travelling more than 24
  egui points, counts as a tap.
* The finger count is taken **per input device**, so a stylus on its own
  device cannot inflate it.
* A pointer that presses less than 0.98 is treated as a **stylus**, not a
  finger: Android reports a constant 1.0 for a finger, while a stylus reports
  its real pressure. A pen resting on the screen therefore does not turn a
  3-finger tap into a 4-finger one.
* A cancelled pointer (an interrupted gesture) is not a tap.
* The tap is delivered **one frame late** (see §3).

**New: `shared/src/modifiers.rs`** — Ctrl/Shift/Alt tracking. winit's Android
backend never sends `WindowEvent::ModifiersChanged`, so egui's modifier state
was always empty and every ⌘/Ctrl shortcut was dead. The modifier keys are
now tracked from egui's key events and published into egui's input state
(Ctrl is published as egui's `command`, as egui asks off macOS). This also
fixes Ctrl+O, Ctrl+Z, Ctrl+S and the rest.

**Changed: `photocraft/upstream/apps/photocraft-android/src/shell.rs`**

* `AndroidShell::process_input` runs in `App::logic`, before the app's frame:
  it updates and publishes the modifiers, feeds the gesture detector and fires
  the gesture.
* `AndroidShell::tap_painted_a_stroke` + the history check: see §3.

**Changed: `shared/src/lib.rs`** — the two new modules are registered.

**Removed from `PhotoCraftActivity.java`**: the Java tap detector (now
Rust). The pen side-channel (`dispatchTouchEvent` /
`dispatchGenericMotionEvent` / `dispatchKeyEvent`) is **kept** — it is the
only possible path to the barrel button if the app ever stops being a
`NativeActivity` — but it is documented as not firing today, and the debug
toasts are gone.

### Enabling it

`Edit ▸ Stylus Settings… ▸ "Enable stylus mode gestures"`, then 3-finger tap
= Undo, 4-finger tap = tool wheel (both configurable in the same panel).

---

## 3. The file browser (issue 3)

### What was broken

Two independent bugs, both fatal:

1. **`startPickerBridge()` was never called.** The Java `Activity` polls
   `<bridge>/request.json` with a `Handler`; without the poll, the request was
   written by Rust and never read — "Browse other locations…" did nothing.
   (The old AGENTS.md claimed the bridge was lazy-started on the first
   `dispatchTouchEvent`; it was not, and that could not have worked either,
   since `dispatchTouchEvent` never runs. `NativeActivity` does have a usable
   `onCreate`.)
2. **The response was polled only while a dialog was pending.** Handing the
   reply to the system picker closes the in-app browser, so `pending` became
   `None` — and the poll was inside `if self.pending.is_some()`. The answer
   was written by Java and never read, so the dialog hung forever.

### What was done

**`photocraft/android/.../PhotoCraftActivity.java`**

* `onCreate` starts the bridge (after `super.onCreate`, which is what loads
  the native library).
* The bridge directory is the app's **own external files dir**
  (`getExternalFilesDir(null)/.picker`): always writable, no "All files
  access" needed, private to the app. The old `/sdcard/CraftApps/.picker` is
  only a fallback now.
* A stale `request.json` from a previous run is deleted at start-up (it would
  otherwise open the system picker at launch).
* The request carries `kind` (`open` / `save` / `folder`), `mimes`, `name` and
  `multiple`; the response carries `paths` (open), `uri` (save/folder) and
  `name`.
* **Open**: the picked files are copied into `<bridge>/inbox/<id>/` and their
  local paths are returned, so the app reads them as ordinary files.
  Multi-select is supported.
* **Save**: the **document URI** is returned (with a persistable write grant),
  so the app writes through SAF and the file lands where the user chose. The
  display name is returned too, so the export picks the right format.
* **Folder**: the tree URI is returned.

**`shared/src/picker_bridge.rs`** — rewritten around a `PickerKind` enum and a
`PickerResponse` struct; the bridge directory comes from the
`CRAFTAPPS_BRIDGE_DIR` environment variable.

**`shared/src/saf.rs`** — `launch_open_document` / `launch_create_document` use
the new bridge API and pass `multiple` through; added
`launch_open_document_tree_bridge` (the JNI tree picker was broken for the
same reason as the other JNI pickers: it is launched from the render thread).

**`photocraft/upstream/apps/photocraft-android/src/file_browser.rs`**

* The reply handed to the system picker now waits in its own slot
  (`FileBrowser::picker`), not in `pending`.
* `FileBrowser::poll_picker` runs **every frame** and answers the waiting
  dialog: `Paths` for an open, `SaveTo(uri)` for a save, `FolderTo(uri)` for a
  folder (which also becomes the app's media location).
* A folder request opens the tree picker instead of the file picker.

**`photocraft/upstream/apps/photocraft-android/src/lib.rs`** — sets
`CRAFTAPPS_BRIDGE_DIR` to the app's external files dir.

### The tap's own dab

A multi-finger tap reaches the canvas as a click (egui-winit turns the first
touch into a pointer press), which paints a dab with the Brush tool — and the
Undo gesture would then undo that dab instead of the user's own last action.

`AndroidShell::process_input` therefore compares the history length across the
tap and, if the history grew and the last step is a stroke ("Brush Tool",
"Pencil", "Eraser"), undoes exactly that step before the gesture's own action
runs. The dab flashes for one frame and disappears; nothing else is touched.

---

## 4. The S Pen's barrel button (issue 1)

**Not fixable in software on this device.** The Tab S6 Lite's S Pen is a
passive Wacom EMR stylus with no Bluetooth radio. Samsung's S Pen Remote SDK
needs a Bluetooth S Pen, and the OS does not deliver the button to third-party
apps on this tablet (it works in Samsung Notes, which is a Samsung app with
private access to the digitizer).

The button needs *both* a Bluetooth S Pen and an input backend that reports
the barrel-button state (winit drops it; only the Java side-channel had it,
and that never fires). The multi-finger gestures are the working replacement.

Two things still need the tool type, which is only available through the same
dead Java channel, and so remain unsupported on this device:

* palm rejection (needs `TOOL_TYPE_PALM`),
* the eraser end of the stylus (needs `TOOL_TYPE_ERASER`).

---

## 5. Smaller changes in the same release

* `shared/src/pen.rs` — `PenSettings` is `#[serde(default)]`, so a `pen.json`
  written by an older build keeps the settings it has instead of losing all of
  them to a failed parse.
* `shared/src/jni_bridge.rs` — `init()` is `#[cfg]`-split, so the shared crate
  builds (and unit-tests) on the host.
* `shared/Cargo.toml` — an `x11` dev-dependency for eframe: without a window
  system winit refuses to compile off Android, and the shared crate's pure
  logic (pen settings, gestures, modifiers) is now unit-tested on the host.
* `shared/src/gestures.rs`, `shared/src/modifiers.rs` — 7 new unit tests.

---

## 6. How to verify on the tablet

1. Install `PhotoCraft-0.4.0-universal.apk` (same signing key, so it updates
   0.3.9 in place).
2. `Edit ▸ Stylus Settings… ▸ Enable stylus mode gestures`.
3. 3-finger tap on the canvas → Undo. 4-finger tap → the tool wheel.
   * With the Brush tool, the tap must not leave a dab behind.
   * A pen resting on the screen must not change the finger count.
4. `File ▸ Open ▸ Browse other locations…` → the system picker opens → pick a
   file → it opens. Same for "Save elsewhere…": the file must appear where it
   was saved.
5. Ctrl+O / Ctrl+Z with a Bluetooth keyboard.

## 7. Still to do (the other four apps)

The gesture detector and the modifier tracker live in the shared crate, so the
other apps only need to call them from their shell; the picker bridge needs
its `Activity` to start the poll and its file browser to poll the response.
See the TODO list in `AGENTS.md`.
