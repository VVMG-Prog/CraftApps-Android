package app.craftlauncher.android;

import android.app.NativeActivity;
import android.content.Intent;
import android.view.KeyEvent;

/**
 * The launcher's activity: a {@link NativeActivity} subclass. The Rust side
 * drives Install / Open entirely through the JNI bridge's activity ref
 * (which *is* this activity) — {@code startActivity} for opening suite apps
 * and the package installer — so no {@code onActivityResult} plumbing is
 * needed here. {@link #launchForResult} is kept for parity with the suite
 * ports' activities.
 *
 * The native library is loaded by the framework via the
 * {@code android.app.lib_name} meta-data, exactly as with a plain
 * NativeActivity.
 */
public class CraftLauncherActivity extends NativeActivity {

    @Override
    protected void onCreate(android.os.Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        applyImmersiveFullscreen();
    }

    /** Force true fullscreen: hide the status bar and the system navigation
     *  bar / tablet taskbar ("dock") so they never overlay the app. Sticky
     *  immersive: a swipe from a screen edge reveals the bars briefly. */
    private void applyImmersiveFullscreen() {
        try {
            if (android.os.Build.VERSION.SDK_INT >= 30) {
                getWindow().setDecorFitsSystemWindows(false);
                android.view.WindowInsetsController c = getWindow().getInsetsController();
                if (c != null) {
                    c.hide(android.view.WindowInsets.Type.systemBars());
                    c.setSystemBarsBehavior(
                        android.view.WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
                }
            } else {
                getWindow().getDecorView().setSystemUiVisibility(
                    android.view.View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                    | android.view.View.SYSTEM_UI_FLAG_FULLSCREEN
                    | android.view.View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                    | android.view.View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                    | android.view.View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                    | android.view.View.SYSTEM_UI_FLAG_LAYOUT_STABLE);
            }
        } catch (Exception ignored) {
            // Fullscreen is cosmetic; never let it break start-up.
        }
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            applyImmersiveFullscreen();
        }
    }
    // ---- Bluetooth keyboard modifier tracking ---------------------------------
    // winit's Android backend doesn't reliably report Ctrl/Shift/Alt meta state
    // from Bluetooth keyboards. We track modifier keys here and synthesize
    // corrected KeyEvents with proper meta state.

    /** Currently pressed modifier keys (bitmask: 1=Ctrl, 2=Shift, 4=Alt). */
    private int modifierState = 0;

    @Override
    public boolean dispatchKeyEvent(KeyEvent event) {
        int keyCode = event.getKeyCode();
        int action = event.getAction();
        boolean down = action == KeyEvent.ACTION_DOWN;

        // Track modifier keys
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

    /**
     * Starts an activity for result. Must be called on the Activity itself:
     * Rust calls this on the bridge's activity ref (which *is* this
     * activity), never via {@code find_class} (attached threads can't
     * see app classes).
     */
    public void launchForResult(Intent intent, int requestCode) {
        runOnUiThread(() -> startActivityForResult(intent, requestCode));
    }
}
