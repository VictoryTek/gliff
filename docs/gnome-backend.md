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

## `gnome-ipc`

`crates/gnome-ipc` finds the desktop session from a plain login (ssh) and
talks to Mutter. Check a host with:

```
gliff-gnome-probe             # session, Mutter versions, monitors, capture a monitor
gliff-gnome-probe --virtual   # the same, capturing a new virtual monitor
```

Each line is `[ OK ]` or `[FAIL]` with the raw detail. The probe stops once it
has a PipeWire node; it does not read frames.
