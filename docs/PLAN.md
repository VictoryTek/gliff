# Plan: a GNOME-capable Gliff fork to replace Sunshine/Moonlight in vexos

Tier 3 (new dependency, new repo, many files, replaces a remote-access feature). Nothing is built until you approve this plan.

## Status

Update this table when a step finishes. A fresh session starts here, then reads the "Decisions and results log" at the end and `git log`.

| Step | State | Notes |
|---|---|---|
| Fork, flake, branch model | done | Fork is `VictoryTek/gliff`. The `upstream` mirror branch is optional and not published; GNOME work lives on `main`. |
| M0 feasibility spike | done, gate passed with a limit | Works unlocked; a locked screen blocks it. Sunshine is replaced anyway (decision). Spike code is in `~/Projects/gliff-spike` on one machine only, not in this repo; its findings are in the log below. |
| M1 Nix packaging | done, one gate item waived | Package builds, 275 tests pass. Hyprland-to-Hyprland run not done (no Hyprland host); assumed to work. |
| M2 the seam | done, one gate item waived | `scripts/check.sh` passes (276 tests). `scripts/e2e.sh` not run (no Hyprland session); assumed to pass. |
| M3 GNOME backend | **in progress**: steps 1 `gnome-ipc`, 2a `gnome-capture` (shared memory), 3 `gnome-input`, 4 `gnome-clipboard` and 5 virtual output, keep-awake and wake-from-idle, and the `GnomeBackend` wiring into `gliff-server` done; 2b dmabuf waits for a real GPU host, then M4 (viewer on GNOME) | Steps: 1 `gnome-ipc` (done), 2a `gnome-capture` (done), 2b dmabuf with modifier negotiation (not started; needs a real GPU host, the VM renders in software), 3 `gnome-input` (done; keymap forwarding refused for now), 4 `gnome-clipboard` (done), 5 virtual output, keep-awake and wake-from-idle (done), then the `GnomeBackend` wiring (done; `gliff-server` runs on GNOME with the CPU video pipeline). |
| M4 viewer and hardware matrix | **in progress**: viewer audit and a first run of the real viewer on a GNOME desktop done (no viewer code change needed); GNOME's shortcut-inhibit prompt, scale, real GPUs and the hardware matrix are open | See "M4 part 1 result". |
| M5 vexos integration | not started | Done in a session in the vexos repo, after M4 passes. Sunshine stays until then. |
| M6 sync automation | written and tested locally; **not yet run on GitHub** | `.github/workflows/upstream-sync.yml` and `scripts/sync-upstream.sh` exist. The script was run against a throwaway clone in every case; the workflow passed `actionlint` but has not run (it needs your push, and the repository setting "Allow GitHub Actions to create and approve pull requests"). See "M6 result". |
| M7 Windows viewer | not started, optional | |

To check the repo: `nix develop -i -k HOME -k TERM path:. -c scripts/check.sh` (the plain `nix develop` fails to link here).

## Context

You want one remote-desktop tool that needs only SSH. Sunshine/Moonlight needs a PIN pairing on every workstation. Gliff (omacom/gliff, MIT, Rust, v0.4.3) is the tool you like, but its server only works on Hyprland. You want to:

1. fork Gliff,
2. keep its Hyprland support as upstream has it,
3. add a GNOME backend, written in Rust,
4. keep the fork in sync with upstream,
5. then drop Sunshine and Moonlight from vexos.

This plan covers the fork, the GNOME backend, how we keep in sync, and the vexos changes.

## How it would work, in plain language

Gliff has two programs. The **viewer** (`gliff`) is the window you look at. The **server** (`gliff-server`) runs on the machine being controlled. SSH starts the server and carries everything, so there are no open ports and no pairing.

The server has five jobs:

- grab pictures of the screen
- squeeze them into video (the GPU encoder, which is the clever part)
- send your keyboard and mouse into the desktop
- share the clipboard
- (optional) make a private extra screen that matches your window

The wire protocol carries only keycodes, pointer moves, clipboard data and video. It never mentions Hyprland. The video code never mentions Hyprland. **Only the "talk to the desktop" part does**: grab screen, inject input, bridge clipboard, create and resize the private screen, keep the screen awake.

So the project is: **give the server a second way to talk to the desktop, this time to GNOME.** Everything else is reused.

**One correction found later:** the viewer is not completely free of Hyprland either. `crates/gliff/Cargo.toml` depends on `hypr-capture`, `hypr-ipc` and `hypr-input`, and `net.rs` calls `hypr_capture::render_node`. My guess is the viewer uses them to find the GPU and to read the local keyboard layout (`hypr-input` exports `watch_keymap` and `keymap_from_names`), but I have **not** read that code. So the seam also covers a small viewer-side part: local keyboard layout detection and GPU selection on a GNOME machine.

On GNOME the "talk to the desktop" part maps almost one-to-one onto GNOME's own remote-desktop interface (Mutter). I checked these interfaces in Mutter's published D-Bus files:

| Server job | Hyprland today | GNOME (Mutter D-Bus) |
|---|---|---|
| Grab a monitor | `ext-image-copy-capture` | `ScreenCast.RecordMonitor`, delivered as a PipeWire video stream |
| Private screen | `hyprctl output create headless` | `ScreenCast.RecordVirtual` (a virtual monitor) |
| Cursor shape and position | capture events | `cursor-mode = 2` (cursor sent as stream metadata) |
| Keys, mouse, scroll | virtual keyboard/pointer protocols | `NotifyKeyboardKeycode`, `NotifyPointerMotionAbsolute`, `NotifyPointerButton`, `NotifyPointerAxis`, `SetKeymap` |
| Clipboard | `ext-data-control` | `EnableClipboard`, `SetSelection`, `SelectionRead`, `SelectionWrite` |
| Find the session | Hyprland socket | the user's session D-Bus at `/run/user/<uid>/bus` |

Gliff's own input commands use evdev keycodes and absolute pointer positions. Mutter accepts exactly those.

### How we keep it in sync with upstream

You asked for a separate "GNOME branch". I recommend a small change to that idea. **One fork, one main branch, two backends in the same code, chosen at runtime** (GNOME or Hyprland, detected from the running session). A long-lived separate GNOME branch would drift, and every upstream change would need hand-merging into it.

