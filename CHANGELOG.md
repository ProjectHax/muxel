# Changelog

All notable changes to muxel are documented here. This project adheres to
[Semantic Versioning](https://semver.org).

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
