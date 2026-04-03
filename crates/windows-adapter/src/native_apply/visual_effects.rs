use std::{
    collections::{BTreeSet, HashMap, HashSet},
    ptr::null_mut,
    sync::{Mutex, OnceLock},
};

use windows_sys::Win32::{
    Foundation::{GetLastError, HWND},
    Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
        DWMWCP_ROUND, DwmSetWindowAttribute,
    },
    Graphics::Gdi::{CreateRectRgn, DeleteObject, SetWindowRgn},
    UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, LWA_ALPHA, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SetLayeredWindowAttributes, SetWindowLongPtrW,
        SetWindowPos, WS_EX_LAYERED,
    },
};

use crate::{WindowOpacityMode, WindowVisualEmphasis};

use super::{
    last_error_message,
    window_geometry::{
        FrameInsets, frame_insets, query_outer_window_rect, query_visible_frame_rect,
        visible_frame_is_compatible,
    },
};

const VISUAL_STYLE_REFRESH_FLAGS: u32 =
    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOMOVE | SWP_NOSIZE | SWP_FRAMECHANGED;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AppliedVisualState {
    pub(super) opacity_alpha: Option<u8>,
    pub(super) opacity_mode: WindowOpacityMode,
    pub(super) disable_visual_effects: bool,
    pub(super) border_color_rgb: Option<u32>,
    pub(super) rounded_corners: bool,
}

impl From<&WindowVisualEmphasis> for AppliedVisualState {
    fn from(value: &WindowVisualEmphasis) -> Self {
        Self {
            opacity_alpha: value.opacity_alpha,
            opacity_mode: value.opacity_mode,
            disable_visual_effects: value.disable_visual_effects,
            border_color_rgb: value.border_color_rgb,
            rounded_corners: value.rounded_corners,
        }
    }
}

pub(super) fn normalized_visual_emphasis(
    visual_emphasis: &WindowVisualEmphasis,
) -> WindowVisualEmphasis {
    if visual_emphasis.opacity_mode != WindowOpacityMode::BrowserSurrogate {
        return visual_emphasis.clone();
    }

    // Chromium-class surrogate rendering already proved unstable on live Edge surfaces.
    // Normalize any stale or externally supplied surrogate request to the safer overlay path
    // before we touch HWND visual state, so first-frame startup cannot regress back to it.
    let mut normalized = visual_emphasis.clone();
    normalized.opacity_mode = WindowOpacityMode::OverlayDim;
    normalized
}

pub(super) fn direct_layered_opacity_alpha(visual_emphasis: &WindowVisualEmphasis) -> Option<u8> {
    match visual_emphasis.opacity_mode {
        WindowOpacityMode::DirectLayered => visual_emphasis.opacity_alpha,
        WindowOpacityMode::BrowserSurrogate => None,
        WindowOpacityMode::OverlayDim => None,
    }
}

pub(super) fn browser_surrogate_alpha(visual_emphasis: &WindowVisualEmphasis) -> Option<u8> {
    match visual_emphasis.opacity_mode {
        WindowOpacityMode::DirectLayered => None,
        WindowOpacityMode::BrowserSurrogate => visual_emphasis.opacity_alpha,
        WindowOpacityMode::OverlayDim => None,
    }
}

pub(super) fn overlay_dim_alpha(visual_emphasis: &WindowVisualEmphasis) -> Option<u8> {
    match visual_emphasis.opacity_mode {
        WindowOpacityMode::DirectLayered => None,
        WindowOpacityMode::BrowserSurrogate => None,
        WindowOpacityMode::OverlayDim => visual_emphasis
            .opacity_alpha
            .map(overlay_dim_alpha_from_window_opacity),
    }
}

pub(super) const fn overlay_dim_alpha_from_window_opacity(opacity_alpha: u8) -> u8 {
    u8::MAX - opacity_alpha
}

pub(super) fn apply_window_opacity(
    hwnd: HWND,
    current_ex_style: u32,
    opacity_alpha: Option<u8>,
    force_clear_layered_style: bool,
) -> Result<(), String> {
    let tracked_hwnd = hwnd as isize;
    if let Some(opacity_alpha) = opacity_alpha {
        let currently_layered = style_has_layered(current_ex_style);
        let layered_style = current_ex_style | WS_EX_LAYERED;
        let _ = { unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, layered_style as isize) } };
        let layered_applied =
            { unsafe { SetLayeredWindowAttributes(hwnd, 0, opacity_alpha, LWA_ALPHA) } };
        if layered_applied == 0 {
            return Err(last_error_message("SetLayeredWindowAttributes"));
        }
        layered_hwnds()
            .lock()
            .expect("layered hwnd registry lock should not be poisoned")
            .insert(tracked_hwnd);
        if should_refresh_after_layered_enable(currently_layered) {
            refresh_window_frame(hwnd);
        }
        return Ok(());
    }

    let currently_layered = style_has_layered(current_ex_style);
    let wm_tracked_layered = layered_hwnds()
        .lock()
        .expect("layered hwnd registry lock should not be poisoned")
        .remove(&tracked_hwnd);
    if should_clear_layered_style(wm_tracked_layered, currently_layered) {
        if currently_layered {
            let _ = { unsafe { SetLayeredWindowAttributes(hwnd, 0, u8::MAX, LWA_ALPHA) } };
        }
        let cleared_style = current_ex_style & !WS_EX_LAYERED;
        let _ = { unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, cleared_style as isize) } };
        if currently_layered {
            refresh_window_frame(hwnd);
        }
    } else if force_clear_layered_style && wm_tracked_layered {
        let _ = { unsafe { SetLayeredWindowAttributes(hwnd, 0, u8::MAX, LWA_ALPHA) } };
    }

    Ok(())
}

