//! Fork-only (not upstream): minimize, maximize and close buttons at the
//! right of the header, on every desktop. Upstream sets an empty layout so
//! the compositor owns the window; here the buttons are wanted on tiling
//! and floating desktops alike. Kept out of `main.rs` so a sync with
//! upstream does not conflict on upstream's own header lines.

use libadwaita as adw;

/// The layout that always carries all three buttons; the system's own may
/// hold only some of them, or none.
const LAYOUT: &str = ":minimize,maximize,close";

pub fn apply(header: &adw::HeaderBar) {
    header.set_decoration_layout(Some(LAYOUT));
}
