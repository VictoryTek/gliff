//! Picks the backend for the running desktop.

use std::sync::Arc;

use anyhow::Result;
use gliff_backend::Backend;
use hypr_wl::Target;

use crate::hypr_backend::HyprBackend;

/// Hyprland is the only backend today.
pub fn discover(target: &Target) -> Result<Arc<dyn Backend>> {
    Ok(Arc::new(HyprBackend::discover(target)?))
}
