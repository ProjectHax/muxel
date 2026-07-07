# Changelog

All notable changes to muxel are documented here. This project adheres to
[Semantic Versioning](https://semver.org).

## [0.0.8] — 2026-07-06

### Added
- **Multi-monitor project windows** — right-click a project → **Open on display N**
  to give it its own full muxel window (sidebar + toolbar + panes) on that monitor;
  switch projects and panes there just like the main window. One window per project:
  selecting a project already open elsewhere **raises** its window instead of stealing
  it. Each window's monitor and exact position/size are saved **in the workspace**, so
  reopening restores every window where it was — dragging a window to another monitor
  updates its pin, and a disconnected monitor keeps the pin for when it returns.
  **Bring back to main window** (or closing the window) returns the project.
- **Built-in browser** — preview links an agent prints, or a locally hosted dev site,
  without leaving muxel and without bundling Chromium: it uses the OS webview (WKWebView
  on macOS, WebView2 on Windows, WebKitGTK on Linux), so it stays light on disk and
  memory. On macOS/Windows a ctrl+clicked URL opens as an in-pane browser (address bar,
  Back, Reload; the URL persists and restores with the workspace); on Linux it opens as
  a separate muxel-managed browser window, falling back to the system browser if WebKit
  isn't installed. Toggle it in **Settings → Behavior** (default on; off routes every
  link to the system browser).
- **Ctrl+click terminal links** — ctrl+click (⌘ on macOS) a URL or file path an agent
  printed to open it; ctrl+hover underlines the target and shows a pointing-hand cursor.
  OSC 8 hyperlinks are honored, and file paths (`./src/x.rs:42`, `~/…`, absolute) resolve
  against the pane's working directory and open in your system default app.
- **Fullscreen mode** — `F11` (rebindable) toggles OS fullscreen with the sidebar fully
  hidden; a floating edge pill brings it back without leaving fullscreen, and `F11` again
  restores your previous sidebar state.
- **Reconnect a dropped SSH project** — when a remote project's connection fails, the
  pane area shows the error with **Reconnect** (re-runs the connect, re-syncs the layout,
  respawns panes) and **Scan for projects** (opens the wizard on that host and scans it);
  both are also in the project's right-click menu.
- **Quit-time tmux cleanup** — when muxel-launched tmux sessions exist, the quit dialog
  offers two opt-in checkboxes (off by default), **Also kill local tmux sessions** and
  **Also kill remote tmux sessions**; the kills are fire-and-forget, so quitting never
  waits on a slow host.

### Changed
- **Panes never vanish on an abnormal exit** — auto-close-on-exit now requires a *clean*
  exit (code 0). A crash, unknown exit code, or transient PTY read error keeps the pane
  as a tombstone: the final screen stays under a banner with the exit code, with a feed
  error and a desktop notification when unattended, and Restart relaunches in place.
- **Closing a pane always kills its tmux session** — local or remote. A *dropped* SSH
  connection exits abnormally and tombstones instead of auto-closing, so its remote
  session stays alive for reconnect; only a deliberate close tears tmux down.
- **Renamed agents show only their custom name** in the sidebar (previously the live
  agent title was appended after it).

### Fixed
- **Reliable child reaping** — the PTY reader retries `EINTR` instead of declaring the
  child dead, carries real read errors into the exit event, and always reaps the child
  (fixing permanent zombie processes and leaked PTY file descriptors). Every exit and
  close is recorded in a new durable event log (`muxel.log`, 1 MB rotation), since the
  GUI often runs with stderr discarded.

### iOS companion app (distributed via TestFlight / App Store, not in these downloads)
- App Store submission prep, and the Live Activity now ends when the app is fully closed.

## [0.0.7] — 2026-07-02

### Added
- **Self-maintaining project memory** — the flat `.muxel/MEMORY.md` becomes a
  structured, greppable memory muxel maintains automatically: one `## Title` section
  per fact with a machine meta line, ordered most-relevant-first, with un-pinned
  entries auto-purged after 30 days and capped (least-recently-used eviction). A
  legacy flat file is imported, never lost. The project row's memory button opens a
  docked, resizable side panel (like the file browser) — search, pin/unpin, add,
  delete, open raw — with its width persisted per workspace. Local and remote (SSH)
  projects; local agents also get `MUXEL_MEMORY_FILE` / `MUXEL_MEMORY_DIR`.
