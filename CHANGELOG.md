# Changelog

All notable changes to muxel are documented here. This project adheres to
[Semantic Versioning](https://semver.org).

## [Unreleased]

### Fixed
- **Type-while-stream no longer thrash-paints the focused terminal** — paste into
  a busy agent and steer in the same pane used to schedule a full terminal paint
  on every PTY batch. Muxel now uses alacritty grid damage for partial draw-list
  rebuilds and a paint-priority policy: user echo (~8 ms cadence) beats stream
  frames (~30 Hz focused, ~10 Hz background). See
  `docs/terminal-paint-architecture.md`.

### Added
- **Remote sessions survive a dropped connection, and reattach on launch** — a lost
  SSH connection (Wi-Fi blip, laptop sleep, host reboot) used to freeze a remote pane
  on a dead socket, so muxel never noticed and the session read as "exited" when you
  reopened. Every SSH connection now keeps itself alive with periodic probes
  (`ServerAliveInterval`, ~60s to detect a drop; set per host in Settings → Remotes,
  blank = 20s default, `0` disables), so a drop is *detected*: the pane shows
  **"Connection lost — reconnecting…"** instead of "exited" — its tmux session is
  still running on the host — and muxel retries on its own until the host is reachable
  and reattaches the agent where it left off. On startup it also reconnects the tmux
  panes of *every* remote project in the background (not just the active one), so
  agents left running on your hosts come back automatically; hosts needing an
  unsaved password are skipped and reconnect when opened.

### Changed
- **Auto-continue also nudges on a soft "I'd hold here" check-in** — a message that
  parks but still offers more work ("I'd hold here unless you want that scaled run.",
  "if you want me to, I can…") now counts as a check-in and gets a `continue`, the
  same as an explicit "Shall I continue?". A message that instead declares it's out
  of work ("nothing further I can do") still stops it — completion is checked first.

## [0.1.4] — 2026-07-18

### Added
- **Auto-continue keeps a stalled agent going** — an agent that plans several phases
  sometimes finishes the first and stops, waiting, with its todo list still
  half-unchecked. Each agent pane now has an **Auto** toggle: while it's on, muxel
  watches the pane and types `continue` (and Enter) whenever the agent goes idle
  with unfinished tasks visible — Claude's `☐` checkboxes or an "N pending" count.
  It also nudges when the agent voluntarily stops to check in ("My recommendation is
  to pause here.", "Shall I continue?"), even with no todo list on screen. "Idle" is
  judged by the screen going still, not by a status guess, so it won't fire over an
  agent that's plainly mid-work (spinner still turning), and it keys the *next* nudge
  off the agent producing something new, so it keeps a multi-phase plan going even
  when the agent finishes a phase and re-pauses in a blink. It won't
  nudge a finished agent, won't answer a permission prompt (that needs a real
  decision from you), and — if `continue` fires a few times and the screen never
  changes at all, i.e. a dead loop — stands down on its own and tells you rather
  than hammering forever. A responsive agent that keeps answering with fresh work
  (even when its tasks are blocked and no checkbox moves) is not given up on — but
  once the agent says it has nothing left it can do ("no responsible work left",
  "nothing further I can do"), auto-continue stops and tells you, so a finished
  agent isn't nudged in circles. Off again after a restart.

## [0.1.3] — 2026-07-14

### Added
- **Wake every agent with a spoken command** — an opt-in toggle in Settings → Speech:
  say the wake phrase (default *"wake up daddy's home"*) into the mic and muxel walks
  every agent pane of every project in turn, relaunching each one whose process isn't
  running, then lands back on the pane you started from. Panes still running are left
  alone, and a pane whose launch previously failed outright is genuinely retried
  rather than skipped. Nothing is drawn over the workspace — the sweep moves through
  the real panes, so you watch them come back — and the tally lands in the
  notifications feed. The transcript that triggers it is treated as a command, not
  dictation: it is never typed into an agent. Matching ignores case, punctuation and
  surrounding filler, so however whisper hears it ("Wake up, Daddy's home!") it fires,
  and the phrase itself is editable.
- **Browser panes get a Forward button, and Reload reloads the page you are on** —
  Reload was injected JavaScript (`location.reload()`), which fails exactly where a
  refresh matters most: error pages, `about:blank`, and sites with a strict content
  security policy. It now uses the webview's native reload, which refreshes the
  document you are actually on — several links deep, if that is where you are —
  rather than the pane's original URL. Forward joins the existing Back beside it.
- **The pane area scrolls horizontally when the panes don't fit** — a pane will not
  shrink below a usable terminal width, so side-by-side panes add up and a layout is
  not guaranteed to fit the window it opens in: open one built on a large monitor
  (typically by pulling it from a remote host that had one) on a smaller display and
  it needs more width than there is. The surplus panes used to lay out past the right
  edge of the window, where they were invisible and unreachable — no scrollbar, no
  clipping. The pane area now scrolls across them instead. Panes still shrink to fill
  a window they do fit in, and scrolling only begins once they can shrink no further,
  so a window that was always big enough is unchanged.

### Fixed
- **Clicking into a browser pane makes it the active pane** — on macOS and Windows the
  embedded webview is a real child window stacked above the rest of the UI, so the OS
  consumed the click and muxel never saw it: the pane kept its old highlight, and a
  Ctrl+V went to whichever pane was focused before — usually pasting into a terminal.
  The page now reports its own clicks, so clicking one focuses its pane and hands it
  the keyboard. muxel's own shortcuts keep working until you click into a page.
- **Remote panes on a Mac host no longer die with `command not found: tmux`** — sshd
  runs a remote command through a shell that is neither login nor interactive, so it
  reads no profile and gets sshd's bare default `PATH`
  (`/usr/bin:/bin:/usr/sbin:/sbin`). A Mac's tmux comes from Homebrew — on Apple
  silicon `/opt/homebrew/bin/tmux` — which is on none of those, so every remote pane
  against such a host closed the instant it opened, shell and agent alike. muxel now
  names the standard prefixes (Homebrew, MacPorts, Linuxbrew, snap, `~/.local/bin`,
  Nix) when it runs `tmux` on a host, for panes, session listing, and session
  teardown alike. They are *appended* to the remote `PATH`, so a host that already
  finds tmux keeps using the exact binary it uses today. The iOS companion app,
  which builds the same commands, is fixed with it.
- **muxel builds again against a current clang** — `whisper-rs` 0.14's bindgen
  mis-sized a whisper.cpp struct under clang 22 (Homebrew's current), so its own
  generated layout assertion failed the build (`attempt to compute
  1_usize - 264_usize`) and no build could complete on such a machine, whatever it
  was building. Upgraded to `whisper-rs` 0.16, whose bindings clang 22 generates
  correctly; local speech-to-text moves to the whisper.cpp it wraps.
- **Icons render for a Windows account that can't read the builder's profile** — in
  debug builds, rust-embed loaded gpui-component's icons from the cargo checkout on
  disk rather than from the binary. A secondary Windows identity (a standard
  AgentWorker account launched via `runas`) cannot read the primary user's profile,
  so those icons never loaded and the title-bar min/max/close controls — and others
  besides — rendered as empty gray plates. Debug builds now bake the icons in, the
  way release builds always did. Thanks to
  [@railapex](https://github.com/railapex)
  ([#11](https://github.com/ProjectHax/muxel/pull/11)).

## [0.1.2] — 2026-07-12

### Added
- **Add an SSH host straight from "New remote project"** — the dialog's host list
  now ends with an **Add host** button (and the empty state leads with it) that
  jumps to Settings → Remotes with a fresh host editor open, so you can add a host
  without leaving to hunt through settings first.
- **The file browser shows git status, and can stage from the tree** — every row now
  carries its git mark (`?` untracked, `A` staged, `M` modified, `D` deleted, `!`
  conflicted), and a **folder carries the strongest status beneath it**, so a
  collapsed folder still tells you something inside it has never been added. Right-
  click a row with anything to stage for **Add to git**; on a folder it stages
  everything under it. Local and remote alike — for a remote project the `git add`
  runs on the host.
- **Remote projects pick their running agents back up** — a tmux session is only
  ever reachable *through* an instance, by name, so an instance that goes away
  strands the agent still running inside it: invisible to muxel, holding the host's
  resources, and impossible to get back. Opening a remote project now lists muxel's
  sessions on the host and re-attaches a pane to any in that project's tree that no
  instance owns, resuming the agent mid-conversation instead of starting a second
  one beside it. Sessions are attributed by their working directory, not their name
  (the name's slug may be the project's or the host's), and only muxel's own
  sessions are ever taken.
- **Dictation tells you when the microphone is blocked, and offers a way in** — a
  macOS app that has been denied the microphone isn't told so: CoreAudio hands it
  silence, which muxel could only report as "no speech captured". Dictation now
  asks macOS for the permission status up front and, when access is denied, says so
  and offers an **Open Settings** button that opens Privacy & Security →
  Microphone. The button appears only when the OS says access is denied — an app
  that has never *requested* the microphone isn't listed on that screen at all, so
  offering it for a merely-absent mic would send you to an empty list.

### Changed
- **"Open shared memory" opens the memory panel** — the project menu item (and the
  command palette's *Open memory*) opened `MEMORY.md` as raw markdown in an editor
  pane. Both now open the docked memory panel beside the sidebar, where entries are
  listed, searchable, pinnable, and editable. The raw file is still one click away,
  from the panel's own "Open MEMORY.md in editor" button.

### Fixed
- **The file browser hid every dot-folder on a local project, and truncated big
  trees** — dotfiles *are* project files (`.github`, `.cargo`, `.gitignore`,
  `.muxel`), but the local walk used the `ignore` crate's default of skipping hidden
  entries, so none of them appeared — while the same project opened over SSH, listed
  with `git ls-files`, showed them all along. (On muxel's own repo: 249 files locally
  vs 255 remotely.) Both sides now list them, and `.git` itself stays out. The
  per-project cap also rose from 10,000 files to 100,000: past the old limit a folder
  whose files all fell beyond the cut simply vanished, which reads as a broken browser
  rather than a limit. The tree is still gitignore-aware, as documented.
- **Shared memory showed as off on a remote project that was plainly using it** —
  `.muxel/MEMORY.md` lives at the project root on the host, and every agent working
  there (desktop's panes, the iOS app's) reads and writes that one file — but the
  *enabled* flag never left the machine it was switched on from. So a project whose
  host had shared memory in active use still showed the toggle off elsewhere. The
  flag now travels with the project in the layout doc peers already exchange, and a
  doc written before it existed says "no opinion" rather than "off" — muxel then
  infers it from the evidence (the host has a memory file, so memory is in use) and
  records the answer for every peer. iOS carries the field through untouched, so it
  can't erase an opinion desktop recorded.
- **A re-attached tmux pane opened with mangled spacing until you typed** — the PTY
  was always opened at 80×24 and only resized once the pane had been laid out. A
  program starting fresh doesn't care (it draws after the resize), but a `tmux
  attach` does: the session's agent painted its UI long ago, so tmux replays it at
  80×24, and the agent redraws only when something prompts it to — a keystroke. Panes
  now remember the grid they last rendered at and reopen at that size, so nothing has
  to resize after the first paint. A new tab takes its size from the pane it joins
  (tabs share bounds), which also covers a session adopted from a host.
- **Remote panes made a new tmux session every launch instead of re-attaching, and
  orphaned the old one** — an instance's session name was resolved two different
  ways. When tmux is enabled muxel *records* `muxel_<project>_<id>` on the instance,
  and the iOS app launches from exactly that recorded name; the desktop's remote
  path ignored it and recomputed `muxel_<host>_<id>`. So the same instance ran under
  two names: the desktop attached to neither the session iOS had created nor the one
  it had left behind itself, minting a duplicate each time — and its teardown, which
  killed the *recomputed* name, never reaped the session it was actually running, so
  they piled up on the host. Every site that launches, checks or kills a session now
  resolves it one way (`tmux::session_for`): the name recorded on the instance wins,
  and a canonical one is derived only when it has none — the same rule iOS follows.
  Quit-time cleanup also no longer skips remote panes that inherit tmux from the
  host default rather than carrying a recorded name.
- **A remote agent pane connected, then instantly quit with `[exited]`** — while a
  remote *shell* pane worked fine. tmux `execvp`s the program it is given with the
  environment of the tmux **server**, which on a remote host is forked from a
  non-interactive ssh command — so its `PATH` is sshd's bare default
  (`/usr/local/bin:/usr/bin:…`). Agents live in a user prefix (`~/.local/bin`, an
  nvm dir) that only a shell profile adds, so `-- claude` died with ENOENT the
  moment the pane opened: the window closed, the session died with it, and ssh
  exited 0 — which muxel read as "finished cleanly" and auto-closed the pane. A
  shell pane escaped it because tmux starts *its* default shell as a login shell.
  Remote programs now run through the user's login shell (`$SHELL -ilc 'exec …'`),
  resolving on the same `PATH` they'd get on that host by hand — the fix the iOS app
  already carried. Applies to remote panes with and without tmux.
- **Every remote action failed on macOS with "keyword controlpath extra arguments
  at end of line"** — scanning a host for projects, remote panes, remote git. ssh
  reads each `-o` argument as a line of `ssh_config` and splits the value on
  whitespace, so muxel's unquoted `-o ControlPath=…` broke apart: the connection
  multiplexing socket lives under the platform data dir, and on macOS that is
  `~/Library/Application Support/…`. ssh rejected the command before connecting.
  The value is now quoted when it contains whitespace.
- **Splitting a tab out into its own pane didn't work on its own pane** — dragging
  a tab onto a pane edge pulls it out into a new split, but doing so on the tab's
  *own* pane was a no-op when the tab was the pane's first one (the drop anchor is
  the first tab, and moving a tab beside itself bailed out). It now splits the tab
  out beside its siblings, so you can peel any tab off into a new pane in place.
- **Garbled glyphs in tmux panes on macOS** — box-drawing and agent glyphs (an
  agent's `⏵⏵ auto mode`, Claude's prompt box) came out as `_` or blanks, and no
  redraw would repair them. tmux picks its UTF-8 mode from `LC_ALL`/`LC_CTYPE`/
  `LANG`, and when those say nothing its client rewrites every non-ASCII cell as
  `_` on the way to the terminal — so the damage was done before muxel ever saw
  the bytes, which is why the pane content itself (`capture-pane`) was fine. macOS
  is where it bites: a GUI app inherits no locale (`launchctl getenv LANG` is
  empty), so a muxel opened from Finder passed none to its children. The tmux
  client is now launched with `-u` (forced UTF-8, and it covers remote panes, whose
  locale belongs to the far host), and local children get a UTF-8 `LANG` when they
  would otherwise inherit no locale at all. A locale you *have* set is never
  overridden.
- **Dictation on a Mac with no microphone reported "microphone error: query
  default input config"** — on a machine with no audio input (a Mac mini or Mac
  Studio with nothing plugged in), CoreAudio still answers the default-input-device
  query, handing back a device that then fails every call made on it. muxel took
  that phantom device to mean a mic existed, so instead of saying there was no
  microphone it surfaced the internal failure of the next call. It now decides
  whether a mic exists from the *enumerated* inputs and says **no microphone found
  — connect an input device**. Dictation also fails at the moment you press the mic
  rather than after you've spoken: the device is opened up front, so a missing mic
  no longer shows "Recording…" for a capture that was never running.

## [0.1.1] — 2026-07-11

### Added
- **Browser is now a preset** — a built-in **Browser** preset opens a web-browser
  pane; pick it anywhere you pick an agent (the new-pane dropdown or the
  hold-a-split-button chooser). Its homepage is configurable in Settings → Agents
  (default `duckduckgo.com`; a bare domain gets `https://`), and you can add more
  browser presets. Embedded pane on macOS/Windows; a separate browser window on
  Linux.
- **Speech-to-text dictation** — a toolbar mic button (`Ctrl+Shift+M` toggle, or
  hold `Ctrl+Shift+H` for push-to-talk) records the microphone and types the
  transcript into the focused agent's prompt, unsubmitted for review (optional
  auto-submit). Pick the engine in Settings → Speech: **Local** whisper.cpp runs
  offline on this machine (the model downloads once on first use), or a
  **Provider** — any OpenAI-compatible `/audio/transcriptions` endpoint (OpenAI,
  Groq, self-hosted) via base URL + model + a keychain-stored API key. Provider
  mode uploads audio to that endpoint; local mode stays on-device.

### Fixed
- **In-app updater on macOS** — the update dialog's download button was dead and
  fetched nothing; it now downloads and applies the macOS build correctly.
- **Linux: stale AppImage mounts no longer accumulate** — a muxel instance run
  from an AppImage that crashed or was killed left behind a dead squashfuse mount
  under `/tmp/.mount_muxel-*`. As leftovers piled up, any filesystem scan (e.g. a
  desktop system monitor's periodic `df`) would stall in the kernel FUSE layer on
  the dead mounts, surfacing on Wayland as a periodic cursor stutter that got
  worse the longer the machine stayed up. muxel now reaps these dead mounts on
  startup (leaving live mounts from other running instances alone).

## [0.1.0] — 2026-07-10

### Added
- **Grok session resume** — the built-in Grok preset now sets `--session-id` /
  `--resume` like Claude, so a Grok pane reopens its prior conversation after a
  muxel restart. Existing Grok presets pick up the flags on seed upgrade (unless
  you already set your own).
- **Codex preset + session resume** — built-in Codex agent preset. Codex mints its
  own session id (no create-time flag), so muxel starts bare, then on restart
  captures the latest `~/.codex/sessions` rollout for the project cwd and launches
  `codex resume <id>`. Resume support now allows `resume_flag` without
  `session_id_flag` for agent-minted CLIs.

### Changed
- **Ctrl+click reuses a project's browser pane** — opening a link navigates the
  browser already open in that project instead of stacking another native webview
  beside it. A popped-out browser keeps its own window and is left alone.
- **Popped-out project windows open with the sidebar hidden** — the window exists to
  show one project on its own monitor, so the project list starts out of the way. Its
  title bar now carries the same sidebar toggle the main window has (the minimal bar it
  used before had none), and Ctrl+Shift+B works there too. Each window remembers its own
  sidebar state: hiding it in a project window no longer hides it in the main window.

### Fixed
- **Opening a workspace or a link with a browser pane aborted on Windows**
  (`0xc0000409`) — WebView2 builds its controller by running a nested Win32
  message pump. Built inline, that pump re-entered gpui while `App`'s `RefCell`
  was still mutably borrowed by the update creating the pane, so the first
  foreground task it ran (a terminal's PTY reader) panicked with "RefCell already
  borrowed". The webview is now built by awaiting wry's `build_as_child_async`,
  which never pumps, from a task that holds no borrow.
- **Browser panes can be popped out** — pop-out silently re-docked a browser pane
  instead of detaching it. A native webview belongs to the window that created it,
  so the pane is now re-created in the pop-out window at the same URL, and rebuilt
  in the main window when docked back.
- **Native webviews outlived their workspace** — switching workspaces dropped the
  editors but kept every `BrowserView`, leaving orphaned WebView2 children under
  instance ids the new workspace would reuse.
- **Ctrl+S (and other plain Ctrl+letter) reached muxel instead of the agent** —
  `SaveFile` was bound globally, so Claude's stash never saw `0x13`. Plain
  `ctrl-<letter>` app bindings now default to `!Terminal` (agents get C0);
  muxel chrome stays on `ctrl-shift-*`. `Ctrl+T` new-tab remains global;
  Settings → terminal passthrough still covers other shapes.
- **Shift+Enter newline in agent TUIs (Grok, etc.)** — bare CR cannot carry
  modifiers, so Shift/Alt+Enter now send `ESC CR` (the sequence those agents
  already treat as a soft newline). Plain Enter is still CR.
- **Terminal image paste and file drop** — Alt+letter is sent as ESC+letter so
  Claude Code's Alt+V image paste works; plain Ctrl+V is host-side smart paste
  (text/paths into the PTY; images forwarded as 0x16 for agents like Grok);
  drag-and-drop pastes shell-quoted paths into the focused terminal; Shift+Insert
  pastes and Ctrl+Insert copies.
- **Terminal file links on Windows** — `file://` URIs used a broken drive encoding
  (`file://D%3A%5C…`), so Ctrl+click looked like a no-op. URIs are now
  `file:///D:/…`, local files open in a muxel editor pane, OSC 8 bare paths
  resolve against the pane cwd, and Ctrl/Cmd down re-hit-tests so the underline
  appears without a mouse move.
- **Terminal mouse reporting for agent TUIs** — when an app enables mouse mode
  (Grok, Claude, vim, …), clicks, drags, and motion are forwarded as SGR/X10
  events so the app receives them (focus the prompt, scroll its own content).
  Hold Shift for local text selection. Wheel reporting was already present;
  button press/release and motion complete it.
- **An agent's `pkill` could kill every agent in every project** — tmux forks its server
  from the first client that needs one and the server keeps that client's command line.
  Since 0.0.9 made local panes default to tmux, that first client was a pane's
  `tmux new-session -A -s muxel_<project>_… -c <project root>`, so the *shared* server's
  argv carried a project name. One server hosts every session, so an agent running
  `pkill -f <project>` to clear its dev server SIGKILLed the server and, with it, every
  muxel session and every agent inside them.
  muxel now starts the server itself from a command line naming no project
  (`tmux start-server ; set -s exit-empty off`, restored on quit), and drops the
  redundant `-c <project root>` from each pane's client. Such a `pkill` can now reach
  only that pane's own tmux client — the session and its agent keep running, and the
  pane **reattaches automatically** instead of leaving a tombstone. The same guard runs
  on remote hosts, from both the desktop's SSH panes and the iOS companion, since a host
  has one tmux server shared by everything on it.
- **A killed tmux session no longer strands the pane** — when the session or the whole
  server dies (the client reports a bare exit 1, no signal), muxel recreates the session
  and relaunches the agent with `--resume <session id>`, restoring the conversation from
  its transcript; only the tmux scrollback is lost. A deliberate `tmux kill-session`, and
  an agent exiting on its own, both leave the client at 0 and still close/tombstone as
  before. The feed and `muxel.log` distinguish *reattached* (the tmux session survived)
  from *session restored* (it didn't, and the agent was resumed).
- **"Close terminal?" opened on the wrong monitor** — pane confirmations always drew in
  the main window, so closing a pane in a project window popped the prompt up over on
  the main window's display, leaving the project window looking frozen. A confirmation
  about a pane (**Close terminal?**, **Close other tabs?**, **Close tabs to the side?**)
  now renders in the window showing that pane and raises it. Everything else (settings,
  git-panel and host-key dialogs) is main-window chrome and stays put.
- **Closing a popped-out project window quit the whole app** (Linux) — the window drew
  the *first-run* title bar, whose close button quits outright because nothing is
  running yet at that point. Its X now closes only that window, and the project returns
  to the main window and is selected there, exactly as **Bring back to main window**
  does. macOS and Windows were unaffected (the OS draws the close button), and the
  return-to-main step now runs on every close path, so all three behave alike.

## [0.0.9] — 2026-07-09

### Changed
- **Local panes use tmux by default** — "New agents run in a tmux session" now defaults
  **on** wherever `tmux` is installed, so a local pane survives a muxel restart and
  reattaches, the way remote panes already did. On a host without tmux the toggle greys
  out (reading "tmux not installed") and nothing changes; your saved preference is left
  alone, so installing tmux later restores your choice rather than silently flipping it.
  Windows is unaffected — muxel's tmux integration is unix-only.

### Fixed
- **A killed pane was reported as a crash** — the OS gives a signalled child no
  exit code of its own, and `portable-pty` substitutes `1`, so a pane that was
  SIGKILLed or hung up was indistinguishable from one that called `exit(1)`. The
  signal name was available and thrown away. muxel now records it: `muxel.log`
  gets `signal=Killed`, and the tombstone reads "process killed — signal Killed"
  rather than "process exited — code 1". This is what tells "muxel closed the
  PTY" (`Hangup`) apart from "something killed the agent" (`Killed`).
- **An agent's `pkill` could wipe out every pane in its project** — with shared
  memory enabled, muxel appended the project's *absolute* path to each agent's
  `--append-system-prompt`, and that lands in the process's argv, which is what
  `pkill -f <pattern>` matches. So an agent running a routine `pkill -f myproject`
  to clear its dev server also SIGKILLed every muxel pane in that project,
  including its own — all in the same second, looking exactly like a mass crash.
  The prompt now refers to `.muxel/MEMORY.md` relative to the agent's working
  directory, naming nothing. (Agents in a worktree, whose cwd isn't the project
  root, still get the absolute path since a relative one wouldn't resolve; local
  agents also still receive it out-of-band via `$MUXEL_MEMORY_FILE`, which argv
  matching can't see.)
- **Windows: npm agents (Codex, etc.) fail to spawn with os error 193** — npm
  installs an extension-less `#!/bin/sh` shim next to `agent.cmd`. portable-pty's
  PATH search returns the shim first, and CreateProcessW rejects it as not a
  valid Win32 application. muxel now resolves bare program names preferring
  PATHEXT (`.cmd`/`.exe`) and skipping shebang shims before spawn. The same
  resolution fixes a bare program name whose only PATH match is such a shim,
  which failed with os error 2 (file not found).
- **Linux AppImage failed to start on modern distros** — the AppImage bundled a
  copy of GLib (and the rest of the GTK/WebKit dependency closure) from the
  Ubuntu 22.04 build runner. It shadowed the host's newer GLib, so the host's
  own `libgtk-3` → `libjson-glib` could not resolve `g_once_init_leave_pointer`
  and the app died at startup on any glib ≥ 2.80 system (RHEL 10, Fedora 40+,
  Ubuntu 24.04). muxel links the *system* GTK/WebKitGTK, so the whole stack now
  comes from the host — which also drops ~60 MB of bundled GTK/WebKit libraries
  (ICU, GStreamer, Pango, Cairo, …) that were never loaded.

### iOS companion app (distributed via TestFlight / App Store, not in these downloads)
- Broad polish pass: side-by-side agents on iPad, git worktree support, a richer launch
  flow, and file/diff viewers. Cross-project sidebar status is now batched into one
  round trip per host, with running/blocked count badges on project rows, a unified
  "can't reach host" state with a reconnect overlay, and pull-to-refresh.
- Fixed: an unknown pane kind (e.g. a browser pane) no longer makes a workspace
  unreadable, and the foreground poll loop resumes when the app becomes active again,
  so status dots and the Live Activity no longer go stale after backgrounding.

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

[Unreleased]: https://github.com/ProjectHax/muxel/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/ProjectHax/muxel/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/ProjectHax/muxel/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/ProjectHax/muxel/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/ProjectHax/muxel/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/ProjectHax/muxel/compare/v0.0.9...v0.1.0
[0.0.9]: https://github.com/ProjectHax/muxel/compare/v0.0.8...v0.0.9
[0.0.8]: https://github.com/ProjectHax/muxel/compare/v0.0.7...v0.0.8
[0.0.7]: https://github.com/ProjectHax/muxel/compare/v0.0.6...v0.0.7
[0.0.6]: https://github.com/ProjectHax/muxel/compare/v0.0.5...v0.0.6
[0.0.5]: https://github.com/ProjectHax/muxel/compare/v0.0.4...v0.0.5
[0.0.4]: https://github.com/ProjectHax/muxel/compare/v0.0.3...v0.0.4
[0.0.3]: https://github.com/ProjectHax/muxel/compare/v0.0.2...v0.0.3
[0.0.2]: https://github.com/ProjectHax/muxel/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/ProjectHax/muxel/releases/tag/v0.0.1
