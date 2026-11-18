//! The RemoteDesktop session's clipboard: take part in the desktop's
//! selection as a remote client.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, Receiver};

use anyhow::{Context, Result};
use zbus::blocking::Proxy;
use zbus::zvariant::{OwnedValue, Value};

/// What Mutter tells a session that has the clipboard enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardSignal {
    /// The desktop's selection changed to offer `mime_types` (empty when it
    /// was cleared). `is_owner` is true when it is this session's own.
    OwnerChanged {
        mime_types: Vec<String>,
        is_owner: bool,
    },
    /// An application pastes from the selection this session set: answer with
    /// [`ClipboardPort::selection_write`] and
    /// [`ClipboardPort::selection_write_done`] for `serial`.
    Transfer { mime_type: String, serial: u32 },
}

/// The clipboard calls of a session, from [`crate::Session::enable_clipboard`].
/// Cheap to clone; calls fail once the session has ended.
#[derive(Clone)]
pub struct ClipboardPort {
    pub(crate) rd: Proxy<'static>,
}

/// A list of strings as Mutter sends `mime-types`. Its interface file says
/// `as`, but on the wire (GNOME 50) it is `(as)`: a struct holding the array.
/// Both are accepted.
fn string_list(v: &OwnedValue) -> Option<Vec<String>> {
    let v = v.try_clone().ok()?;
    Vec::<String>::try_from(v.try_clone().ok()?)
        .ok()
        .or_else(|| <(Vec<String>,)>::try_from(v).ok().map(|(list,)| list))
}

/// Mutter's `SelectionOwnerChanged` options.
pub(crate) fn parse_owner_changed(options: &HashMap<String, OwnedValue>) -> (Vec<String>, bool) {
    let mime_types = options
        .get("mime-types")
        .and_then(string_list)
        .unwrap_or_default();
    let is_owner = options
        .get("session-is-owner")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false);
    (mime_types, is_owner)
}

impl crate::Session {
    /// Enable the clipboard. The signals arrive on the returned channel,
    /// which ends when the session does. Call before [`crate::Session::start`]
    /// (the order Mutter's own portal uses).
    pub fn enable_clipboard(&self) -> Result<(ClipboardPort, Receiver<ClipboardSignal>)> {
        let (tx, rx) = mpsc::channel();
        // Listen first: the signals start as soon as the clipboard is on.
        let owner = self.rd.receive_signal("SelectionOwnerChanged")?;
        let transfer = self.rd.receive_signal("SelectionTransfer")?;
        let owner_tx = tx.clone();
        std::thread::spawn(move || {
            for msg in owner {
                let Ok(options) = msg.body().deserialize::<HashMap<String, OwnedValue>>() else {
                    continue;
                };
                let (mime_types, is_owner) = parse_owner_changed(&options);
                let signal = ClipboardSignal::OwnerChanged {
                    mime_types,
                    is_owner,
                };
                if owner_tx.send(signal).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            for msg in transfer {
                let Ok((mime_type, serial)) = msg.body().deserialize::<(String, u32)>() else {
                    continue;
                };
                if tx
                    .send(ClipboardSignal::Transfer { mime_type, serial })
                    .is_err()
                {
                    break;
                }
            }
        });
        let options: HashMap<&str, Value> = HashMap::new();
        self.rd
            .call::<_, _, ()>("EnableClipboard", &(options,))
            .context("RemoteDesktop.EnableClipboard")?;
        Ok((
            ClipboardPort {
                rd: self.rd.clone(),
            },
            rx,
        ))
    }
}

impl ClipboardPort {
    /// Become the selection's owner, offering `mime_types`. With an empty list
    /// the key is left out, which Mutter reads as "unset": it gives the
    /// selection up only if this session's source still holds it, so it is
    /// safe to call when someone else has copied since.
    pub fn set_selection(&self, mime_types: &[String]) -> Result<()> {
        let mut options: HashMap<&str, Value> = HashMap::new();
        if !mime_types.is_empty() {
            options.insert("mime-types", Value::from(mime_types.to_vec()));
        }
        self.rd
            .call::<_, _, ()>("SetSelection", &(options,))
            .context("SetSelection")
    }

    /// Ask the selection's owner for `mime_type`. The descriptor yields the
    /// bytes and then EOF. It is a read end; Mutter makes it non-blocking.
    pub fn selection_read(&self, mime_type: &str) -> Result<OwnedFd> {
        let fd: zbus::zvariant::OwnedFd = self
            .rd
            .call("SelectionRead", &(mime_type,))
            .context("SelectionRead")?;
        Ok(fd.into())
    }

    /// Answer a [`ClipboardSignal::Transfer`]: the descriptor to write the
    /// data to. Close it, then call [`ClipboardPort::selection_write_done`].
    pub fn selection_write(&self, serial: u32) -> Result<OwnedFd> {
        let fd: zbus::zvariant::OwnedFd = self
            .rd
            .call("SelectionWrite", &(serial,))
            .context("SelectionWrite")?;
        Ok(fd.into())
    }

    pub fn selection_write_done(&self, serial: u32, success: bool) -> Result<()> {
        self.rd
            .call::<_, _, ()>("SelectionWriteDone", &(serial, success))
            .context("SelectionWriteDone")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_changed_options_are_read() {
        let mut o = HashMap::new();
        o.insert(
            "mime-types".to_string(),
            OwnedValue::try_from(Value::from(vec![
                "text/plain".to_string(),
                "image/png".into(),
            ]))
            .unwrap(),
        );
        o.insert(
            "session-is-owner".to_string(),
            OwnedValue::try_from(Value::from(true)).unwrap(),
        );
        assert_eq!(
            parse_owner_changed(&o),
            (
                vec!["text/plain".to_string(), "image/png".to_string()],
                true
            )
        );
    }

    #[test]
    fn the_struct_wrapped_list_mutter_really_sends_is_read_too() {
        // What GNOME 50 put on the bus, seen live: `(as)`, not `as`.
        let wrapped = zbus::zvariant::StructureBuilder::new()
            .add_field(vec!["text/plain;charset=utf-8".to_string()])
            .build()
            .unwrap();
        let mut o = HashMap::new();
        o.insert(
            "mime-types".to_string(),
            OwnedValue::try_from(Value::from(wrapped)).unwrap(),
        );
        assert_eq!(
            parse_owner_changed(&o),
            (vec!["text/plain;charset=utf-8".to_string()], false)
        );
    }

    #[test]
    fn missing_options_mean_an_empty_selection_owned_by_someone_else() {
        assert_eq!(parse_owner_changed(&HashMap::new()), (vec![], false));
    }
}
