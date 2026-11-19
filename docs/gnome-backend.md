# GNOME backend

gliff's server can serve a GNOME (Mutter) desktop as well as Hyprland. This
page covers the GNOME side only; the design shared by both is in
`docs/architecture.md`, and the work plan and its results are in
`docs/PLAN.md`.

GNOME has no public API for this. The backend uses Mutter's private D-Bus
interfaces (`org.gnome.Mutter.RemoteDesktop`, `ScreenCast`, `DisplayConfig`),
which promise no compatibility between GNOME releases.

## Limit

A locked screen cannot be captured or controlled. GNOME's lock screen runs in
the `unlock-dialog` session mode, which does not allow screencast; Shell
closes every remote-desktop session when the screen locks and refuses new
ones until it is unlocked.

The same refusal ("Session creation inhibited") happens when the screen has
blanked from idle, even with screen lock switched off. Waking it with
`gdbus call --session --dest org.gnome.ScreenSaver --object-path
/org/gnome/ScreenSaver --method org.gnome.ScreenSaver.SetActive false` lets
capture start again. With screen lock on, that call probably shows the
password prompt instead.

## `gnome-ipc`

`crates/gnome-ipc` finds the desktop session from a plain login (ssh) and
talks to Mutter. Check a host with:

```
gliff-gnome-probe             # session, Mutter versions, monitors, capture a monitor
gliff-gnome-probe --virtual   # the same, capturing a new virtual monitor
```

Each line is `[ OK ]` or `[FAIL]` with the raw detail. The probe stops once it
has a PipeWire node; it does not read frames.

## `gnome-capture`

`crates/gnome-capture` reads a stream's frames from PipeWire and emits the
same capture events as the Hyprland capturer. Frames are in shared memory
(CPU-readable only); dmabuf comes later. Check it with:

```
gliff-gnome-capture-probe --png frame.png     # capture the primary monitor
gliff-gnome-capture-probe --virtual           # capture a new virtual monitor
gliff-gnome-capture-probe --cursor embedded   # cursor painted into the frames
```

The probe reports the frame size, whether the pixels are real, the frame rate,
whether a recapture works and what the cursor did; `--png` saves the first
frame so it can be looked at.

## `gnome-input`

`crates/gnome-input` sends gliff's key, pointer, button and wheel commands to
the session. Check it with the probe; aim clicks and keys at a test window,
never at someone's work:

```
gliff-gnome-input-probe move:960,540 click:left type:"Hello 42" wheel:3
gliff-gnome-input-probe caps    # taps Caps Lock twice and checks it toggled and returned
```

`move` is checked by reading the pointer back through the capture stream.
Keys are sent as keycodes read with the server's keyboard layout, so a viewer
with a different layout types the wrong characters until keymap forwarding
exists; a keymap sent by the viewer is refused with an error.

## `gnome-clipboard`

`crates/gnome-clipboard` bridges gliff's clipboard to the desktop through a
RemoteDesktop session of its own. Check it with the probe (`roundtrip`
replaces the desktop's clipboard while it runs):

```
gliff-gnome-clipboard-probe roundtrip      # both directions, a big item, echo and withdraw rules
gliff-gnome-clipboard-probe serve "text" 20   # offer text, report each paste (paste into a test window)
gliff-gnome-clipboard-probe watch 20          # report and read what the desktop copies
```

When an owner goes away or withdraws, Mutter hands the selection back to
whatever held it before, so a withdraw is reported as an empty selection and the
older one then reappears.

## Virtual output and keep-awake

`gnome_capture::VirtualOutput` creates a private screen and removes it when it
is dropped; `gnome_ipc::power` wakes a blanked screen and keeps the desktop
from going idle. Check them with the probe:

```
gliff-gnome-output-probe              # create, resize, two at once, remove, with the screen blanked first
gliff-gnome-output-probe idle 50      # control: with no output, report the screen state every 10 s
gliff-gnome-output-probe hold 80      # with an output, the same
```

To see keep-awake working, lower the idle delay for the test and put it back
(`gsettings set org.gnome.desktop.session idle-delay 30`, then `300` or your
own value): `idle` should show the screen blank, `hold` should not. The probe
blanks the screen on purpose to test the wake, so run it on a test machine.

The monitor exists only while something reads its stream, and Mutter sizes it
from the format the reader asks for, so a resize is a new format request. The
virtual monitor is always scale 1.

## Running the server on GNOME

`gliff-server` picks Hyprland when an instance is running and GNOME otherwise
(`crates/gliff-server/src/backend.rs`); nothing needs to be configured. It
runs with the CPU video pipeline, because capture is shared memory only until
dmabuf capture exists. A mirrored session (the default) shows the real screen;
`--headless` creates a private screen the size of the viewer's window, which
lives as long as the session. To check the whole path without a viewer, run
this on the GNOME machine:

```
cargo build -p gliff-server -p gnome-backend --example serve_probe
serve_probe target/debug/gliff-server --headless      # or without --headless to mirror
serve_probe target/debug/gliff-server --headless --clipboard 6   # also offers a selection and waits 6 s
gliff-gnome-clipboard-probe rawserve "text" 14        # an ordinary application copying, in another shell
gliff-gnome-clipboard-probe rawwatch 14               # an ordinary application watching
```

`serve_probe` is a headless gliff client: it checks the greeting, the first
frame, the new monitor, the pointer, the cursor shape, resizes and that the
monitors are back to normal afterwards. It does not decode video.

## The viewer on GNOME

The `gliff` viewer needs no GNOME-specific code: it reads the local keymap and
picks the render node through ordinary Wayland protocols. Two GNOME behaviours
show up when you use it:

- The first time the window takes the keyboard, GNOME Shell asks whether to
  allow capturing system shortcuts, and asks again on every launch (the
  viewer has no application id, so GNOME has nothing to remember the answer
  for). Until you allow it, shortcuts such as Super stay with the local
  desktop.
- A private (`--headless`) screen is always scale 1, so on a HiDPI display
  the remote desktop looks small. Mutter can give a virtual monitor another
  scale, but that crashed GNOME Shell on the software-rendered test VM, so it
  is off; see the "M4 part 1 result" entry in `docs/PLAN.md`.

## Keeping up with upstream

This fork merges omacom/gliff's `main` regularly. `scripts/sync-upstream.sh`
makes the merge branch (`--dry-run` shows what it would do without keeping
anything), and `.github/workflows/upstream-sync.yml` runs it every Monday,
tests the result and opens a draft pull request, labelled "needs GNOME mirror"
when upstream changed code that calls Hyprland directly. For it to open pull
requests, enable "Allow GitHub Actions to create and approve pull requests"
in the repository's Actions settings. A conflict fails the run; resolve it by
hand with `git merge upstream/main`. When upstream changes a file under
`.github/workflows`, the workflow's own token cannot push it: add a
`SYNC_TOKEN` repository secret (a token with the `workflow` scope) or merge by
hand.
