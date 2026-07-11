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
  Terminals and editors move across; a browser pane is re-created in the new window
  at the same URL (a native webview belongs to the window that built it).
- **Re-dock in place** — a popped-out pane remembers where it came from; the Dock
  button returns it to its original location.
- **Close terminates** — closing a pop-out window kills its terminal (with a
  confirmation). Editors and browser panes just close.

## Agents

- **Built-in agent presets** — Shell, Claude, opencode, Amp (ampcode), Grok
  (x.ai), Hermes, Ollama, **Ollama Code**, Pi, and a **Browser** preset, each with
  its own icon. On Windows the default shell is **PowerShell**, with **Cmd**
  offered as a second preset (instead of the single "Shell"). A preset is either
  a terminal agent or a **Browser** (opens a browser pane at its homepage) —
  switch the type and edit the fields in Settings → Agents.
- **Ollama Code** — runs a coding agent backed by an Ollama model via
  `ollama launch <agent> --model <model>` (seeded as `ollama launch opencode
  --model glm-5.2:cloud`); change the agent or model in the preset's args.
- **Configurable launch** — per agent: program, model + model flag, effort +
  effort flag, extra args (shell-style quoting: `--append-system-prompt "be terse"`
  stays one argument, matching the iOS companion's parser; an unbalanced quote
  falls back to space-splitting with a feed warning), environment variables,
  system-prompt injection
  (via a CLI flag or by typing it in at startup), and a runner startup delay
  (ms to wait after the agent's first output before a runner types — for slow
  starters like opencode; 0 = auto-wait until output goes quiet).
- **Installed-binary autodetect** — agents whose binary isn't on `PATH` are hidden
  from the new-agent menus and marked "not installed" in settings; they reappear
  automatically once installed. A GUI launch reconstructs the bin dirs the desktop
  environment otherwise omits — on macOS a Dock/Finder launch restores Homebrew and
  `~/.local/bin`; on Linux a desktop-entry / AppImage launch restores `~/.local/bin`,
  `~/.opencode/bin` (opencode's installer default), Linuxbrew, and friends — so
  agents are detected and spawnable the same as from a terminal.
- **Graceful launch failure** — if an agent can't be spawned, the pane falls back
  to a shell showing the underlying error instead of crashing. If even the
  fallback shell can't start, the pane shows the failure in place (the toolbar
  Restart retries) and the error lands in the NOTIFICATIONS feed.
- **Session resume** — resume-capable agents reopen their prior conversation after
  a muxel restart. Two shapes, both configurable per preset:
  - **Host-minted** (Claude, Grok): `session_id_flag` + `resume_flag` — muxel
    launches with `--session-id` the first time and `--resume` on restart.
  - **Agent-minted** (Codex): only `resume_flag` (`resume`) — first launch is bare;
    before restart muxel reads the real id from `~/.codex/sessions` (matched by
    project cwd) and relaunches as `codex resume <id>`. One Codex pane per project
    is the reliable shape (same-cwd multi-pane can race on "latest").
  If the saved session is gone, the pane quietly starts fresh.
- **Broadcast** — `Ctrl+Shift+I` opens a broadcast bar; type a line and Enter (or
  Send) writes it + a newline to every agent pane in the active project at once.
- **Speech-to-text dictation** — a toolbar mic button (or `Ctrl+Shift+M` to
  toggle, or hold `Ctrl+Shift+H` to push-to-talk) records the microphone and
  types the transcript into the **focused** agent's prompt, left unsubmitted so
  you can review before pressing Enter (or enable auto-submit). Two engines,
  chosen in Settings → Speech: **Local** runs whisper.cpp entirely on your
  machine (the model downloads once on first use), or a **Provider** — any
  OpenAI-compatible `/audio/transcriptions` endpoint (OpenAI, Groq, a self-hosted
  server) via a base URL + model + API key stored in the OS keychain. Provider
  mode uploads your recorded audio to that endpoint; local mode never leaves the
  machine. macOS asks for microphone permission on first use.