- **Reusable SSH login identities** — a named credential (user + auth + key/password)
  defined once in Settings → Identities and referenced by many hosts, so a shared
  login is entered, stored, and rotated in one place. The host editor gains a
  credentials picker; deleting an identity detaches its hosts.
- **Snippets** — reusable text typed into an already-running pane (unlike runners,
  which spawn a new agent). Send one from the toolbar Snippets dropdown, the command
  palette, or a terminal tab's right-click menu; each snippet chooses whether it
  auto-submits. Managed in Settings → Snippets.
- **Developer console** — `F12` opens a popped-out error log (opt-in in settings).
- **tmux scrollback** — launching a tmux session now turns on tmux mouse mode, so the
  scroll wheel reaches tmux copy-mode history instead of only the visible screen
  (remote, remote-tmux, and local tmux-mode projects).
- **Scan for remote projects** — the new-remote-project wizard can scan a host for
  `.muxel/workspace.json` markers and list the found roots; click one to fill the path.
- **Local ↔ mobile peering** — a local project with tmux enabled mirrors its layout to
  `<root>/.muxel/workspace.json` (newer-wins), so the new iOS companion can SSH into
  the machine and drive the same panes.
- **"Ollama Code" preset** — runs a coding agent backed by an Ollama model via
  `ollama launch <agent> --model <model>` (seeded as opencode + `glm-5.2:cloud`;
  editable in the preset's args).
- **Reorder projects** — right-click a project → **Move up** / **Move down**
  (alongside drag-to-reorder).
- **More keyboard shortcuts** — `Ctrl+Shift+G` toggles the "new agents get a git
  worktree" switch, and `Ctrl+Alt+1`–`Ctrl+Alt+9` open a new pane running the Nth
  agent preset. Both rebindable.
- **Open project memory anytime** — right-click a project → **Open shared memory** (or
  a command-palette entry), even when memory injection is off (the file is created on
  demand).
- **SSH compression** — an opt-in per-host `Compression=yes`, worth it on slow links.

### Changed
- **Single instance per workspace** — the instance lock is now per-workspace, so two
  muxel windows can run side by side on different workspaces; opening a workspace
  another window already holds is refused in the selector with an inline "in use" note.
- **Notifications while focused** — a desktop notification is no longer shown while
  muxel's own window is focused (the in-app NOTIFICATIONS feed still records it).
- **Untouched shell panes close without a prompt** — closing a default-shell pane
  idle at its prompt (no foreground command, only tab) skips the confirmation; a
  running command, another tab, or an agent still asks.
- **SSH defaults** — every ssh invocation now sets `ConnectTimeout=15` (an unreachable
  host fails promptly instead of hanging ~2 min) and `IdentitiesOnly=yes` when an
  explicit key file is set (so ssh doesn't exhaust the server's `MaxAuthTries` before
  the right key). Both overridable per host.
- **Changed host key** — a changed remote host key raises an actionable dialog
  (stored vs presented SHA256 fingerprints side by side) with a destructive
  **Trust new key** that clears the stale entry and retries the operation, instead of
  a silent SSH refusal.
- **Quote-aware preset args** — `--flag "two words"` now stays a single argument;
  unbalanced quotes degrade to space-splitting with a warning.
- **Active-tab styling** — the active tab is marked with a thin theme-accent underline
  in the pane header, plus minor sidebar spacing polish.

### Fixed
- **Agent detection on Linux GUI / AppImage launches** — a desktop-entry or AppImage
  launch inherits a minimal `PATH`; muxel now reconstructs `~/.local/bin`,
  `~/.opencode/bin` (opencode's installer default), Linuxbrew, and friends, so agents
  are detected and spawnable the same as from a terminal (matching the existing macOS
  fix).
- **Spurious "done" notifications** — marker-less terminals (plain shells, agents with
  no markers) no longer flip to "done" on incidental output such as a focus-change
  repaint when you click the pane; they reach "done" only from the bell or process
  exit.
- **No crash when the fallback shell fails to spawn** — the pane shows the failure in
  place (Restart retries) and the error lands in the notifications feed, instead of
  panicking.
- **Silent save failures surfaced** — a failed local save (workspace, settings,
  memory, layout backup) is now reported in the notifications feed + dev console
  rather than lost.
- **Terminal cursor** — muxel no longer paints the terminal cursor while the app has
  hidden it.

### iOS companion app (new — distributed via TestFlight / App Store, not in these downloads)
- A native **SwiftUI iOS app** that connects over SSH and **peers with desktop muxel**
  — it reads/writes the same per-project `.muxel/workspace.json` and uses muxel's exact
  tmux session naming, so the phone drives the same sessions. Remote-only.
- A live **SSH PTY terminal** (SwiftTerm) with a themeable identity, an accessory key
  row (Esc / Ctrl / Tab / arrows / paste), pinch-to-zoom, OSC-52 copy, and tmux-backed
  scrollback; a collapsible, resizable sidebar of hosts/projects; status badges;
  on-device background **notifications** plus a **Live Activity** status bar (Lock
  Screen + Dynamic Island).
- **Security**: SSH **key + password** auth and shared login identities, secrets in the
  **Keychain**, **trust-on-first-use** host keys with a changed-key prompt, jump hosts,
  and an optional App Lock. Licensed GPL-3.0 with an App Store (GPLv3 §7) exception.

## [0.0.6] — 2026-06-26

### Added
- **System tray** — a "Minimize to the system tray on close" setting: closing the
  window iconifies muxel to a tray icon instead of quitting. The tray menu lists
  every agent with its live status and the most recent notifications; clicking an
  entry restores muxel and focuses that project + pane, and "Quit" exits. Linux
  (StatusNotifierItem), Windows, and macOS.
- **Jump to project** — `Ctrl+1`–`Ctrl+9` switches to the Nth project in the
  sidebar (rebindable; shown in the `Ctrl+Shift+/` cheat sheet).

### Changed
- **Working-tree diff opens as a tab** — a pane's "View changes" (and the project /
  worktree diffs) now opens the read-only diff as a new tab in the pane it's
  diffing, instead of splitting off a separate pane.
- **Agent status detection** — Claude's working state is read from its
  "esc to interrupt" status line (reliable through long "Computing…" phases)
  rather than an output-activity timer, and a finished turn shows **done** (held
  until you attend the pane) even when the agent never rang the bell.

### Fixed
- **Session resume** — a deleted or expired session no longer hangs the pane. muxel
  checks whether the session still exists on disk before resuming (and recovers
  from a "No conversation found" hang or a non-zero exit), starting a fresh session
  instead.

## [0.0.5] — 2026-06-26

### Added
- **Git diff panel** — a toolbar toggle opens a right-side panel listing the
  active project's changed files, color-coded by status. Click a file to open its
  diff in a dedicated window with a remembered Split / Unified toggle: Unified is
  a colored diff whose text is selectable + copyable, Split is a side-by-side view
  (green/red, line numbers). Per-file Stage / Unstage / Discard / Open, plus
  commit-all with a message. Works for local and remote (SSH) projects.
- **Worktrees tab** — alongside the changed files, list a project's git
  worktrees, browse each one's changes, merge a worktree into a chosen branch, and
  delete a worktree (local or remote) once no instance is using it.
- **Terminal mouse modes** — three global copy/paste behaviors for terminal
  panes: copy/paste keys, a right-click menu, or copy-on-select.
- **PowerShell and Cmd presets on Windows** — the shell launcher offers
  PowerShell and Cmd as first-class options.
- **Drag-install on macOS** — the DMG now includes an Applications shortcut.

### Changed
- **Agent recovery** — when an instance exits with an invalid or failed session,
  muxel automatically respawns a fresh instance of the same type instead of
  leaving the pane dead.

### Fixed
- **Notification click** — clicking a muxel desktop notification now raises the
  existing muxel window via its app association, instead of popping a second
  "muxel is ready" notification.

## [0.0.4] — 2026-06-25

### Added
- **Localization** — the UI is translated into 24 languages, auto-detected from
  your OS locale on startup with a Settings → Appearance → Language picker that
  switches live (no restart). Untranslated strings fall back to English; a
  `scripts/translate.py` generator (re)builds the catalogs.
- **Session resume** — agents that support it (Claude) resume their previous
  conversation when a pane is relaunched, via a stable per-pane session id.
- **Single instance per workspace** — opening a workspace already open in another
  muxel window shows an "already open" screen instead of clobbering its shared
  layout and settings.

### Changed
- **Profiles are now "workspaces"** — renamed throughout the UI, with the active
  workspace name shown in the title bar. Existing `profiles.json` / `profiles/`
  migrate automatically.
- **Agent launcher button** — shows just the agent's name (dropped the "New:"
  prefix).
