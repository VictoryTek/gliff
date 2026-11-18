//! Find the graphical session to serve, from a login that has no desktop
//! environment of its own (an `ssh -T` shell).
//!
//! `loginctl` names the user's active local graphical session. From it we
//! take the runtime directory and the session bus address; nothing is read
//! from the environment unless the caller already has the right values.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// A desktop session we can talk to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicalSession {
    pub uid: u32,
    /// The logind session id, for messages.
    pub id: String,
    pub runtime_dir: PathBuf,
    /// The session bus address.
    pub bus_address: String,
}

/// What `loginctl show-session` says about one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionInfo {
    pub id: String,
    pub kind: String,
    pub class: String,
    pub active: bool,
    pub remote: bool,
}

/// Parse `KEY=value` lines.
pub(crate) fn parse_properties(text: &str) -> HashMap<&str, &str> {
    text.lines().filter_map(|l| l.split_once('=')).collect()
}

/// Session ids from `loginctl list-sessions --no-legend` that belong to `uid`.
/// The columns are SESSION UID USER SEAT TTY.
pub(crate) fn sessions_of(listing: &str, uid: u32) -> Vec<String> {
    listing
        .lines()
        .filter_map(|l| {
            let mut c = l.split_whitespace();
            let id = c.next()?;
            let u: u32 = c.next()?.parse().ok()?;
            (u == uid).then(|| id.to_string())
        })
        .collect()
}

pub(crate) fn session_info(id: &str, props: &str) -> SessionInfo {
    let p = parse_properties(props);
    let get = |k: &str| p.get(k).copied().unwrap_or("").to_string();
    SessionInfo {
        id: id.to_string(),
        kind: get("Type"),
        class: get("Class"),
        active: p.get("Active") == Some(&"yes"),
        remote: p.get("Remote") == Some(&"yes"),
    }
}

/// The session to serve: a local, active, graphical user session. A remote
/// session (our own ssh login) is never the desktop.
pub(crate) fn pick(infos: &[SessionInfo]) -> Option<&SessionInfo> {
    infos.iter().find(|s| {
        s.class == "user" && s.active && !s.remote && matches!(s.kind.as_str(), "wayland" | "x11")
    })
}

/// The bus address for a runtime directory, unless `env` already holds one.
pub(crate) fn bus_address(env: Option<String>, runtime_dir: &std::path::Path) -> String {
    env.filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("unix:path={}/bus", runtime_dir.display()))
}

pub fn current_uid() -> Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata("/proc/self")
        .context("read /proc/self")?
        .uid())
}

fn loginctl(args: &[&str]) -> Result<String> {
    let out = Command::new("loginctl")
        .args(args)
        .output()
        .context("run loginctl")?;
    if !out.status.success() {
        bail!(
            "loginctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Find the current user's graphical session.
pub fn find() -> Result<GraphicalSession> {
    let uid = current_uid()?;
    let listing = loginctl(&["list-sessions", "--no-legend"])?;
    let mut infos = Vec::new();
    for id in sessions_of(&listing, uid) {
        let props = loginctl(&[
            "show-session",
            &id,
            "-p",
            "Type",
            "-p",
            "Class",
            "-p",
            "Active",
            "-p",
            "Remote",
        ])?;
        infos.push(session_info(&id, &props));
    }
    let Some(s) = pick(&infos) else {
        bail!(
            "no active local graphical session for uid {uid} (sessions seen: {})",
            if infos.is_empty() {
                "none".to_string()
            } else {
                infos
                    .iter()
                    .map(|s| format!("{} {}/{}", s.id, s.kind, s.class))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    };
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
    let bus_address = bus_address(std::env::var("DBUS_SESSION_BUS_ADDRESS").ok(), &runtime_dir);
    Ok(GraphicalSession {
        uid,
        id: s.id.clone(),
        runtime_dir,
        bus_address,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const LISTING: &str = "\
    2 1000 nimda seat0 -
    5 1000 nimda -     -
   c1 0    gdm   seat0 tty1
";

    #[test]
    fn sessions_are_filtered_by_uid() {
        assert_eq!(sessions_of(LISTING, 1000), vec!["2", "5"]);
        assert_eq!(sessions_of(LISTING, 0), vec!["c1"]);
        assert!(sessions_of(LISTING, 7).is_empty());
    }

    #[test]
    fn properties_parse() {
        let p = parse_properties("Type=wayland\nClass=user\nActive=yes\nRemote=no\n");
        assert_eq!(p["Type"], "wayland");
        assert_eq!(p["Remote"], "no");
    }

    fn info(id: &str, kind: &str, class: &str, active: bool, remote: bool) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            kind: kind.into(),
            class: class.into(),
            active,
            remote,
        }
    }

    #[test]
    fn our_ssh_login_is_never_the_desktop() {
        let infos = vec![
            info("5", "tty", "user", true, true),
            info("2", "wayland", "user", true, false),
        ];
        assert_eq!(pick(&infos).unwrap().id, "2");
        // The ssh login alone is not a desktop.
        assert!(pick(&infos[..1]).is_none());
    }

    #[test]
    fn inactive_and_greeter_sessions_are_skipped() {
        let infos = vec![
            info("1", "wayland", "greeter", true, false),
            info("2", "wayland", "user", false, false),
        ];
        assert!(pick(&infos).is_none());
    }

    #[test]
    fn session_info_reads_loginctl_output() {
        let s = session_info("2", "Type=wayland\nClass=user\nActive=yes\nRemote=no\n");
        assert_eq!(s, info("2", "wayland", "user", true, false));
    }

    #[test]
    fn bus_address_prefers_the_environment() {
        let dir = Path::new("/run/user/1000");
        assert_eq!(
            bus_address(Some("unix:abstract=x".into()), dir),
            "unix:abstract=x"
        );
        assert_eq!(
            bus_address(Some(String::new()), dir),
            "unix:path=/run/user/1000/bus"
        );
        assert_eq!(bus_address(None, dir), "unix:path=/run/user/1000/bus");
    }
}
