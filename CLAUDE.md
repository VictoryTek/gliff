# gliff

Run `scripts/check.sh` before every push or PR. It runs the CI checks
(rustfmt, clippy with warnings as errors, the unit tests); a clippy warning
is a CI failure. `scripts/e2e.sh` covers the hardware paths and needs a
Hyprland session; the GPU cases need Vulkan compute and a VA-API H.264
codec. Run it before merging changes to the video pipeline. See
`docs/architecture.md` for the design and `docs/development.md` for the
build and the testing tools.

## Agent skills

### Issue tracker

GitHub Issues in `VictoryTek/vexdesk` (this fork) via `gh`; never file into upstream `omacom/gliff`. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.
