# muxel

A native, GPU-accelerated **multi-agent terminal multiplexer** for running many
AI coding agents (Claude, opencode, Amp, …) and shells side by side — built in
Rust on [GPUI](https://github.com/zed-industries/zed) (Zed's UI framework).

muxel is shaped like an **agent manager**, not "another terminal app": a tiled,
tabbed workspace where each pane is a real terminal running a coding agent or
shell, with first-class git worktrees, live agent-status tracking, scheduled
runs, and notifications. It's a native desktop GUI (not a TUI) — every pane
embeds a real terminal emulator over a PTY.

## Features

- **Tiled, tabbed panes** — split horizontally/vertically, drag tabs and panes to
  re-dock (Zed-style), pop a pane out into its own window, maximize, and
  double-click a divider to even out a split.
- **Agents & presets** — each pane launches from a preset (program, args, model,
  system prompt, env, status markers). Built-ins for Claude, opencode, Amp and
  more, all editable in Settings.
- **Live agent status** — each pane shows running / working / awaiting-input /
  done, detected from the terminal output, with desktop + in-app notifications on
  transitions and a cross-project dashboard.
- **Git worktrees** — first-class, named, color-coded worktrees shared by panes,
  with a review/dispose flow and GitHub PR helpers (via the `gh` CLI).
- **Runners & Loops** — save a prompt + agent as a *runner* to launch on demand,
  or a *Loop* to run it on a schedule (every N minutes/hours, or a daily time).
- **Shared memory** — opt a project into a `.muxel/MEMORY.md` that agents read
  and append lessons to across runs.
- **Remote development (SSH)** — projects that live on a remote host: shells and
  agents run there over one multiplexed SSH connection, with remote git, a remote
  file browser, persistent tmux sessions, and **roaming layouts** (your pane
  layout syncs to the host, so another machine restores the session).
- **Editor & tools** — a built-in code editor, a git-diff viewer, a Ctrl+P file/
  command palette, and per-project startup-agent sets.
- **Persistence & profiles** — projects, pane layouts, split sizes, tabs, and
  window geometry are saved and restored; multiple profiles, each with its own
  workspace.
- **In-app updates** — check for and apply updates from within the app.

See [FEATURES.md](FEATURES.md) for the full catalogue.

## Install

**Linux / macOS:**

```sh
curl -fsSL https://muxel.sh/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://muxel.sh/install.ps1 | iex
```

Or grab a package from the [Releases](https://github.com/projecthax/muxel/releases):

- **Linux** (x86_64 + aarch64): `.deb`, `.rpm`, `.AppImage`, or portable `.tar.gz`
- **macOS** (universal — Intel + Apple Silicon): `.dmg` or `.zip`
- **Windows** (x64 + arm64): a signed **installer** (`.exe`, per-user, no admin,
  with in-app auto-updates) or a portable `.zip`

## Build from source

Requires a recent stable Rust toolchain (pinned in `rust-toolchain.toml`).

```sh
cargo run -p muxel
```

The first build compiles GPUI from source (a git dependency on Zed), so expect it
to take a while.

### Linux build dependencies

Wayland/X11 + Vulkan + fontconfig + D-Bus development libraries — on Debian/Ubuntu:

```sh
sudo apt-get install -y \
  clang lld cmake pkg-config \
  libfontconfig-dev libfreetype6-dev \
  libwayland-dev wayland-protocols \
  libxkbcommon-dev libxkbcommon-x11-dev \
  libxcb1-dev libx11-dev libx11-xcb-dev \
  libasound2-dev libvulkan-dev libdbus-1-dev
```

(See `.github/workflows/ci.yml` for the exact set used in CI.)

## Releases & CI

GPUI can't be cross-compiled across operating systems (Metal on macOS, DirectX on
Windows, Vulkan on Linux), so each platform builds on its own native runner.

- **CI** (`.github/workflows/ci.yml`) — `fmt`, `clippy`, and tests on Linux
  (x86_64 + arm64), macOS, and Windows on every push / PR.
- **Releases** (`.github/workflows/release.yml`) — triggered by a `vX.Y.Z` tag;
  builds every package above and attaches them to the GitHub Release:

  ```sh
  git tag v0.1.0 && git push origin v0.1.0
  ```

macOS release builds are Developer-ID-signed and notarized when the Apple signing
secrets are configured; Windows `.exe`s are Authenticode-signed via Azure Trusted
Signing as a post-release step (`scripts/sign-windows.sh`). tmux session
persistence is Unix-only (disabled on Windows).

## License

muxel is **dual-licensed**:

- **Open source** — [GNU GPL-3.0](LICENSE): free to use, modify, and
  redistribute, provided any version you distribute is also released under
  GPL-3.0.
- **Commercial** — a separate license from **ProjectHax LLC** for use that can't
  comply with the GPL (e.g. embedding muxel in a closed-source product). See
  [LICENSING.md](LICENSING.md).

By submitting a contribution you agree to the terms in
[CONTRIBUTING.md](CONTRIBUTING.md), which keep both licenses possible.
