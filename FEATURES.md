# muxel — Features

muxel is a GPUI-based, multi-agent terminal multiplexer: run several coding agents
(Claude, opencode, Amp, …) and shells side by side in a tiled, tabbed workspace
with first-class git worktrees, agent status tracking, and notifications.

This file is the canonical catalogue of what muxel can do. **When a user-facing
feature is added or changed, update the matching entry here in the same change**
(see `CLAUDE.md`).

## Panes & layout

- **Recursive split layout** — panes form a horizontal/vertical split tree; any
  pane can be split again, nesting freely.
- **Resizable splits** — drag the divider between panes; sizes persist per project.
- **Minimum pane width** — panes can't shrink so narrow that an agent TUI becomes
  unusable (keeps a sane terminal width).
- **Drag-to-dock split (Zed-style)** — drag a tab onto a pane edge to pull it out
  into a new split, or onto the center to add it as a tab; drag a pane by its
  title bar to relocate the whole pane, with a highlighted drop zone.
- **Swap panes** — drop a dragged pane on another pane's center to swap their
  positions.
- **Maximize** — temporarily expand one pane to fill the work area.
- **Pane cards** — rounded "card" panes with an accent ring + glow on the active
  pane, hover highlight, and a configurable border style.

## Tabs

- **Tabs per pane** — each pane is a tab group; `Ctrl+T` opens a new tab in the
  active pane.
- **Drag & reorder** — drag tab pills to reorder within a pane, move them to other
  panes, or drop at a precise insertion point.
- **Pinned tabs** — pin a tab to the leftmost block; pins behave fluidly when
  dragged past unpinned tabs.
- **Tab context menu** — right-click a tab to Rename, Duplicate, Pin/Unpin, Close
  tabs to the left / right, Close others, or Close.
- **Tab cycling** — keyboard shortcuts cycle to the next/previous tab.

## Pop-out windows

- **Detach a pane** — pop a pane out into its own OS window without terminating it.
- **Re-dock in place** — a popped-out pane remembers where it came from; the Dock
  button returns it to its original location.
- **Close terminates** — closing a pop-out window kills its terminal (with a
  confirmation).

## Agents

- **Built-in agent presets** — Shell, Claude, opencode, Amp (ampcode), Grok
  (x.ai), Hermes, Ollama, and Pi, each with its own icon. On Windows the default
  shell is **PowerShell**, with **Cmd** offered as a second preset (instead of the
  single "Shell").
- **Configurable launch** — per agent: program, model + model flag, effort +
  effort flag, extra args, environment variables, system-prompt injection
  (via a CLI flag or by typing it in at startup), and a runner startup delay
  (ms to wait after the agent's first output before a runner types — for slow
  starters like opencode; 0 = auto-wait until output goes quiet).
- **Installed-binary autodetect** — agents whose binary isn't on `PATH` are hidden
  from the new-agent menus and marked "not installed" in settings; they reappear
  automatically once installed. On macOS, a Dock/Finder launch reconstructs the
  Homebrew and `~/.local/bin` dirs that launchd otherwise omits, so agents are
  detected and spawnable the same as from a terminal.
- **Graceful launch failure** — if an agent can't be spawned, the pane falls back
  to a shell showing the underlying error instead of crashing.
- **Session resume** — resume-capable agents (Claude out of the box) get a stable
  per-pane session id: muxel launches with `--session-id` the first time and
  `--resume` on restart, so a pane reopens its previous conversation instead of
  starting fresh. If the saved session is gone, it quietly starts a new one in the
  same slot. Configurable per preset (`session_id_flag` / `resume_flag`), so other
  agents can opt in.
- **Broadcast** — `Ctrl+Shift+I` opens a broadcast bar; type a line and Enter (or
  Send) writes it + a newline to every agent pane in the active project at once.
- **Shared project memory** — opt-in per project: agents are told (via their system
  prompt) to read and append durable lessons to a `.muxel/MEMORY.md` file shared
  across every agent and run in that project. muxel creates the file, git-ignores
  `.muxel/`, and works for local and remote SSH projects (the file lives in the
  project's working dir on whichever host). Enable it on a project (sidebar
  right-click or Settings → Projects); a memory button on the project row opens the
  file in the editor. Plain shells are skipped.

