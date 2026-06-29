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
entitlements + TestFlight).

```sh
brew install xcodegen
cd ios
xcodegen generate          # generates Muxel.xcodeproj from project.yml
open Muxel.xcodeproj
```

In Xcode: set your signing team, then run on a **real device** (Keychain,
background refresh, and notifications don't behave correctly in the Simulator).

Dependencies (Swift Package Manager, resolved by Xcode):
- [Citadel](https://github.com/orlandos-nl/Citadel) — SSH client (SwiftNIO SSH).

> v1 needs only Citadel's **exec** channel (`run`). The terminal is a polled
> `capture-pane` viewer + `send-keys` input bar (`TerminalPaneView`), so the live
> PTY / SwiftTerm work is **not** on the v1 critical path. The remaining Citadel
> spike items for v1 are **private-key auth parsing** and the **TOFU host-key
> validator** (both flagged in `CitadelSSHConnection.swift`); password auth + exec
> are implemented. If a pinned version fails to resolve, bump it to the latest tag.

> Post-MVP: add [SwiftTerm](https://github.com/migueldeicaza/SwiftTerm) fed by a
> Citadel PTY channel for a smooth live terminal (replaces the polled view).

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

Background polling can't hold a live PTY, so per muxel tmux session we read four
tmux format vars in one round-trip and feed them into the ported `classify`:

- screen text → `tmux capture-pane -p -t =<session>` (the marker scan / `visible_text`)
- exited → `#{pane_dead}`
- bell → `#{window_bell_flag}`
- idle seconds → `now - #{window_activity}` (the `idle_for` equivalent)

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

v1 (lean MVP): connect, collapsible sidebar of hosts/projects, add hosts/projects,
view + attach panes/tabs, launch instances, live status badges, on-device
background notifications, secrets in Keychain.

Future: APNs push via a remote watcher daemon (instant background alerts); tmux
**control mode** (`-C`) for structured multi-pane multiplexing; split-tree
rendering/editing; worktree creation; editor/diff panes.

## License

The iOS app is licensed under **GPL-3.0** (the repo-root `LICENSE`), with an
**additional permission under GPLv3 section 7** allowing distribution through the
Apple App Store / TestFlight — see `ios/LICENSE`. This resolves the usual GPLv3 ↔
App Store terms conflict for the iOS target; the rest of muxel stays plain GPL-3.0.
