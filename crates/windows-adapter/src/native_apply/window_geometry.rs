use std::mem::zeroed;

use flowtile_domain::Rect;

use crate::TILED_VISUAL_OVERLAP_X_PX;

use super::{
    ApplyOperation, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute, GetWindowRect, HWND, RECT,
    last_error_message,
};

pub(super) fn translated_window_rect(
    hwnd: HWND,
    operation: &ApplyOperation,
) -> Result<TranslatedRect, String> {
    let target_visible_rect = compensated_visible_rect(operation)?;
    let visible_width = i32::try_from(target_visible_rect.width)
        .map_err(|_| format!("window {} width exceeded Win32 limits", operation.hwnd))?;
    let visible_height = i32::try_from(target_visible_rect.height)
        .map_err(|_| format!("window {} height exceeded Win32 limits", operation.hwnd))?;
    let outer_rect = query_outer_window_rect(hwnd)?;
    let visible_rect = query_visible_frame_rect(hwnd)
        .filter(|visible_rect| visible_frame_is_compatible(outer_rect, *visible_rect))
        .unwrap_or(outer_rect);
    let insets = frame_insets(outer_rect, visible_rect);

    let width = visible_width
        .checked_add(insets.left)
        .and_then(|value| value.checked_add(insets.right))
        .ok_or_else(|| format!("window {} translated width overflowed", operation.hwnd))?;
    let height = visible_height
        .checked_add(insets.top)
        .and_then(|value| value.checked_add(insets.bottom))
        .ok_or_else(|| format!("window {} translated height overflowed", operation.hwnd))?;

    Ok(TranslatedRect {
        x: target_visible_rect.x.saturating_sub(insets.left),
        y: target_visible_rect.y.saturating_sub(insets.top),
        width,
        height,
    })
}

fn compensated_visible_rect(operation: &ApplyOperation) -> Result<Rect, String> {
    if !operation.suppress_visual_gap {
        return Ok(operation.rect);
    }

    let x = if operation.rect.x > 0 {
        operation
            .rect
            .x
            .saturating_sub(TILED_VISUAL_OVERLAP_X_PX.max(0))
    } else {
        operation.rect.x
    };

    Ok(Rect::new(
        x,
        operation.rect.y,
        operation.rect.width,
        operation.rect.height,
    ))
}

pub(super) fn query_outer_window_rect(hwnd: HWND) -> Result<RECT, String> {
    let mut rect: RECT = unsafe { zeroed() };
    let ok = { unsafe { GetWindowRect(hwnd, &mut rect) != 0 } };
    if !ok {
        return Err(last_error_message("GetWindowRect"));
    }
    Ok(rect)
}

pub(super) fn query_visible_frame_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect: RECT = unsafe { zeroed() };
    let result = {
        unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut rect as *mut _ as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
        }
    };
    (result >= 0).then_some(rect)
}

pub(super) fn frame_insets(outer_rect: RECT, visible_rect: RECT) -> FrameInsets {
    FrameInsets {
        left: (visible_rect.left - outer_rect.left).max(0),
        top: (visible_rect.top - outer_rect.top).max(0),
        right: (outer_rect.right - visible_rect.right).max(0),
        bottom: (outer_rect.bottom - visible_rect.bottom).max(0),
    }
}

pub(super) fn visible_frame_is_compatible(outer_rect: RECT, visible_rect: RECT) -> bool {
    rect_is_non_empty(outer_rect)
        && rect_is_non_empty(visible_rect)
        && visible_rect.left >= outer_rect.left
        && visible_rect.top >= outer_rect.top
        && visible_rect.right <= outer_rect.right
        && visible_rect.bottom <= outer_rect.bottom
}

fn rect_is_non_empty(rect: RECT) -> bool {
    rect.right > rect.left && rect.bottom > rect.top
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FrameInsets {
    pub(super) left: i32,
    pub(super) top: i32,
    pub(super) right: i32,
    pub(super) bottom: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TranslatedRect {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: i32,
    pub(super) height: i32,
}
