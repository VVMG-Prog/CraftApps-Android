package app.vectorcraft.android;

import android.app.NativeActivity;
import android.content.Intent;
import android.view.KeyEvent;
import android.view.MotionEvent;

/**
 * VectorCraft's activity: a {@link NativeActivity} subclass driven by the
 * Rust eframe shell.
 *
 * <ul>
 *   <li>Bluetooth keyboard modifier tracking: winit's Android backend doesn't
 *       reliably report Ctrl/Shift/Alt meta state, so modifiers are tracked
 *       here and corrected KeyEvents are synthesized.</li>
 *   <li>Stylus side-channel: {@link #dispatchTouchEvent} still fires for
 *       NativeActivity subclasses. Pen-relevant state changes (tool type,
 *       barrel buttons) are forwarded to Rust via {@code nativeOnPenEvent};
 *       winit drops all of this, so without the hook the app can't tell a
 *       stylus from a finger. Throttled to state changes: a handful of JNI
 *       calls per stroke, not per motion event.</li>
 *   <li>SAF pickers: {@link #launchForResult} runs
 *       {@code startActivityForResult} on the UI thread (a call from the
 *       eframe render thread fails silently); results come back through
 *       {@code nativeOnActivityResult}.</li>
 * </ul>
 *
 * The native library is loaded by the framework via the
 * {@code android.app.lib_name} meta-data, exactly as with a plain
 * NativeActivity.
 */
public class VectorCraftActivity extends NativeActivity {
    // ---- Bluetooth keyboard modifier tracking ---------------------------------
    /** Currently pressed modifier keys (bitmask: 1=Ctrl, 2=Shift, 4=Alt). */
    private int modifierState = 0;

    @Override
    public boolean dispatchKeyEvent(KeyEvent event) {
        int keyCode = event.getKeyCode();
        int action = event.getAction();
        boolean down = action == KeyEvent.ACTION_DOWN;

        // Samsung S Pen button can arrive as KEYCODE_STYLUS_BUTTON_PRIMARY. Map it to
        // the primary barrel button press/release in the pen side-channel.
        if (keyCode == KeyEvent.KEYCODE_STYLUS_BUTTON_PRIMARY) {
            int motionAction =
                    down ? MotionEvent.ACTION_BUTTON_PRESS : MotionEvent.ACTION_BUTTON_RELEASE;
            nativeOnPenEvent(
                    motionAction,
                    MotionEvent.TOOL_TYPE_STYLUS,
                    down ? MotionEvent.BUTTON_STYLUS_PRIMARY : 0,
                    MotionEvent.BUTTON_STYLUS_PRIMARY);
            return super.dispatchKeyEvent(event);
        }

        int mask = 0;
        if (keyCode == KeyEvent.KEYCODE_CTRL_LEFT || keyCode == KeyEvent.KEYCODE_CTRL_RIGHT) {
            mask = 1;
        } else if (keyCode == KeyEvent.KEYCODE_SHIFT_LEFT || keyCode == KeyEvent.KEYCODE_SHIFT_RIGHT) {
            mask = 2;
        } else if (keyCode == KeyEvent.KEYCODE_ALT_LEFT || keyCode == KeyEvent.KEYCODE_ALT_RIGHT) {
            mask = 4;
        }

        if (mask != 0) {
            if (down) {
                modifierState |= mask;
            } else {
                modifierState &= ~mask;
            }
            return super.dispatchKeyEvent(event);
        } else {
            int eventMeta = event.getMetaState();
            int neededMeta = 0;
            if ((modifierState & 1) != 0) neededMeta |= KeyEvent.META_CTRL_ON | KeyEvent.META_CTRL_LEFT_ON;
            if ((modifierState & 2) != 0) neededMeta |= KeyEvent.META_SHIFT_ON | KeyEvent.META_SHIFT_LEFT_ON;
            if ((modifierState & 4) != 0) neededMeta |= KeyEvent.META_ALT_ON | KeyEvent.META_ALT_LEFT_ON;

            if ((eventMeta & neededMeta) != neededMeta) {
                KeyEvent corrected = new KeyEvent(
                    event.getDownTime(),
                    event.getEventTime(),
                    action,
                    keyCode,
                    event.getRepeatCount(),
                    eventMeta | neededMeta,
                    event.getDeviceId(),
                    event.getScanCode(),
                    event.getFlags() | KeyEvent.FLAG_SOFT_KEYBOARD,
                    event.getSource()
                );
                return super.dispatchKeyEvent(corrected);
            }
            return super.dispatchKeyEvent(event);
        }
    }

    // ---- Stylus side-channel ---------------------------------------------------
    /** Last tool type / button state forwarded to Rust (throttle state changes). */

    // TOOL_TYPE_PALM was added in API 35; we compile against 34, so define it locally.
    private static final int TOOL_TYPE_PALM = 5;

