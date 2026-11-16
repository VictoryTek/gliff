# Plan: a GNOME-capable Gliff fork to replace Sunshine/Moonlight in vexos

Tier 3 (new dependency, new repo, many files, replaces a remote-access feature). Nothing is built until you approve this plan.

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

### M2 result: the seam (2026-11-15)

- New crate `gliff-backend`: the `Backend` trait (`list_outputs`, `create_virtual_output`, `set_mode`, `remove_output`, `align_mode`, `keep_awake`, `start_capture`, `start_input`, `start_clipboard`), the handle traits (`CaptureControl`, `InputControl`, `ClipboardControl`) and neutral `OutputInfo` / `CreateError`. Frames, input commands and clipboard events keep the `hypr-capture` / `hypr-input` types, so a GNOME backend produces the same `CapturedFrame`.
- `gliff-server` changes: `session.rs` and `clipboard.rs` call the trait instead of `hypr_ipc` / `Capturer` / `Input` / `Clipboard`; `main.rs` only gains two `mod` lines. The Hyprland calls moved unchanged into `src/hypr_backend.rs`; `src/backend.rs::discover` picks the backend (Hyprland only for now).
- Rulings: (1) `keep_awake` returns the periodic check closure and `session.rs` keeps the existing `KeepAwake::poll` thread, to keep the diff small. (2) `align_mode` was added to the trait because Hyprland's scale-divisibility rule (`clean_mode`) is compositor specific. (3) `discover` is a free function in `gliff-server` because the trait crate cannot depend on its implementations. (4) `Config` keeps its `Hyprland`-typed `target` and `render_node` for now, so `main.rs` is untouched.
- Verification: `scripts/check.sh` (rustfmt, clippy `-D warnings`, 276 unit tests) passes. `scripts/e2e.sh` was NOT run (no Hyprland session available); by decision it is assumed to still pass.

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
| `crates/gliff-server/src/clipboard.rs` | `Box<dyn ClipboardControl>` instead of `Clipboard` | the seam (M2) |
| `crates/gliff-server/src/main.rs` | two `mod` lines | register our new modules |
| `Cargo.toml`, `crates/gliff-server/Cargo.toml` | member, dependency lines | register our new crate |
| `Cargo.lock` | generated | follows the lines above |

Still to come, and the places most likely to need an upstream edit: the viewer (`crates/gliff/src/keymap.rs`, `net.rs`, `paintable.rs`, M4) and `gliff-probe` (M3 probes go in a new binary or module first).