- **Shared project memory** — opt-in per project: agents are told (via their system
  prompt) to `grep` and add durable lessons to a `.muxel/MEMORY.md` file shared
  across every agent and run in that project. muxel creates the file, git-ignores
  `.muxel/`, and works for local and remote SSH projects (the file lives in the
  project's working dir on whichever host). Local agents also get its path in a
  `MUXEL_MEMORY_FILE` env var. Enable it on a project (sidebar right-click or
  Settings → Projects); a memory button on the project row opens the manager. Plain
  shells are skipped.
- **Self-maintaining memory** — each fact is one `##` section carrying a machine
  meta line (id, dates, tags). muxel keeps the file **most-relevant-first** (recently
  used entries and 📌 pinned ones rise to the top), **timestamps** every entry,
  **auto-purges** un-pinned entries unused for 30 days, and **caps** it at 40
  un-pinned entries (evicting the least-recently-used) — so it stays small and
  greppable and never needs hand-pruning. A legacy flat `MEMORY.md` is imported, not
  lost, on first open.
- **Memory panel** — the project row's memory button toggles a docked, resizable
  second-sidebar panel (like the file browser, sharing its slot; width persists per
  workspace): search/grep the entries, add one (title + note + tags), pin/unpin
  (pinned entries are exempt from purge and cap), delete (with confirm), or open the
  raw `MEMORY.md` in the editor. Entries load and persist over SSH for remote
  projects too.

## Agent status

- **Real lifecycle badges** — each pane shows **working**, **idle**, **blocked**,
  or **done**, color-coded (blue / gray / amber / green) on the tab pill, sidebar
  icon, dashboard, and notification dots. A marker-based agent whose turn finishes
  is held at **done** until you attend the pane — even if it never rang the bell.
- **Per-agent detection markers** — status is inferred from on-screen TUI markers
  (e.g. Claude's "esc to interrupt" spinner, a permission prompt), with built-in
  defaults per agent and **editable working/blocked markers per preset**.
- **Heuristic fallback** — agents without markers fall back to bell + output
  activity. They reach **done** only from the bell or process exit, never from a
  quiet spell — so an incidental redraw (e.g. a focus repaint when you click the
  pane) can't fake a finished turn.

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

## Snippets

- **Type saved text into an existing pane** — reusable snippets (a managed list in
  Settings → Snippets) are typed straight into an already-running pane, unlike
  runners (which spawn a new agent). Each snippet records whether it **auto-submits**
  (presses Enter) or just drops the text into the input for you to review.
- **Three ways to send** — the toolbar **Snippets** dropdown (sends to the active
  pane), the command palette (*Send snippet: …*), or right-click a terminal tab →
  **Send snippet** (sends to that specific pane). Multi-line text goes in via a
  bracketed-paste-aware insert so it won't submit on its own newlines.

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

## Built-in browser

- **System webview, not bundled Chromium** — preview links agents print (or a
  locally hosted dev site) without leaving muxel. Uses the OS engine (WKWebView on
  macOS, WebView2 on Windows, WebKitGTK on Linux), so it's light on disk and memory.
- **macOS/Windows: an embedded pane** — ctrl+click a URL and it opens as a browser
  pane beside the terminal, with an address bar, Back, and Reload; the URL persists
  and restores with the workspace.
- **Linux: a separate browser window** — gpui can't embed WebKitGTK, so links open
  in a muxel-managed browser window (a crash-isolated `muxel --browser` process);
  if WebKit isn't installed it falls back to the system browser with a note.
- **Browser as a preset** — the built-in **Browser** preset opens a web-browser
  pane; pick it anywhere you pick an agent (the toolbar's new-pane dropdown, or
  hold a pane's split / `+` button and choose it). Configure its homepage — and
  add more browser presets with their own homepages — in **Settings → Agents**
  (default `duckduckgo.com`; a bare domain gets `https://`). On macOS/Windows it's
  an embedded pane in the layout; on Linux it opens in a separate browser window.
- **Optional** — Settings → Behavior → "Open ctrl+clicked links in the built-in
  browser" (default on); off routes every link to the system browser.

## Notifications

- **Desktop notifications** — fired when an agent finishes a turn or needs attention
  (a blocked prompt / the terminal bell), but only while muxel's window isn't
  focused — no toast pops over the app you're already looking at (the in-app feed
  still records it). Clicking the notification raises muxel and jumps to the pane
  that fired it.
- **In-app NOTIFICATIONS sidebar** — a category above PROJECTS collecting agent
  events **and** all app messages (git results, SSH connections, and save errors —
  workspace, settings, workspace list, project memory, and layout backups —
  everything that used to be a pop-up toast goes here instead; persistent save
  failures report once per cause, not on every autosave). Agent rows are
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
- **Developer console** (Settings → Behavior → "Developer console", toggled with F12) —
  an opt-in popped-out window logging errors as they happen. A failed agent launch
  shows the program it tried, the working directory, and the OS error/code; git, save,
  and SSH errors land here too. Timestamped, newest first, selectable/copyable, with a
  Clear button. F12 is a no-op until the setting is enabled.

## Terminal

- **alacritty-based emulator** — full VTE terminal with truecolor support.
- **Selection & clipboard** — mouse text selection and copy/paste (`⌘C`/`⌘V` on
  macOS, `Ctrl+Shift+C`/`Ctrl+Shift+V` elsewhere). A global Settings → Behavior
  choice picks the mouse copy/paste style: **right-click copy/paste** (default —
  right-click copies the selection, or pastes when nothing is selected), a
  **right-click Copy/Paste menu**, or **copy on select** (selecting copies
  immediately; right-click pastes). **Paste**: plain `Ctrl+V` is host-side smart
  paste — text and file paths go into the PTY; an image is forwarded as raw
  Ctrl+V (`0x16`) so agents that read the OS clipboard (Grok) can attach it.
  Claude Code on Windows uses `Alt+V` (sent as `ESC v`) for images.
  `Shift+Insert` pastes and `Ctrl+Insert` copies. File **drag-and-drop** pastes
  shell-quoted paths into the focused terminal.
- **Mouse reporting** — when an app enables mouse mode (Grok, Claude, vim, …),
  clicks, drags, and motion are forwarded as SGR/X10 mouse events so the app can
  focus its prompt, scroll its own pane, set the cursor, etc. Hold **Shift** to
  force local text selection instead. The wheel already forwarded to mouse-aware
  apps; button reports complete that path.
- **Scrollback** — history with a draggable overlay scrollbar; clear it via
  `Ctrl+Shift+K` or the tab's "Clear scrollback" menu item. The mouse wheel
  scrolls history, or — for full-screen apps that enable mouse reporting
  (opencode, grok, vim, tmux) — is forwarded to the app so it scrolls its own
  content (tmux mouse mode is turned on automatically for tmux-backed panes).
- **Scrollback search** — `Ctrl+Shift+F` (while a terminal is focused) opens a
  search bar that highlights matches and jumps through them (Enter / ↑ / ↓),
  scanning the full history.
- **Clickable links** — `Ctrl`/`Cmd`+click opens what's under the cursor: an
  `http(s)` URL, an OSC 8 hyperlink (e.g. `ls --hyperlink` or agent markdown
  links), or a **file path** (absolute, `~/`, or relative to the pane's working
  directory). Local files open in a muxel editor pane; only paths that exist are
  clickable, and a trailing `:line:col` is understood. `Ctrl`/`Cmd`+hover
  underlines the link and shows a pointing-hand cursor (Ctrl/Cmd down re-hit-tests
  without requiring a mouse move).
- **Focus reporting** — forwards focus in/out to the PTY (DECSET 1004) so agents
  know when their pane is active.
- **OSC-52 clipboard** — programs in the terminal (including over SSH/tmux) can
  copy to the system clipboard via `OSC 52`; clipboard *reads* are answered with
  an empty reply, so a remote can probe for support but never see your clipboard.
- **Color queries** — answers `OSC 10/11/12` and `OSC 4` color queries from the
  active theme's terminal palette, so TUIs detect dark/light mode correctly (and
  the answer always matches what's painted).
- **Exit codes** — a pane's child exit status is captured, so close-on-exit and
  session-resume recovery can tell a clean `exit` from a crash (a deliberate quit
  no longer triggers resume recovery).
- **Crash tombstones** — a pane whose process dies abnormally (non-zero exit, or
  the PTY failing outright) is never auto-closed: it keeps its final screen under
  a "process exited — code N" banner, fires an error in the NOTIFICATIONS feed
  (plus a desktop notification when unattended), and Restart relaunches in place.
  Only a clean exit (code 0) qualifies for auto-close. A process that was *killed*
  is named as such — "process killed — signal Hangup/Killed/Terminated" — instead
  of being reported as a crash, since the OS gives a signalled child no exit code
  of its own and it would otherwise be indistinguishable from `exit(1)`.
- **Event log** — pane lifecycle events (every exit with its code and signal, every
  close, auto-closes, PTY read errors) are appended to `muxel.log` in the data dir
  (rotated at 1 MB), so "why did this pane disappear?" is answerable even when
  the app runs with stderr discarded.
- **Content inset** — a small margin around the grid so a too-wide TUI truncates
  inside the pane rather than against the border.
- **Key routing** — `Tab` / `Shift+Tab` go to the focused terminal rather than
  moving UI focus. `Shift+Enter` / `Alt+Enter` send `ESC CR` so agent TUIs
  (Grok, etc.) can insert a soft newline; plain `Enter` stays CR (submit).
- **Agent-first plain Ctrl+letter** — muxel app shortcuts that are plain
  `Ctrl+A`…`Ctrl+Z` (no Shift) do **not** fire while a terminal is focused, so
  agents receive them as normal C0 chords (Claude `Ctrl+S` stash, shell
  `Ctrl+R`, …). Muxel chrome prefers `Ctrl+Shift+*`. Exceptions that stay global
  in a terminal: `Ctrl+T` (new tab). `Ctrl+P` is special-cased (palette only when
  no terminal is focused). Extra chords can still be listed under Settings →
  Keybindings → terminal passthrough.
- **Ctrl+P shared with the agent** — the command palette is on `Ctrl+Shift+P`
  (always), while `Ctrl+P` opens it only when no terminal is focused — so a focused
  agent (e.g. opencode) receives it. Deselect the pane (click the toolbar) and
  `Ctrl+P` reaches muxel again.

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

- **Empty-workspace onboarding** — a fresh workspace shows a centered get-started
  screen (the muxel mark, an **Add a project** folder picker, a **New remote
  project (SSH)** shortcut into the wizard, and the keyboard-shortcuts chord) in
  the work area until the first project is added.
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
- **Reorder & rearrange** — reorder projects by dragging a row, or right-click a
  project → **Move up** / **Move down** (disabled at the ends) for an explicit,
  discoverable alternative; the order persists. Swap/move instances between panes
  from the sidebar.
- **Instance names** — custom names with inline rename and right-click menus.
- **Resizable sidebar** — drag to resize (up to half the window); width persists.
- **Fullscreen mode** — `F11` (rebindable) toggles OS fullscreen with the sidebar
  fully hidden. A floating pill at the left edge brings the sidebar back without
  leaving fullscreen; `F11` again exits and restores the previous sidebar state.
- **Multi-monitor** — right-click a project → **Open on display N** to give it a
  full muxel window (toolbar + panes) on that monitor; switch projects and panes
  there like in the main window. It opens with the **sidebar hidden** — the window
  exists to show one project, so the project list starts out of the way; its title
  bar's toggle (or Ctrl+Shift+B) brings the sidebar back for that window alone.
  One window per project: selecting a project that's open elsewhere **raises** its
  window instead of stealing it.
  Every project window's monitor + exact position/size is saved **in the
  workspace**, so reopening the workspace restores each window right where it
  was — dragging a window to another monitor updates its pin, and a
  disconnected monitor keeps the pin for when it returns. **Bring back to
  main window** or closing the window returns the project. Heavy chrome
  (settings, command palette, notification feed) stays in the main window, which
  is raised automatically when needed — but a confirmation about a *pane*
  ("Close terminal?", "Close other tabs?") opens in the window showing that
  pane, and raises it, so the prompt is never stranded on another monitor.
- **No auto-created project** — start empty; add projects via a folder picker.
- **Startup agents** — save the project's open agents as a startup set (preset +
  worktree flag) and relaunch them in one click from the project menu.

## Remote development (SSH)

- **Remote projects** — create a project that lives on a remote host over SSH
  (the project list's network button → pick a host, enter the remote directory,
  optionally verify it). Shells and agents then run on the remote, in a pane that
  behaves exactly like a local one. Local muxel still owns the UI, layout, and
  settings.
- **Scan for remote projects** — in the new-remote-project wizard, "Scan for
  projects" searches the host for existing muxel projects (`.muxel/workspace.json`
  markers, heavy dirs pruned) and lists the found roots; clicking one fills in the
  directory and name so you can open it without typing the path. Mirrors the iOS
  companion app's host scan.