- `upstream` branch: an exact copy of omacom/gliff, fast-forward only. A weekly job updates it.
- `main`: upstream plus **our additions**. The additions are new crates plus one small "seam" change.
- The **seam**: Gliff's server calls Hyprland directly in about 8 places in `crates/gliff-server/src/session.rs` (77 KB) and `main.rs`. We replace those calls with a small `Backend` interface. This is the only edit to upstream files that matters. It's the only place merge conflicts will happen, so we keep it as small as possible.
- The GNOME code lives in **new crates** (`gnome-ipc`, `gnome-capture`, `gnome-input`, `gnome-clipboard`) that upstream never touches, so those never conflict.
- Every week a CI job merges `upstream` into `main`, builds, runs the tests, and opens a pull request for you to approve. It labels the PR **"needs GNOME mirror"** if upstream changed the Hyprland-only crates (`hypr-*`) or the session code. Those are the changes that need a matching change in the GNOME crates. Changes to video, protocol, viewer or clipboard flow automatically.
- **First step upstream:** before writing any code, open an issue upstream saying "we'd like to add a compositor backend interface and a GNOME backend". If they accept the seam, our fork shrinks to just the GNOME crates and the sync problem mostly disappears. If not, we carry the small seam patch ourselves.

## Verified facts vs unverified

Verified by reading sources this session:
- Gliff's wire messages (`gliff-proto/src/msg.rs`): no Hyprland content. Keys are evdev keycodes. Pointer motion is absolute.
- Capture API (`hypr-capture`): frames carry `DmabufInfo` (fd, fourcc, modifier, planes) or a wl_shm memfd, plus damage. `CaptureMode::Shm` already exists and forces the CPU path.
- Hyprland coupling in `session.rs` is listed above. Clipboard bridge and transfer logic are generic once started.
- Mutter's D-Bus interface files list every method in the table above, and they document **no authentication**. They also say the API is "private and not intended to be used outside of the integrated system", with "no compatibility between versions".
- Mutter's `CreateSession` code in `meta-remote-desktop.c` has no caller check. gnome-shell issue #3943 proposes an allow-list but records no decision.
- Upstream Gliff has no issues or PRs mentioning GNOME, KDE or a backend interface. There is no CONTRIBUTING file. It has 325 commits, 27 open issues and 39 open PRs.

**Not verified:**
- Whether an SSH-launched process can use Mutter's private API without a prompt on your GNOME version.
- Whether Mutter refuses screen capture while the session is locked.
- How Mutter's `SetKeymap` takes a keymap from the client.
- The PipeWire dmabuf modifier negotiation details on AMD, Intel and NVIDIA.
- Whether the GTK4 viewer runs well on GNOME.

The first four are what the first spike is for.

## Correction to what I told you earlier

I said Sunshine's KMS capture was chosen because of two GNOME portal bugs, Sunshine issues #2838 and #1631. I took that from the comment in `modules/sunshine.nix`. A research agent checked the issues. They are about NVFBC and encoder probing, not GNOME portals. They don't support that claim. They are not evidence about how the portal behaves unattended, so I'm not relying on them. The `modules/sunshine.nix` comment is inaccurate, but I'll leave it alone since it's out of scope (the file goes away anyway).

## Milestones

Each milestone has a gate. If a gate fails we stop and report. We don't stack workarounds.

**M0 — Feasibility spike (days). Throwaway code, in `~/Projects/gliff-spike`, not in vexos.**
A small Rust program (zbus + pipewire crates). Run it over SSH on one of your real GNOME vexos hosts, with nobody touching the machine:
- create a RemoteDesktop + ScreenCast session
- `RecordMonitor` and `RecordVirtual`, receive PipeWire frames (shared memory first, then dmabuf)
- inject a key, a pointer move, a scroll
- set the clipboard and read it back
- repeat with the screen **locked**, and with the session on a different GPU brand if you have one

Gate: works with no prompt on current GNOME → continue. Works only through the xdg-desktop-portal route (one approval click per workstation, then remembered) → I come back to you, because that partly reintroduces the "setup per workstation" you dislike, though far lighter than PIN pairing. Works through neither → plan stops.

**M1 — Fork and Nix packaging.**
- You create the fork (`gh repo fork omacom/gliff` is an outward-facing action on your account, so you run it; I will not).
- I clone it locally outside vexos (e.g. `~/Projects/gliff`) and add `flake.nix`, with `packages.default` built from the Cargo workspace and `nixosModules.default` (installs `gliff` and `gliff-server`, sets up the Vulkan/VA-API libraries).
- Build dependencies come from upstream's PKGBUILD: cargo, pkgconf, nasm, a C++ compiler, gtk4 ≥ 4.14, libadwaita ≥ 1.5, libva ≥ 2.20, libdrm, libxkbcommon, mesa, vulkan-loader, wayland.
- Gate: unmodified Gliff builds under Nix. It runs Hyprland → Hyprland between two vexos hosts, using the `gliff-probe all` checks.

**M2 — The seam (no behavior change).**
Extract the `Backend` interface in `gliff-server`: `discover`, `list_outputs`, `create_virtual_output`, `set_mode`, `remove_output`, `keep_awake`, `start_capture`, `start_input`, `start_clipboard`. Move the Hyprland calls behind it with the smallest possible diff. Gate: existing tests and the Hyprland end-to-end script (`scripts/e2e.sh`) still pass.

