//! Picks the backend for the running desktop.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use gliff_backend::Backend;
use gnome_backend::GnomeBackend;
use hypr_wl::Target;

use crate::hypr_backend::HyprBackend;

/// Hyprland when an instance is running, else GNOME.
pub fn discover(target: &Target) -> Result<Arc<dyn Backend>> {
    let hypr = match HyprBackend::discover(target) {
        Ok(backend) => return Ok(Arc::new(backend)),
        Err(e) => e,
    };
    match GnomeBackend::discover() {
        Ok(backend) => Ok(Arc::new(backend)),
        Err(gnome) => Err(anyhow!(
            "no supported desktop found. Hyprland: {hypr:#}. GNOME: {gnome:#}"
        )),
    }
}