## Agent status

- **Real lifecycle badges** — each pane shows **working**, **idle**, **blocked**,
  or **done**, color-coded (blue / gray / amber / green) on the tab pill, sidebar
  icon, dashboard, and notification dots.
- **Per-agent detection markers** — status is inferred from on-screen TUI markers
  (e.g. Claude's "esc to interrupt" spinner, a permission prompt), with built-in
  defaults per agent and **editable working/blocked markers per preset**.
- **Heuristic fallback** — agents without markers fall back to bell + output
  activity (working / idle / done).

## Git worktrees

- **First-class worktrees** — named, color-coded git worktrees shared by one or
  more panes; toggle "create a git worktree" when spawning.
- **Inheritance** — a new tab or split joins its pane's worktree; a duplicate
  inherits the source's; otherwise a fresh worktree is created (toggle on).
- **Visual coding** — the pane outline + glow tint to the worktree color, a name
  badge on a uniform pane, a per-tab color dot, and a matching dot in the sidebar.
- **Sidebar grouping** — panes are grouped under colored worktree subheaders;
  rename a worktree inline or from the context menu.
- **Dispose flow** — when a worktree's last pane closes (or its agent exits), a
  clean worktree is removed silently; otherwise a modal offers **Commit & close**,
  **Merge & close**, **Discard**, or **Keep**.
- **Unmerged-commit detection** — the dispose flow also catches commits not yet in
  the base branch (not just uncommitted changes), so committed work isn't silently
  orphaned; Merge lands them on the base.
- **Kept worktrees** — "kept" (detached) worktrees stay in the sidebar and can be
  resumed (spawn a new agent into them) or resolved later.
- **Review workflow** — each worktree shows its uncommitted-change count;
  right-click for **View changes** (opens the git-diff pane), **Review** /
  **Security Review** (spawns that runner *inside* the worktree to review its
  diff), **Discard changes** (reset the worktree to its base, keeping it), or
  **Discard worktree** (close its panes + delete the worktree and branch).
- **GitHub PRs** — when the `gh` CLI is installed, the worktree menu also offers
  **Push branch**, **Create PR…** (push + open the PR-create page), and **Open PR**
  (open the branch's PR in a browser); these run off the main thread and toast the
  result.

## Runners

- **One-click task launchers** — predefined runners (e.g. Review, Security Review)
  spawn an agent that auto-types a task prompt. The toolbar "Run task" dropdown
  lists them; click to run, or the pencil to edit one in Settings → Runners.
- **Templated prompts** — `{{input}}` is substituted with run-time details.
- **Auto mode** — send a configurable number of Shift+Tab presses (then Enter) at
  startup to reach auto-accept mode.
- **Ephemeral + restore-safe** — on app restore a runner re-types its prompt but
  does not auto-submit.

## Loops

- **Scheduled task launchers** — run a saved prompt on a chosen agent in a chosen
  project on a timer: every N minutes, every N hours, or daily at a local time.
- **Unattended firing** — when due, a loop spawns a fresh agent as its own new
  pane appended at the end of its project's layout, types the prompt, optionally
  sends auto-mode Shift+Tab presses, and respects the agent's startup delay (so
  opencode works). The pane is **visible but not focused** and never switches your
  active project — a loop firing on a timer can't interrupt what you're typing.
- **Post-run policy** — leave the agent running, or exit it once it finishes its
  turn (with a max-runtime safety cap). A still-running loop won't stack a second
  copy.
- **Managed from the main window** — a toolbar "Loops" dropdown lists your loops:
  click one to run it now, the pencil to edit it in Settings → Loops, or "New
  loop…" to create one. Schedules survive restarts (a daily-at whose time passed
  while closed fires once on next launch). Loops fire only while muxel is running.

## Notifications

