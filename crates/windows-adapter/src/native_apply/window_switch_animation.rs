use flowtile_domain::Rect;

use crate::{ApplyOperation, WindowSwitchAnimation};

pub(super) fn uses_window_switch_animation(operation: &ApplyOperation) -> bool {
    operation
        .window_switch_animation
        .as_ref()
        .is_some_and(|animation| {
            operation.apply_geometry
                && (animation.from_rect != operation.rect)
                && animation.frame_count > 1
                && animation.duration_ms > 0
        })
}

pub(super) fn animated_frame_operation(
    operation: &ApplyOperation,
    progress: f32,
) -> ApplyOperation {
    let Some(animation) = &operation.window_switch_animation else {
        return operation.clone();
    };

    let frame_rect = interpolate_rect(animation, operation.rect, progress);
    ApplyOperation {
        rect: frame_rect,
        window_switch_animation: None,
        ..operation.clone()
    }
}

pub(super) fn interpolate_rect(
    animation: &WindowSwitchAnimation,
    target_rect: Rect,
    progress: f32,
) -> Rect {
    Rect::new(
        interpolate_i32(animation.from_rect.x, target_rect.x, progress),
        interpolate_i32(animation.from_rect.y, target_rect.y, progress),
        interpolate_i32(
            animation.from_rect.width as i32,
            target_rect.width as i32,
            progress,
        )
        .max(0) as u32,
        interpolate_i32(
            animation.from_rect.height as i32,
            target_rect.height as i32,
            progress,
        )
        .max(0) as u32,
    )
}

fn interpolate_i32(from: i32, to: i32, progress: f32) -> i32 {
    let delta = (to as f32) - (from as f32);
    (from as f32 + delta * progress).round() as i32
}

pub(super) fn ease_out_cubic(progress: f32) -> f32 {
    let one_minus = 1.0 - progress.clamp(0.0, 1.0);
    1.0 - one_minus * one_minus * one_minus
}
