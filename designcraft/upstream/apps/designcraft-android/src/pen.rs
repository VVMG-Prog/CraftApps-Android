//! Stylus side-channel JNI entry.
//!
//! winit's Android backend drops the pen tool type and barrel-button state,
//! so `DesignCraftActivity.dispatchTouchEvent` forwards pen state changes
//! here. The shared [`craftapps_shared::pen`] module debounces, remaps
//! buttons to actions, and queues them for the frame loop.

/// `DesignCraftActivity.nativeOnPenEvent(action, toolType, buttonState,
/// actionButton)` — see the activity's `dispatchTouchEvent` override.
///
/// `action`: `MotionEvent.getActionMasked()`; `toolType`: `getToolType()`
/// (2 = stylus, 4 = eraser); `buttonState`: `getButtonState()` bits;
/// `actionButton`: `getActionButton()` for press/release transitions.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_app_designcraft_android_DesignCraftActivity_nativeOnPenEvent(
    _env: *mut jni::sys::JNIEnv,
    _this: jni::sys::jobject,
    action: jni::sys::jint,
    tool_type: jni::sys::jint,
    button_state: jni::sys::jint,
    action_button: jni::sys::jint,
    x: jni::sys::jfloat,
    y: jni::sys::jfloat,
) {
    // SAFETY: the JVM invokes this with valid integer args on the UI thread;
    // `on_pen_event` is pure synchronized state, never touches the JNIEnv.
    craftapps_shared::pen::on_pen_event(action, tool_type, button_state, action_button, x, y);
}