    @Override
    public boolean dispatchTouchEvent(MotionEvent event) {
        // Handle Samsung S Pen button proprietary actions
        int rawAction = event.getActionMasked();
        if (isSPenButtonAction(rawAction)) {
            // Synthesize button press: map to standard action with button state
            int mappedAction = mapSPenButtonAction(rawAction);
            int buttons = event.getButtonState() | MotionEvent.BUTTON_STYLUS_PRIMARY;
            // DEBUG: show toast so user can see if Java receives the event
            android.widget.Toast.makeText(this, "S Pen button: " + rawAction, android.widget.Toast.LENGTH_SHORT).show();
            nativeOnPenEvent(mappedAction, event.getToolType(0), buttons,
                MotionEvent.BUTTON_STYLUS_PRIMARY, event.getX(), event.getY());
            // Also let it through as a normal touch event
        }
        int action = event.getActionMasked();
        int tool = event.getToolType(event.getActionIndex());
        int buttons = event.getButtonState();
        // Forward stylus/eraser/palm state changes only; fingers need no tracking.
        // Also catch button press/release actions which can arrive here.
        boolean penRelevant = tool == MotionEvent.TOOL_TYPE_STYLUS
                || tool == MotionEvent.TOOL_TYPE_ERASER
                || tool == TOOL_TYPE_PALM
                || tool == MotionEvent.TOOL_TYPE_MOUSE
                || (buttons & (MotionEvent.BUTTON_STYLUS_PRIMARY
                        | MotionEvent.BUTTON_STYLUS_SECONDARY)) != 0
                || action == MotionEvent.ACTION_BUTTON_PRESS
                || action == MotionEvent.ACTION_BUTTON_RELEASE;
        if (penRelevant && (tool != lastPenTool || buttons != lastPenButtons)) {
            lastPenTool = tool;
            lastPenButtons = buttons;
            nativeOnPenEvent(
                    action,
                    tool,
                    buttons,
                    event.getActionButton());
        } else if (!penRelevant) {
            lastPenTool = -1;
            lastPenButtons = -1;
        }
        return super.dispatchTouchEvent(event);
    }

    private native void nativeOnPenEvent(int action, int toolType, int buttonState, int actionButton, float x, float y);

    // ---- Stylus button via generic motion / key events ---------------------------
    // Samsung S Pen button presses don't always come through dispatchTouchEvent.
    // Some devices deliver them as generic motion events (SOURCE_STYLUS) or as
    // KEYCODE_STYLUS_BUTTON_PRIMARY key events. Handle both so the button is reliable.

    @Override
    public boolean dispatchGenericMotionEvent(MotionEvent event) {
        int source = event.getSource();
        // Stylus button events via generic motion (e.g. hover button press).
        if ((source & android.view.InputDevice.SOURCE_STYLUS) != 0) {
            int action = event.getActionMasked();
            int tool = event.getToolType(event.getActionIndex());
            int buttons = event.getButtonState();
            if (action == MotionEvent.ACTION_BUTTON_PRESS
                    || action == MotionEvent.ACTION_BUTTON_RELEASE
                    || (buttons & (MotionEvent.BUTTON_STYLUS_PRIMARY
                            | MotionEvent.BUTTON_STYLUS_SECONDARY)) != 0) {
                // Forward as-is; Rust handles ACTION_BUTTON_PRESS/RELEASE and held-state.
                // Don't consume; let the framework also process it.
                nativeOnPenEvent(action, tool, buttons, event.getActionButton(), event.getX(), event.getY());
            }
        }
        return super.dispatchGenericMotionEvent(event);
    }