pub(super) fn query_window_ex_style(hwnd: HWND) -> Result<u32, String> {
    let ex_style = { unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } };
    if ex_style == 0 {
        let error = { unsafe { GetLastError() } };
        if error != 0 {
            return Err(format!("GetWindowLongPtrW failed with Win32 error {error}"));
        }
    }

    Ok(ex_style as u32)
}

pub(super) const fn style_has_layered(ex_style: u32) -> bool {
    (ex_style & WS_EX_LAYERED) != 0
}

pub(super) const fn should_refresh_after_layered_enable(currently_layered: bool) -> bool {
    !currently_layered
}

pub(super) const fn should_clear_layered_style(
    wm_tracked_layered: bool,
    currently_layered: bool,
) -> bool {
    wm_tracked_layered && currently_layered
}

pub(super) fn should_skip_visual_write(
    previous_state: AppliedVisualState,
    desired_state: AppliedVisualState,
    force_clear_layered_style: bool,
    wm_tracked_layered: bool,
    currently_layered: bool,
) -> bool {
    previous_state == desired_state
        && (!force_clear_layered_style || !wm_tracked_layered || !currently_layered)
}

fn refresh_window_frame(hwnd: HWND) {
    let _ = { unsafe { SetWindowPos(hwnd, null_mut(), 0, 0, 0, 0, VISUAL_STYLE_REFRESH_FLAGS) } };
}

pub(super) fn layered_hwnds() -> &'static Mutex<HashSet<isize>> {
    static LAYERED_HWNDS: OnceLock<Mutex<HashSet<isize>>> = OnceLock::new();
    LAYERED_HWNDS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn applied_visual_states() -> &'static Mutex<HashMap<isize, AppliedVisualState>> {
    static APPLIED_VISUAL_STATES: OnceLock<Mutex<HashMap<isize, AppliedVisualState>>> =
        OnceLock::new();
    APPLIED_VISUAL_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn sync_window_native_clip_mask(
    hwnd: HWND,
    full_visible_rect: Option<flowtile_domain::Rect>,
    clip_rect: Option<flowtile_domain::Rect>,
) -> Result<(), String> {
    let tracked_hwnd = hwnd as isize;
    let Some(full_visible_rect) = full_visible_rect else {
        clear_window_native_clip_mask(hwnd, tracked_hwnd);
        return Ok(());
    };
    let Some(clip_rect) = clip_rect else {
        clear_window_native_clip_mask(hwnd, tracked_hwnd);
        return Ok(());
    };

    let outer_rect = query_outer_window_rect(hwnd)?;
    let visible_rect = query_visible_frame_rect(hwnd)
        .filter(|visible_rect| visible_frame_is_compatible(outer_rect, *visible_rect))
        .unwrap_or(outer_rect);
    let insets = frame_insets(outer_rect, visible_rect);
    let Some(region_bounds) = native_clip_region_bounds(full_visible_rect, clip_rect, insets)
    else {
        clear_window_native_clip_mask(hwnd, tracked_hwnd);
        return Ok(());
    };

    let region = unsafe {
        CreateRectRgn(
            region_bounds.left,
            region_bounds.top,
            region_bounds.right,
            region_bounds.bottom,
        )
    };
    if region.is_null() {
        return Err(last_error_message("CreateRectRgn"));
    }

    let applied = unsafe { SetWindowRgn(hwnd, region, 1) };
    if applied == 0 {
        let _ = unsafe { DeleteObject(region as _) };
        return Err(last_error_message("SetWindowRgn"));
    }

    native_clip_masked_hwnds()
        .lock()
        .expect("native clip mask registry lock should not be poisoned")
        .insert(tracked_hwnd);
    Ok(())
}

