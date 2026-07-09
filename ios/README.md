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
> it to fail, so a swipe scrolls rather than selecting. **Held backspace** is
> accelerated: iOS repeats `deleteBackward` at a flat rate for a custom key-input view,
> so `TerminalSession.send` detects a sustained run of backspace bytes and emits extra
> ones (as raw bytes, not re-entrant `deleteBackward` calls), ramping up the longer
> it's held. A themed **accessory key row** (`TerminalAccessoryRow`) replaces
> SwiftTerm's stock gray bar above the keyboard: Esc, a sticky **Ctrl** (SwiftTerm's
> public `controlModifier` — it combines the next typed character and auto-resets,
> which un-highlights the key), Tab, auto-repeating arrows that honor
> application-cursor mode (SS3 vs CSI), and paste via SwiftTerm's bracketed-paste-aware
> `paste(_:)`. **Pinch-to-zoom** resizes the font live (integer snap, 9–24 pt,
> persisted in `muxel.terminal.fontSize` and applied to every terminal); the font
> setter drives SwiftTerm's resize → the same PTY window-change path as rotation.
> **OSC-52** copies from the remote land in the system pasteboard (`set-clipboard on`
> is enabled on attach alongside `mouse on`). Haptics are deliberately minimal: light
> ticks on accessory keys, a medium tick on sticky-Ctrl engage, a throttled warning
> buzz on the terminal bell, and selection ticks on tab-menu actions — nothing on
> scroll or key auto-repeat. The background poll
> (`PollService`) still drives status badges +
> notifications. Password auth, exec, and **SSH key auth** (ed25519 / RSA, with
> passphrase — ECDSA is detected but not parseable by Citadel) are implemented in
> `CitadelSSHConnection.swift`. **Host keys are enforced** with trust-on-first-use
> (`TOFUHostKeyValidator`, a `.custom` NIOSSH validator over `HostKeyStore`): the
> first key is pinned silently; a changed key refuses the connection and prompts with
> the stored vs presented fingerprints ("Trust new key" re-pins). **Jump hosts**
> tunnel via Citadel's `SSHClient.jump(to:)` — a direct-tcpip channel on the bastion
> carrying a full SSH session to the target, the `ssh -J` equivalent (single hop;
> chains are rejected in the form).

## The muxel remote protocol (keep in sync with `muxel-core`)

The Swift code mirrors these stable contracts. If `muxel-core` changes them, update
the matching Swift port and the `RemoteLayout` version handling.

| Concern | Rust source | Swift port |
|---|---|---|
| tmux session name `muxel_<host-slug>_<uuid8>` | `crates/muxel-core/src/tmux.rs` | `Tmux/TmuxSession.swift` |
| `tmux new-session -A -d -s … -c … -- prog args` | `tmux.rs` | `Tmux/TmuxCommands.swift` |
| `RemoteLayout` v1 JSON (`.muxel/workspace.json`) | `crates/muxel-core/src/lib.rs` | `Models/RemoteLayout.swift` |
| Pane tree (`leaf`/`split` tagged enum, legacy `instance` leaf) | `crates/muxel-core/src/pane.rs` | `Models/PaneNode.swift` |
| Pane-tree mutations (`split`/`move_into_split`/`add_tab`/`remove`/`normalize`) | `pane.rs` | `Models/PaneMutations.swift` |
| Launch resolution (`compose_args`/`resolve_launch`/`session_resume_args`) | `agent.rs` | `Models/AgentLaunch.swift` |
| Worktree naming (`slug`/`branch_name`/`dir_name`/`next_worktree_color`) | `worktree.rs`, `lib.rs` | `Models/WorktreeNaming.swift` |
| `Instance` / `Worktree` / `InjectionMode` | `lib.rs`, `worktree.rs`, `agent.rs` | `Models/*.swift` |
| `.muxel/` read + write (prep one-liner, backup, gitignore) | `crates/muxel/src/integrations.rs` | `Interop/RemoteLayoutStore.swift` |
| Status `classify` + `latch_done` | `crates/muxel-terminal/src/view.rs` | `Status/AgentStatus.swift` |
| Per-agent `default_markers` | `crates/muxel/src/app.rs` | `Status/Markers.swift` |
| Theme palettes (chrome + terminal) | `crates/muxel/assets/themes/*.json` | `Theme/MuxelTheme.swift` |
| Login identities (shared credentials) | `crates/muxel-core/src/lib.rs` (`Identity`) | `Models/Store.swift` (`Identity`) |
| Quote-aware word split (extra args / custom command) | `crates/muxel-core/src/shell.rs` | `Util/Shell.swift` |

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
- Host keys use **TOFU**: the fingerprint is pinned on first connect and a changed
  key refuses the connection with a stored-vs-presented prompt (mirrors muxel's
  `accept-new`, plus an explicit re-trust step).
- **Jump-host auth is explicit**: desktop's `-J` delegates the bastion login to ssh
  config/agent; iOS has neither, so the bastion authenticates with a picked saved
  Identity or, by default, the same credential as the target host (`user@` in the
  jump string overrides the user). The bastion's host key gets its **own TOFU slot**
  (`HostKeyStore.Scope.jump`), so a bastion swap can't masquerade as the target.
- **Keepalive is app-level**: Citadel/NIOSSH expose no protocol keepalive, so
  `keepaliveSecs` runs a serialized `run("true")` ping (clamped ≥5s) — the
  `ServerAliveInterval` analogue; a failed ping drops the dead transport so the next
  call reconnects.

