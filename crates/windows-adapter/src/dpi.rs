use windows_sys::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
    DPI_AWARENESS_CONTEXT_UNAWARE, DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED,
    GetThreadDpiAwarenessContext, SetThreadDpiAwarenessContext,
};

pub(crate) fn ensure_current_thread_per_monitor_v2(component: &'static str) -> Result<(), String> {
    if current_thread_is_per_monitor_v2() {
        return Ok(());
    }

    let _ = {
        // SAFETY: The adapter explicitly requires PMv2 coordinates for DWM/outer-rect
        // translation. Setting the awareness for the current runtime thread is a synchronous
        // Win32 call that affects only this thread.
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    };

    if current_thread_is_per_monitor_v2() {
        Ok(())
    } else {
        Err(format!(
            "{component} requires Per Monitor DPI Aware v2 thread context; current thread awareness is {}",
            current_thread_awareness_label()
        ))
    }
}

fn current_thread_is_per_monitor_v2() -> bool {
    let current = {
        // SAFETY: This is a read-only query for the current thread DPI awareness context.
        unsafe { GetThreadDpiAwarenessContext() }
    };
    awareness_context_equals(current, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
}

fn current_thread_awareness_label() -> &'static str {
    let current = {
        // SAFETY: This is a read-only query for the current thread DPI awareness context.
        unsafe { GetThreadDpiAwarenessContext() }
    };

    if awareness_context_equals(current, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
        "per-monitor-v2"
    } else if awareness_context_equals(current, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) {
        "per-monitor-v1"
    } else if awareness_context_equals(current, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) {
        "system-aware"
    } else if awareness_context_equals(current, DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED) {
        "unaware-gdi-scaled"
    } else if awareness_context_equals(current, DPI_AWARENESS_CONTEXT_UNAWARE) {
        "unaware"
    } else {
        "unknown"
    }
}

fn awareness_context_equals(left: DPI_AWARENESS_CONTEXT, right: DPI_AWARENESS_CONTEXT) -> bool {
    let equal = {
        // SAFETY: Both values are DPI awareness context handles returned by or defined for Win32.
        unsafe { AreDpiAwarenessContextsEqual(left, right) }
    };
    equal != 0
}
