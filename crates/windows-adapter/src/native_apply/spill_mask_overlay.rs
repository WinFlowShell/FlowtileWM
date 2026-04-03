use std::collections::BTreeSet;

pub(crate) fn active_spill_mask_overlay_owner_hwnds_snapshot() -> BTreeSet<u64> {
    BTreeSet::new()
}

pub(crate) fn hide_spill_mask_overlay_if_initialized(_owner_hwnd: u64) -> Result<(), String> {
    Ok(())
}