## Where things are stored

- **On device** (this app's own store; the desktop's config lives on the desktop):
  host library + project list as Codable JSON in the app-support dir; SSH
  passwords/keys in the **Keychain** (`kSecAttrAccessibleAfterFirstUnlock`).
- A damaged `store.json` is never silently reset: it's preserved as
  `store.json.corrupt` and the app tells you (credentials are unaffected — they live
  in the Keychain).
- The optional **App Lock** (Face ID / passcode, toggled in the Identities sheet) is
  a UI-level gate only — Keychain accessibility deliberately stays
  `kSecAttrAccessibleAfterFirstUnlock` so background polling and notifications keep
  working while the phone is locked in a pocket.
- **On the remote host** (shared with desktop muxel): per-project layout at
  `<remote_root>/.muxel/workspace.json` (`RemoteLayout` v1).

## Status / roadmap

v1 (lean MVP): connect, collapsible sidebar of hosts/projects, add + **edit** hosts
in place (secrets are kept unless replaced, matching identity editing; delete asks
first — host/project/identity deletion all confirm with what exactly is removed),
add projects by path or by **scanning the host** for `.muxel/` markers
(`ProjectDiscovery`), **live SSH PTY terminal** for every pane (SwiftTerm + Citadel
`withPTY`, attached so TUI agents work), panes/tabs resolved by uuid8 suffix (so
desktop-created panes show up too), launch instances with quote-aware custom
commands (long-press a tab to rename / duplicate / close, with a close
confirmation), live status badges + a running-count on the selected project,
**Test connection** to verify a saved credential — or the in-form credentials
*before* saving, right in the host editor — **jump hosts** (single-hop `ssh -J`
equivalent, with a pickable bastion login), **keepalive**, an optional **App Lock**
(Face ID / passcode), on-device background notifications (with a quiet sidebar
pointer when permission is denied), secrets in the Keychain, and a **themeable
terminal identity** — a mono, prompt-caret chrome over the muxel palette (shared
`DesignKit` components: prompt headers/labels, themed form sections, one
`CenteredState` for every empty/loading/error state, and a transient
`NoticeBanner` instead of blocking error alerts), with a theme picker (Catppuccin,
Tokyo Night, Gruvbox, Everforest, Solarized, Matrix, …) that recolors the chrome
**and** the live terminal (bg/fg/cursor + ANSI + keyboard/accessory row) together.
Palettes are ported from the desktop theme JSONs (`Theme/MuxelTheme.swift`); the
default is Catppuccin Mocha, matching `muxel.svg`. **Login identities** (a sidebar
sheet) let you define a reusable login once — user + auth + imported key/passphrase
or keychain password — and point many hosts at it instead of re-entering
credentials; the secret is stored in the Keychain under the identity id and shared
by every host that references it.

**Live status bar** (a Live Activity): while the app is minimized, a Lock Screen /
Dynamic Island view lists **each agent instance** and its state — **needs input**
(rang the bell), **finished** (exited), **working**, or idle — with the ones waiting
for you sorted first. A bell on a live pane is treated as "needs input" (the closest
background-safe signal; muxel can't scrape the screen for a real prompt marker), and
a clean exit as "finished". It stays present the whole time you're backgrounded (idle
instances included) and ends when you reopen — and fully closing the app
(`applicationWillTerminate`) ends it too, so nothing lingers in the Dynamic Island /
Lock Screen after muxel is gone (a long-suspended app that skips that callback falls
back to the ~20-min stale timeout). The
widget lives in a `MuxelWidgets` app-extension target and renders from a shared
`MuxelActivityAttributes` payload; it's driven by the same `StatusPoller` as the
notifications. Note "needs attention" is the exit/bell
signal, not true input-blocking (which needs screen scraping the app doesn't do), and
without APNs the backgrounded activity only refreshes at the OS poll cadence
(~15 min+) so it's marked stale between updates.

**iPad split view** renders the desktop's `PaneNode` split tree with live terminals
side by side (a recursive `ProportionalSplit` layout over the same tree desktop
persists), with basic split editing ("open in split right/below" from a tab's
menu, written back to `workspace.json`) and per-leaf focus; iPhone keeps the flat
tab strip. **Worktree creation** (a launch toggle) runs `git worktree add` over SSH
and registers the `Worktree` record so desktop adopts it. **Richer launch**: system
prompt (CliFlag / TypeIn injection), model, and session resume (`--session-id` →
`--resume`). **Editor/diff panes** created on desktop render read-only on the phone
(remote file cat + `git diff` with +/- coloring). **Live-grid status**: an attached
pane's screen text feeds the ported `classify` + markers, so working/blocked is real
while a pane is open. **Cross-project status**: the sidebar shows running/needs-input
badges for every connected project (one batched `list-panes -a` sweep per host).

Future: APNs push via a remote watcher daemon (instant background alerts + live
status-bar updates); tmux **control mode** (`-C`); interactive split resize + tab
reorder; worktree deletion/disposal UX; agent-preset editing; editor saving / diff
staging.

## License

The iOS app is licensed under **GPL-3.0** (the repo-root `LICENSE`), with an
**additional permission under GPLv3 section 7** allowing distribution through the
Apple App Store / TestFlight — see `ios/LICENSE`. This resolves the usual GPLv3 ↔
App Store terms conflict for the iOS target; the rest of muxel stays plain GPL-3.0.
