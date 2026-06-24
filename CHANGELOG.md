# Changelog

All notable changes to muxel are documented here. This project adheres to
[Semantic Versioning](https://semver.org).

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

[0.0.2]: https://github.com/ProjectHax/muxel/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/ProjectHax/muxel/releases/tag/v0.0.1
