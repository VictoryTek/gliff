//! Which of the server's layouts is in use, read from GNOME's settings.
//!
//! The compositor serves one keymap holding every layout (one group each) and
//! says which group is active only to a client that has the keyboard focus,
//! which the server never has. GNOME Shell keeps the active input source in
//! memory and, on every interactive switch (Super+Space, the indicator menu),
//! writes the list of sources in most-recently-used order to
//! `org.gnome.desktop.input-sources` `mru-sources`: its first entry is the
//! active source. (The `current` key is deprecated and ignored by the Shell.)
//! The Shell builds the keymap from `sources` in order, one group per source,
//! and appends the locale's layout as one more, last group when there is room
//! (`js/misc/keyboardManager.js`, GNOME Shell 50.6).
//!
//! The settings are read with the `dconf` command (`dconf read KEY`, which
//! needs no schema and no running service), the way `loginctl` is used to find
//! the session. The first MRU entry is taken as the group only when it can be
//! checked against the keymap itself: input sources are not per window, every
//! source is an `xkb` one, no two are the same, there are at most three, the
//! stored MRU list is empty or holds every source, and the layout names in the
//! keymap's `xkb_symbols` line (`pc_de_us_2_...`) are the sources' in order,
//! with at most one more, the locale's, after them. Anything else (an input
//! method, an unresolved layout id, a stored list that is only partly current,
//! a missing `dconf`) leaves the active layout unknown, and keys go as they did
//! before this was read.
//!
//! Not covered: a switch made while IBus was not ready is not written to
//! `mru-sources` by the Shell, and a stored list that is a full ordering but
//! was left stale that way is believed.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, Weak};
use std::time::{Duration, Instant};

use gnome_ipc::GraphicalSession;

use crate::{Inner, ServerKeymap};

const SCHEMA: &str = "/org/gnome/desktop/input-sources";
/// How often `mru-sources` is read while the server has more than one layout.
/// It bounds how long a layout switch can go unseen (plus the read itself):
/// a key typed in that time is sent for the old layout.
const POLL: Duration = Duration::from_millis(200);
/// `sources` and `per-window` are read again after this many polls even if
/// `mru-sources` did not change (a layout added or removed in Settings).
const SETTINGS_EVERY: u32 = 25;
/// A `dconf` that has not answered by then is killed and the read counts as a
/// failure.
const READ_LIMIT: Duration = Duration::from_secs(2);

/// The active layout is group `group` of the keymap's `of`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActiveLayout {
    pub group: u32,
    pub of: u32,
}

type Sources = Vec<(String, String)>;

/// `[('xkb', 'us'), ('xkb', 'de+nodeadkeys')]`, optionally led by a type
/// annotation (`@a(ss) []`): the (kind, id) pairs.
pub(crate) fn parse_sources(text: &str) -> Option<Sources> {
    let t = text.trim();
    let t = if t.starts_with('@') {
        t.split_once(' ')?.1.trim()
    } else {
        t
    };
    let t = t.strip_prefix('[')?.strip_suffix(']')?;
    if t.contains('\\') {
        return None;
    }
    let mut words = Vec::new();
    let mut rest = t;
    while let Some(start) = rest.find(['\'', '"']) {
        let quote = rest[start..].chars().next()?;
        let body = &rest[start + 1..];
        let end = body.find(quote)?;
        words.push(body[..end].to_owned());
        rest = &body[end + 1..];
    }
    if words.len() % 2 != 0 {
        return None;
    }
    Some(
        words
            .chunks(2)
            .map(|p| (p[0].clone(), p[1].clone()))
            .collect(),
    )
}

