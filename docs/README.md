# CraftApps Android ports — documentation

Everything an agent (or a human) needs to know about the Android ports, in
one index.

| Document | What it is |
|---|---|
| [`../AGENTS.md`](../AGENTS.md) | **The main document.** The project, the build rules, the protocols (JNI bridge, picker bridge, pen runtime), the hard-won lessons and the TODO list. Read this first. |
| [`../PATCHES.md`](../PATCHES.md) | What the port changes in relation to the upstream apps, and the shared crate. |
| [`android-upstream-patches.md`](android-upstream-patches.md) | The short list of upstream patches that must be **re-applied when re-pinning** a new upstream tag. |
| [`audit-2026-10-19.md`](audit-2026-10-19.md) | The audit that found the dead code paths and the promised-but-missing patches. |

## The fix logs

One file per release: the symptom, the root cause, the change file by file,
how to verify it on the tablet, and what is still open.

| Release | What it fixed |
|---|---|
| [`fixes-0.4.0.md`](fixes-0.4.0.md) | The **`NativeActivity` input trap** (the Java side never receives input — the root cause of the dead gestures, the dead modifiers and the dead pen side-channel), the **multi-finger gestures** in Rust, the **keyboard modifiers** in Rust, and the **file picker bridge** (never started, never polled). |
| [`fixes-0.4.1.md`](fixes-0.4.1.md) | The **S Pen's side button**: the vendored android-activity patch that recovers the button and the tool type from the motion events. (This also corrects the old, wrong "the button does not work" note.) |
| [`fixes-0.4.3.md`](fixes-0.4.3.md) | The **button while the pen hovers** (and the reversed hovering indicator of 0.4.2), the **two missing upstream patches** (the menu bar could disappear; the port crate was not in the layer table), and the **dead code** of the old JNI picker path. |

> 0.4.2 was an intermediate build with the reversed hovering indicator; it is
> superseded by 0.4.3 and is not documented separately.

## The rule

Every fix is documented: the log entry above, a durable lesson in
`AGENTS.md`, and — when it touches an upstream patch or a protocol — the
matching update in `PATCHES.md` and `docs/android-upstream-patches.md`.
See the "Documentation Rule" in `../AGENTS.md`.

## Diagnostics in the app

The Stylus Settings (Edit ▸ Stylus Settings…) carry the diagnostics for the
stylus work; they are the fastest way to tell what the tablet actually
sends:

| Indicator | What it tells you |
|---|---|
| ● Primary / Secondary / Eraser | Which pen buttons the app currently sees as held. |
| ● Pen hovering | The pen is near the screen (hover events are arriving). |
| Last stylus event: action …, buttons 0x…, tool … | The last motion event: the action (7/9/10 = hover, 0/1/2/3 = down/up/move/cancel, 11/12 = button press/release), the button bits (`0x20` = the stylus' primary button) and the tool type (1 = finger, 2 = stylus, 4 = eraser, 5 = palm). |