- **Editor close** — closing an editor with no unsaved changes no longer prompts.

### Fixed
- **Session resume reliability** — a relaunched agent resumes its own session
  unconditionally instead of probing the (possibly not-yet-flushed) on-disk session
  file, fixing the intermittent "didn't resume" / "Session ID already in use".

## [0.0.3] — 2026-06-25

### Added
- **Quit shortcuts** — `Cmd+Q` / `Ctrl+Q` quits muxel; a second `Cmd+Q` quits even
  while the close-confirm dialog is up.
- **macOS clipboard** — `Cmd+C` / `Cmd+V` copy and paste in terminal panes.

### Changed
- **Update dialog** — shows the release's full changelog rendered as markdown, in a
  bigger, resizable window.

### Fixed
- **Modal input passthrough** — clicks, scroll, and hover no longer fall through a
  dialog's backdrop to the panes behind it.
- **macOS agent detection** — reconstruct the GUI-launch `PATH` (Homebrew,
  `~/.local/bin`) so agents are found when muxel is launched from Finder/Dock, not
  only from a terminal.
- **Windows agent detection** — find agents installed as `.exe` / `.cmd` / `.bat`
  via `PATHEXT` (e.g. npm-shimmed `claude`).
- **Windows console flashes** — suppress the cmd-window flash from background
  `git` / `ssh` calls (`CREATE_NO_WINDOW`).