- **Desktop notifications** — bell-driven (an agent rings the terminal bell when it
  needs attention or finishes); fired only when the pane isn't focused. Clicking the
  notification raises muxel and jumps to the pane that fired it.
- **In-app NOTIFICATIONS sidebar** — a category above PROJECTS collecting agent
  events **and** all app messages (git results, SSH connections, save errors —
  everything that used to be a pop-up toast goes here instead). Agent rows are
  click-to-navigate (jump to the pane + dismiss); all rows are individually
  dismissable, with a clear-all. Collected even when desktop notifications are off.
- **Controls** — an enable/disable toggle and a "send test notification" button.
- **System tray** (Settings → Behavior → "Minimize to the system tray on close") —
  closing the window iconifies muxel to a tray icon instead of quitting. The tray
  menu lists every agent with its live status and the most recent notifications;
  clicking one restores muxel and focuses that project + pane, and "Quit" exits for
  real. Linux uses StatusNotifierItem (needs an AppIndicator/SNI host — standard on
  KDE, the AppIndicator extension on GNOME); Windows/macOS use the notification-area
  / status-bar item. (Stock GPUI can only iconify, so the window still appears in the
  taskbar; restoring from the tray is best-effort on Wayland — the dash always works.)

## Terminal

- **alacritty-based emulator** — full VTE terminal with truecolor support.
- **Selection & clipboard** — mouse text selection and copy/paste (`⌘C`/`⌘V` on
  macOS, `Ctrl+Shift+C`/`Ctrl+Shift+V` elsewhere). A global Settings → Behavior
  choice picks the mouse copy/paste style: **right-click copy/paste** (default —
  right-click copies the selection, or pastes when nothing is selected), a
  **right-click Copy/Paste menu**, or **copy on select** (selecting copies
  immediately; right-click pastes).
- **Scrollback** — history with a draggable overlay scrollbar; clear it via
  `Ctrl+Shift+K` or the tab's "Clear scrollback" menu item. The mouse wheel
  scrolls history, or — for full-screen apps that enable mouse reporting
  (opencode, grok, vim) — is forwarded to the app so it scrolls its own content.
- **Scrollback search** — `Ctrl+Shift+F` (while a terminal is focused) opens a
  search bar that highlights matches and jumps through them (Enter / ↑ / ↓),
  scanning the full history.
- **Clickable URLs** — `Ctrl`/`Cmd`+click opens an `http(s)` URL under the cursor.
- **Focus reporting** — forwards focus in/out to the PTY (DECSET 1004) so agents
  know when their pane is active.
- **Content inset** — a small margin around the grid so a too-wide TUI truncates
  inside the pane rather than against the border.
- **Key routing** — `Tab` / `Shift+Tab` go to the focused terminal rather than
  moving UI focus.
- **Ctrl+P shared with the agent** — the command palette is on `Ctrl+Shift+P`
  (always), while `Ctrl+P` opens it only when no terminal is focused — so a focused
  agent (e.g. opencode, which uses `Ctrl+P`) receives it. Click the toolbar to
  **deselect** the active pane (move focus off the terminal) and `Ctrl+P` reaches
  muxel again.
- **Terminal pass-through keys** — additional chords listed in Settings →
  Keybindings are sent to the focused terminal instead of triggering muxel's
  shortcut, for other agent key conflicts.

## Editor & tools

- **Code editor pane** — open and edit files in a pane (save / save-as).
- **File browser** — a second, toggleable sidebar (the project row's **files**
  button) showing the project's files as an expandable, gitignore-aware folder
  tree with a search box; click a file to open it in an editor. Resizable; width
  persists. Right-click a row for: copy path, copy relative path, reveal in the OS
  file manager, rename on disk, and open a terminal in that directory.