/// The layout names a keymap text gives its groups, in order: the
/// `xkb_symbols` name is `pc_de(nodeadkeys)_us_2_fr_3_inet(evdev)...` (the
/// first layout, then each further one followed by its number). `None` when it
/// does not have that form for `layouts` layouts.
pub(crate) fn group_names(text: &str, layouts: u32) -> Option<Vec<String>> {
    let start = text.find("xkb_symbols \"")? + "xkb_symbols \"".len();
    let name = &text[start..start + text[start..].find('"')?];
    // Split at `_` outside parentheses (a variant can contain one).
    let mut tokens = Vec::new();
    let (mut depth, mut from) = (0i32, 0);
    for (i, c) in name.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            '_' if depth == 0 => {
                tokens.push(&name[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    tokens.push(&name[from..]);
    let mut tokens = tokens.into_iter();
    if tokens.next()? != "pc" {
        return None;
    }
    let mut names = vec![tokens.next()?.to_owned()];
    for k in 2..=layouts {
        names.push(tokens.next()?.to_owned());
        if tokens.next()? != k.to_string() {
            return None;
        }
    }
    names
        .iter()
        .all(|n| !n.is_empty() && n.parse::<u32>().is_err())
        .then_some(names)
}

/// A source id (`de+nodeadkeys`) as the keymap names its layout
/// (`de(nodeadkeys)`).
fn group_name_of(id: &str) -> String {
    match id.split_once('+') {
        Some((layout, variant)) => format!("{layout}({variant})"),
        None => id.to_owned(),
    }
}

/// The active group, when the first MRU entry maps onto one for certain.
/// `groups` are the names of the server keymap's layouts.
pub(crate) fn active_layout(
    sources: &[(String, String)],
    mru: &[(String, String)],
    per_window: bool,
    groups: &[String],
) -> Option<ActiveLayout> {
    let n = sources.len();
    let all_xkb = sources.iter().all(|(kind, _)| kind == "xkb");
    let distinct = sources
        .iter()
        .enumerate()
        .all(|(i, (_, id))| sources[..i].iter().all(|(_, other)| other != id));
    // The Shell's keymap has the sources' layouts in order, then the locale's
    // when it is not among them (a fourth source can start a second keymap).
    let fits = (1..=3).contains(&n)
        && (groups.len() == n || groups.len() == n + 1)
        && sources
            .iter()
            .zip(groups)
            .all(|((_, id), name)| group_name_of(id) == *name);
    // The stored list is the whole list in MRU order after any interactive
    // switch; a partly current one (a source added or removed since) the Shell
    // reads differently at start-up, so it says nothing sure. Empty: no
    // switch has been made, the first source is active.
    let whole = mru.len() == n && sources.iter().all(|s| mru.contains(s));
    if !(all_xkb && distinct && fits && !per_window && (mru.is_empty() || whole)) {
        return None;
    }
    let group = mru
        .first()
        .and_then(|m| sources.iter().position(|s| s == m))
        .unwrap_or(0);
    Some(ActiveLayout {
        group: u32::try_from(group).ok()?,
        of: u32::try_from(groups.len()).ok()?,
    })
}

/// Run `command` and collect its output, killing it after `limit`.
fn output_within(mut command: Command, limit: Duration) -> std::io::Result<(bool, String)> {
    let mut child: Child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "dconf did not answer",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut text = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut text);
    }
    Ok((status.success(), text.trim().to_owned()))
}

/// `dconf read` of one key: `None` when it is unset (the schema's default
/// stands), an error when `dconf` cannot run, fails or hangs.
fn read(session: &GraphicalSession, key: &str) -> std::io::Result<Option<String>> {
    let mut command = Command::new("dconf");
    command
        .args(["read", &format!("{SCHEMA}/{key}")])
        .env("XDG_RUNTIME_DIR", &session.runtime_dir)
        .env("DBUS_SESSION_BUS_ADDRESS", &session.bus_address);
    let (ok, text) = output_within(command, READ_LIMIT)?;
    if !ok {
        return Err(std::io::Error::other(format!("dconf read {key} failed")));
    }
    Ok((!text.is_empty()).then_some(text))
}

/// A list setting: unset is an empty list, text that does not parse is `None`.
fn read_list(session: &GraphicalSession, key: &str) -> Option<Sources> {
    match read(session, key) {
        Ok(None) => Some(Vec::new()),
        Ok(Some(text)) => parse_sources(&text),
        Err(_) => None,
    }
}

/// Keep `keymap`'s active layout current while it has several layouts, until
/// every clone of it is gone.
pub(crate) fn follow(session: GraphicalSession, keymap: &ServerKeymap) {
    let weak = keymap.downgrade();
    let spawned = std::thread::Builder::new()
        .name("gnome-active-layout".into())
        .spawn(move || poll(&session, &weak));
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot follow the active layout; keys go by the layout list");
    }
}

