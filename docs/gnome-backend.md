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
same capture events as the Hyprland capturer. By default frames are in shared
memory (CPU-readable); with `Options::dmabuf` set (the modifiers the GPU can
import) Mutter's own buffers are handed on as dmabufs for the GPU encoder, and
`Capturer::use_shm` goes back to shared memory. Check it with:

```
gliff-gnome-capture-probe --png frame.png     # capture the primary monitor
gliff-gnome-capture-probe --virtual           # capture a new virtual monitor
gliff-gnome-capture-probe --cursor embedded   # cursor painted into the frames
```

The probe reports the frame size, whether the pixels are real, the frame rate,
whether a recapture works and what the cursor did; `--png` saves the first
frame so it can be looked at.

For the stream of an existing monitor the format we offer names a maximum frame
rate of 0, which Mutter reads as no limit (`Options::unlimited_rate`, set by
the mirror mode). Without it Mutter skips any frame that comes less than
1/maximum after the last one, and for a monitor its own maximum is the
monitor's refresh rate, which on a 60 Hz monitor cost about a third of the
frames (38 to 40 of 59 a second, seen on the NVIDIA desktop; see "A changing
screen and a client that stops reading, on GNOME" in `docs/PLAN.md`). A
monitor now streams at the display's rate (59.4 a second measured on a 60 Hz
monitor, shared memory and dmabuf alike). Every such format is followed by
the same format without the request, so a Mutter whose monitor offer cannot
meet it (46's starts at 1, where a fixed 0 leaves nothing in common) takes the
plain one and streams as before, at about two thirds of the display's rate. A
format Mutter hands back to fix the modifier keeps the request only if what it
handed back allows 0. Only 50.6 has run; 46 was emulated there (see "Mutter 46
and the frame rate" in `docs/PLAN.md`).

A virtual monitor (`--headless`) is offered no maximum at all: Mutter gives the
new monitor the refresh rate it finds there, so naming 0 makes a 0 Hz monitor
(seen: a `1920x1080@0.000` mode and buffers with no pixels). A private screen
was never limited: 59.2 frames a second with no limit named, 56 through the
server on the GPU pipeline once its rate ladder stepped up.

The dmabuf path needs a GPU, so it has its own probe in `gnome-backend`:

```
gliff-gnome-dmabuf-probe --connector HDMI-1 --secs 5 --move   # a monitor; the cursor moves so the screen changes
gliff-gnome-dmabuf-probe --virtual                            # a virtual monitor, resized through three sizes
```

It prints which modifiers Vulkan can import and which Mutter chose, encodes
every frame on the GPU, compares the decoded picture with a shared-memory
capture (monitor mode), and checks that `use_shm` works. The screen must be
free to use while it runs; it wakes the screen and keeps it awake.

## `gnome-input`

`crates/gnome-input` sends gliff's key, pointer, button and wheel commands to
the session. Check it with the probe; aim clicks and keys at a test window,
never at someone's work:

```
gliff-gnome-input-probe move:960,540 click:left type:"Hello 42" wheel:3
gliff-gnome-input-probe caps    # taps Caps Lock twice and checks it toggled and returned
```

`move` is checked by reading the pointer back through the capture stream.

Keys go as keycodes, which Mutter reads with the server's layout, as long as
the viewer's layout is the server's (or the viewer has sent none, or the
server has several layouts and the viewer's is one of them, since the active
one is not known here), so nothing changes for the common case. The server's layout is read from the compositor
(a Wayland keyboard receives the seat's keymap, like the viewer reads its own).
When the viewer's layout differs, every key goes through the viewer's layout on
the server and is sent as the keysym it makes there (a German viewer's Y key is
`z` on a US desktop); Mutter finds the key for the keysym and presses the Shift
it needs. In that mode Shift, AltGr and the lock keys stay on the server side
of the translation (a Shift that reached Mutter as well would apply twice, and
a French Shift+1 is `1`); Shift reaches Mutter for keys that are not characters
(Shift+Left), for clicks and for the wheel, and is let go before the next
character; Control, Alt and Super go as themselves; the keypad's keys go as the
plain characters and navigation keys (so the server's Num Lock does not matter)
and the keypad starts as the server's Num Lock is; only one character key is
held on the server at a time, and it is let go before a key that is not a
character and before a click or scroll. Limits: a character that has no
key at level 1 or 2 of the server's layout (`ö` on a US desktop, anything that
needs AltGr there) is not typed, because Mutter's keysym call neither makes a
key for it nor presses AltGr; give the server the layouts its users need. A Caps
Lock that is on at the server inverts letters, as it would at the keyboard.
Check it with the layout probe, which opens a fullscreen window on the primary
monitor, clicks into its text field and types with several layouts, comparing
what the field received:

```
gliff-gnome-layout-probe      # run on a machine nobody is using
gliff-gnome-layout-probe fr   # only the scenarios for the named viewer layouts; it also prints
                              # how keys go and how far each server layout is from the viewer's
```

A server with several layouts: the viewer's layout is compared with each of
them. If it is none of them, key codes would be wrong whichever is active, so
the keysym mode above is used (Mutter looks the keysym up in the layout in
use). If it is one of them, key codes are kept, as the active layout is not
known here, so a viewer on the server's second layout while the first is
active is typed with the first (not helped: a German viewer on a us+de server
with `us` active typed `yz` for `zy`, measured; see "Several layouts on the
server" in `docs/PLAN.md`).

Layouts are compared on every key except the extension keys (F13 and up, media,
vendor) that carry only function, media and vendor symbols (or none) in both. Two machines with different
xkeyboard-config versions give the same layout different keys there (F23
against a media key, five `I60x` keys one has and the other lacks), and
comparing every key took a viewer on the very same layout for another one and
sent keysyms instead of key codes. Dead keys, Compose, modifiers and F1 to F12
are still compared; a media key the viewer remapped to another one is not told
apart.

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
virtual monitor takes the viewer's scale (see the limits below).

## Running the server on GNOME

`gliff-server` picks Hyprland when an instance is running and GNOME otherwise
(`crates/gliff-server/src/backend.rs`); nothing needs to be configured. On a
machine with a GPU the server uses the GPU video pipeline and Mutter's frames
reach it as dmabufs; with `--video cpu`, `--capture shm`, or no usable GPU it
uses the CPU pipeline and shared memory. If the first GPU encode fails (say the
dmabuf comes from another GPU than the one the server encodes on), the server
falls back to the CPU pipeline by itself. A headless session is first offered
dmabufs for the render node the server would pick by default
(`GLIFF_RENDER_NODE`, else `renderD128`); when the server uses another node
(`--render-node`) the headless session switches to shared memory and the CPU
pipeline, since a dmabuf made for one GPU can lose another GPU's Vulkan device
when imported. Set `GLIFF_RENDER_NODE` to the GPU Mutter renders on to keep the
GPU pipeline on such a machine. A mirrored session (the default) shows the real screen;
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
frame (and which pipeline made it: `Gpu` or `Cpu`), the new monitor (and, for
a private screen, that its refresh rate is usable: at least 30 Hz), the
pointer, the cursor shape, resizes and that the monitors are back to normal
afterwards, and with `--headless` the scale (2, 1.5, a 4K window and a
change at the same size). It does not decode video unless asked: with
`--decode RRGGBB` it decodes the stream with the viewer's own CPU decoder
(`gliff-sw`, for either server pipeline) and requires the middle of the
picture to be that colour, within 16 on each channel, within 25 s (cover the
screen with `gliff-gnome-animate 60 solid:RRGGBB CONNECTOR`, `CONNECTOR` being
the mirrored monitor or `Meta-` for a private screen). It checks the colour
and the picture size, not motion, sharpness or timing. `--decode-gpu` (with
`--decode`) uses the viewer's GPU decoder instead (VA-API decode and the Vulkan
recombine, read back); a debug build run outside the package needs the Vulkan
loader on `LD_LIBRARY_PATH`, which the packaged binaries have built in. A
change of the stream that only paces it (the frame-rate cap) keeps the decoder,
as the viewer does, and a quiet stream has the CPU decoder drained as the
viewer's idle drain does. With `--headless` the colour is also required after
each resize and scale step (`decoded after ...`), at the new stream size. With `--rich-clipboard` it
also moves an image, two files (one of 3 MB) and a folder through the server's
clipboard in both directions, with a plain Mutter client playing the desktop's
applications (it replaces the desktop's clipboard while it runs):

```
serve_probe target/debug/gliff-server --headless --rich-clipboard
```

`--load SECS` (on a mirrored screen, with `gliff-gnome-animate` running on it;
the animation prints how many frames it painted and how many the compositor
reported as presented, which is what the screen really showed)
reads the stream while the picture changes all the time and reports frames per
second, bytes a frame, the longest gap and the server's CPU and memory;
`--stall SECS` adds a stretch where the client stops reading, and checks that
the server's memory stays bounded and that frames come promptly when it reads
again (it does not check that the first ones are fresh):

```
gliff-gnome-animate 90 &                  # a fullscreen window that changes every frame
serve_probe target/debug/gliff-server --load 30 --stall 20
```

On a private screen the animation has to be told which monitor to cover, and
it waits (up to 30 s) for it to appear; the probe then skips the resize and
scale steps:

```
gliff-gnome-animate 90 30 Meta- &         # covers the private screen once it exists
serve_probe target/debug/gliff-server --headless --load 40
```

`--headless --soak SECS` keeps the session up for SECS seconds, resizing it
every few seconds (sizes and scales up to 4K at scale 2), and prints the
server's memory, open files and threads after every full cycle of sizes; it
fails on a resize without the stream asked for and a frame, or on growth beyond a margin after the first
cycle, which is a tolerance and not a leak detector (a first 8-minute run on
the NVIDIA desktop showed no growth, and a 5-minute one with per-cycle
samples held flat to 4 kB; hours would be needed to say more).

## The viewer on GNOME

The `gliff` viewer needs no GNOME-specific code: it reads the local keymap and
picks the render node through ordinary Wayland protocols. Two GNOME behaviours
show up when you use it:

- The first time the window takes the keyboard, GNOME Shell asks whether to
  allow capturing system shortcuts. Until you allow it, shortcuts such as
  Super stay with the local desktop. GNOME remembers the answer only for an
  app it can match to a desktop file, and it matches a window that was
  started through that file: launch gliff from the application grid (the
  package installs `gliff.desktop`; `gio launch` on the file does the same).
  Started from a terminal or over ssh as plain `gliff`, GNOME does not match
  the window to the file and asks again on every launch. No viewer change
  is needed, and the viewer sets no application id. Checked on the NVIDIA
  desktop; see "The shortcut prompt on GNOME" in `docs/PLAN.md`.
- Do not run the viewer and `gliff-server` on the same desktop with the
  clipboard on: both sides then share one clipboard, each sees the other's
  copy as new, and the bridge ping-pongs (about 4,600 requests in four
  minutes, one "Pasting text" notification each). For a one-machine test
  start the viewer with `--no-clipboard`.
- A resize or scale change of a private screen makes Mutter give the one
  virtual monitor a new mode and reload its monitor configuration
  (`ensure_virtual_monitor` in its virtual stream source), and while that
  happens windows on it are reported on another monitor. Seen on a desktop that
  also has physical monitors, with the test window of `gliff-gnome-animate`:
  a fullscreen window set to the private screen stays off it until it is put
  back (6 to 13 times in a run of nine steps; until then a decode check saw
  the desktop background), while a maximized or a floating window that had
  settled on the private screen was reported on a physical monitor at the
  first resize, and again at the second in the two runs that printed the
  trail (about 0.2 to 0.4 s each), and was back on the private screen each
  time (six runs in all, two with the trail; the number of departures counted
  was 1 in four runs and 2 in the two with the trail, and the 200 ms sampling
  can miss or merge short hops). A window's position and size after it
  returns, other applications' windows, and a host whose only monitor is the
  private screen were not checked.
- A private (`--headless`) screen follows the viewer's scale, as far as
  Mutter allows it (the nearest scale the mode supports, and none that leaves
  under 800x480 logical pixels). Tested on a real NVIDIA GPU; on the
  software-rendered test VM a scale of 1.75 crashed GNOME Shell once, so a
  host without a GPU may do the same. A new scale at the same pixel size is
  ignored by Mutter until the size changes, and in our runs a size the screen
  already had at another scale came back with that earlier scale (the stream
  and the pointer mapping follow the scale Mutter applied). See "Scale (HiDPI) on GNOME" in
  `docs/PLAN.md`.

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