**M3 — GNOME backend, in this order:**
1. `gnome-ipc`: session discovery (graphical session via `loginctl`, `XDG_RUNTIME_DIR`, D-Bus address), Mutter sessions over zbus.
2. `gnome-capture`: PipeWire stream → the same `CapturedFrame` type. Shared memory first, dmabuf with modifier negotiation second.
3. `gnome-input`: key, pointer, scroll, keymap.
4. `gnome-clipboard`: plug into Gliff's existing clipboard bridge.
5. Virtual output create, resize, remove, keep-awake (idle inhibit instead of Hyprland's screen-power calls).

Gate per step: a probe command (extend `gliff-probe`) proves the step on a real GNOME host, then end to end with the viewer.

**M4 — Viewer on GNOME and hardware matrix.** First read every use of `hypr_*` in `crates/gliff/src` (`grep -rn 'hypr_' crates/gliff crates/gliff-probe`) and add the viewer part of the seam (local keymap detection, GPU/render-node choice) with a GNOME implementation. Then check the GTK4 viewer on a GNOME desktop: keyboard grab, release hotkey, theme. Test AMD, Intel and NVIDIA hosts and the VM role. Test both mirror mode and `--headless` mode.

**M5 — vexos integration (Tier 3 in this repo, after the fork works).**
- Follow the existing `vex-vpn` precedent in `flake.nix`: a flake input for the fork (`inputs.nixpkgs.follows = "nixpkgs"`), plus a module import in the roles table.
- New `modules/gliff.nix` mirroring the feature pattern in `modules/sunshine.nix` (`vexos.features.gliff.enable`, default on, off for vanilla). Imported by the same roles that import Sunshine today: desktop, htpc, server, vanilla. Stateless and headless-server have no Sunshine today, so they stay as they are unless you say otherwise.
- Remove Sunshine and Moonlight **only after** M4 passes on every host type: `modules/sunshine.nix`, its imports in `configuration-{desktop,server,htpc,vanilla}.nix`, `services.sunshine.settings.encoder` in `modules/gpu/{amd,intel,nvidia}.nix`, `pkgs.moonlight-qt` in `modules/desktop-common.nix`, the Moonlight and Sunshine dock favourites in `home-*.nix` and `modules/gnome-htpc.nix`, the sunshine entries in `just/features.just` and `template/features.nix`, and the `uinput` group grant.
- Add the new input to the daily or weekly lock-update workflow, as the vex-vpn input is missing from both today.
- Leave SSH exactly as it is (password login stays on). Add your public key to `authorized_keys` if you want prompt-free logins.

**M7 — Windows viewer (optional, after M5).** Dropping Moonlight also drops the native Windows client, so this needs its own answer.
- The existing viewer cannot be built for Windows as is: it uses GTK4/libadwaita, Vulkan plus VA-API decoding (`gliff-vk`; VA-API is Linux-only) and the `hypr-*` crates.
- The protocol is portable. It is CBOR messages over the stdio of an `ssh` child process (`ssh -T host gliff-server --stdio`), and Windows ships an OpenSSH client. The handshake lets a client ask for plain H.264 4:2:0 (`Single420`), and the CPU decoder `gliff-sw` (OpenH264) is portable.
- Step 1 (an hour): try the existing Linux viewer under WSL2/WSLg with CPU decode (`--video cpu`). Untested; shortcut capture in WSLg may be poor.
- Step 2 if that is not good enough: a native Windows viewer as a new crate reusing `gliff-proto`, `gliff-transport` and `gliff-sw`, with its own window, input capture and clipboard. Start with 4:2:0 video, add the 4:4:4 recombine later. Real work, comparable in size to the GNOME backend.
- Not covered: Windows as the machine being controlled. That would be a different server backend and is out of scope.

**M6 — Sync automation.** The weekly upstream-merge CI job and the "needs GNOME mirror" label described above.

## Difficulties and how we handle them

1. **Mutter's API is private and may change between GNOME releases.** NixOS pins the GNOME version, so we only face a change when we update. We cover it with a NixOS VM test that boots GNOME and runs the probe, so a nixpkgs bump that breaks it is caught before you switch. We keep the portal route (via the `ashpd` crate) as a second choice the server can pick at runtime.
2. **Unattended permission.** M0 answers this first. See the gate.
3. **PipeWire dmabuf negotiation.** Getting zero-copy video from PipeWire into Vulkan needs the right modifiers per GPU. We start with shared memory (slower, works everywhere, Gliff already has this CPU path) and add dmabuf per GPU once correct. NVIDIA already needs shared memory in upstream for capture, so that is no regression.
4. **Frame timing.** Gliff asks for a frame when it wants one. PipeWire pushes frames. We keep only the newest frame, drop older ones, and re-send the last frame on a keyframe request. Mutter only sends frames when the screen changes.
5. **Keyboard layouts.** The client sends its keymap and the server applies it. Mutter's `SetKeymap` has to be tested for that. Fallback: send keysyms or rely on matching layouts.
6. **Locked screen and sleeping displays.** Hyprland uses screen-power control. GNOME needs an idle inhibit, and capture while locked is unverified (M0).
7. **Password prompts.** You use password SSH. The viewer is a GUI app, so ssh can't prompt in a terminal. We test what happens, then fix it with an askpass helper or the key route. We never turn off password login.
8. **Upstream drift.** Mitigated by the small seam, the weekly merge PR, the label, and the upstream issue. Honest cost: someone has to look at one PR a week, and some upstream changes will need a GNOME-side change by hand.
9. **Upstream lock-in and effort.** Realistic size: a few days for M0, then weeks for M3 and M4. A fork you maintain indefinitely. Sunshine stays in vexos until M4 passes, so you are never left without remote access.
10. **Licence.** Gliff is MIT. Keep its LICENSE and NOTICE (it bundles OpenH264, BSD-2). Our GNOME crates are ours. Using Mutter's D-Bus API does not make our code GPL.

## Process rules for this work

- I never run `git add`, `commit`, `push` or `stash`. You fork, commit and push. I give one commit message per change.
- The Gliff fork lives outside this repo. The vexos-side changes (M5) are Tier 3 here: plan, reviewer subagent, preflight, `nix flake show --impure`, and dry-builds of `vexos-desktop-{amd,nvidia,vm}`, plus server and htpc for the roles touched.
- After 3 failed fix cycles on any milestone I stop and report.

## Verification

- M0: the spike prints what worked unattended, with output pasted to you.
- M1–M4: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace` (upstream's `scripts/check.sh`), `gliff-probe all` on each host, and a live session on each GPU brand in mirror and headless mode.
- M3: NixOS VM test that boots GNOME and runs the probe.
- M5: `nix flake show --impure`, then `nix eval --impure` or dry-build per target, then `bash scripts/preflight.sh` must exit 0.

## Scaffold after approval (outside vexos)

```
~/Projects/gliff/              # clone of your fork
  flake.nix                    # package + nixosModule (M1)
  crates/
    gliff-backend/             # NEW: Backend trait (the seam)
    gnome-ipc/                 # NEW: session discovery, Mutter via zbus
    gnome-capture/             # NEW: PipeWire -> CapturedFrame
    gnome-input/               # NEW: keys, pointer, scroll, keymap
    gnome-clipboard/           # NEW: Mutter selection -> existing bridge
    gnome-backend/             # NEW: GnomeBackend (the gnome-* crates behind the trait)
    (all upstream crates)
  .github/workflows/
    upstream-sync.yml          # weekly merge PR + "needs GNOME mirror" label
    ci.yml                     # fmt, clippy, tests, Nix build, VM test
  docs/gnome-backend.md        # how the GNOME side works
~/Projects/gliff-spike/        # throwaway M0 spike
```

## Decisions I need from you at the gates, not now

- After M0: if only the portal route works, do you accept one remembered approval click per workstation?
- After M1: are you happy for me to open the "backend interface" issue upstream? That is outward-facing, so I'll draft it and you post it.

## Decisions and results log

### M0 result (2026-11-15) — spike run on a vexos desktop VM (GNOME Shell 50.6, software rendering)

- Over SSH, with nobody at the machine: RemoteDesktop + ScreenCast sessions, `RecordMonitor` and `RecordVirtual` (shared-memory frames with real pixels), and the clipboard (two sessions, both directions) all worked with no prompt reaching the caller.
- Key, pointer and scroll calls were accepted; their effect on the desktop was not observed. Cursor metadata never arrived (no cursor meta block on any buffer), so absolute-pointer behaviour is unconfirmed.
- **Locked screen: blocked.** After `loginctl lock-session` the open session was closed in 3.3 s and a new one failed with `Session creation inhibited`; it worked again after `loginctl unlock-session`. This matches Mutter/GNOME Shell source (`unlock-dialog` mode does not allow screencast, so Shell calls `inhibit_remote_access()`).
- `SetKeymap` is untrusted: the run that called it coincided with a GNOME Shell SIGSEGV in the capture path (llvmpipe texture allocation); three later runs without it did not crash. `KeymapCapabilities` reads back empty on this Mutter. The keysym or matching-layout fallback is needed.
- Not tested: dmabuf, real GPUs (AMD/Intel/NVIDIA), an idle-triggered lock, the login screen, reading what a desktop app copied.

### Decision (2026-11-15)

This fork **replaces Sunshine/Moonlight**. The cost is accepted: a locked workstation cannot be reached with this backend. Sunshine stays in vexos until M4 passes, as planned.

### M1 result: Nix packaging (2026-11-15)

- `flake.nix` + `nix/package.nix` + `nix/module.nix` build the unmodified upstream workspace (`packages.default`) and provide `nixosModules.default` (`programs.gliff.enable`: installs the package, enables `hardware.graphics`). The Nix build runs the unit tests (275 at the time).
- Fixed along the way: `dbus` was missing from the build inputs; the Vulkan loader is linked into the real executables because it is opened with `dlopen` and the RPATH shrink would drop it.
- Not done: the Hyprland-to-Hyprland run with `gliff-probe all` (no Hyprland host). Waived by decision; assumed to work.
- Nix ignores untracked files in a git repo, so before a commit use `nix build path:.#default`.

### M2 result: the seam (2026-11-15)

- New crate `gliff-backend`: the `Backend` trait (`list_outputs`, `create_virtual_output`, `set_mode`, `remove_output`, `align_mode`, `keep_awake`, `start_capture`, `start_input`, `start_clipboard`), the handle traits (`CaptureControl`, `InputControl`, `ClipboardControl`) and neutral `OutputInfo` / `CreateError`. Frames, input commands and clipboard events keep the `hypr-capture` / `hypr-input` types, so a GNOME backend produces the same `CapturedFrame`.
- `gliff-server` changes: `session.rs` and `clipboard.rs` call the trait instead of `hypr_ipc` / `Capturer` / `Input` / `Clipboard`; `main.rs` only gains two `mod` lines. The Hyprland calls moved unchanged into `src/hypr_backend.rs`; `src/backend.rs::discover` picks the backend (Hyprland only for now).
- Rulings: (1) `keep_awake` returns the periodic check closure and `session.rs` keeps the existing `KeepAwake::poll` thread, to keep the diff small. (2) `align_mode` was added to the trait because Hyprland's scale-divisibility rule (`clean_mode`) is compositor specific. (3) `discover` is a free function in `gliff-server` because the trait crate cannot depend on its implementations. (4) `Config` keeps its `Hyprland`-typed `target` and `render_node` for now, so `main.rs` is untouched.
- Verification: `scripts/check.sh` (rustfmt, clippy `-D warnings`, 276 unit tests) passes. `scripts/e2e.sh` was NOT run (no Hyprland session available); by decision it is assumed to still pass.

### M3 step 1 result: `gnome-ipc` (2026-11-17)

- New crate `crates/gnome-ipc` (zbus blocking API, no async runtime needed). `discover` finds the user's active local graphical session from an ssh login with `loginctl` (skips our own remote login and greeter sessions) and builds the session bus address. `mutter` connects to the bus, reads both Mutter `Version` properties, lists monitors (`DisplayConfig.GetCurrentState`) and wraps a linked RemoteDesktop + ScreenCast `Session` (`record` a monitor or a virtual monitor, `start`, wait for the PipeWire node, `stop`). Each `Session` has its own bus connection and closes it on drop, so Mutter ends the session even when a listener thread is still blocked. Probe binary: `gliff-gnome-probe [--virtual]`.
- Verified on the GNOME 50.6 VM over ssh (`nimda@192.168.100.71`), with no `DBUS_SESSION_BUS_ADDRESS` or `XDG_RUNTIME_DIR` set: session found, Mutter RemoteDesktop v1 and ScreenCast v4, monitor and virtual capture both reach a PipeWire node, repeated runs leave no sessions behind, a bad bus address fails with a clear message. A temporary check (deleted) showed that a failed `RecordMonitor`, an unstarted session and a fresh session all release their listener threads once dropped. `scripts/check.sh` passes; the crate has 8 unit tests.
- Not verified: locked screen (known limit), a failed `Start`, more than one monitor, anything about frames (step 2).
- Review: two rounds with Sol (`gpt-6.1-sol`, default effort). Round 1: one `should` (listener threads kept the connection alive on failed create or start, and Mutter rejects `Stop` before `Start`); fixed with a connection-closing guard. Round 2: approved, nothing open. Sol could not run cargo in its sandbox, so every test result above is from this machine.

### M3 step 2a result: `gnome-capture`, shared-memory frames (2026-11-17)

- New crate `crates/gnome-capture`: `Capturer::start(Options, sink)` connects to the PipeWire node `gnome-ipc` opened (over the socket in the user's runtime dir, no environment variable needed) and emits the same `CaptureEvent`s as the Hyprland capturer. `request_frame`, `recapture` and drop match the pull model `gliff-server` uses; PipeWire pushes, so `pacer.rs` keeps the newest frame, replaces an unsent one, and can send the last frame again. Frames are copied into a ring of memfd buffers (the `buffers` the consumer may hold, plus one for re-sends), so PipeWire buffers go back at once. `cursor.rs` turns cursor metadata into `CursorShape` and `CursorPos` events. Probe: `gliff-gnome-capture-probe [--virtual] [--connector NAME] [--cursor hidden|embedded|metadata] [--png FILE] [--secs N]`.
- Upstream edit (approved): two lines in `crates/hypr-capture/src/lib.rs` plus the new file `external.rs`; see the footprint table. `nix/package.nix` (ours) gained `pipewire` and the bindgen hook.
- Verified on the GNOME 50.6 VM (software rendering): a monitor capture gives a correct 1920x1080 frame (the saved PNG was the real desktop, colours and orientation right), first frame in 150 to 350 ms, recapture re-sends the last frame, a virtual monitor works (the format renegotiated once, so two `Ready` events), `--cursor embedded` and `hidden` work. Cursor shape (24x24) and position now arrive in `metadata` mode. This resolves the M0 gap, whose likely cause was asking for the metadata with a fixed size instead of a size range. `scripts/check.sh` passes (12 unit tests in the crate) and `nix build path:.` passed.
- Bug found while testing: a frame request made before the format was negotiated was lost when the ring was built; fixed with a test.
- Not verified: dmabuf (2b), a moving or hiding cursor live (needs input, step 3; covered by unit tests and Sol's reading of Mutter's source only), a real GPU, mid-capture resize beyond the one virtual renegotiation, a slow consumer under load, PipeWire stream-error paths.
- **Idle blanking blocks capture even with screen lock off.** On the VM (idle delay 300 s, `lock-enabled` false), after five idle minutes `org.gnome.ScreenSaver.GetActive` was true and Mutter refused sessions with "Session creation inhibited". `org.gnome.ScreenSaver.SetActive(false)` over ssh cleared it and capture worked again. With screen lock enabled this would probably show a password prompt instead (not tested). Consequence for step 5: wake the screen before creating a session and inhibit idle while one runs. A machine that is already locked still cannot be served (the M0 limit).
- Review: two rounds with Sol (`gpt-6.1-sol`, default effort). Round 1: three `should` findings, all fixed: cursor shape was keyed on a cursor id that Mutter reuses (S1), the cursor was never hidden (S2), and a full ring dropped every newer frame while an unheld frame was pending (S3). Round 2: approved. Sol could not run cargo, so test results are from this machine.

### M3 step 3 result: `gnome-input` (2026-11-17)

- New crate `crates/gnome-input`. `Translator` (pure, in `translate.rs`) turns gliff's `InputCmd`s into Mutter RemoteDesktop calls and tracks held keys and buttons, the pointer extent and `Stop`, the way `hypr-input` does: motion clamps to extent-1, a wheel with discrete steps is a wheel event and one without is smooth scrolling (Mutter counts 10.0 as one step, the same unit as Wayland), `ReleaseAll` and `Stop` release what is held, and after `Stop` nothing more is sent. `Input` applies the calls through `gnome_ipc::InputPort` (new, from `Session::input()`) and releases everything when dropped. Probe: `gliff-gnome-input-probe ACTION...` (`move:X,Y`, `click[:button]`, `type:TEXT`, `wheel:N`, `caps`, `wait:MS`).
- Mutter's input calls (from its published D-Bus XML, gnome-50): `NotifyKeyboardKeycode`, `NotifyPointerButton`, `NotifyPointerAxis`, `NotifyPointerAxisDiscrete`, `NotifyPointerMotionAbsolute(stream, x, y)`. It also has `SetKeymap`, `SetKeymapLayoutIndex` and `ConnectToEIS`, none used yet.
- Verified on the GNOME 50.6 VM: absolute moves land exactly where sent (read back through the capture stream's cursor metadata); Caps Lock toggles and returns, so keys reach Mutter; in a GNOME Console window running a raw-mode logger, left and right clicks arrived with press and release, `Hello 42` typed exactly (Shift for `H` included), and a wheel of +3 and -2 gave three down and two up events. `scripts/check.sh` passes (7 translator tests) and `nix build path:.` passed. No upstream file is touched beyond the root `Cargo.toml` member line.
- **Known gap: keymap forwarding.** Keys go as evdev keycodes, which Mutter reads with the server's own layout, so a viewer with a different layout types the wrong characters. `SetKeymap` is refused with an error, not ignored. Mutter's own `SetKeymap` crashed GNOME Shell once in the M0 spike (cause unproven), so it stays off until tested on its own. Options when this is picked up: translate keycodes to keysyms with the viewer's keymap (`NotifyKeyboardKeysym`), or test `SetKeymap` in isolation. When wiring, check how `gliff-server` treats an error from `send(SetKeymap)`.
- Not verified: scale above 1, a virtual or second monitor's coordinate space, key repeat, modifier shortcuts, the effect of smooth scrolling, touch.
- Review: two rounds with Sol (`gpt-6.1-sol`, default effort). Round 1: one `blocker`, `Stop` did not release held keys or refuse later input; fixed with a test. Round 2: approved. Sol could not run cargo, so test results are from this machine.

### M3 step 4 result: `gnome-clipboard` (2026-11-17)

- New crate `crates/gnome-clipboard`, plus `crates/gnome-ipc/src/clipboard.rs` (`ClipboardPort`, `ClipboardSignal`, `Session::enable_clipboard`) and `Session::create_remote_only` (a RemoteDesktop session with no screen cast; the ScreenCast proxy in `Session` became optional). `Clipboard::start(address, sink)` creates its own session, so the clipboard starts and ends independently of capture and input. It offers what gliff's bridge needs: `offer(mime_types)` (empty withdraws), `receive(mime)` (a pipe made blocking, yielding the bytes then EOF), and the events `Selection` and `Paste`.
- How it maps to Mutter: `SelectionOwnerChanged` becomes `Selection` unless the session is the owner or the list carries our marker mime `application/x-gliff-gnome-source` (added by `offer`, like Hyprland's `OWN_SOURCE_MIME`, so a copy a clipboard manager republishes is not reported as new). `SelectionTransfer` becomes `Paste`: the consumer writes into a pipe of ours and a thread carries it to Mutter's `SelectionWrite` descriptor, then calls `SelectionWriteDone`. A withdraw is `SetSelection` with no `mime-types`, which Mutter treats as "unset our source" and does only if our source still holds the selection.
- Verified on the GNOME 50.6 VM (`gliff-gnome-clipboard-probe roundtrip`, against plain Mutter clients): the bridge sees a copy and reads it; another application sees the remote's copy and pastes it; 2 MB intact both ways; no echo of our own offer or of a marker-carrying republished copy; a withdraw is reported empty to other clients; a withdraw when we no longer hold the selection changes nothing; the bridge still hears copies after a withdraw; an offer straight followed by a withdraw leaves nothing behind. Real applications: Ctrl+Shift+V into a GNOME Console window received exactly the offered text, and selecting and Ctrl+Shift+C in Console was seen and read through the bridge (checked again on the final build). `scripts/check.sh` passes and `nix build path:.` passed (before the last fix, which changed no packaging).
- **Mutter facts worth keeping** (all seen live or in the 50.6 source): its interface file says `mime-types` is `as`, but on the wire it is `(as)`, so the parser accepts both. `SetSelection` with an empty `mime-types` list does nothing visible; omitting the key unsets. `DisableClipboard` never clears Mutter's "clipboard enabled" flag, so `EnableClipboard` afterwards fails with "Already enabled" and notifications stay off: never withdraw that way. When an owner goes away, Mutter hands the selection back to whatever held it before, so a withdraw is reported as empty and the older selection then reappears. The clipboard of the VM's own apps shows up in `Selection` events from the start.
- Not matched to Hyprland: no echo guard for the empty report our own withdraw causes. The server answers it by telling the viewer the desktop clipboard is empty, which is harmless, and the restored older selection could not be hidden by a guard anyway.
- Not verified: a consumer that never closes the paste descriptor (the pump thread then waits forever, as the app waits), many concurrent pastes, non-text types, file lists, the clipboard on a session shared with capture and input. When wiring: how the server treats a failed `offer`.
- Review: two rounds with Sol (`gpt-6.1-sol`, default effort). Round 1: one `blocker` (the withdraw switched notifications off for good, which my probe had not checked because it only watched another client) and one `should` (the client-side ownership flag raced with Mutter's notifications); both fixed by using Mutter's own conditional unset and deleting the flag. Round 2: approved. Sol could not run cargo, so test results are from this machine.

### M3 step 5 result: virtual output, keep-awake, wake from idle (2026-11-18)

- `gnome_capture::VirtualOutput` (new, `output.rs`) is a private screen: `create(desktop, size, cursor, sink)`, `connector()`, `monitor()`, `resize(w, h)`, `close()`, and it removes the monitor when dropped. It owns the session, its stream, the capture and the idle inhibition, because on Mutter the virtual monitor is not a separate object: it exists only once something is connected to the stream, and its size is the format that consumer asks for. So `Capturer::resize(w, h)` (new) asks the stream for a new fixed size, which Mutter answers by resizing the monitor, and `Options::size` asks for a size from the start (otherwise 1920x1080). Mutter names the monitor itself (`Meta-0`, `Meta-1`), so `create` names it by what is new in `DisplayConfig`, under an exclusive lock on `gliff-virtual-output.lock` in the runtime directory so two creations at once (two gliff processes) cannot take the same monitor.
- `gnome_ipc::power` (new): `wake` (`org.gnome.ScreenSaver.SetActive(false)` if `GetActive`), `IdleInhibitor` (`org.gnome.SessionManager.Inhibit`, flag 8 = idle, on a bus connection of its own so it ends with the process), `is_blanked`, `idle_inhibited`, `blank` (for tests). `create` wakes the screen first and holds the inhibitor until the output is dropped. `Session::monitors()` was added.
- Verified on the GNOME 50.6 VM with `gliff-gnome-output-probe` (software rendering): with the screen really blanked, `create` wakes it and succeeds (the blanked screen made session creation fail before); a 1280x720 virtual monitor reports that size and 60 Hz, scale 1, and delivers frames with pixels of that size; resizes to 1600x900, 1024x768, 1366x768, 1920x1080 and 1280x720 are each reported by Mutter within 65 to 355 ms and each gives frames of exactly that size; two outputs at once get `Meta-0` and `Meta-1` and resize independently; closing one leaves the other; `close` waits until the monitor is gone; idle inhibition is on while an output exists and off after. Keep-awake test: with `idle-delay` set to 30 s (set and restored by hand, back to 300), the screen blanked at 30 s with no output (`gliff-gnome-output-probe idle 50`) and stayed awake for 80 s with one (`hold 80`), with the monitor and session intact and a frame still arriving. `scripts/check.sh` passes (13 unit tests in `gnome-capture`, including one that the fixed-size format names exactly one size).
- The virtual monitor is placed to the right of the real one (`x` 1280 beside a 1280 wide monitor), so the desktop is extended while it exists; it is removed with it. While the output existed the VM's own monitor (`Virtual-1`) once showed 1280x800 at 75 Hz instead of 1920x1080, and went back when the output ended. It did not show in the later runs; the cause is not known and may be the VM's display, not Mutter.
- **Not done, on purpose:** scale. A virtual monitor is always scale 1 here; `set_mode`'s scale argument would need `DisplayConfig.ApplyMonitorsConfig` with every monitor listed, which can disturb the real desktop, so it waits for the wiring step to show whether it is needed. Frame rate: the stream is requested at up to 240 fps and Mutter gives the monitor 60 Hz. Suspend is not inhibited, only idle (flag 4 would add it; not tested because suspending the VM would end the test).
- For the wiring: `Capturer::start` takes its sink at creation, and `create_virtual_output` in the `Backend` trait comes before `start_capture`. `VirtualOutput::create` therefore takes the sink; `GnomeBackend` must forward events to the sink `start_capture` later receives and replay the first `Ready`, or the trait needs a different order. A mirrored (real) monitor needs `power::wake` and an `IdleInhibitor` as well; only a virtual output has them today. The `keep_awake` check for a virtual output is `power::wake` on a bus connection.
- Review: two rounds with Sol (`gpt-6.1-sol`, default effort). Round 1: one `should`, two creations at once could both take the first new monitor; I reproduced it (three processes all reported `Meta-0`) and fixed it with the lock (now `Meta-0`, `Meta-1`, `Meta-2`). Round 2: approved. Sol could not run cargo or the VM, so test results are from this machine.
- Not verified: a locked screen (still the known limit), screen lock enabled, hotplug while an output exists, a second monitor's coordinate space for input, sizes above 1920x1080, a real GPU.

### M3 wiring result: `gnome-backend` and `gliff-server` on GNOME (2026-11-18)

- New crate `crates/gnome-backend`: `GnomeBackend` implements the `Backend` trait from the `gnome-*` crates. `create_virtual_output` makes a `VirtualOutput` (which starts its capture, because Mutter makes the monitor only then) and keeps it under the name the server chose; Mutter's own name (`Meta-0`) is mapped to that name in `list_outputs`. The server's sink arrives later, in `start_capture`, so events before that are kept (the newest of each kind) and replayed (`forward.rs`). A real monitor is captured on `start_capture` (a new session, a wake, an idle inhibitor), `start_input` finds the session and stream of a capture by output name (the server starts the capture first), `start_clipboard` wraps `gnome-clipboard`, `keep_awake` is `power::wake` every 2 s, `set_mode` resizes a virtual output (scale and refresh are ignored), `remove_output` closes it, and `forced_capture_mode` returns shared memory.
- `gliff-backend` gained `Backend::forced_capture_mode` (default `None`). `gliff-server`: `backend.rs` (ours) tries Hyprland and falls back to GNOME, `session.rs` applies the forced mode (four lines, approved), and `Cargo.toml` gains the dependency. Nothing else of the server changed. The server now links PipeWire whatever the desktop, so a package for another distribution needs `pipewire` as a dependency (the Nix package has it).
- Verified on the GNOME 50.6 VM with `serve_probe` (`cargo build -p gnome-backend --example serve_probe`), a headless gliff client that drives a real `gliff-server --stdio` over its pipes: `--headless` (new monitor appears while the session runs, frames at the announced size, the pointer lands where sent, a cursor shape arrives, resizes to 1280x720 and 1600x900 give a new stream config and a frame after it, the server exits cleanly and the monitors are back to the original one) passed 6 of 6 on the final build and 20 of 20 before a last lint-only edit; mirror mode (the VM's own screen) passed 3 of 3; both also pass when the screen was blanked first (the server wakes it); the CPU pipeline was chosen. Clipboard through the server with plain Mutter clients standing in for applications: an application's copy reaches the client as an offer (`text/plain;charset=utf-8`, `text/plain`, `UTF8_STRING`, `STRING`, `TEXT`), and the client's offer is visible to an application, whose paste makes the server ask the client for the item. `scripts/check.sh` passes (`forward.rs` has 2 tests, `cursor.rs` 3 new ones) and `nix build path:.` passes.
- **Bug found by the end-to-end run: no cursor image.** Mutter sends the cursor image once, when the cursor first appears on the stream, and again only when it changes. On a new virtual monitor the cursor arrives from another monitor, and in about a third of headless runs no image ever came (the capture log showed position-only updates), so a viewer would have drawn no cursor. Mirror mode never failed (10 of 10). `CursorTracker` now supplies a plain arrow until a real image arrives; 20 of 20 headless runs then had a cursor shape. Mutter's behaviour is in `meta-screen-cast-virtual-stream-src.c` (`cursor_bitmap_invalid`, `is_cursor_metadata_valid`).
- Not verified: the real viewer (M4); a client answering the server's clipboard request (the probe does not, so the pasted text was empty and the data path is only covered by step 4's checks); files and non-text types; a real GPU (the VM has no render node, so the GPU tier and `--capture dmabuf` could not be exercised: GNOME ignores a requested `dmabuf` and takes shared memory); `--output NAME` for a second monitor; a cursor that leaves the stream (Mutter then sends no cursor data and the viewer keeps the last position); scale above 1; locked screens; key repeat and shortcuts (keycodes use the server's layout, and `SetKeymap` is still refused).
- Review: one round with Sol (`gpt-6.1-sol`, default effort): approved, with one nit that I declined (a second `start_capture` on the same virtual output replaces the first sink without replaying; the server attaches once). Sol could not run cargo or the VM, so test results are from this machine.
- Noise: when a session ends the server logs three `zbus ... Failed to remove match rule: Broken pipe` warnings. They come from signal listeners dropped after the bus connection was closed on purpose; they are harmless.

### M4 part 1 result: the viewer on GNOME (2026-11-18)

- **Audit of every `hypr_*` use in the viewer (`crates/gliff/src`): no seam and no viewer edit needed.** `keymap.rs` calls `hypr_input::watch_keymap`, which reads the keymap from the Wayland `wl_keyboard` of whatever compositor `WAYLAND_DISPLAY` names; `net.rs` calls `hypr_capture::render_node`, which asks the compositor's `zwp_linux_dmabuf` feedback for its render node, with an environment override and a default; `paintable.rs` uses `hypr_ipc::clean_mode` only as arithmetic for integer-scale sampling. All three are generic Wayland and work on GNOME. The `clean_mode` slack is wider than a GNOME server needs (its `align_mode` is the identity), which only affects how eagerly the viewer picks integer scaling.
- **Run on the GNOME 50.6 VM, on its own desktop** (`gliff --connect 127.0.0.1:PORT --headless` against `gliff-server --listen`, built from this tree): the viewer read the local keymap through Wayland (35355 bytes) and sent it, found no VA-API device and fell back to the CPU pipeline by itself, connected, decoded the first frame, the server resized the private screen to the window (1280x712) and adapted the bitrate. The window covered the screen.
- **GNOME asks before it lets the viewer capture shortcuts, every launch.** GDK's `inhibit_system_shortcuts` makes GNOME Shell show a modal "allow shortcuts" dialog; the viewer logged `focused` then `unfocused` 60 ms later and `inhibited=false` until the dialog was answered (Tab, Tab, Enter on the VM gave `inhibited=true`). A second launch asked again, and a `gliff.desktop` file in `~/.local/share/applications` did not stop that (tested, then removed). The viewer sets no application id (`adw::Application::builder().build()`); an id plus a matching desktop file is the likely way for GNOME to remember the answer, but that is an upstream edit in `main.rs` and was not tried. Not checked: the release hotkey, that keys really reach the remote, and the theme; they need someone looking at the window.
- **Scale (`gnome-capture`):** `Capturer::resize(w, h, scale)` and `Options::scale` ask Mutter for a preferred scale with the PipeWire tag `org.gnome.preferred-scale` (what gnome-remote-desktop does). Seen on the VM: a monitor created at 1920x1080 got 1, 1.25, 1.5, 1.75, 2 and 3 as asked; 2560x1440 got 1, 1.25 and 1.5; a scale that leaves under 800x480 logical pixels is refused (1280x720 at 2 stays at 1); a new scale at the same size does nothing (tried a pixel-format change and a pass through a smaller size; neither worked, so that code was removed); changing the size does apply the new scale. **Creating a 2560x1440 monitor at scale 1.75 crashed GNOME Shell (a segfault in Mesa's `libgallium`, software rendering) and ended the VM's session, leaving the login screen.** Because of that `GnomeBackend::set_mode` still ignores the scale: a GNOME virtual output stays at scale 1, so a HiDPI viewer gets a physically sized, small-looking remote UI. Turning it on is one argument in `set_mode`; do it only after testing on real GPU hosts.
- **Bug fixed: a monitor name can be reused.** Mutter reuses `Meta-0` once its monitor is gone, so a new output created while the previous one was still being torn down was not "new" by connector name and `create` failed ("no new monitor appeared"). `Monitor` now carries Mutter's serial (virtual monitors are numbered) and monitors are told apart by connector and serial.
- Review: one round with Sol (`gpt-6.1-sol`, default effort): approved; one nit (the `scales` probe could fail falsely on a same-size step, since no new `Ready` arrives) fixed. Sol could not run cargo or the VM, so test results are from this machine.
- Tools: `gliff-gnome-output-probe scales` and `fresh` (sizes and scales), `gliff-gnome-input-probe key:tab|enter|esc|left|right`.
- **The VM's session was lost during this work** (the Shell crash above): the user session has to be logged in again on the VM; anything that was open in it is gone. Nothing was run on the VM after that, so the final tree was not run there: the last edits (removing the same-size scale code, holding scale back in the backend, the serial in `Monitor`) are covered by `scripts/check.sh` and by earlier runs only (`gliff-gnome-output-probe` 23 of 23 with the serial change and before the removal).
- Not done: keyboard grab and release hotkey check by eye, theme, a real GPU (the VM has no render node), AMD, Intel and NVIDIA hosts, scale on a real GPU, `--output NAME`.

### M6 result: weekly upstream sync (2026-11-19)

- `scripts/sync-upstream.sh` (new, ours) fetches `upstream/main` and, when it has commits we do not, merges it into a new branch `sync/upstream-<short sha>` in a temporary worktree (the checkout it runs in is not touched, nothing is pushed). It prints and writes (`--summary FILE`, and `$GITHUB_OUTPUT` in CI) the upstream commits, the files changed, and whether the change **needs a look at the GNOME side**: upstream touched `crates/hypr-*`, `crates/gliff-server/src/{session,main,clipboard}.rs` or the viewer's `keymap.rs`, `net.rs`, `paintable.rs` (the pattern is `NEEDS_MIRROR` at the top of the script). Exit codes: 0 done or nothing to do or already made, 2 bad use or a dirty tree, 3 conflict. `--dry-run` merges and keeps nothing.
- `.github/workflows/upstream-sync.yml` (new, ours): Mondays 06:17 UTC and on demand, fork only. Job `merge` runs the script, pushes the branch and opens a **draft** pull request in this repository (every `gh` call has `GH_REPO` set, because in a fork `gh` otherwise targets the parent project), with the label "needs GNOME mirror" when the script says so; a conflict fails the job with the file list in its summary (GitHub mails a failed run). Job `test` runs fmt, clippy and the tests in the same Arch container as `ci.yml`, on the merged branch (pull requests made with the workflow token start no other workflows, so it does not rely on `ci.yml`). Job `report` takes the pull request out of draft when the tests pass, and otherwise comments with the run link and leaves it a draft. A repeat run for the same upstream commit does nothing ("already made"), and a PR that was merged makes the next run "up to date".
- Tested: the script, in a throwaway clone with a fake upstream, for: nothing new (exit 0, `changed=false`), a dirty tree (exit 2), a clean merge with no GNOME-side file (`needs_mirror=false`), the same run again (`already-made`), `--dry-run` (no branch left), a change to `crates/hypr-input` (`needs_mirror=true`, file listed), and a conflict in `session.rs` (exit 3, file listed, no branch or worktree left behind). The workflow file passed `actionlint`; the script's only `shellcheck` notes were two false alarms (literal backticks in `sed`), now silenced.
- **Limit and the optional secret:** the workflow's own token cannot push a branch that changes `.github/workflows/*` (a GitHub rule), so when upstream changes a workflow file the script says so (`touches_workflows`, and a line in the PR text) and the `merge` job stops with an error unless a repository secret `SYNC_TOKEN` (a token with the `workflow` scope) exists, which the checkout then uses for the push. Without the secret, those weeks need a manual `git merge upstream/main`. The script also refuses a shallow clone (exit 2), and deletes the branch it made on any failed run.
- Review: one round with Sol (`gpt-6.1-sol`, default effort): one `should` (the workflow-file push limit above, fixed) and two nits (branch left behind when writing the summary fails; a shallow clone), both fixed and re-tested with the cases above plus a workflow-file change, a failed summary write and a shallow clone. Sol could not run Actions, so the workflow is untested as stated below.
- **Not tested:** the workflow itself (nothing here can run GitHub Actions), including `gh pr create/ready/comment`, the label creation, the Arch container build with the two added packages, and `mbx` in the `test` job (copied from `ci.yml`). Before relying on it: push, enable "Allow GitHub Actions to create and approve pull requests", and start it once from the Actions tab (`workflow_dispatch`). With upstream unchanged it should report "up to date".
- Upstream edit (approved): `.github/actions/setup/action.yml` gets `pipewire clang` in its package list, because the existing `ci.yml` could not compile `gnome-capture` without them (see the footprint table). It is untested for the same reason.
- Not done from the plan's original sync text: the `upstream` mirror branch (the script fetches `upstream/main` directly) and the plan's "VM test" job.

### Decisions (2026-11-15)

- No issue is opened upstream; the fork proceeds on its own (MIT licence).
- GNOME support lives on `main` behind the runtime-selected backend; there is no GNOME branch. The `upstream` branch is only an optional mirror of omacom's `main`.

## Rule: keep upstream files in sync-able shape

This fork merges omacom's `main` regularly, so our features must not make that merge harder.

- New code goes in new crates and new files (`gliff-backend`, `gnome-*`, `hypr_backend.rs`, `backend.rs`, `nix/`, `flake.nix`, `docs/gnome-backend.md`).
- An upstream-owned file is edited only when there is no other way, with the smallest change that works (one line where possible). No reformatting, renaming or tidying of upstream code. Upstream docs (`README.md`, `docs/architecture.md`, ...) are not edited; GNOME documentation goes in `docs/gnome-backend.md`.
- Every upstream-owned file we edit is listed here with the reason. A new entry needs a reason too.
- `Cargo.lock` is generated: after a merge, regenerate it instead of hand-merging.
- To see the footprint at any time: `git diff --name-status upstream/main` (`M` rows are upstream files we changed).

Upstream files edited so far:

| File | Change | Why |
|---|---|---|
| `crates/gliff-server/src/session.rs` | calls go through the `Backend` trait | the seam (M2); the one large edit |
| `crates/gliff-server/src/session.rs` | a four-line `let cfg = Config { capture: backend.forced_capture_mode()..., ..cfg }` after the backend is found (approved 2026-11-18, additive only) | GNOME has only shared-memory capture, so its backend forces `--capture shm`; without it the GPU tier is chosen on a machine with a GPU and fails at the first frame |
| `crates/gliff-server/src/clipboard.rs` | `Box<dyn ClipboardControl>` instead of `Clipboard` | the seam (M2) |
| `crates/gliff-server/src/main.rs` | two `mod` lines | register our new modules |
| `Cargo.toml`, `crates/gliff-server/Cargo.toml` | member, dependency lines (one member line per new crate: `gliff-backend`, `gnome-ipc`, `gnome-capture`, `gnome-input`, `gnome-clipboard`, `gnome-backend`; the `gnome-backend` dependency line in `gliff-server`) | register our new crates |
| `crates/hypr-capture/src/lib.rs` | two added lines: `mod external;` and `pub use external::FrameSource;` (approved 2026-11-17, additive only) | `CapturedFrame` and `CaptureBuffer` have private fields, so a GNOME frame can only be built inside this crate. The constructors live in the new file `external.rs`, a child module that can see those fields; no existing line changed. |
| `.github/actions/setup/action.yml` | two words added to the pacman package list: `pipewire clang` (approved 2026-11-19, additive only) | CI builds in an Arch container; `gnome-capture` needs libpipewire, and its bindgen needs clang, so CI could not compile the workspace without them |
| `Cargo.lock` | generated | follows the lines above |

Still to come, and the places most likely to need an upstream edit: the viewer (`crates/gliff/src/keymap.rs`, `net.rs`, `paintable.rs`, M4) and `gliff-probe` (M3 probes go in a new binary or module first).
