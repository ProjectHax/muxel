# muxel — iOS companion app

A native SwiftUI app that connects over SSH to a remote host, attaches to the
**same tmux sessions** muxel desktop uses, lets you view panes/tabs and launch new
agent instances, and surfaces blocked/finished notifications via on-device polling.

It is **remote-only** (no local terminals) and a **peer of desktop muxel**: it
reads/writes the same per-project `.muxel/workspace.json` and uses muxel's exact
tmux session naming, so the phone and desktop see the same sessions.

This app is **independent of the Rust cargo workspace** — `cargo` does not build it,
and the CI gate (`cargo fmt/clippy/test/build`) does not cover it. It re-implements
(ports) a small, versioned slice of `muxel-core`'s *remote protocol* and must track
it (see the contract below).

## Build

Requires macOS + Xcode 15+ and an Apple Developer account (for background
entitlements + TestFlight). The app deploys to **iOS 17+** (Citadel 0.12+ requires
it).

```sh
brew install xcodegen
cd ios
xcodegen generate          # generates Muxel.xcodeproj from project.yml
open Muxel.xcodeproj
```

In Xcode: set your signing team, then run on a **real device** (Keychain,
background refresh, and notifications don't behave correctly in the Simulator).

Dependencies (Swift Package Manager, resolved by Xcode; tracked at their latest
releases via `from:` in `project.yml`):
- [Citadel](https://github.com/orlandos-nl/Citadel) — SSH client (SwiftNIO SSH);
  0.12+, which sets the iOS 17 floor.
- [SwiftTerm](https://github.com/migueldeicaza/SwiftTerm) — terminal emulator for the
  live PTY terminal (1.13+).

> The terminal is a **live SSH PTY** (`LiveTerminalView` + SwiftTerm), fed by a
> Citadel `withPTY` channel that runs `exec tmux attach` / `new-session -A`
> **attached** (`TerminalPaneView` resolves which). This replaced the earlier polled
> `capture-pane` viewer — interactive TUI agents (claude) crash if they initialize
> in a *detached* session, so the pane must be a real attached terminal, exactly like
> desktop's `ssh -t`. Live terminals are owned by an app-level `TerminalStore`
> (`AppState.terminals`), keyed by instance id — **not** by the SwiftUI view — so a
> pane stays connected as you navigate away and back (the iOS analogue of desktop's
> `MuxelApp.terminals`). A session is torn down only by Close instance, host delete,
> or quitting the app; it auto-reconnects on foreground if a background suspend
> dropped the transport. The PTY is opened at SwiftTerm's **actual** grid size (the
> attach is deferred until the view reports its first size) rather than a fixed 80×24
> that a later resize had to correct — opening at the wrong size made tmux position
> the cursor for an 80-column screen inside the phone's narrower grid, garbling
> full-width TUI output. It also switches on SwiftTerm's **Metal GPU renderer**
> (`setUseMetal(true)`, available in 1.13+) once the view is in a window: the default
> CoreGraphics path composites cached row "stripes" and left stale glyphs stacked on
> cell redraws (text drawn over itself); the Metal path rasterizes the current grid
> each frame. **Scrollback:** the panes are full-screen (alternate screen) so there's
> no terminal-native scrollback — history lives in tmux. On attach we enable tmux
> `mouse on`, and a one-finger **vertical** swipe sends mouse-wheel events to scroll it,
> with momentum on lift (`TerminalSession`). The scroll gesture only begins for
> vertical pans (so the edge back-swipe passes through) and SwiftTerm's pans wait for
> it to fail, so a swipe scrolls rather than selecting. The background poll
> (`PollService`) still drives status badges +
> notifications. Password auth, exec, and **SSH key auth** (ed25519 / RSA, with
> passphrase — ECDSA is detected but not parseable by Citadel) are implemented in
> `CitadelSSHConnection.swift`. Remaining spikes: the **TOFU host-key validator**
> (still `.acceptAnything()`) and **jump-host** support.

## The muxel remote protocol (keep in sync with `muxel-core`)

The Swift code mirrors these stable contracts. If `muxel-core` changes them, update
the matching Swift port and the `RemoteLayout` version handling.

| Concern | Rust source | Swift port |
|---|---|---|
| tmux session name `muxel_<host-slug>_<uuid8>` | `crates/muxel-core/src/tmux.rs` | `Tmux/TmuxSession.swift` |
| `tmux new-session -A -d -s … -c … -- prog args` | `tmux.rs` | `Tmux/TmuxCommands.swift` |
| `RemoteLayout` v1 JSON (`.muxel/workspace.json`) | `crates/muxel-core/src/lib.rs` | `Models/RemoteLayout.swift` |
| Pane tree (`leaf`/`split` tagged enum, legacy `instance` leaf) | `crates/muxel-core/src/pane.rs` | `Models/PaneNode.swift` |
| `Instance` / `Worktree` / `InjectionMode` | `lib.rs`, `worktree.rs`, `agent.rs` | `Models/*.swift` |
| `.muxel/` read + write (prep one-liner, backup, gitignore) | `crates/muxel/src/integrations.rs` | `Interop/RemoteLayoutStore.swift` |
| Status `classify` + `latch_done` | `crates/muxel-terminal/src/view.rs` | `Status/AgentStatus.swift` |
| Per-agent `default_markers` | `crates/muxel/src/app.rs` | `Status/Markers.swift` |

### Status without an attached terminal (the polling enabler)

Background polling can't hold a live PTY, so per muxel tmux session we read three
tmux format vars in one `display-message` round-trip and feed them into the ported
`classify`:

- exited → `#{pane_dead}`
- bell → `#{window_bell_flag}`
- idle seconds → `now - #{window_activity}` (the `idle_for` equivalent)

We deliberately do **not** scrape the screen with `capture-pane -p` (the would-be
`visible_text` marker input). Some tmux builds crash the whole server on that
command — a stock AlmaLinux/RHEL 10 package (`tmux 3.3a-…el10`) reliably segfaults
the server on `capture-pane`, taking every session down — and desktop muxel never
calls it (it reads markers from its live PTY grid). The cost is marker-based
`working`/`blocked` detection while *backgrounded*: with no screen text, status
degrades to `done` (exit/bell) and `working`/`idle` (recent activity), and
marker-less panes never latch. The attached `LiveTerminalView` still shows full
agent state when a pane is open. (`TmuxCommands.capturePane` and `defaultMarkers`
remain as tested protocol-port builders for a future live-grid status path.)

### Auth differences vs desktop

iOS has no ssh-agent and no `ssh`/`sshpass`/ControlMaster CLI. So:
- Supported auth: **Key** (imported private key in Keychain, optional passphrase) and
  **Password** (Keychain). `SshAuth::Agent` is not applicable.
- Multiplexing = **one SSH connection per host with multiple channels** (one exec
  channel for tmux control, one PTY channel per attached pane) — the functional
  equivalent of ControlMaster.
- Host keys use **TOFU**: the fingerprint is stored on first connect and a change is
  flagged (mirrors muxel's `accept-new`).

## Where things are stored

- **On device** (this app's own store; the desktop's config lives on the desktop):
  host library + project list as Codable JSON in the app-support dir; SSH
  passwords/keys in the **Keychain** (`kSecAttrAccessibleAfterFirstUnlock`).
- **On the remote host** (shared with desktop muxel): per-project layout at
  `<remote_root>/.muxel/workspace.json` (`RemoteLayout` v1).

## Status / roadmap

v1 (lean MVP): connect, collapsible sidebar of hosts/projects, add hosts/projects
(by path or by **scanning the host** for `.muxel/` markers — `ProjectDiscovery`),
**live SSH PTY terminal** for every pane (SwiftTerm + Citadel `withPTY`, attached so
TUI agents work), panes/tabs resolved by uuid8 suffix (so desktop-created panes show
up too), launch instances (long-press a tab to rename / duplicate / close, with a
close confirmation), live status badges + a running-count on the selected
project, **Test connection** to verify a saved credential, on-device background
notifications, secrets in the Keychain.

Future: APNs push via a remote watcher daemon (instant background alerts); tmux
**control mode** (`-C`) for structured multi-pane multiplexing; split-tree
rendering/editing; worktree creation; editor/diff panes.

## License

The iOS app is licensed under **GPL-3.0** (the repo-root `LICENSE`), with an
**additional permission under GPLv3 section 7** allowing distribution through the
Apple App Store / TestFlight — see `ios/LICENSE`. This resolves the usual GPLv3 ↔
App Store terms conflict for the iOS target; the rest of muxel stays plain GPL-3.0.
