# The Windows viewer lives in fork-owned files and needs no protocol change

VexDesk syncs with `omacom/gliff`, so every edit to an upstream-owned file can conflict on a later merge. The Windows viewer's code therefore goes only in fork-owned places: `crates/vexdesk/`, `packaging/windows/` and `.github/workflows/windows.yml`. An upstream-owned file changes only for a small portability fix in a crate both platforms share (`gliff-proto`, `gliff-transport`, `gliff-sw`). Each such fix is logged in the PLAN.md table of patches to upstream files, and only there. No upstream pull request is opened or drafted.

## Consequences

- No protocol changes. The keyboard layout is sent as XKB text that only names the layout (for example `pc+de`), which the server's xkbcommon completes from its own XKB data. A new layout-name field in `Hello` would have touched `gliff-proto` and both servers.
- ssh passwords are asked for inside the app by making `vexdesk` its own `SSH_ASKPASS` helper, set in the environment of the `ssh` it starts, so `gliff-transport/src/ssh.rs` stays as upstream wrote it.