pub(super) fn native_clip_region_bounds(
    full_visible_rect: flowtile_domain::Rect,
    clip_rect: flowtile_domain::Rect,
    insets: FrameInsets,
) -> Option<windows_sys::Win32::Foundation::RECT> {
    let full_right = full_visible_rect
        .x
        .saturating_add(full_visible_rect.width.min(i32::MAX as u32) as i32);
    let full_bottom = full_visible_rect
        .y
        .saturating_add(full_visible_rect.height.min(i32::MAX as u32) as i32);
    let clip_right = clip_rect
        .x
        .saturating_add(clip_rect.width.min(i32::MAX as u32) as i32);
    let clip_bottom = clip_rect
        .y
        .saturating_add(clip_rect.height.min(i32::MAX as u32) as i32);
    let intersection_left = full_visible_rect.x.max(clip_rect.x);
    let intersection_top = full_visible_rect.y.max(clip_rect.y);
    let intersection_right = full_right.min(clip_right);
    let intersection_bottom = full_bottom.min(clip_bottom);
    let width = intersection_right.saturating_sub(intersection_left);
    let height = intersection_bottom.saturating_sub(intersection_top);
    if width <= 0 || height <= 0 {
        return None;
    }
    if intersection_left == full_visible_rect.x
        && intersection_top == full_visible_rect.y
        && intersection_right == full_right
        && intersection_bottom == full_bottom
    {
        return None;
    }

    let left = insets
        .left
        .saturating_add(intersection_left.saturating_sub(full_visible_rect.x));
    let top = insets
        .top
        .saturating_add(intersection_top.saturating_sub(full_visible_rect.y));
    Some(windows_sys::Win32::Foundation::RECT {
        left,
        top,
        right: left.saturating_add(width),
        bottom: top.saturating_add(height),
    })
}

fn clear_window_native_clip_mask(hwnd: HWND, tracked_hwnd: isize) {
    let was_masked = native_clip_masked_hwnds()
        .lock()
        .expect("native clip mask registry lock should not be poisoned")
        .remove(&tracked_hwnd);
    if was_masked {
        let _ = unsafe { SetWindowRgn(hwnd, std::ptr::null_mut(), 1) };
    }
}

fn native_clip_masked_hwnds() -> &'static Mutex<HashSet<isize>> {
    static NATIVE_CLIP_MASKED_HWNDS: OnceLock<Mutex<HashSet<isize>>> = OnceLock::new();
    NATIVE_CLIP_MASKED_HWNDS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn native_clip_masked_hwnds_snapshot() -> BTreeSet<u64> {
    native_clip_masked_hwnds()
        .lock()
        .expect("native clip mask registry lock should not be poisoned")
        .iter()
        .copied()
        .filter(|hwnd| *hwnd > 0)
        .map(|hwnd| hwnd as u64)
        .collect()
}

pub(super) fn apply_gapless_visual_policy(hwnd: HWND) {
    set_dwm_u32_attribute(hwnd, DWMWA_BORDER_COLOR as u32, DWMWA_COLOR_NONE);
}

pub(super) fn apply_window_border(hwnd: HWND, visual_emphasis: &WindowVisualEmphasis) {
    let border_color = visual_emphasis.border_color_rgb.unwrap_or(DWMWA_COLOR_NONE);
    let _ = visual_emphasis.border_thickness_px;
    set_dwm_u32_attribute(hwnd, DWMWA_BORDER_COLOR as u32, border_color);
}

#[cfg(test)]
mod tests {
    use super::{native_clip_masked_hwnds, native_clip_masked_hwnds_snapshot};

    #[test]
    fn native_clip_mask_snapshot_reflects_tracked_hwnds() {
        {
            let mut hwnds = native_clip_masked_hwnds()
                .lock()
                .expect("native clip mask registry lock should not be poisoned");
            hwnds.clear();
            hwnds.extend([101_isize, 202_isize, -1_isize]);
        }

        assert_eq!(
            native_clip_masked_hwnds_snapshot(),
            [101_u64, 202_u64].into_iter().collect()
        );

        native_clip_masked_hwnds()
            .lock()
            .expect("native clip mask registry lock should not be poisoned")
            .clear();
    }
}

pub(super) fn apply_window_corners(hwnd: HWND, visual_emphasis: &WindowVisualEmphasis) {
    let corner_preference = if visual_emphasis.rounded_corners {
        DWMWCP_ROUND
    } else {
        DWMWCP_DONOTROUND
    };
    set_dwm_i32_attribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
        corner_preference,
    );
}

fn set_dwm_i32_attribute(hwnd: HWND, attribute: u32, value: i32) {
    let _ = {
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attribute,
                &value as *const _ as *const _,
                std::mem::size_of::<i32>() as u32,
            )
        }
    };
}

fn set_dwm_u32_attribute(hwnd: HWND, attribute: u32, value: u32) {
    let _ = {
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attribute,
                &value as *const _ as *const _,
                std::mem::size_of::<u32>() as u32,
            )
        }
    };
}
