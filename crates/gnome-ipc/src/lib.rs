//! Talk to GNOME's Mutter from outside the desktop.
//!
//! [`discover`] finds the graphical session from a plain login (ssh);
//! [`mutter`] connects to its session bus and drives Mutter's private
//! `RemoteDesktop` and `ScreenCast` interfaces. Mutter documents no
//! compatibility between releases, so every call's error text is kept
//! verbatim: it is the evidence when a GNOME update breaks something.

pub mod discover;
pub mod mutter;

pub use discover::{find as find_session, GraphicalSession};
pub use mutter::{connect, monitors, versions, CursorMode, Monitor, Session, Stream, StreamSource};
