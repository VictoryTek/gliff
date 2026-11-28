# VexDesk

VexDesk is a fork of gliff that adds a GNOME server and a native Windows viewer. This glossary covers the words the viewers and their docs use.

## Language

### Connecting

**Machine**:
A remote computer the viewer connects to, named by its ssh address. Each **Machine** has a tab.
_Avoid_: Host (in the UI), server, remote

**Session**:
One live connection to a **Machine**, from connect to stop. A dropped link ends the connection, and reconnecting brings the **Session** back.
_Avoid_: Connection (for the whole lifetime), stream

**Viewer**:
The program that shows a **Machine**'s screen and sends it input: `gliff` on Linux, `vexdesk` on Windows.
_Avoid_: Client app

### Screens

**Mirror**:
A **Session** that shows one of the **Machine**'s real screens, named or the default.
_Avoid_: Monitor mode

**Private screen**:
A **Session** that shows an extra screen created on the **Machine** to match the **Viewer**'s window, which no one at the **Machine** sees.
_Avoid_: Headless (except as the server flag `--headless`), virtual monitor

### Fork upkeep

**Upstream-owned file**:
A file that also exists in `omacom/gliff`. Every upstream sync can conflict on it.
_Avoid_: Core file, shared file

**Fork-owned file**:
A file that exists only in VexDesk. Upstream syncs never touch it.
