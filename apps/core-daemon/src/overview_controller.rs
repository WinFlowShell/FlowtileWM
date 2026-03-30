use std::sync::mpsc::Sender;

use crate::{
    control::ControlMessage,
    overview_engine::{OverviewScene, build_overview_scene},
    overview_surface::{OverviewOverlay, OverviewSurfaceError, install_overview_control_sender},
};
use flowtile_domain::RuntimeMode;
use flowtile_wm_core::CoreDaemonRuntime;

pub(crate) struct OverviewSurfaceController {
    overlay: OverviewOverlay,
    last_scene: Option<OverviewScene>,
}

impl OverviewSurfaceController {
    pub(crate) fn spawn(
        control_sender: Sender<ControlMessage>,
    ) -> Result<Self, OverviewSurfaceError> {
        install_overview_control_sender(control_sender);
        Ok(Self {
            overlay: OverviewOverlay::spawn()?,
            last_scene: None,
        })
    }

    pub(crate) fn sync(&mut self, runtime: &CoreDaemonRuntime) -> Result<(), OverviewSurfaceError> {
        if !runtime.management_enabled()
            || runtime.state().runtime.boot_mode == RuntimeMode::SafeMode
        {
            if self.last_scene.take().is_some() {
                self.overlay.hide()?;
            }
            return Ok(());
        }

        let next_scene = build_overview_scene(runtime.state())?;
        if self.last_scene.as_ref() == next_scene.as_ref() {
            return Ok(());
        }

        match next_scene.as_ref() {
            Some(scene) => self.overlay.show(scene.clone())?,
            None => self.overlay.hide()?,
        }
        self.last_scene = next_scene;
        Ok(())
    }
}
