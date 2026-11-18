//! Keep the desktop awake while a session runs.
//!
//! Mutter refuses to create a session while the screen is blanked ("Session
//! creation inhibited"), and the screen blanks after the idle delay even with
//! the lock off, because nobody is touching the machine. So a served desktop
//! needs two things: [`wake`] the screen before a session is created, and an
//! [`IdleInhibitor`] held while it runs so the idle timer never fires.
//!
//! A screen that is really locked is not handled: waking it would need the
//! user's password.

use anyhow::{Context, Result};
use zbus::blocking::Connection;

use crate::mutter::{connect, proxy, Closing};

const SCREENSAVER: &str = "org.gnome.ScreenSaver";
const SCREENSAVER_PATH: &str = "/org/gnome/ScreenSaver";
const SESSION: &str = "org.gnome.SessionManager";
const SESSION_PATH: &str = "/org/gnome/SessionManager";

/// `org.gnome.SessionManager.Inhibit` flag: do not mark the session idle.
const INHIBIT_IDLE: u32 = 8;

/// True when the screen saver is active: the idle delay has blanked the
/// screen (or it is locked).
pub fn is_blanked(conn: &Connection) -> Result<bool> {
    proxy(conn, SCREENSAVER, SCREENSAVER_PATH, SCREENSAVER)?
        .call("GetActive", &())
        .context("ScreenSaver.GetActive")
}

/// Switch the screen saver off if it is on. Returns whether it was.
/// Blank the screen as the idle delay would. For tests.
pub fn blank(conn: &Connection) -> Result<()> {
    proxy(conn, SCREENSAVER, SCREENSAVER_PATH, SCREENSAVER)?
        .call::<_, _, ()>("SetActive", &(true,))
        .context("ScreenSaver.SetActive(true)")
}

pub fn wake(conn: &Connection) -> Result<bool> {
    if !is_blanked(conn)? {
        return Ok(false);
    }
    proxy(conn, SCREENSAVER, SCREENSAVER_PATH, SCREENSAVER)?
        .call::<_, _, ()>("SetActive", &(false,))
        .context("ScreenSaver.SetActive(false)")?;
    Ok(true)
}

/// True when something (this process or another) is inhibiting idle.
pub fn idle_inhibited(conn: &Connection) -> Result<bool> {
    let actions: u32 = proxy(conn, SESSION, SESSION_PATH, SESSION)?
        .get_property("InhibitedActions")
        .context("SessionManager.InhibitedActions")?;
    Ok(actions & INHIBIT_IDLE != 0)
}

/// Stops the session from going idle while it lives. It has a bus connection
/// of its own, so the inhibition also ends if the process dies.
pub struct IdleInhibitor {
    conn: Closing,
    cookie: u32,
}

impl IdleInhibitor {
    pub fn new(address: &str, reason: &str) -> Result<Self> {
        let conn = Closing::new(connect(address)?);
        let cookie: u32 = proxy(&conn, SESSION, SESSION_PATH, SESSION)?
            .call("Inhibit", &("gliff", 0u32, reason, INHIBIT_IDLE))
            .context("SessionManager.Inhibit")?;
        Ok(Self { conn, cookie })
    }
}

impl Drop for IdleInhibitor {
    fn drop(&mut self) {
        if let Ok(p) = proxy(&self.conn, SESSION, SESSION_PATH, SESSION) {
            let _ = p.call::<_, _, ()>("Uninhibit", &(self.cookie,));
        }
    }
}
