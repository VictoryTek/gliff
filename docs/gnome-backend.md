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