- **Windows stack overflow** — raise the main-thread stack to 8 MiB, fixing a crash
  when launching an agent.

## [0.0.2] — 2026-06-24

### Added
- **`Ctrl+T` / `Ctrl+Shift+T` clone the active pane** — a new tab or split spawns a
  fresh instance of whatever agent you're on (a shell from a shell, grok from grok)
  instead of the toolbar's "new agent" selector. Falls back to the selector if the
  pane has no matching preset.
- **"Git diff" in the project right-click menu** — open a project's working-tree
  diff directly.
- **Review-and-commit by file** — stage and commit individual changed files.

### Changed
- **Shell panes show their working directory** — the sidebar, tabs, and pop-out
  window title now show a shell's cwd instead of the raw `user@host:dir`. Agent
  titles are unchanged.
- **Clicking a desktop notification jumps to its pane** — raises muxel and switches
  to the project + pane that fired it.
- **Selecting a pane from the sidebar focuses its terminal** — you can type
  immediately instead of the pane only highlighting.
- **Richer git notifications** — push/pull/fetch/stash report the result detail, not
  just success/failure.
- **The file browser follows a newly created project.**
- **The main window has an explicit title** — fixes "Unknown" in the window switcher
  / taskbar (notably under the AppImage).

### Fixed
- **AppImage no longer poisons spawned build tools** — the AppImage runtime's env
  leakage (`APPDIR`/`APPIMAGE`/a self-referential `MAKE`/mount-path
  `PATH`/`LD_LIBRARY_PATH`) is stripped from shells muxel spawns, so cmake/make and
  friends build normally instead of relaunching muxel.
- **SSH connection test distinguishes a wrong remote path** from a generic
  connection failure.
- **Terminal no longer garbles during pane drags** — resize is deferred while a pane
  or divider is being dragged.
- **Remote-project button no longer clips** in a narrow sidebar.

### Project
- "Sponsor" button via GitHub Sponsors + Stripe (ProjectHax org).
- New README banner.

## [0.0.1] — 2026-06-24

Initial public release.

[0.0.3]: https://github.com/ProjectHax/muxel/compare/v0.0.2...v0.0.3
[0.0.2]: https://github.com/ProjectHax/muxel/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/ProjectHax/muxel/releases/tag/v0.0.1