    // ---- SAF pickers ------------------------------------------------------------
    /**
     * Starts an activity for result. Must run on the UI thread: Rust calls
     * this from the eframe render thread, where startActivityForResult fails
     * silently.
     */
    public static void launchForResult(android.app.Activity activity, Intent intent, int requestCode) {
        activity.runOnUiThread(() -> activity.startActivityForResult(intent, requestCode));
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == REQ_BRIDGE_OPEN || requestCode == REQ_BRIDGE_SAVE) {
            handleBridgeResult(requestCode, resultCode, data);
            return;
        }
        nativeOnActivityResult(requestCode, resultCode, data);
    }

    private void handleBridgeResult(int requestCode, int resultCode, Intent data) {
        long id = bridgeRequestId;
        bridgeRequestId = -1;
        if (resultCode != RESULT_OK || data == null || data.getData() == null) {
            writePickerResponse(id, new String[0], true);
            return;
        }
        try {
            android.net.Uri uri = data.getData();
            // Copy to a temp file Rust can read
            java.io.File inbox = new java.io.File("/sdcard/CraftApps/.picker/inbox/" + id);
            inbox.mkdirs();
            String name = "picked";
            try (android.database.Cursor cur = getContentResolver().query(uri, null, null, null, null)) {
                if (cur != null && cur.moveToFirst()) {
                    int idx = cur.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME);
                    if (idx >= 0) name = cur.getString(idx);
                }
            } catch (Exception e) { /* use default */ }
            java.io.File out = new java.io.File(inbox, name);
            try (java.io.InputStream in = getContentResolver().openInputStream(uri);
                 java.io.OutputStream outStream = new java.io.FileOutputStream(out)) {
                byte[] buf = new byte[8192];
                int n;
                while ((n = in.read(buf)) > 0) outStream.write(buf, 0, n);
            }
            writePickerResponse(id, new String[]{out.getAbsolutePath()}, false);
        } catch (Exception e) {
            writePickerResponse(id, new String[0], true);
        }
    }

    private native void nativeOnActivityResult(int requestCode, int resultCode, Intent data);

    // ---- File picker bridge (polls for Rust requests) --------------------
    private android.os.Handler pickerHandler = new android.os.Handler(android.os.Looper.getMainLooper());
    private boolean pickerBridgeStarted = false;
    private static final int REQ_BRIDGE_OPEN = 1001;
    private static final int REQ_BRIDGE_SAVE = 1002;
    private long bridgeRequestId = -1;

    private void startPickerBridge() {
        pickerHandler.postDelayed(new Runnable() {
            @Override public void run() {
                checkPickerRequest();
                pickerHandler.postDelayed(this, 500);
            }
        }, 500);
    }

    private void checkPickerRequest() {
        try {
            java.io.File req = new java.io.File("/sdcard/CraftApps/.picker/request.json");
            if (!req.isFile()) return;
            String text = new String(java.nio.file.Files.readAllBytes(req.toPath()));
            req.delete();
            org.json.JSONObject r = new org.json.JSONObject(text);
            long id = r.getLong("id");
            String kind = r.getString("kind");
            bridgeRequestId = id;
            Intent intent;
            int code;
            if ("open".equals(kind)) {
                intent = new Intent(Intent.ACTION_OPEN_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType("*/*");
                intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
                code = REQ_BRIDGE_OPEN;
            } else {
                intent = new Intent(Intent.ACTION_CREATE_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE)
                    .setType("application/octet-stream").putExtra(Intent.EXTRA_TITLE, r.optString("name", "file"));
                code = REQ_BRIDGE_SAVE;
            }
            try {
                startActivityForResult(intent, code);
            } catch (Exception e) {
                writePickerResponse(id, new String[0], true);
                bridgeRequestId = -1;
            }
        } catch (Exception e) {
            // No request or parse error; keep polling
        }
    }

    private void writePickerResponse(long id, String[] paths, boolean cancelled) {
        try {
            org.json.JSONObject resp = new org.json.JSONObject();
            resp.put("id", id);
            resp.put("cancelled", cancelled);
            org.json.JSONArray arr = new org.json.JSONArray();
            for (String p : paths) arr.put(p);
            resp.put("paths", arr);
            java.io.File dir = new java.io.File("/sdcard/CraftApps/.picker");
            dir.mkdirs();
            java.io.File tmp = new java.io.File(dir, "response.json.tmp");
            java.nio.file.Files.write(tmp.toPath(), resp.toString().getBytes());
            tmp.renameTo(new java.io.File(dir, "response.json"));
        } catch (Exception e) { /* ignore */ }
    }


    // Samsung S Pen button proprietary action codes.
    // When the S Pen button is held, Samsung uses these instead of the standard
    // ACTION_DOWN/MOVE/UP. Without handling them, the button press is invisible.
    private static final int ACTION_DOWN_WITH_1ST_BUTTON = 211;
    private static final int ACTION_MOVE_WITH_1ST_BUTTON = 213;
    private static final int ACTION_UP_WITH_1ST_BUTTON = 212;
    private static final int ACTION_CANCEL_WITH_1ST_BUTTON = 214;

    /** Returns true if this is a Samsung S Pen button action. */
    private static boolean isSPenButtonAction(int action) {
        return action == ACTION_DOWN_WITH_1ST_BUTTON
            || action == ACTION_MOVE_WITH_1ST_BUTTON
            || action == ACTION_UP_WITH_1ST_BUTTON
            || action == ACTION_CANCEL_WITH_1ST_BUTTON;
    }

    /** Maps Samsung button actions to standard actions + button state. */
    private static int mapSPenButtonAction(int action) {
        switch (action) {
            case ACTION_DOWN_WITH_1ST_BUTTON: return MotionEvent.ACTION_DOWN;
            case ACTION_MOVE_WITH_1ST_BUTTON: return MotionEvent.ACTION_MOVE;
            case ACTION_UP_WITH_1ST_BUTTON: return MotionEvent.ACTION_UP;
            case ACTION_CANCEL_WITH_1ST_BUTTON: return MotionEvent.ACTION_CANCEL;
            default: return action;
        }
    }

}
