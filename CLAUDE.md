# CLAUDE.md

## What muxel is

muxel is a GPUI-based multi-agent terminal multiplexer (built on Zed's GPUI +
gpui-component): a tiled, tabbed workspace for running coding agents (Claude,
opencode, Amp, …) and shells side by side, with first-class git worktrees, agent
status tracking, and notifications. See `FEATURES.md` for the full feature list.

It's a native desktop GUI app (not a TUI) — each pane embeds a real terminal
emulator running a PTY child process.

## Architecture

Cargo workspace, four crates (depend downward only):

- `crates/muxel-core` — the **pure** domain model: the pane layout tree
  (`PaneNode`), agent presets (`AgentPreset`), worktree naming, tmux arg helpers,
  and the persisted `Workspace` / `Project` / `Instance` / `Settings` types. No UI,
  no I/O — fully unit-tested. Most logic that *can* live here, should.
- `crates/muxel-store` — persistence: workspaces, `workspace.json`, and settings,
  loaded/saved under the platform config/data dirs (XDG on Linux).
- `crates/muxel-terminal` — the PTY child + `alacritty_terminal` emulator
  (`TerminalSession`, `session.rs`), its GPUI view + custom paint element
  (`TerminalView`/`TerminalElement`, `view.rs`/`element.rs`), and agent status
  detection (`AgentStatus`, `classify`).
- `crates/muxel` — the GPUI application (`app.rs`, the large `MuxelApp` entity):
  window, sidebar, toolbar, pane rendering, settings UI (`settings_view.rs`),
  editor (`editor.rs`), git/tmux side effects (`integrations.rs`), and embedded
  assets (icons, themes) wired up in `main.rs`.

`ios/` — a separate **Swift/SwiftUI iOS companion app** (remote-only; peers with
desktop muxel over SSH/tmux). It is **not** a cargo crate — the `cargo` gate does
not build it. It re-implements a versioned slice of `muxel-core`'s *remote protocol*
(tmux session naming, `RemoteLayout` `.muxel/workspace.json`, `classify`/markers); if
you change those Rust contracts, update the matching Swift port. See `ios/README.md`.

### Key concepts

- **Pane tree** — a project's layout is `Option<PaneNode>`: `Leaf(LeafData {
  tabs: Vec<Uuid>, active })` (a tab group) or `Split { direction, sizes,
  children }`. Mutations (`split`, `add_tab`, `move_tab_to`, `move_into_split`,
  `move_pane_beside`, `remove`, …) are pure functions in `pane.rs` with tests.
- **Instance vs terminal** — an `Instance` is persisted metadata (id, program,
  worktree, …); the live `TerminalView`/PTY is owned by the app, keyed by instance
  id in `MuxelApp.terminals`. Runtime-only state (e.g. `AgentStatus`) is **not**
  persisted.
- **Back-compat persistence** — every new field on a persisted struct gets
  `#[serde(default)]` so older `workspace.json` / settings still load. Bump
  `PRESET_SEED_VERSION` when adding a built-in preset so existing workspaces merge it.
- **GPUI patterns** — render builds an element tree with `div()…`; handlers use
  `cx.listener(|this, ev, window, cx| …)`; the terminal is a custom `Element`
  (manual layout/paint). Mouse `on_drag_move` fires for all listeners — guard with
  `ev.bounds.contains(&ev.event.position)`.
- **Native webview overlay rule** — browser panes (`browser.rs`) are native child
  windows that draw ABOVE all gpui content. `MuxelApp::any_overlay_open` must list
  **every** modal/palette/menu/drag flag; when you add a new overlay, add its flag
  there or the webview will float over it.

## Building & testing a feature

cargo is installed via rustup (`~/.cargo/bin/cargo`).

Run this gate after any change, and fix everything before considering it done:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # warnings are errors
cargo test --workspace                                   # unit/integration tests
cargo build -p muxel                                     # the GUI binary
```

- **Where tests go** — pure logic in `muxel-core` (pane ops, worktree, presets,
  settings seeding) is unit-tested there; prefer extracting decision logic into
  pure, testable functions (e.g. `classify` in `view.rs`). `muxel-terminal` has
  integration-style tests that spawn a real PTY.
- **gpui `#[test]` gotcha** — in a file that does `use gpui::*`, `#[test]`
  resolves to gpui's attribute macro. In test modules, import only what you need
  (`use super::{Foo, bar}`) instead of `use super::*` so the built-in `#[test]` is
  used.

### Verifying GUI changes

Unit tests can't see the UI, so for anything visual:

- **Smoke test** (does it launch without panicking?) — run the binary against an
  isolated workspace with a timeout; exit code `124` means it stayed up (good):

  ```sh
  s=$(mktemp -d); XDG_CONFIG_HOME="$s/config" XDG_DATA_HOME="$s/data" \
    timeout 5s ./target/debug/muxel >/dev/null 2>&1; echo "exit $? (124=ok)"; rm -rf "$s"
  ```

- **Interactive** — `scripts/dev.sh` runs muxel against an isolated sandbox
  (`.muxel-dev/`) so testing never touches the real workspace. A fresh workspace shows
  the first-run welcome dialog; accepted-terms / window geometry / layout live
  under the sandbox's data dir.
- State each visual behavior that needs a human's eyes — the harness can build and
  smoke-test, but can't see colors/layout.

## Features doc

`FEATURES.md` (repo root) is the canonical catalogue of user-facing features.
**When you add or change a user-facing feature, update `FEATURES.md` in the same
change** — add or edit its entry under the right category (sentence or two each;
add a new `##` category if none fits).

## Git commits

- Never put personal email, phone, home paths, API keys, or session URLs in commits.
- Do not append `Claude-Session:`, `Co-Authored-By:`, or other attribution trailers unless I explicitly ask.
- Before committing, run `git config user.email` and confirm it is the project's intended commit identity (not a personal address).
- Commit messages: subject + body only — no tooling metadata or session links.