- **Markdown & image rendering** — `.md`/`.markdown` files render as formatted
  markdown and image files (`png`, `jpg`, `gif`, `webp`, `bmp`, `svg`, …) render as
  images, both by default, with a header **Raw / Rendered** toggle to view the
  source (e.g. an SVG's XML or the markdown text).
- **Git diff panel** — a toolbar button (far right) toggles a collapsible
  right-side panel with two tabs:
  - **Files** — the active project's changed files (added / modified / deleted /
    renamed / untracked, color-coded). Click a file to open its diff in a dedicated
    OS window with its own title bar and a **Split / Unified toggle** (remembered):
    **Unified** is a colored diff (green additions / red deletions) whose text is
    selectable + copyable (read-only); **Split** is a side-by-side view (old left /
    new right, aligned, changed rows tinted green/red with line numbers) for quick
    at-a-glance reading. Re-clicking focuses the existing window. Per-file context menu:
    View diff, Stage, Unstage, Discard (with confirmation), Open file. A footer
    commits **all** changes with a message.
  - **Worktrees** — every worktree of the active project, expandable to its changed
    files (same rows + diff windows). Per worktree: **Merge into…** any branch
    (checks it out + merges, then offers to remove the worktree), and **Delete** the
    worktree + its branch (enabled only when no instance is loaded in it).
  Works for local and remote (SSH) projects; panel width persists per workspace.
- **Git diff pane** — a simpler read-only pane showing the working-tree diff for a
  directory; opens as a **new tab** in the pane it's diffing (from a pane's "View
  changes", the project menu, or a worktree) rather than splitting off a new pane.
- **Command palette / global search** — quick navigation and search across the
  workspace.
- **Find in project** — search within the active project.

## Sidebar & projects

- **Project list** — projects with live per-agent status rows; collapse a project.
- **Branch label** — each project row shows its git repo's current branch with a
  branch icon (refreshed live).
- **Project git** — right-click a git project for: git diff (opens the diff pane;
  local projects), switch branch (submenu), new branch, commit, pull, push, fetch,
  and stash / pop / drop stash; each runs off the main thread and toasts its
  result. Destructive actions (switching with a dirty tree, pop/drop stash) ask
  first.
- **Reviewed commit** — the commit dialog lists every changed/untracked file with
  a checkbox (all checked by default); only the checked files are committed, so
  stray files are never swept in. The button shows the count (e.g. *Commit (3)*),
  and a clean tree just toasts “Nothing to commit”.
- **Reorder & rearrange** — drag-reorder projects; swap/move instances between
  panes from the sidebar.
- **Instance names** — custom names with inline rename and right-click menus.
- **Resizable sidebar** — drag to resize (up to half the window); width persists.
- **No auto-created project** — start empty; add projects via a folder picker.
- **Startup agents** — save the project's open agents as a startup set (preset +
  worktree flag) and relaunch them in one click from the project menu.

## Remote development (SSH)

- **Remote projects** — create a project that lives on a remote host over SSH
  (the project list's network button → pick a host, enter the remote directory,
  optionally verify it). Shells and agents then run on the remote, in a pane that
  behaves exactly like a local one. Local muxel still owns the UI, layout, and
  settings.
- **SSH host library** — Settings → Remotes manages saved hosts with the common
  options: hostname/alias, port, user, auth (ssh-agent, key file, or password),
  ProxyJump, agent forwarding, host-key policy, keepalive, and extra `-o` options.
  A "Test connection" button verifies a host.