- **SSH host library** — Settings → Remotes manages saved hosts with the common
  options: hostname/alias, port, user, auth (ssh-agent, key file, or password),
  ProxyJump, agent forwarding, host-key policy, keepalive, compression (for slow
  or high-latency links), and extra `-o` options. Two safe defaults are applied
  automatically: a `ConnectTimeout` so an unreachable host fails promptly instead
  of hanging a pane, and `IdentitiesOnly` when an explicit key file is set (so ssh
  doesn't offer every agent key first and trip the server's `MaxAuthTries`) — both
  overridable via a matching extra `-o`. A "Test connection" button verifies a host.
- **Changed host key dialog** — when a host's key no longer matches `known_hosts`
  (a reinstalled server — or a man-in-the-middle), connection tests, project
  connects, and remote git operations raise an actionable dialog instead of a raw
  OpenSSH error: the stored and newly presented `SHA256:` fingerprints side by
  side (mirroring the iOS companion's prompt) with a destructive **Trust new
  key** button that removes the stale entry via `ssh-keygen -R` (hashed entries
  and `[host]:port` forms included) and retries — the reconnect then re-pins the
  new key through ssh's `accept-new`. Cancel keeps refusing. Host-key state lives
  entirely in OpenSSH's `known_hosts`; muxel keeps no key store of its own.
- **Login identities** — Settings → Identities defines reusable logins (a name +
  user + auth + key file or keychain password). A host can reference an identity
  instead of entering credentials inline, so one login is defined once and shared by
  many hosts (and rotated in one place). Selecting an identity on a host hides its
  inline credential fields; the password is stored once in the keychain under the
  identity and reused across every host that references it. Deleting an identity
  falls its hosts back to inline/ssh-agent.
- **Secure passwords** — saved SSH passwords are stored in the OS keychain (Secret
  Service / macOS Keychain / Windows Credential Manager), never in muxel's config,
  and fed to ssh via `sshpass`. Password auth requires `sshpass` (Linux/macOS only;
  the panel warns when it's missing) — Windows uses key-file or ssh-agent auth,
  which work everywhere.
- **Resilient sessions** — remote panes default to a persistent tmux session on
  the host, so a dropped connection is survivable: reconnecting re-attaches the
  still-running agent. One multiplexed SSH connection per host is shared by the
  pane and all git calls. Launching a tmux session (remote, or a local tmux-mode
  project) enables tmux `mouse on`, so the pane's scroll wheel scrolls tmux's own
  copy-mode history instead of just the visible screen.
- **Roaming layouts** — a remote project's pane layout is mirrored to the host at
  `<remote_root>/.muxel/workspace.json`, so opening the same project from another
  machine restores the whole session (the tmux-backed panes re-attach to their
  still-running agents). muxel pushes the layout as you rearrange panes and, on
  connect, loads whichever copy — local or remote — is newer; the replaced copy is
  kept as a one-level backup on each side. Automatic for every remote project; no
  setup required. **Renaming a pane** also syncs live between peers: while connected,
  each side re-reads the shared file every few seconds and adopts a peer's renamed
  pane label in place (no teardown), so a rename on desktop or iOS shows up on both.
  (Structural changes — adding/removing panes — still reconcile on the next connect.)
- **Shareable local projects** — a *local* project also mirrors its layout to
  `<root>/.muxel/workspace.json` (and git-ignores `.muxel/`) when tmux mode is on,
  so its panes are tmux sessions a peer can attach to. The iOS companion app can
  then SSH into the machine, read that file, and bring up / drive the same panes —
  the desktop didn't need to be opened as a "remote" project. The shared
  `tmux_session` name keeps a pane addressing the same session from either side.
- **Reconnect on failure** — when a remote project's SSH connection fails (or
  drops), the pane area shows the error with **Reconnect** (re-runs the connect
  pre-flight, re-syncs the layout, respawns panes) and **Scan for projects**
  (opens the wizard preset to that host and scans it) buttons; both are also in
  the project's right-click menu, so a live-but-flaky connection can be retried
  without reselecting the project.
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
- **Single instance per workspace** — each workspace is locked while open, so two
  muxel windows can run side by side on **different** workspaces but never the same
  one (which would clobber its layout). Picking a workspace another window already
  holds is refused in the selector with an inline "in use" note; pick a different one
  or close the other window. The lock releases when you switch workspaces or on exit
  (even a crash), so no stale lock blocks the next launch.
- **Full restore** — pane layout, split sizes, window geometry, and sidebar width
  are persisted and restored on launch.

## Settings & theming

- **Settings modal** — sections for Appearance, Editor, Behavior, Agents, Runners,
  Snippets, Loops, Remotes, Projects, and Keybindings.
- **Themes** — ~22 bundled themes with a switcher (Catppuccin, Gruvbox, Tokyo
  Night, Solarized, Ayu, Everforest, and more).
- **Sizing** — whole-app zoom plus independent UI, terminal, and code/diff font
  sizes.
- **Keybindings** — configurable shortcuts with a rebind UI, a cheat-sheet overlay
  (`Ctrl+Shift+/`), `Alt+1–9` to jump to a pane's Nth tab, `Ctrl+1–9` to switch to
  the Nth project, `Ctrl+Alt+1–9` to open a new pane running the Nth agent preset,
  `Ctrl+Shift+G` to toggle the "new agents get a git worktree" switch, `Ctrl+Shift+A`
  to focus the next agent needing attention (blocked, then done), and `Cmd+Q`
  (`Ctrl+Q` elsewhere) to quit from any focus.
- **Behavior** — immediate-save appearance, confirm destructive actions, quit
  confirmation, per-kind close confirmation (terminal on, editor/diff off by
  default), and auto-close a pane when its process exits **cleanly** (an
  abnormal exit always leaves a tombstone pane instead). The terminal
  confirmation is skipped for an untouched **shell** pane — one sitting idle at
  its prompt with no foreground command and no other tabs — since closing it
  loses nothing.
- **Local tmux by default** — "New agents run in a tmux session" defaults **on**
  whenever `tmux` is installed, so local panes survive a muxel restart and reattach
  (matching remote panes); the toggle greys out and has no effect when tmux isn't
  found. tmux is unix-only, so this never applies on Windows.
- **Agents survive a stray `pkill`** — muxel starts the tmux server itself, from a
  command line naming no project, so an agent running `pkill -f <project>` (to clear
  its own dev server) can't match the *shared* server and kill every session with it.
  Such a `pkill` reaches only that pane's tmux client: the session and the agent keep
  running, and muxel **reattaches the pane automatically** — you see it blink, not die.
  Local and remote alike (SSH panes and the iOS companion do the same), since a host
  has one tmux server shared by every session on it.
- **Killed tmux sessions come back** — if the tmux session (or the whole server) dies
  anyway, the pane doesn't tombstone: muxel recreates the session and relaunches the
  agent with `--resume`, so a resume-capable agent picks its conversation back up where
  it left off (tmux scrollback is the only casualty). A deliberate `tmux kill-session`,
  and an agent simply quitting, still close the pane normally. The feed says which
  happened — *reattached* (session survived) or *session restored* (agent resumed).
- **tmux lifecycle** — closing a **pane** always kills its tmux session (local or
  remote); a *dropped* SSH connection never auto-closes — it leaves a tombstone
  pane, keeping the remote session reconnectable. Quitting the **app** leaves
  sessions alive by design (they reattach next launch): when any exist, the quit
  dialog offers two checkboxes — **Also kill local tmux sessions** and **Also
  kill remote tmux sessions** — both off by default; the kills are
  fire-and-forget (remote ones reuse the warm SSH connection), so quitting never
  waits on a slow host.

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