/// What is read once in a while: the sources and whether input sources are
/// per window, and the keymap they were read for.
struct Settings {
    generation: u64,
    sources: Sources,
    per_window: bool,
}

/// `per-window`: unset is off; anything unreadable is `None`.
fn read_per_window(session: &GraphicalSession) -> Option<bool> {
    match read(session, "per-window") {
        Ok(None) => Some(false),
        Ok(Some(t)) if t == "true" => Some(true),
        Ok(Some(t)) if t == "false" => Some(false),
        _ => None,
    }
}

fn poll(session: &GraphicalSession, weak: &Weak<Mutex<Inner>>) {
    let mut settings: Option<Settings> = None;
    let mut ticks = 0u32;
    loop {
        std::thread::sleep(POLL);
        // The keymap is held only for a moment, never across a `dconf` run.
        let Some((generation, layouts, groups)) =
            ServerKeymap::upgrade(weak).map(|k| k.generation_and_groups())
        else {
            return;
        };
        let mut found = None;
        if layouts >= 2 {
            match read(session, "mru-sources") {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!("no dconf command; keys go by the layout list");
                    // Nothing is followed any more: do not leave the last
                    // layout in use standing.
                    if let Some(keymap) = ServerKeymap::upgrade(weak) {
                        keymap.set_active(generation, None);
                    }
                    return;
                }
                // A failed or slow read: unknown this time, tried again.
                Err(_) => {}
                Ok(mru) => {
                    ticks += 1;
                    let stale = settings.as_ref().is_none_or(|s| s.generation != generation);
                    if stale || ticks.is_multiple_of(SETTINGS_EVERY) {
                        settings = read_list(session, "sources")
                            .zip(read_per_window(session))
                            .map(|(sources, per_window)| Settings {
                                generation,
                                sources,
                                per_window,
                            });
                    }
                    let mru = mru.as_deref().map_or(Some(Vec::new()), parse_sources);
                    found = settings.as_ref().zip(mru).and_then(|(s, mru)| {
                        active_layout(&s.sources, &mru, s.per_window, &groups)
                    });
                }
            }
        } else {
            settings = None;
        }
        let Some(keymap) = ServerKeymap::upgrade(weak) else {
            return;
        };
        keymap.set_active(generation, found);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Sources {
        list.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    fn xkb(ids: &[&str]) -> Sources {
        ids.iter()
            .map(|id| ("xkb".to_owned(), (*id).to_owned()))
            .collect()
    }

    #[test]
    fn sources_are_read_as_dconf_prints_them() {
        assert_eq!(
            parse_sources("[('xkb', 'us'), ('xkb', 'de+nodeadkeys')]\n"),
            Some(pairs(&[("xkb", "us"), ("xkb", "de+nodeadkeys")]))
        );
        assert_eq!(parse_sources("@a(ss) []"), Some(Vec::new()));
        assert_eq!(
            parse_sources("[('ibus', 'anthy')]"),
            Some(pairs(&[("ibus", "anthy")]))
        );
        assert_eq!(parse_sources("[('xkb', 'us'), ('xkb')]"), None);
        assert_eq!(parse_sources("not a list"), None);
    }

    fn layout(name: &str, variant: &str) -> String {
        use xkbcommon::xkb;
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(
            &ctx,
            "",
            "",
            name,
            variant,
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compile")
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn the_layouts_of_a_keymap_are_read_from_its_symbols_name() {
        assert_eq!(
            group_names(&layout("us,de", ",nodeadkeys"), 2),
            Some(names(&["us", "de(nodeadkeys)"]))
        );
        assert_eq!(
            group_names(&layout("de,us,fr", ""), 3),
            Some(names(&["de", "us", "fr"]))
        );
        // A variant with an underscore in it.
        assert_eq!(
            group_names(&layout("us,ru", ",phonetic_winkeys"), 2),
            Some(names(&["us", "ru(phonetic_winkeys)"]))
        );
        // More layouts asked for than the name has, and text that has none.
        assert_eq!(group_names(&layout("us,de", ""), 3), None);
        assert_eq!(group_names("xkb_keymap {}", 2), None);
    }

    #[test]
    fn a_source_is_named_as_the_keymap_names_its_layout() {
        assert_eq!(group_name_of("us"), "us");
        assert_eq!(group_name_of("de+nodeadkeys"), "de(nodeadkeys)");
    }

    #[test]
    fn the_first_most_recently_used_source_is_the_active_one() {
        let sources = xkb(&["us", "de"]);
        let groups = names(&["us", "de"]);
        let group = |mru: &Sources| active_layout(&sources, mru, false, &groups).map(|a| a.group);
        // Nothing recorded yet: the first source.
        assert_eq!(group(&Vec::new()), Some(0));
        assert_eq!(group(&xkb(&["us", "de"])), Some(0));
        // Switched to German: it leads the list.
        assert_eq!(group(&xkb(&["de", "us"])), Some(1));
        // A list that is not the whole of the sources (one was added or
        // removed since, or only one survived a start): the Shell reads such a
        // list differently at start-up, so nothing is claimed.
        assert_eq!(group(&xkb(&["de"])), None);
        assert_eq!(group(&xkb(&["fr", "de"])), None);
        assert_eq!(group(&xkb(&["de", "us", "fr"])), None);
    }

    #[test]
    fn the_index_is_the_group_only_when_the_keymap_agrees() {
        let us_de = xkb(&["us", "de"]);
        let mru = xkb(&["de", "us"]);
        let of = |groups: &[&str]| active_layout(&us_de, &mru, false, &names(groups));
        assert_eq!(of(&["us", "de"]), Some(ActiveLayout { group: 1, of: 2 }));
        // The locale's layout appended as a last group.
        assert_eq!(
            of(&["us", "de", "fr"]),
            Some(ActiveLayout { group: 1, of: 3 })
        );
        // The order, a layout that is not a source, too many, too few.
        assert_eq!(of(&["de", "us"]), None);
        assert_eq!(of(&["us", "fr"]), None);
        assert_eq!(of(&["us", "de", "fr", "ru"]), None);
        assert_eq!(of(&["us"]), None);
        // An unresolved layout id: the Shell leaves it out, so `de` is group 0
        // of `de, us` and not group 1 of two.
        let broken = xkb(&["nonexistent", "de"]);
        assert_eq!(
            active_layout(
                &broken,
                &xkb(&["de", "nonexistent"]),
                false,
                &names(&["de", "us"])
            ),
            None
        );
        // Input sources per window: the stored list is not kept current.
        assert_eq!(
            active_layout(&us_de, &mru, true, &names(&["us", "de"])),
            None
        );
        // An input method, a repeated layout, nothing listed, four sources.
        let ibus = pairs(&[("xkb", "us"), ("ibus", "anthy")]);
        assert_eq!(
            active_layout(&ibus, &mru, false, &names(&["us", "us"])),
            None
        );
        assert_eq!(
            active_layout(&xkb(&["us", "us"]), &mru, false, &names(&["us", "us"])),
            None
        );
        assert_eq!(active_layout(&[], &mru, false, &names(&["us", "de"])), None);
        let four = xkb(&["us", "de", "fr", "ru"]);
        assert_eq!(
            active_layout(&four, &mru, false, &names(&["us", "de", "fr", "ru"])),
            None
        );
    }

    #[test]
    fn a_command_that_does_not_answer_is_killed() {
        let mut slow = Command::new("sleep");
        slow.arg("5");
        let started = Instant::now();
        let err = output_within(slow, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        let mut echo = Command::new("echo");
        echo.arg("hi");
        assert_eq!(
            output_within(echo, Duration::from_secs(5)).unwrap(),
            (true, "hi".to_owned())
        );
        let missing = Command::new("gliff-no-such-command");
        assert_eq!(
            output_within(missing, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }
}