- **Secure passwords** — saved SSH passwords are stored in the OS keychain (Secret
  Service / macOS Keychain / Windows Credential Manager), never in muxel's config,
  and fed to ssh via `sshpass`. Password auth requires `sshpass` (Linux/macOS only;
  the panel warns when it's missing) — Windows uses key-file or ssh-agent auth,
  which work everywhere.
- **Resilient sessions** — remote panes default to a persistent tmux session on
  the host, so a dropped connection is survivable: reconnecting re-attaches the
  still-running agent. One multiplexed SSH connection per host is shared by the
  pane and all git calls.
- **Roaming layouts** — a remote project's pane layout is mirrored to the host at
  `<remote_root>/.muxel/workspace.json`, so opening the same project from another
  machine restores the whole session (the tmux-backed panes re-attach to their
  still-running agents). muxel pushes the layout as you rearrange panes and, on
  connect, loads whichever copy — local or remote — is newer; the replaced copy is
  kept as a one-level backup on each side. Automatic for every remote project; no
  setup required.
- **Remote git** — the branch label and the project git menu (switch/new branch,
  commit, pull, push, fetch, stash) operate on the remote repo over the shared
  connection; remote status is polled off the UI thread.
- **Remote files** — the file browser lists a remote project's files over SSH
  (gitignore-aware via `git ls-files`, else `find`); opening a file reads it over
  SSH into the editor and Ctrl+S writes it back. Open-in-terminal opens a remote
  shell in that directory.

## Workspaces & persistence

- **Workspaces** — multiple workspaces, each with its own projects + layout; a startup workspace
  selector.
- **Single instance per workspace** — if a workspace is already open in another muxel
  window, launching again with the same workspace refuses to load it and shows an
  alert, so the two can't overwrite each other's workspace + settings. Different
  workspaces still run side by side, and the lock releases on exit (even a crash), so
  no stale lock blocks the next launch.
- **Full restore** — pane layout, split sizes, window geometry, and sidebar width
  are persisted and restored on launch.

## Settings & theming

- **Settings modal** — sections for Appearance, Editor, Behavior, Agents, Runners,
  Projects, and Keybindings.
- **Themes** — ~22 bundled themes with a switcher (Catppuccin, Gruvbox, Tokyo
  Night, Solarized, Ayu, Everforest, and more).
- **Sizing** — whole-app zoom plus independent UI, terminal, and code/diff font
  sizes.
- **Keybindings** — configurable shortcuts with a rebind UI, a cheat-sheet overlay
  (`Ctrl+Shift+/`), `Alt+1–9` to jump to a pane's Nth tab, `Ctrl+1–9` to switch to
  the Nth project, `Ctrl+Shift+A` to focus the next agent needing attention
  (blocked, then done), and `Cmd+Q` (`Ctrl+Q` elsewhere) to quit from any focus.
- **Behavior** — immediate-save appearance, confirm destructive actions, quit
  confirmation, per-kind close confirmation (terminal on, editor/diff off by
  default), and auto-close a pane when its process exits.

## Localization

- **Many languages** — the UI auto-detects your OS locale on startup and can be
  switched live from Settings → Appearance → Language (no restart). Any
  untranslated string falls back to English.
- **Translation catalogs** — bundled per-language JSON under `assets/i18n/`,
  (re)generated by `scripts/translate.py`, which drives the `claude` (sonnet) or
  `opencode` CLI in batches of 25 and keeps technical terms / product names (tmux,
  SSH, git, worktree, Claude, …) and `{placeholder}` tokens untranslated.
  `python3 scripts/translate.py --check` keeps the catalog in sync with the code.

## Platform & distribution

- **Cross-platform** — Linux (x86_64 + arm64), macOS (Intel + Apple Silicon), and
  Windows (x86_64 + arm64).
- **Desktop integration** — app icon and a `.desktop` launcher entry (also the
  notification icon).
- **In-app updates** — check for and apply updates from within the app: it
  fetches the latest GitHub Release and self-replaces in place (the AppImage, the
  portable binary, the Windows `.exe`, or the macOS `.app`), then relaunches;
  package-managed installs get the right upgrade command instead. The update
  dialog shows the release's full changelog rendered as markdown, and is
  resizable.
- **Windows installer** — a basic per-user Inno Setup installer (`.exe`, no
  admin) with a Start Menu shortcut and an uninstaller; it installs to a
  user-writable location so the in-app auto-updater keeps working without
  elevation. A portable `.zip` is also published.
- **Packaging & CI** — release packaging per OS/arch on native runners (.deb /
  .rpm / AppImage / .tar.gz for Linux, .dmg / .zip for macOS, an installer .exe +
  .zip for Windows) and continuous integration. The macOS `.dmg` opens to the
  standard drag-onto-Applications layout (the app beside an Applications
  shortcut). Windows builds are Authenticode-signed; macOS builds are
  Developer-ID-signed + notarized when an Apple cert is configured (else ad-hoc
  signed).
