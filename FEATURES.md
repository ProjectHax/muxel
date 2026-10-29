# muxel — Features

muxel is a GPUI-based, multi-agent terminal multiplexer: run several coding agents
(Claude, opencode, Amp, …) and shells side by side in a tiled, tabbed workspace
with first-class git worktrees, agent status tracking, and notifications.

This file is the canonical catalogue of what muxel can do. **When a user-facing
feature is added or changed, update the matching entry here in the same change**
(see `CLAUDE.md`).

## Panes & layout

- **Hotbar** — a bar along the bottom of the main window opens full views in place
  of the panes: **Dashboard** (every agent's status), **Changes** (the active
  project's git changes, with commit), **Memory** (its shared memory), **Files** (its
  file tree), and **Accomplishments** (every project's log). A view fills everything
  right of the project sidebar (docked panels and the git-diff panel step aside);
  switching projects in the sidebar retargets Changes, Memory, and Files. The active
  button is drawn filled with an accent bar above it, the bar names what's showing,
  and clicking the button again returns to the panes.
- **Recursive split layout** — panes form a horizontal/vertical split tree; any
  pane can be split again, nesting freely.
- **Resizable splits** — drag the divider between panes; sizes persist per project.
  Cached terminals receive the final size after the drag, so responsive TUIs redraw
  without requiring a keypress or scroll.
- **Minimum pane width** — panes can't shrink so narrow that an agent TUI becomes
  unusable (keeps a sane terminal width).
- **Scrollable pane area** — because panes have that minimum width, a layout with
  enough panes side by side needs more width than a small display has — most often
  a layout built on a large monitor and then reopened on a laptop, or pulled from a
  remote host that had one. The pane area scrolls horizontally in that case, so the
  panes that don't fit can still be reached instead of falling off the right edge.
  Panes still shrink to fill a window they do fit in; scrolling only begins once
  they can shrink no further. Drag its scrollbar, swipe a trackpad sideways, or
  hold **Shift** and use the wheel — a plain wheel is left to the pane under the
  pointer, so scrolling an agent's scrollback never drags the strip sideways.
- **Drag-to-dock split (Zed-style)** — drag a tab onto a pane edge to pull it out
  into a new split, or onto the center to add it as a tab; drag a pane by its
  title bar to relocate the whole pane, with a highlighted drop zone.
- **Swap panes** — drop a dragged pane on another pane's center to swap their
  positions.
- **Per-project focus** — switching away from a project and back restores the
  pane that was last focused there, falling back safely if that pane was removed.
- **Missing folders are created** — a pane whose folder doesn't exist on its host
  (the project's, a worktree's or its own launch folder: deleted, or never made on
  the machine a synced layout was opened on) creates it, parents included, before
  starting its program — locally and on a remote host. Before, the program started
  in the home folder instead. A local folder that can't be created stops the
  launch with the reason. A remote Windows pane using cmd still needs its folder
  to exist.
- **Responsive cold project restore** — opening a project whose agents have not
  started yet paints the selected pane first and admits the remaining terminal
  processes in bounded waves. On Windows the next pair starts only after every
  terminal in the project, including any an earlier activation started or is
  still launching, reaches provider readiness or settles after visible output; a
  terminal holds it for at most five seconds once its process is running, so a
  silent command cannot stall the rest. A launch still running after 20 seconds
  stops holding back the others and keeps showing "Starting terminal…" until its
  process comes up, or until Restart, closing the pane or leaving the project
  supersedes it. Editors, diffs and browsers open
  without waiting on terminals, and switching projects cancels the old project's
  pending waves.
- **Maximize** — temporarily expand one pane to fill the work area. The whole tab
  group comes along: every tab stays in the tab bar and switching tabs keeps the
  pane maximized, as does closing or popping out the tab being shown while others
  remain. While a pane is maximized, selecting another agent (sidebar,
  notification, tray) moves the maximize to it, so that agent comes up full-size
  instead of hiding behind.
- **Pane cards** — rounded "card" panes with an accent ring + glow on the active
  pane, hover highlight, and a configurable border style.

## Tabs

- **Tabs per pane** — each pane is a tab group; `Ctrl+T` opens a new tab in the
  active pane.
- **Non-blocking agent creation** — a new agent tab paints and focuses first; PTY
  creation and child startup run on a worker, with stale launches discarded if the
  tab closes or is replaced before startup completes.
- **Direct pane creation controls** — the pane-local tab button opens the agent
  picker and creates the chosen agent in that tab group. Split buttons clone the
  shown pane's agent in one click; right-click opens the alternate-agent picker.
- **Drag & reorder** — drag tab pills to reorder within a pane, move them to other
  panes, or drop at a precise insertion point.
- **Pinned tabs** — pin a tab to the leftmost block; pins behave fluidly when
  dragged past unpinned tabs.
- **Tab context menu** — right-click a tab to Rename, Duplicate, Pin/Unpin, Restart
  agent, Close tabs to the left / right, Close others, or Close. Duplicate clears
  copied conversation state so a harness can mint a new session instead of
  deliberately resuming the source. **Restart agent** (resume-capable agents only)
  does what the toolbar Restart does for that tab.
- **Restart** — the toolbar Restart (also the tab menu's *Restart agent* and the
  palette's *Restart agent*) stops the pane's process *and* its tmux session,
  local or remote, then relaunches in place in a fresh session. A resume-capable
  agent comes back on its saved conversation, so an updated harness binary takes
  effect without losing it; a shell or other program starts over. On Windows,
  a Grok restart first saves any newly discovered conversation. If that save
  fails, the pane keeps running and reports the error.
- **Tab cycling** — keyboard shortcuts cycle to the next/previous tab.

## Pop-out windows

- **Detach a pane** — pop a pane out into its own OS window without terminating it.
  Terminals and editors move across; a browser pane is re-created in the new window
  at the same URL (a native webview belongs to the window that built it).
- **Re-dock in place** — a popped-out pane remembers where it came from; the Dock
  button returns it to its original location. Re-docking a working agent clears
  that pane's stale finished or needs-input card.
- **Close terminates** — closing a pop-out window kills its terminal (with a
  confirmation). Editors and browser panes just close.
- **Workspace changes** — switching workspaces closes the previous workspace's
  pop-out and per-monitor project windows without interrupting the switch.

## Agents

- **Built-in agent presets** — Shell, Claude, opencode, Amp (ampcode), Grok
  (x.ai), Muse Code (Meta), Hermes, Ollama, **Ollama Code**, Pi, and a **Browser**
  preset. On Windows the default shell is **PowerShell**, with **Cmd** and
  **Git Bash** offered as additional presets (instead of the single "Shell"). A
  preset is either a terminal agent or a **Browser** (opens a browser pane at its
  homepage) — switch the type and edit the fields in Settings → Agents.
- **Git Bash on Windows** — when Git for Windows is installed, a **Git Bash**
  preset runs its `bash.exe` as an interactive login shell, so the pane gets the
  MSYS tools (`ls`, `grep`, `ssh`, …) rather than a bare shell. muxel finds bash
  by walking the standard install locations, the root of whichever `git.exe` is on
  `PATH` (so a custom install directory works), and Scoop's app directory; set
  `MUXEL_GIT_BASH` to point at a portable install. It never resolves a bare
  `bash`, which on Windows is the WSL launcher rather than Git Bash. Panes open in
  the project or worktree directory instead of jumping to your home directory, and
  the preset is hidden when Git isn't installed.
- **Ollama Code** — runs a coding agent backed by an Ollama model via
  `ollama launch <agent> --model <model>` (seeded as `ollama launch opencode
  --model glm-5.2:cloud`); change the agent or model in the preset's args.
- **Muse Code** — launches Meta's installed `muse` CLI, with optional model and
  reasoning-effort overrides. It uses Muse's configured defaults otherwise.
  Local retained sessions bind to their pane through Muse's root lifecycle
  hooks and resume the exact conversation after restart, including panes in
  the same folder. The status dot follows Muse's spinner and completion title,
  with permission notifications marking a blocked turn. Where the title can't
  be read (under tmux, or on a remote host) the dot follows output activity,
  and local hooks still mark blocked and finished turns. Hook records are read
  in the background. SessionStart context
  delivers file-link, memory and custom instructions without submitting a turn.
  Hooks are configured per launch outside the project; existing user, project
  and managed hooks are preserved. Only permission notifications invoke the
  notification helper, whose work is limited to one second and exits successfully
  on unavailable data or blocked I/O. Windows uses the native binary selected by
  the installed launcher. Custom hook overrides and nonstandard batch wrappers
  keep their own configuration; automatic capture requires the native hook route.
  Strict managed-hook schemas also launch with their configuration unchanged.
  Positional input after `--` is preserved when configuring hooks.
  Reattaching to a surviving local tmux pane retains its hook binding; a new
  process rotates it. Closing a pane removes its generated hook metadata.
- **Configurable launch** — per agent: program, model + model flag, effort +
  effort flag, extra args (shell-style quoting: `--append-system-prompt "be terse"`
  stays one argument, matching the iOS companion's parser; an unbalanced quote
  falls back to space-splitting with a feed warning), environment variables,
  system-prompt injection
  (via a CLI flag or by typing it in at startup), and a runner startup delay
  (ms to wait after the agent's first output before a runner types — for slow
  starters like opencode, and for Grok, whose animated welcome screen never goes
  quiet; 0 = auto-wait until output goes quiet).
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
  Outside-created panes show the requested program's launch failure directly;
  their approved command is never replaced with a shell.
- **Session resume** — resume-capable agents reopen their prior conversation after
  a muxel restart. Two shapes, both configurable per preset:
  - **Host-minted** (Claude, Grok): `session_id_flag` + `resume_flag` — muxel
    launches with `--session-id` the first time and `--resume` on restart.
  - **Agent-minted** (Codex): only `resume_flag` (`resume`) — first launch is bare;
    muxel captures the UUID Codex publishes for that pane and stores its rollout path.
    Local tmux panes read their own `pane_title` from the recorded tmux socket on a
    background worker because tmux does not forward that title to the terminal view.
    Before relaunch, muxel checks a saved rollout path and uses `codex resume <id>`.
    A restored pane that needs history gets one background lookup before its
    launch, never on the UI thread and never twice per pane in a run: a saved
    UUID with no saved path is confirmed by its dated directory, then by one walk
    of `~/.codex/sessions`; a started pane without a UUID binds the newest rollout
    recorded in its directory that no other pane owns, unless its own tmux
    session still runs (title capture binds that one exactly) or an unbound
    sibling in that directory still runs, in this app or in tmux, since that
    sibling's conversation would look like the newest one. The launch waits for
    the lookup at most two seconds; an unbound pane whose lookup finds nothing,
    is skipped, or runs out of time starts fresh, and Import can still find an
    older conversation.
    An unreadable rollout that could change the chosen conversation leaves
    discovery unresolved, including shortened title hints and Import; confirmed
    subagent rollouts remain excluded. In restore and Import, a confirmed
    subagent also excludes its unreadable compression siblings.
    Discovery checks equally recent candidates before binding a pane, keeps
    verified newer conversations when an older candidate is unresolved, and can
    use a readable copy when a compression sibling cannot be read.
    The immutable
    `thread-id` title field preserves that binding across conversation renames; shortened
    UUIDs resolve in their timestamp's local day directory, with one neighboring day
    checked near a date boundary. Only matching filenames are opened to validate the
    pane's directory and top-level thread identity; subagent rollouts never rebind
    a pane. Multiple Codex
    panes can share a project without resuming one another's conversations.
  **Conversation switches stick** — an agent can swap its conversation inside a live
  pane (`/resume`, `/clear`, a fork) without restarting the PTY, and muxel rebinds the
  pane to what's actually on screen instead of the conversation it launched. Local
  Claude panes learn the switch from a process-local `SessionStart` hook keyed to the
  pane; directly launched Codex panes learn it from a later OSC title. Both are accepted only after the
  new UUID's own on-disk session proves it real and no sibling pane already owns it —
  terminal titles carry no sender identity, so an unverified one cannot rebind a pane.
  A Claude conversation is matched by UUID across Claude's whole store rather than only
  the pane's own directory, so one moved by `/cd`, a worktree change, or a `/resume`
  that reached into another project is still found and still resumes; it has to sit
  directly inside that store, which is what stops a stray path from rebinding a pane,
  and `/clear` or a fork may announce its new UUID before Claude has written the first
  record. A Codex pane stays tied to its own directory. Remote Claude panes keep their
  starter binding. Claude bindings save the exact transcript path. Launch checks that
  path once and reads its current working directory on a worker; a pending hook's
  validated cwd still takes precedence for that launch. Legacy or moved
  transcripts share one fallback search per launch batch, with the found path saved
  for later launches. Launch preflight lists the hook directory once per batch and
  reads only panes with a pending record. Read-aloud and control reads use the saved
  transcript path. Recurring Codex rollout discovery, Claude hook validation and
  Grok's process lookup run in the background. A newer Claude hook supersedes an
  older pending launch, and consumed hook records cannot revive a discarded or
  expired conversation. A missing Claude store permits fresh launches; a hook
  record read error affects only that pane; an error listing the hook directory
  refuses the batch. Preset edits during restore continue the
  launch automatically. Each pane keeps one pending lookup across pop-out and
  docking. Failed Codex hints retry with a delay that grows from
  one to thirty seconds, even while the day directory changes. A sibling-owned
  tmux title is reread under backoff so a later unowned title can bind; its
  rollout validation is cached only while the same sibling owns it. Other
  sibling-owned hints wait until ownership or binding changes. Launch,
  directory, hint or binding changes reset the delay. The name index is reread
  in full on a worker when its size or modification time changes; unchanged
  indexes take one stat. Claude and Grok retry unresolved results with backoff.
  Discovery continues in popped-out terminals. A result applies only when
  the pane still has the same launch, binding and directory, and no sibling owns it.
  On Windows, an accepted Grok conversation stays reserved to its pane even
  while its metadata save is pending.
  A saved Claude conversation is abandoned only once muxel has confirmed its
  transcript is really gone — a session store it couldn't read leaves the pane's
  resume id alone — and the pane then quietly starts fresh.
  A saved Codex conversation is abandoned when its validated saved rollout
  path and compression sibling are both absent or non-files, when a complete walk
  of the rollout store does not find it, or when Codex itself reports that the
  exact resumed ID is missing soon after launch. Under local tmux that message
  leaves with tmux's screen, so a `codex resume` whose rollout muxel could not
  confirm, launched into a tmux session that was not running, counts as failed
  when its client exits cleanly within ten seconds: the pane starts fresh
  instead of closing. Such an early exit cannot be told from any other early
  Codex exit there, so it gives up the unconfirmed id. Reattaching a running
  session, a confirmed rollout, and remote panes keep close-on-exit; a remote
  resume is never checked locally, and telling its failure from a quick quit
  would need another round trip to the host. An invalid path, unreadable or
  missing store, unfinished walk, or other early failure keeps the id and
  attempts resume.
  Remote Codex panes never check the local session store. Archived Codex sessions
  require unarchiving in Codex before they can resume.
  Before a local Claude relaunch, pending hook records are handled through the
  newest binding before resume arguments are chosen. If a recovered binding
  cannot be saved, or a pending record cannot be read, launch stops and the
  record remains available for retry; failing only to remove a record whose
  binding was saved is logged and the pane launches, unless a newer record waits
  behind it. A record that can't be adopted yet (a `/clear` before Claude
  creates its project directory) is kept and retried until it validates or a
  newer one replaces it, as long as the process that wrote it runs: a relaunch
  that starts a new process drops it, while reattaching a live tmux session
  keeps it.
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
  machine. macOS asks for microphone permission on first use. If the mic can't be
  used you're told as soon as you press the button, rather than after speaking: no
  input device says so plainly, and a *blocked* mic (which macOS otherwise reports
  only as silence) comes with an **Open Settings** button that opens the OS
  microphone privacy screen. (Local whisper isn't
  available on Windows-on-ARM — whisper.cpp can't build there — so use a Provider;
  every other platform has both.)
- **Spoken wake command** — an opt-in toggle in Settings → Speech: say the wake
  phrase (default *"wake up daddy's home"*) into the mic and muxel walks every
  agent pane of every project in turn, relaunching each one whose process isn't
  running, then lands back on the pane you started from. Nothing is drawn over the
  workspace — the sweep moves through the real panes, so you watch them come back.
  Panes still running are left untouched, and the transcript that triggers it is
  treated as a command: it isn't typed into any agent. Matching ignores case,
  punctuation and surrounding filler, so however whisper hears it ("Wake up,
  Daddy's home!") it fires; the phrase itself is editable. A project shown in a
  second window keeps its own focus — its dead panes are relaunched without
  stealing it. The result lands in the notifications feed either way.
- **Spoken report** (optional) — with the wake command on, a second toggle has muxel
  greet you with what it found ("Good evening. Two agents are offline. Bringing
  them back online.") and sign off when the sweep finishes ("All systems online.
  Standing by."). The greeting is timed to land before the panes start moving. Turn
  it off and the wake command runs silently.
- **Read aloud (accessibility)** — a speaker button in the toolbar (or
  `Ctrl+Shift+R`, or *Read the last reply aloud* in the `Ctrl+P` palette) reads the
  **focused** agent's last reply out loud. It reads only the model's own words — its final message (usually the summary of what it did) by
  default, or the whole last turn — and **never code**: fenced blocks, diffs,
  commands, tool calls and their output are skipped, and markdown, emoji and
  symbols are flattened so nothing is read out as punctuation. For a local Claude
  pane the reply comes from Claude's own session transcript, so it's exact even
  after it has scrolled off screen; for other agents (Codex, Gemini, …) muxel finds
  it in the terminal's scrollback, which muxel keeps 10,000 lines of per pane. When
  there's nothing to read, it says so out loud, so the button is usable without
  looking at the screen.
- **Pause, resume and start over, per pane** — every agent pane has its own
  read-aloud controls in its header (and the toolbar's act on the focused pane):
  the speaker reads, then becomes **pause** / **resume**, with **start over** and
  **stop** beside it. Each pane keeps its own place: reading another pane pauses the
  one speaking, which resumes where it left off — at the start of the sentence it
  paused in — whenever you come back to it. A tab being read shows a speaker (or
  pause) badge that pauses or resumes it without switching to it. `Ctrl+Shift+R`
  reads / pauses / resumes the focused pane, `Ctrl+Alt+R` starts it over, and
  `Ctrl+Alt+S` stops whatever is speaking; all three are in the `Ctrl+P` palette.
- **Read Aloud settings** — Settings → Read Aloud: show or hide the read-aloud buttons;
  final message vs. whole turn; **auto-read** when the focused pane's agent (or any
  agent) finishes, queued one reply at a time (shells are never auto-read); say the
  agent's name first; a length limit (about 30 s / 1 min / 3 min) that stops at the
  end of a sentence; say "link" instead of spelling out URLs; say only a path's file
  name; read tables row by row or skip them; and the voice itself (below).
- **The voice** — Settings → Read Aloud picks the synthesizer, in the same three
  flavours as dictation:
  - **System** (default) — the voice the OS already ships (`say` on macOS, SAPI on
    Windows, `spd-say`/`espeak` on Linux). No model, no key, no network, works on a
    fresh install. Pick any installed voice from a list of those in your UI language
    (macOS and Windows; on Linux, type its name).
  - **Local** — **Kokoro-82M**, a neural voice run on your machine, with six
    English reads to pick from. The model downloads once (~89 MB) the first time it
    speaks and nothing leaves the machine after that. Speech is streamed a sentence
    at a time, so it starts talking in about a second. Only in builds made with the
    `voice-local` cargo feature.
  - **Provider** — any OpenAI-compatible `/audio/speech` endpoint, reusing the base
    URL and keychain API key the Speech section already stores; the voice and model
    (e.g. `onyx` / `tts-1`) are yours to set. What it reads is sent to that endpoint.

  A **speaking rate** control (0.5×–2×) applies to the System and Provider voices,
  and a **Test voice** button speaks a sample line. Every engine degrades rather than
  fails: a provider that errors, a model that won't download, or a machine with no
  audio device at all falls back to the system voice, and failing even that, stays
  quiet. (The local voice, like local whisper, is unavailable on Windows-on-ARM; use a
  Provider there.)
- **Shared project memory** — opt-in per project: agents are told (via their system
  prompt) to `grep` and add durable lessons to a `.muxel/MEMORY.md` file shared
  across every agent and run in that project. muxel creates the file, git-ignores
  `.muxel/`, and works for local and remote SSH projects (the file lives in the
  project's working dir on whichever host). Local agents also get its path in a
  `MUXEL_MEMORY_FILE` env var. Enable it on a project (sidebar right-click or
  Settings → Projects); a memory button on the project row opens the manager. Plain
  shells are skipped.
  Settings → Projects → **New projects** can turn it on for every project added later.
- **Self-maintaining memory** — each fact is one `##` section carrying a machine
  meta line (id, dates, tags). muxel maintains the file **on its own**, with or
  without the memory panel open: a section an agent adds is stamped within about a
  minute of being written (local projects; remote ones within ~10 minutes of an
  agent's turn ending, or at the daily pass), and every memory-enabled project is
  maintained once a day. It keeps the file **newest-first** (📌 pinned entries on
  top) and **auto-purges** un-pinned entries 30 days after they were added; a safety
  cap of 200 un-pinned entries evicts the oldest first, never an arbitrary pick among
  same-day entries. A file that can't be read is reported, never treated as empty,
  so nothing is saved over it. A legacy flat `MEMORY.md` is imported, not lost, on
  first open.
- **Memory panel** — the project row's memory button toggles a docked, resizable
  second-sidebar panel (like the file browser, sharing its slot; width persists per
  workspace): search/grep the entries, add one (title + note + tags), pin/unpin
  (pinned entries are exempt from purge and cap), delete (with confirm), or open the
  raw `MEMORY.md` in the editor. Each change is applied to the file as it is on disk
  when saved, so sections agents added while the panel was open are kept. Entries
  load and persist over SSH for remote projects too.
- **Accomplishment log** — opt-in per project (sidebar right-click or Settings →
  Projects): agents are told to append one line to `.muxel/ACCOMPLISHMENTS.md` each
  time they finish a feature, fix, refactor, or perf/docs/test improvement —
  `- date | type | summary | agent | #key` — with a shell append, **never reading the
  file**, so logging costs agents almost nothing. **Entries can be corrected**: if a
  result later changes (more got fixed, part was reverted), the agent appends a
  line with the same `#key` (grepping for it if needed, still not reading the file)
  and muxel folds it into the original, which keeps its date and shows "updated"
  with the correction's day. muxel fills in the agent's name, stamps undated lines,
  keys unkeyed ones (so any entry can be corrected), drops duplicates, and prunes
  entries whose last change is older than the history window
  (**14 days** by default, 1–90 in Settings → Projects) **on its own**: a background
  pass maintains every log-enabled project once a day (at startup and after each day
  rollover), with or without a view open, and changing the window re-prunes them
  all. Remote projects are maintained once their host is connected (pruning never
  opens a connection by itself). muxel rewrites the file only when something
  actually changes. Works for local and remote SSH projects; local agents also get
  the path in `MUXEL_ACCOMPLISHMENTS_FILE`.
  Settings → Projects → **New projects** can turn it on for every project added later.
- **Accomplishments panel** — a check button on the project row opens a docked,
  resizable panel (sharing the file-browser/memory slot) listing the log by day,
  newest first, with colored type tags, per-type totals, search, delete, and a
  quick-add box (`fix: what was done`). It updates live while open: local logs are
  watched by file stamp, and remote ones refresh when an agent finishes a turn.
- **All-projects accomplishments** — the title-bar check button (or the palette's
  "Show accomplishments") swaps the pane area for an overview of every log-enabled
  project, grouped by day and then project, with type and project filters and
  search. It can **pop out** into its own window, to keep on another monitor.

## Agent status

- **Real lifecycle badges** — each pane shows **working**, **idle**, **blocked**,
  or **done**, color-coded (blue / gray / amber / green) on the tab pill, sidebar
  icon, dashboard, and notification dots. A marker-based agent whose turn finishes
  is held at **done** until the agent works again — even if it never rang the bell —
  and that completion survives a muxel restart. Focusing the pane marks its
  notification read without rewriting lifecycle state or age. Sidebar badges include
  coarse age while work is blocked or done, show recent idle activity briefly
  (`idle · 12m`), omit ordinary middle age, and call out panes idle for three days
  or more (`stale · 4d`). Long pane titles ellipsize before the badge instead of
  pushing status out of the sidebar. Title-derived lifecycle and automatic-name
  updates on known agent panes are accepted only from that provider's semantic
  title shape; existing marker, bell, and process-exit signals still apply.
  Provider-owned screen rows can add precise state that titles omit: Claude
  permission forms report **blocked**, while positive background counts displayed
  by Claude and Grok remain **working** even when their titles look idle. Claude's
  visible foreground progress row also preserves **working** when a narrow tmux
  pane truncates the usual interrupt marker. Brief
  Grok redraw gaps do not forge completion. Ambiguous waiting rows do not claim
  active work. Local Codex `/rename` values are read
  from its session index by the pane's captured session UUID. The index is checked
  for changes and reread in full when it changes; truncating it rebuilds the names. Child commands
  such as `npm` cannot replace the session name or forge a title-derived state
  transition. Remote Codex panes retain the provider's structural title fallback.
- **Per-agent detection markers** — status is inferred from on-screen TUI markers
  (e.g. Claude's "esc to interrupt" spinner, a permission prompt), with built-in
  defaults per agent and **editable working/blocked markers per preset**.
- **Heuristic fallback** — agents without markers fall back to bell + output
  activity. They reach **done** only from the bell or process exit, never from a
  quiet spell — so an incidental redraw (e.g. a focus repaint when you click the
  pane) can't fake a finished turn.
- **Event-driven status snapshots** — terminal output, semantic titles, bells,
  process exit, resize invalidation, and exact quiet/debounce deadlines update one
  per-launch runtime snapshot. Sidebar and render reads never rescan the terminal
  or mutate lifecycle state. When an agent switches conversations in place (a
  `/resume`), evidence from around the switch can't complete a turn or notify:
  the pane shows the agent's current state until the new conversation's own
  evidence arrives, and the Enter that picked the conversation doesn't count as
  a turn in it. During uncertainty, `ctl` treats the pane as Working: ordinary
  sends are refused and `wait` keeps waiting; `send --force` retains its override.
  An agent that prints nothing more after the switch settles a couple of seconds
  later, on a timer, so an idle pane accepts sends again; an old prompt or
  activity row still on screen stays uncertain until it goes or a new title
  confirms it.
  A Grok conversation switch releases old loop ownership even if saving fails.
  A confirmed provider switch clears the previous conversation's automatic name,
  including a switch back to the same UUID between reads; a transcript path
  upgrade alone keeps the name.
  Each pane uses its own discovery history, so another pane's refresh cannot
  discard its valid current title.
  The snapshot carries a launch id and
  provider-binding, input and sequence revisions; without provider hooks its coverage remains
  explicitly limited to visible terminal evidence.
- **Auto-continue** — an agent that lays out a multi-phase plan sometimes finishes
  the first phase and just stops, waiting, with the todo list still half-unchecked.
  Each agent pane has an **Auto** toggle in its header: while it's on, muxel watches
  the pane and, whenever the agent goes idle with work still to do, types `continue`
  and presses Enter for you. The message is yours to change in Settings → Behavior
  → Auto-continue message (kept to one line; blank restores `continue`), and
  right-clicking a pane's **Auto** button gives that one pane its own, in a popup
  pre-filled with what it types now. A pane's message stays with it, across
  restarts, until the pane is closed; blank goes back to the default. While
  auto-continue is on with a pane's own message, its button reads **Auto\***. It fires when it can see pending work — Claude's `☐`
  checkboxes or an "N pending" count — or when the agent voluntarily stops to check
  in ("My recommendation is to pause here.", "Shall I continue?", "I'd hold here
  unless you want that scaled run."), so a plan keeps moving without you babysitting
  it. "Idle" is judged by the screen going still
  rather than by a status guess, so it won't fire over an agent that's plainly
  mid-work (spinner still turning) even if its "working" marker isn't recognized;
  and the *next* nudge keys off the todo list actually moving, so it follows a
  multi-phase plan even when a phase finishes and the agent re-pauses in a blink. It
  never nudges a genuinely-finished agent, and never answers a permission prompt
  (that needs your real yes/no). If `continue` fires a few times and the screen
  never changes at all — a dead loop, like an agent erroring out the instant it
  resumes — it stands down on its own and posts a notification rather than
  hammering forever; but an agent that keeps answering with fresh work (even when
  its remaining tasks are blocked on you and no checkbox moves) is left running.
  When the agent says it's out of work it can do ("no responsible work left",
  "nothing further I can do"), auto-continue stops and notifies you rather than
  nudging a finished agent in circles. Runtime-only: it's off again after a restart.

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
- **Built-in reviews that don't edit unasked** — the built-in Review and Security
  Review prompts tell the agent not to edit, stage, or commit; cover staged,
  unstaged, and untracked work (or the branch's own commits when the tree is
  clean); and ask for verified findings, worst first, with file:line, a failure or
  attack scenario, and a fix. Security Review ranks its findings in a table with a
  recommendation for each, suggests which to fix, and asks you — then fixes only
  the ones you pick. An upgrade refreshes only prompts you never edited.
- **Templated prompts** — `{{input}}` is substituted with run-time details.
- **Auto mode** — send a configurable number of Shift+Tab presses (then Enter) at
  startup to reach auto-accept mode.
- **Ephemeral + restore-safe** — on app restore a runner re-types its prompt but
  does not auto-submit.
- **Shared runners** — runners from a team library appear in their own section of
  the "Run task" dropdown (see *Team libraries*).

## Snippets

- **Type saved text into an existing pane** — reusable snippets (a managed list in
  Settings → Snippets) are typed straight into an already-running pane, unlike
  runners (which spawn a new agent). Each snippet records whether it **auto-submits**
  (presses Enter) or just drops the text into the input for you to review.
- **Three ways to send** — the toolbar **Snippets** dropdown (sends to the active
  pane), the command palette (*Send snippet: …*), or right-click a terminal tab →
  **Send snippet** (sends to that specific pane). Multi-line text goes in via a
  bracketed-paste-aware insert so it won't submit on its own newlines.
- **Shared snippets** — snippets from a team library appear in their own section of
  the Snippets dropdown (see *Team libraries*).

## Loops

- **Scheduled task launchers** — run a saved prompt on a chosen agent in a chosen
  project on a timer: every N minutes, every N hours, or daily at a local time.
- **Runs the agent it names** — a loop keeps its own agent, starting with the one
  selected in the toolbar when you create it, and never runs "whatever is selected"
  when it fires (a shell there would run the prompt as commands). A loop whose
  agent preset was deleted, or one saved as "Current" before this, is switched off
  with a note in the feed instead; its editor opens on the toolbar's agent, so one
  save fixes it. A run task whose preset was deleted says so rather than running
  another agent.
- **Unattended firing** — when due, a loop spawns a fresh agent as its own new
  pane appended at the end of its project's layout, types the prompt, optionally
  sends auto-mode Shift+Tab presses, and respects the agent's startup delay (so
  opencode and Grok work). The pane is **visible but not focused** and never switches your
  active project — a loop firing on a timer can't interrupt what you're typing.
- **Provider startup input** — runners, TypeIn presets and loops type their
  task once the agent's output has gone quiet (or after the preset's startup
  delay), and at the 30-second cap if it never does, for any agent.
  A `ctl new --prompt` task is stricter: it waits for Claude's editable
  composer and bracketed-paste mode, Codex's live editor (under its model
  status line, `? for shortcuts` or context-left footer; a status line whose
  model name has no digit and no `gpt-`/`codex-` prefix isn't recognised
  without one of the other two) and Ready title, or
  Grok's semantic idle state after its configured startup delay. In a tmux
  pane, which forwards no program titles unless tmux's own `set-titles` is on,
  muxel reads the provider's title through tmux (`#{pane_title}`) while that
  task waits; it never changes tmux options. Codex folder-trust menus receive no
  task input; readiness failure leaves the provider running and reports
  `startup_error` through `ctl status` and `show`.
  Permission text inside Claude's pasted task does not block its submission.
  `ctl wait` keeps a newly created task pending through startup and paste, then
  confirms submission only from a fresh provider turn or rendered user row.
  Enter admission alone stays pending. Codex can receive one extra Enter after
  five seconds only if the exact task is visible on one ASCII editor row with
  its cursor at the end and no new user row; ambiguous reflow gets no retry. Task text
  is never pasted twice. Grok and Claude do not inherit that retry. After ten
  seconds without acceptance, `ctl wait` reports `submission_failed`, with an
  explanation in `startup_error`. Providers without a verified input/acceptance
  protocol (including Muse) refuse controller startup tasks without typing.
  A task's input stops if the pane is replaced, someone types, the writer fails,
  or readiness times out; it never replays a submitted task automatically.
- **Post-run policy** — leave the agent running, or exit it once it finishes its
  turn (with a max-runtime safety cap). A still-running loop won't stack a second
  copy. Cleanup requires current attributable evidence from the launch and
  conversation known before the first submitted turn: the one muxel launched
  the pane into (a Claude loop's minted session id), or the first one an agent
  such as Codex names. A later first identity cannot establish which
  conversation received that prompt. Switching conversations or reopening
  its pane stops tracking the old run without closing the new one. At the
  runtime cap, uncertain or unidentified runs stop tracking without closing the
  pane, so later scheduled runs can fire. If a prompt preceded the agent naming
  its conversation, even an Exit-policy loop leaves its pane open for a human
  to close.
- **Managed from the main window** — a toolbar "Loops" dropdown lists your loops:
  click one to run it now, the pencil to edit it in Settings → Loops, or "New
  loop…" to create one. Schedules survive restarts (a daily-at whose time passed
  while closed fires once on next launch). Loops fire only while muxel is running.
- **Scrolling dropdown lists** — in the Run task, Snippets and Loops dropdowns a
  long list (private and library items alike) scrolls within up to 60% of the
  window height, while the title and the "New loop…" footer stay in place.
- **Shared loops** — loops from a team library appear in their own section of the
  Loops dropdown, switched off until you turn them on (see *Team libraries*).

## Team libraries

- **Shared libraries from a git repo** — Settings → Libraries adds a git repository
  by URL (optional branch, editable display name). muxel clones it into its data
  dir and keeps it read-only, using your existing git auth. Selecting a library in
  the list shows its URL, branch, last successful pull, item counts, and any errors
  or skipped items; the list marks a library with an error (red dot) or an
  operation in progress (…).
- **Automatic updates** — each library is pulled with `git pull --ff-only` at launch
  and every 5 minutes. **Pull now** updates it on demand, **Re-sync from
  repository** resets the clone to the remote branch, and **Remove** deletes the
  library. Re-sync first checks the clone (offline, read-only): with no local
  changes it starts right away; otherwise it asks, saying how many modified or
  untracked files and local commits will be lost.
- **One file per library** — a `muxel-library.toml` at the repo root holds
  `[[snippets]]`, `[[runners]]` and `[[loops]]`. An agent is named by its preset
  name; a runner without one runs *Current*, and a loop without one runs the
  preset you pick when you turn it on.
- **In the toolbar dropdowns** — library items appear in the Snippets, Run task and
  Loops dropdowns in a section per library, after your private items.
- **Read-only, with local copies** — library items can't be edited in muxel; change
  them in the repo. **Make a local copy** creates an independent private copy that
  is no longer synced.
- **Safe shared loops and runners** — shared loops arrive switched off; turning one
  on shows its full content and asks for a project, plus an agent from your
  presets if the loop doesn't name one. It switches off again when any field other
  than its name changes. A shared loop never runs on *Current*: if its agent no
  longer resolves (preset renamed or deleted), its next run switches it off and
  says why. A shared runner shows its full prompt for confirmation before the
  first run of each new version.
- **Sanitized text** — the names, texts, prompts and preset names of library items,
  and the parts of a library file quoted in messages, have control characters
  (except newlines and tabs) and invisible formatting characters removed.

## Built-in browser

- **System webview, not bundled Chromium** — preview links agents print (or a
  locally hosted dev site) without leaving muxel. Uses the OS engine (WKWebView on
  macOS, WebView2 on Windows, WebKitGTK on Linux), so it's light on disk and memory.
- **macOS/Windows: an embedded pane** — ctrl+click a URL and it opens as a browser
  pane beside the terminal, with an address bar and Back / Forward / Reload buttons;
  Reload refreshes the page you are actually on (several links deep, if that's where
  you are), not the pane's original URL. The URL persists and restores with the
  workspace. Clicking into the page makes it the active pane — so paste and the
  toolbar act on the browser, not on whichever pane you were in before — and hands
  it the keyboard; muxel's own shortcuts keep working until you click into a page.
  The toolbar can open the current page in the system browser. Native page context
  menus remain usable, and WebView2 stores its profile under muxel's app-data
  directory rather than beside the executable.
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

## Outside control (`muxel ctl`)

- **Let another agent drive muxel** — Settings → Outside control → "Allow outside
  tools to control muxel" (off by default) lets a program on the same computer — an
  orchestrating agent such as xAI's Grok Bot, a script, another coding agent — work
  the agents running in muxel through the `muxel ctl` command: list projects, the
  pane layout and every agent (program, model, worktree, focus), see each one's
  status (working / blocked / done / idle / exited), read its last prompt and last
  reply, send it a prompt, answer the question it is blocked on, and press keys.
  Every command prints JSON.
- **Open panes from another tool** — `muxel ctl new PROJECT` opens a tab using
  that project's toolbar preset. `--model M` and `--effort E` select per-pane values for
  plain and prompt launches, retained on restart. The preset needs Model flag /
  Effort flag configured and absent from Extra args; literal argv cannot use
  these options (`parameter_unsupported`). Values are 1–64 characters from
  `[A-Za-z0-9._:/@+-]` without a leading `-`. Ask displays the choices before
  directory details; preset overrides remain Allow-eligible. Flag templates
  containing `{}` substitute
  the value into one argument. New Claude presets use `--effort`; new Codex
  presets use `--config=model_reasoning_effort={}`. Existing presets are unchanged.
  Parameterized creation uses control protocol v3 so older hosts refuse it;
  plain creation and status polling remain v2. Hosts require the exact command version.
  Choose `--preset NAME` (or its id), or pass
  `-- PROGRAM ARGS...` to launch literal argv, including spaces, quotes and empty
  arguments. `--prompt TEXT` (or `--prompt -` for stdin) submits a user task
  through the existing one-shot runner path for a configured non-shell terminal
  preset. This replaces the preset's configured system prompt with the task;
  preset arguments are kept. The typed first turn is exactly the task, without
  trailing whitespace (at most 10,000 characters, so the approval card can show
  all of it). Automatic
  file-link and memory instructions use the preset's separate flag/config
  transport when available, and are omitted for TypeIn-only presets.
  The task text is discarded after first launch.
  Remote tmux owner discovery keeps the task pending until that same launch
  can prepare its terminal; refused ownership or launch permission cancels it.
  Restart, restore and resume neither re-type nor resubmit it, including launches without a resumable
  conversation. A duplicate of the pane starts without it, and the phone never
  types or submits it. Literal, shell and browser launches refuse prompts with
  `prompt_unsupported`. `--cwd`
  accepts an existing absolute directory, including a linked worktree outside
  the project root; it defaults to the root. Automatic memory and accomplishments
  instructions still address the project's files when the launch directory differs.
  `--name` names the tab (1–100 visible characters; `--from` likewise), and
  `--split right|down` creates a pane. Both refuse control, bidi, zero-width and
  line-separator characters.
  Creation preserves the selected tab, focus,
  project and window by default, without raising the window. `--focus` selects
  the new pane, its project and window only when the request was approved with
  a click; under Allow the pane opens in the background as usual, so a request
  can't move keystrokes the user is typing into it, and the reply's `focused`
  says which happened. `--background` remains accepted as a
  compatibility no-op. JSON uses `focus` (default false); the legacy `background`
  field is accepted and ignored. The JSON reply identifies the pane and launch.
  ALL outside creation requires both "Allow outside tools to control muxel" and
  the separate **Outside terminal creation** policy in Settings → Outside control.
  It defaults to **Off**. **Ask** requires a click for each request; **Allow**
  opens only presets whose program is a known agent provider (the built-in agent
  presets' programs — Claude, Codex, Grok, opencode, Amp, Muse, Hermes, Ollama,
  pi — by executable name) without a click, when it is given as an absolute
  path or found through absolute `PATH` entries. Any other program, such as
  `ssh host`, `wsl.exe`, `python` or `node`, and a relative path or `PATH`
  entry, which the caller's `--cwd` could supply, still Asks under Allow.
  All literal argv, including agent argv, and shell-classified
  presets/defaults also require "Also allow typing into shell panes". Configured
  non-literal presets not classified as shells need only the first two grants.
  Literal argv and shell presets always Ask, even under Allow. All three grants
  default off; upgrades do not infer creation permission from older
  control/command grants. Users enable the required settings themselves.
  The existing shell/non-shell classifier is a soft policy for trusted configured
  presets, not an executable allowlist.
  Commands run as the host muxel process with its permissions and environment,
  outside the caller's sandbox; coding agents and configurable presets are not a
  security sandbox. Creation is checked at admission and rechecked at mutation
  time before executable discovery, placement, persistence or spawning.
  Quitting, deletion and remote projects refuse
  creation. Literal launches on Windows require a native executable (installed
  Codex resolves to its packaged executable); batch wrappers cannot preserve
  arbitrary argv. Literal launches bypass preset rewriting and tmux wrapping.
  Built-in output markers still track work and permission prompts without
  modifying the command or injecting startup input.
  They restore stopped; project selection, Restart and wake never replay their
  opening brief. The pane's explicit **Run again** action reruns the saved argv.
  The sidebar labels an unspawned restored literal pane as **stopped**. Retained
  exited terminals keep their **done** badge and completion age.
  Literal panes never claim or clean up tmux sessions, including stale peer
  metadata. A layout written back by an older peer that doesn't know them
  cannot clear a pane's literal argv, launch directory or outside-creation
  marks. Phone duplicates keep them stopped without assigning a tmux session;
  closing them still removes their layout entry and owned worktree normally.
  Launch directories are stored separately from owned worktrees, so closing a
  pane never deletes its cwd and migration never registers it as owned.
  Without `--prompt`, preset and default launches keep toolbar injection, resume and hooks.
  `new` opens terminal panes; browser presets (including a browser default) are
  not supported yet and receive a refusal.
  Ask answers admission immediately with a pending request UUID; the CLI polls
  once a second and prints the final reply. `--no-wait` returns the pending ID for
  scripts to poll with `ctl new-status ID`. The floating card shows declared
  caller/pane information (`--from LABEL` and `MUXEL_INSTANCE_ID`), the
  destination project, preset with the program and arguments it runs, cwd,
  pane name and the whole prompt, or full literal argv. Long details scroll,
  with arguments shown one per line, including empty arguments. Every value
  shows control, bidi and zero-width characters as visible escapes
  (`\u{202e}`); leading or repeated blank lines, long blank runs and stacked
  combining marks in the prompt are counted (`\n×3`) rather than drawn.
  Labels are not authenticated identities. **Allow once** / **Deny** require
  mouse clicks. Both buttons stay disabled for 750 ms after each card appears,
  and double-clicks are ignored. Mouse-down and release must belong to the same
  armed card, so a held press cannot transfer across expiry. Scrolling stays in
  the card; keyboard input remains in the terminal. Cards appear in FIFO order
  with a waiting count and an OS
  notification, and native browser views yield to them.
  Approvals expire 120 seconds after admission (`approval_timeout`), including
  time behind another card; denial returns `approval_denied`. Turning creation
  or outside control off refuses every pending request and removes its card.
  Outside control off also stops the server, so polling callers receive
  `host_unavailable` after connection retries. Connection and host-availability
  failures retry up to three times against the same UUID; the CLI never reissues creation.
  Exhausted retries and uncoded status errors name the UUID to query with
  `new-status` and advise against repeating `new` while the original may still wait.
  Finished replies remain available for ten minutes (latest 256); at most 64
  approvals wait, and approvals do not survive a host restart. Creation requests
  use protocol v2, or v3 when selecting model/effort, so older hosts refuse
  unsupported fields.
  Turning creation off leaves existing panes and typing permissions unchanged
  and does not cancel launches already admitted for spawning. Deferred requests
  still recheck the current grant. Human toolbar creation is unaffected.
- **Reads what the agent actually said** — `show` returns the last prompt and the
  model's last reply the way read-aloud finds them (a local Claude pane's session
  transcript, otherwise the pane's scrollback), and for a blocked
  agent the question with its numbered options read off the screen. `wait` blocks
  until the agent's turn ends — it finishes, asks something, or exits — then
  returns the same, so a caller can `send` then `wait` without polling. An answer
  to a question counts as a turn too, so `answer` then `wait` works the same way.
- **Refuses what would go wrong** — `send` won't type into an agent that is
  working or waiting on a question (unless forced), `answer` only picks an option
  the prompt actually offers, and a pane that isn't running says so rather than
  swallowing the input. Shell panes are off limits unless "Also allow typing into
  shell panes" is on too, since typing into a shell is running commands. That
  setting also grants `new` shell preset/default and all literal argv launches
  when the separate outside terminal-creation grant is enabled. Existing shell
  writes require global control and this command grant even when creation is off.
  Turning "Also allow typing into shell panes" off refuses subsequent control
  writes to every literal argv pane, even agent-looking or interpreter programs,
  and every program outside the known built-in agent providers, including SSH
  and WSL shell presets. Existing-pane writes recognize known agents by executable
  name, including custom paths and agents in remote projects.
  An agent that falls back to a shell after a failed launch requires the same
  shell-input grant, including restored panes. A successful agent relaunch uses
  the agent policy again.
  Configured non-literal known agents remain controllable while outside control is on;
  disabling all outside control is a separate setting. Delayed Enter and keys
  recheck the shell-input grant and terminal/session availability before writing.
  Revocation does not stop processes
  or undo input already delivered.
  `send` waits for its delayed Enter and withholds it if local input changes or
  the pane closes or restarts. A withheld send fails and attempts to release only
  its own tmux claim; timeout, command failure or older tmux can leave the claim
  in place under its existing aging rules.
- **Popped-out terminals** — `ctl` keeps reading their status and screen and can
  send input when they move to a separate window or dock again. Their activity
  timestamps stay current, so `wait` recognizes completed or blocked turns.
  Activity and attendance follow the same conversation attribution as docked
  terminals; redocking uncertain state preserves existing attention cards.
  On Windows, Grok conversation discovery follows the live terminal in either
  window.
  Scheduled loops stay tracked across popout and redock.
  Restart docks a popped-out terminal before replacing its process.
- **Scripted control** — `ctl` exposes refusal codes and launch IDs, and
  `send --launch-id ID` checks the launch again before pasting. Its success
  receipt confirms writer admission with `pasted: true`.
  Codex, Claude and Grok sends then observe fresh provider acceptance for about
  ten seconds (three more after a retry) and report `submission_confirmed` and
  `enter_retries`. Unknown acceptance is reported as false with an
  `unconfirmed_reason` (`timed_out`, `input_changed` or `interrupted`); once
  Enter is in, the reply is always this receipt, never an error. muxel doesn't
  wait when the pane can't show a receipt — an unsupported provider, or no
  readable idle status title (tmux hides titles) with a Grok, multi-paragraph or
  repeated prompt — and reports null. Turning outside control off stops the
  later Enters and keys of `send`, `answer` and `keys`. Only Windows Codex may
  receive one extra Enter after five seconds, with the same launch, permission
  and input guards and an exact fully visible ASCII prose draft in its idle
  composer. A compact composer frame captured before paste and an added blank
  cursor line must prove that its first Enter was consumed, with no resize or
  title change. Wrapped prose is supported; clipped, multiline, Unicode,
  completion or ambiguous drafts are left for inspection. Claude and Grok get no
  automatic retry. Text is never pasted again, and an unconfirmed receipt does
  not authorize resending it.
- **Never touches your sessions to read them** — every read (`screen`, `show`,
  `wait`, and read-aloud) is answered from muxel's own terminal buffer, which holds
  10,000 lines per pane — five times the most any of these commands will return. It
  never shells out to `tmux capture-pane`, which costs a subprocess on every poll
  and, on tmux builds that abort on it (RHEL 10's stock tmux among them), takes the
  whole tmux server down and every local agent with it. An agent can therefore be
  polled as fast as its driver likes without being disturbed. The one thing tmux
  alone knew — a pane's history from before muxel attached to an adopted session —
  fills back in as the pane produces output.
- **An agent can find its own pane** — programs muxel starts directly on this
  computer, including shells, receive `MUXEL_INSTANCE_ID` set to the pane's id,
  which `muxel ctl` takes as AGENT. An agent or script inside a pane
  can check its own status or read its own screen without guessing at its name.
  If a local UI command cannot start, its fallback shell receives the same
  pane id without copying the failed command's transport variables.
  A pane kept in tmux gets it when muxel creates its session, from tmux 3.2 on
  (which can hand a new session's program a variable). On older tmux it goes
  without, rather than inheriting another pane's id from the tmux server. Remote
  panes go without too: muxel keeps the
  variable off the ssh client, which an ssh `SendEnv` setting could forward, and
  clears it from the remote tmux server, which a muxel pane on that host may have
  started. The iOS app clears it the same way before every session it creates. A
  session muxel reattaches to or adopts keeps the environment it started with.
- **Local and owner-only** — the running app listens on a random loopback port and
  writes it, with a fresh random token, to `control.json` in muxel's data directory,
  readable only by the user; nothing is reachable from the network, and a request
  without the token is refused. Turning the setting off (or quitting) closes it.
  Before connecting, `muxel ctl` checks that the process that wrote `control.json`
  is still running, so a file left behind by a muxel that was killed isn't
  trusted. The check goes by process id alone, so if that id has since gone to
  another process, the file still passes. On Linux and macOS that is any process;
  on Windows it is one the user may open, such as one of their own.
- **Settings → Outside control** — a setup page that walks through connecting
  another agent in four steps: turn on outside control (with a live status: on and
  listening, starting, or off), let the agent run commands on this computer (with
  where to allow it in Grok Bot's own settings), copy the skill (with a preview of
  exactly what gets pasted), and **Test**, which runs the same `muxel ctl` command
  the agent will and shows what muxel answered — or why it couldn't. It also shows
  this computer's muxel command, with a copy button, and links to Grok Bot's
  documentation.
- **Instructions for the other agent** — `muxel ctl skill` (or Settings → Outside
  control → "Copy skill") produces a ready-made skill: the commands, what each status
  means, and the rules — one prompt at a time, `wait` after `send`, and permission
  prompts left to the user unless they've said otherwise. Paste it into Grok Bot
  (whose "Execution on Local Computer" setting must allow it to run commands here)
  or any agent that can run shell commands.
- **Works across several computers** — one skill serves every computer the agent
  works on: it names the muxel binary it was copied from, and otherwise says how to
  find muxel there (the running app records its own path in `control.json`, whose
  location it gives for macOS, Linux and Windows, then the usual install places and
  the `PATH`). Every reply carries `host`, the computer whose muxel answered, so an
  agent working with several computers can tell which one it reached.
- **However muxel was installed on Linux** — `muxel ctl` works from the .deb or
  .rpm (`/usr/bin/muxel`), the tarball or install script, and the AppImage, which
  is run with `ctl` like any command (`muxel-linux-x86_64.AppImage ctl panes`); a
  running AppImage records the `.AppImage` file, not its temporary mount, as its
  path. It needs no display, so it runs from an agent's background shell. The skill
  covers the usual snags: an AppImage without FUSE (`APPIMAGE_EXTRACT_AND_RUN=1`), a
  caller's `LD_LIBRARY_PATH` that doesn't suit muxel (`env -u LD_LIBRARY_PATH`),
  and a caller whose home or `XDG_DATA_HOME` differs from muxel's (`MUXEL_CONTROL=`
  pointing at its `control.json`). CI checks that `muxel ctl` starts from each
  release package.
- **At a Windows prompt too** — muxel is a GUI program on Windows, which Windows
  gives no console, so `muxel ctl` attaches to the console of the Command Prompt or
  PowerShell it was typed in to print its reply there; piped and redirected output
  goes where it was sent, as before.
- **Shared agents take turns** — an agent can be reached from more than one muxel:
  a remote project opened on several computers, or a local tmux project another
  computer attached to. Before typing, muxel checks a marker on the agent's tmux
  session — which every muxel attached to it reads — and refuses while another
  muxel's turn with it is still open ("muxel on *host* is using this agent"),
  then marks it as its own. Two muxels that reach an idle agent at the same moment
  settle it between them: only one types. A marker left by a muxel that quit or
  crashed lapses once the agent is seen to finish, or when its grace period runs
  out. `show` reports it as `controller`: which computer last typed into the agent
  this way, whether that was this muxel, and whether its turn is still open.
  `send --force` overrides it.

## Terminal

- **alacritty-based emulator** — full VTE terminal with truecolor support.
- **Bounded output backpressure** — each pane's reader-to-UI queue admits at most
  256 KiB / 64 chunks. A noisy child pauses at that boundary and resumes as the
  pane drains. While the pane is open (its session lives and the pane keeps
  reading), terminal bytes and the final exit event stay in order and none are
  dropped to protect memory. The UI separately holds one bounded drain batch
  while parsing it. Closing a pane never waits on its queue, and output not yet
  shown is discarded. Failed Windows launches also release output backpressure
  before closing their terminal. On macOS and Linux muxel sends the program one
  SIGHUP.
  If the pane was still reading the program's terminal when it closed, muxel
  kills a program still running about five seconds after that SIGHUP. A program
  that had already closed its terminal is not force-killed, as before. The
  operating system's own hangup when the pane's terminal closes is separate and
  outside muxel's control, so a program is not guaranteed those five seconds.
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
  shell-quoted paths into the focused terminal. File clipboard and drop paths
  use the pane's launch shell (PowerShell, cmd or POSIX, including Git Bash) for
  one parse outside existing quotes. Manual shell switches and other input
  grammars are not detected or protected. Unknown Windows programs receive
  double-quoted text, with no shell-safety guarantee. Cmd refuses percent/exclamation filenames;
  cmd and the fallback refuse embedded double quotes and trailing backslashes.
  Invalid/control-character paths are refused in every format with a
  notification, without inserting part of the file list.
- **Pasting images and files into a remote pane** — an agent on an SSH host (in
  tmux or not) can read neither this machine's clipboard nor its files, so in a
  remote pane a pasted image, or files copied in a file manager or dropped on the
  pane, are first copied to the host and their paths *there* are pasted instead —
  remote Claude Code attaches a pasted screenshot just as it would locally. Files
  land in `~/.cache/muxel/paste` (owner-only; the user's temp folder on a Windows
  host) and are cleared after a week. An image an agent won't take (a TIFF, as
  copying from Preview gives, or a BMP) goes as a PNG. Folders and files over 25 MB
  aren't copied, and the notification feed says which. Text pastes as it always
  has. Uploaded paths use the original terminal's launch shell and the same
  filename guard; a completion for a replaced or closed terminal is discarded.
  Moving the terminal into its own window or re-docking it keeps that destination.
  Windows agent panes receive generic quoted path text; only a shell pane uses
  the configured host shell's format.
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
  links), a literal Markdown inline link, a `file://` URI, or a **file path**
  (Windows/Unix absolute, `~/`, or relative to the pane's working directory).
  Local files open in a muxel editor pane; only paths that exist are
  clickable, and a trailing `:line:col` is understood. `Ctrl`/`Cmd`+hover
  underlines the link and shows a pointing-hand cursor (Ctrl/Cmd down re-hit-tests
  without requiring a mouse move).
- **Links in new tabs** — middle-clicking a terminal link opens it without stealing
  terminal focus; browser target=_blank, window.open, Ctrl+click, and middle-click
  requests stay in the source project and open as Muxel tabs. Right-clicking a
  terminal link opens link actions instead of copying or pasting through the PTY.
- **Focus reporting** — forwards focus in/out to the PTY (DECSET 1004) so agents
  know when their pane is active.
- **OSC-52 clipboard** — programs in the terminal (including over SSH/tmux) can
  copy to the system clipboard via `OSC 52`; clipboard *reads* are answered with
  an empty reply, so a remote can probe for support but never see your clipboard.
- **Color queries** — answers `OSC 10/11/12` and `OSC 4` color queries from the
  active theme's terminal palette, so TUIs detect dark/light mode correctly (and
  the answer always matches what's painted).
- **Prompt query replies** — device-attributes (`DA1`/`DA2`), status, color and
  OSC-52 read queries are answered directly on the PTY reader thread, while the
  asker is still waiting, even when the UI is busy or backgrounded. A late reply
  is read as typing (tmux forwards one into the pane, so a remote agent's prompt
  fills with `0;2501;1c`). Replies keep query order: anything asked after a
  cursor-position or mode report waits for that report to go first.
- **Exit codes** — a pane's child exit status is captured, so close-on-exit and
  session-resume recovery can tell a clean `exit` from a crash (a deliberate quit
  no longer triggers resume recovery). On Windows, process exit is observed
  independently of the console's output pipe, so a dead child cannot leave a
  pane looking live and accepting input forever. Final output is drained before
  the exit banner appears.
- **Crash tombstones** — a pane whose process dies abnormally (non-zero exit, or
  the PTY failing outright) is never auto-closed: it keeps its final screen under
  a "process exited — code N" banner, fires an error in the NOTIFICATIONS feed
  (plus a desktop notification when unattended), and Restart relaunches in place
  (resuming a resume-capable agent's conversation).
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
  `Alt/Option+Backspace` sends `ESC DEL` (delete word), `Ctrl+Backspace` sends
  `Ctrl+W` (delete word) and `Cmd+Backspace` (macOS) sends `Ctrl+U` (delete to
  line start), matching iTerm2's natural text editing and Warp.
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
- **Git marks in the browser** — each row carries its git status: `?` for a file git
  hasn't been told about, `A` staged, `M` modified, `D` deleted, `!` conflicted.
  Folders carry the strongest status beneath them, so a collapsed folder still shows
  that something inside is unadded. Right-click anything with something to stage for
  **Add to git** (a folder stages everything under it) — works on remote projects
  too, where the `git add` runs on the host.
- **Markdown & image rendering** — `.md`/`.markdown` files render as formatted
  markdown and image files (`png`, `jpg`, `gif`, `webp`, `bmp`, `svg`, …) render as
  images, both by default, with a header **Raw / Rendered** toggle to view the
  source (e.g. an SVG's XML or the markdown text). Paragraphs combining inline
  images and line breaks render without crashing, including linked HTML images.
- **Rendered-text copy parity** — Ctrl+C and Ctrl+Insert copy the exact selected
  text in rendered Markdown, images, and settings fields without intercepting Ctrl+C
  from a focused terminal.
- **Resource reuse** — opening a local file that already has an editor tab focuses
  that tab and reloads it from disk when its buffer is clean; dirty buffers are
  never overwritten. File links can jump to `#L12C4`, and local HTML links open in
  the browser preview while HTML source panes offer a **Preview** action.
- **External file freshness** — open local text files, including rendered Markdown,
  update within 500 ms when another tool writes them. Refresh keeps cursor and scroll
  position. Dirty buffers show **Reload / Keep mine** instead of being overwritten;
  deleted or unreadable files remain open with a warning. The same behavior works in
  main, secondary, and popped-out editor windows.
- **Resource tab grouping** — files, diffs, and browser pages opened by default join
  the nearest pane of the same type; explicit New Tab and New Pane commands still
  use the requested tab or split placement. Opening another resource as a tab
  preserves the surrounding split geometry, including narrow resource columns.
- **Resize preferences** — project, file/memory, and right-hand tool sidebars keep
  their preferred logical-pixel widths as the window changes size. The center
  splits redistribute proportionally at every level. Temporary minimum-size
  clamps do not save over those widths or proportions; enlarging the window or
  reopening a sidebar restores its preference. Divider dragging updates the
  preference, and double-clicking a center divider makes its split equal.
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
- **Instance names** — muxel persists each program's changing auto-title after a
  short debounce, so restored panes and resume views keep their useful names.
  Custom inline names remain a separate override; clearing one falls back to the
  latest auto-title. Rename opens with the current value selected in a full-width
  editor and commits on Enter or blur without the opening click prematurely
  saving it. Bare session UUIDs and transient startup titles such as `cmd.exe`
  or the agent executable name are never used as display names.
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
- **No duplicate projects** — opening a folder that's already a project in this
  workspace is refused with an “already open” error instead of adding a second
  copy. The same goes for remote projects (same host + directory): the
  new-remote-project wizard stays open and says so inline. Symlinked or
  trailing-slash spellings of the same path count as the same project, and on
  Windows hosts so do differences in case or separator.
- **Startup agents** — save the project's open agents as a startup set (preset +
  worktree flag) and relaunch them in one click from the project menu.

## Remote development (SSH)

- **Remote projects** — create a project that lives on a remote host over SSH
  (the project list's network button → pick a host, enter the remote directory,
  optionally verify it). Shells and agents then run on the remote, in a pane that
  behaves exactly like a local one. Local muxel still owns the UI, layout, and
  settings.
- **Import an agent you started outside muxel** — a project's right-click menu has
  **Import…**, which scans for agents running, or last run, outside muxel and lists
  what this project can take over. A **tmux session** is the best case: muxel
  attaches a pane to it, the agent keeps running exactly as it is, and the pane
  survives a restart — this works for any program, including opencode and Amp,
  which can't resume a conversation. An agent **running outside tmux** can't have
  its terminal taken over at all, so muxel offers to resume its conversation in a
  new pane instead and says plainly in the row that the one you started keeps
  running on it too. A **past conversation** with nothing running on it is the clean
  case: muxel starts the agent and resumes where it left off. Rows are ordered with
  the project's own first, then the ones muxel can genuinely attach to, then the
  most recently used; sessions elsewhere on the machine are still listed, tagged
  "outside this project", so you can pull one in deliberately. Each row says what
  that agent was working on, which is what tells two sessions in the same directory
  apart: a live pane's own title (`Quest changes review`), or, for a conversation,
  the summary the agent wrote for it — falling back to the prompt it opened with,
  unwrapped from the slash command that carried it. A pane that hasn't been given
  anything to do yet says nothing, rather than filling the row with the agent's own
  name. Anything a pane
  already holds — a session or a conversation — is left out, so importing can never
  put two panes on one agent. Works for remote projects over their existing SSH
  connection; Windows hosts have no tmux and no conversations to find.

- **Running agents are picked back up** — opening a remote project looks for muxel
  tmux sessions still alive on the host in that project's tree, and re-attaches a
  pane to any the workspace no longer has an instance for. An agent whose workspace
  was lost keeps running on the host with nothing pointing at it; this brings it
  back mid-conversation, exactly where it was, rather than starting a second one
  beside it. It adopts only muxel's own sessions, only within the project, and never
  one a pane already owns or one you deliberately closed — your own tmux is left
  alone. A pane already owns its session even when its saved record names the
  wrong tmux server, as a copy of the layout from before a restart moved the
  session does, and a pane in another project on the same host (one nested in
  this project's folder, say) owns its own, so neither comes back as a second
  pane on the same agent.
- **Closing a remote agent really stops it** — closing a remote pane kills its tmux
  session on the host, and muxel *confirms* the session is gone rather than assuming
  the kill landed. A kill that fails (a blip, a host that is busy or briefly
  unreachable) is retried over the following half-minute, and anything still
  unfinished is finished on the next connect to that host — aimed at the session's
  real name, even if the host has been renamed since. Until it is confirmed dead, the
  close is remembered, so a session whose kill is still in flight is never adopted
  back into a pane: a closed agent stays closed across a muxel restart. A confirmed
  close is still remembered for the rest of the run, so a connect that listed the
  host's sessions a moment before the kill landed can't bring the pane back either.
  Unconfirmed closes are listed in the developer log.
  Discovery and import retain the session's socket as well as its name. Attach,
  restart, close and quit use that owner, leaving a same-named session on another
  socket alone. Older panes resolve their owner before teardown. Claim cleanup
  includes any socket discovery in its two-second deadline and reaps timed-out
  commands before returning.
  Recorded named sockets round-trip across desktop and phone, including names a
  newer peer introduced, without dropping the rest of a shared layout. A restart
  whose owner cannot be saved or confirmed stops with an error instead of silently
  reattaching the old agent.
  A legacy close saves its selected server before the first kill and keeps that
  owner through lost replies, retries and reconnect cleanup. Only one restart
  per pane can resolve and stop a session at a time; stale completions from a
  closed pane or a previous workspace cannot overwrite its new owner. A pending
  restart follows its project back to the main window if its original window
  closes. The iOS companion serializes all layout changes with ownership binding,
  verifies saved pane membership before attachment even for an unchanged owner,
  preserves an already saved owner when a stale attachment disagrees, and rebuilds
  that attachment once from the refreshed layout. It discards stale publication
  after a project switch. Its host status sweep
  includes recorded named sockets and uses the same session as attachment.
- **Remote Windows hosts** — a saved host can be marked **Windows**, and muxel then
  speaks PowerShell to it instead of `sh`: panes, the file browser, editor saves,
  layout sync, the memory file, project scan, remote git, and the connection test
  all work the same as on a Linux host. Every command muxel sends is encoded so it
  reads identically whether the host's OpenSSH `DefaultShell` is `cmd.exe` (the
  Windows default) or PowerShell — there is nothing to configure on the far side
  beyond enabling the SSH server. Interactive panes run PowerShell, pwsh 7+, or
  `cmd.exe`, chosen per host, and load your profile so agents installed by npm or
  winget are on `PATH`.
  Paths, file contents and arguments retain literal apostrophes, including
  smart quotes; the same quoting handles Windows system speech text and voices.
  The one real difference: Windows has no tmux, so a pane there lasts as long as
  its SSH connection rather than outliving it. On reconnect muxel relaunches the
  pane and the agent resumes its saved conversation, so the thread survives even
  though the process does not — and the tmux options are hidden for Windows hosts
  rather than offered and quietly ignored.
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
  still-running agent. A host's panes share multiplexed SSH connections across all
  of its projects, at most 8 panes to a connection — under sshd's default limit of
  10 sessions per connection, past which the host refuses the next one ("Session
  open refused by peer") and the pane has to open a connection of its own. muxel's
  own git, layout-sync and tmux calls ride a separate connection, so a background
  poll never takes a pane's place. Projects opening together (a workspace, launch)
  wait for the first to connect instead of each opening its own, that connect brings
  up every connection its panes are about to use, and "Connected to …" is shown once
  per host rather than once per project.
  Launching a tmux session (remote, or a local tmux-mode project) enables tmux
  `mouse on`, so the pane's scroll wheel scrolls tmux's own copy-mode history
  instead of just the visible screen.
- **Survives a dropped connection** — every SSH connection keeps itself alive with
  periodic probes (`ServerAliveInterval`), so a drop (Wi-Fi blip, laptop sleep, host
  reboot) is *detected* — roughly a minute of silence — instead of the pane freezing
  on a dead socket forever. A dropped remote tmux pane then shows **"Connection lost —
  reconnecting…"** (not "exited"), because its session is still running on the host,
  and muxel keeps retrying on its own — backing off to one attempt every 30s, for as
  long as the outage lasts — until the host is reachable and reattaches the agent
  right where it left off. Resuming a laptop whose Wi-Fi isn't up yet just costs a
  retry or two. An owner lookup or terminal launch already recovering a pane keeps
  its place across retry ticks; an attempt still pending after 60 seconds yields
  to a new attempt so a hung worker cannot stall recovery forever.
  The notification feed reports an outage once per host, not per pane:
  one entry counts the panes still reconnecting and turns into "reconnected" when
  they're all back, so waking with a dozen agents running is one line, not two dozen
  (dismiss it and it stays dismissed until the next outage). On a laptop, a drop
  noticed while the lid is shut (a Power Nap, maintenance or wake-timer wake in your
  bag) waits for the lid to open before
  reattaching — attaching resizes the host's tmux window, which would otherwise yank
  it out from under you if you're using that session from another machine. Working
  lid-down on an external display reconnects as usual. Works on macOS, Windows, and
  Linux with systemd-logind or elogind. (Tune or disable the probe per host in
  Settings → Remotes → Keepalive; blank uses a 20s default, `0` turns it off.)
- **Reconnecting never resizes someone else's session** — a remote tmux pane joining
  a session another client is already attached to stays out of the window size
  (tmux 3.2+'s `ignore-size`), so a laptop reconnecting on a desk, asleep or
  unattended can't resize a session you're using from another computer. Once you
  type, paste, click or scroll in a terminal on that machine, its panes take part in
  the size again, and the window follows whoever used it last, as tmux always does.
  A pane alone in its session attaches normally and always follows its own size;
  with tmux older than 3.2 on the host, panes attach the old way.
- **Reattaches everything on launch** — on startup muxel reconnects the tmux panes of
  *every* remote project in the background, not just the one you had open, so agents
  left running on your hosts come back automatically. Hosts that would need a password
  you haven't saved are skipped (no password storm) and reconnect when you open them.
- **Roaming layouts** — a remote project's pane layout is mirrored to the host at
  `<remote_root>/.muxel/workspace.json`, so opening the same project from another
  machine restores the whole session (the tmux-backed panes re-attach to their
  still-running agents). muxel pushes the layout as you rearrange panes and, on
  connect, loads whichever copy — local or remote — is newer; the replaced copy is
  kept as a one-level backup on each side. A machine coming back after changing
  nothing takes the host's copy whenever it differs from the one it last saw, so
  its old copy never overwrites what other machines recorded meanwhile — which
  conversation each pane is in, which tmux server its session runs on. Automatic
  for every remote project; no setup required. **Changes sync live**: while connected, each side re-reads the
  shared file every few seconds and applies a peer's changes in place — panes a peer
  adds appear, attached to their running tmux sessions; panes it closes go away;
  renames carry over — without restarting any pane both sides share. Only real
  layout changes are pushed: a pane's size and agent status are per-machine and never
  trigger one. A re-read that started before this machine's own push is thrown away
  rather than mistaken for a peer's change, so a pane you just closed never pops back
  on a slow connection. A failing push is reported once (not on every retry) and
  retried in the background.
- **Shareable local projects** — a *local* project also mirrors its layout to
  `<root>/.muxel/workspace.json` (and git-ignores `.muxel/`) when tmux mode is on,
  so its panes are tmux sessions a peer can attach to. The iOS companion app can
  then SSH into the machine, read that file, and bring up / drive the same panes —
  the desktop didn't need to be opened as a "remote" project. The shared
  `tmux_session` name keeps a pane addressing the same session from either side.
  Panes a peer creates over SSH — the iOS app, or muxel on another machine that
  opened this one as a remote project — show up in the local window within a few
  seconds, attached to the peer's tmux session (found by the pane id at the end of
  the session name when the peer didn't record one). Opening a folder that another
  machine already has as a remote project (or the iOS app works in) brings up its
  saved panes, attached to their running agents, instead of a fresh pane.
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
- **Separate user data dir** — `muxel --user-data-dir DIR` puts settings in
  `DIR/config`, state and the control endpoint in `DIR/data`, and credentials in
  a separate keychain service on Windows, macOS and Linux. This separates Muxel
  profiles for testing or a second instance; it is not a security sandbox.
  `DIR` holds the control bearer token, workspace and settings and must be private
  to the intended OS account. On Windows, these files inherit `DIR`'s ACL;
  `--user-data-dir` does not impose an owner-only ACL.
  Outside tools pass the same flag before `ctl`, and panes
  inherit the root even when a preset sets empty or foreign profile variables.
  AppImage cleanup retains the selected root even when its path shares the
  runtime mount's prefix.
  An isolated run refuses a foreign `MUXEL_CONTROL` endpoint. Browser website
  data persists under `DIR/data/webview2` on Windows; isolated macOS and Linux
  browser views use nonpersistent storage, discarded when each view closes.
  If Linux's browser helper cannot start, links, browser presets and restored
  browser panes report failure instead of opening the system browser.
  Local tmux panes require tmux 3.2+ to pass the root and endpoint per session;
  older versions show an upgrade error without launching a fallback shell.
  Restarting the pane after upgrading tmux checks support again.
  Each isolated root uses its own tmux socket locally and over SSH. Discovery,
  reattachment, claims, restart, close and quit stay on that socket, including
  when older or peer metadata names another server. The regular profile keeps the `mxl` socket and reattaches legacy
  default-socket sessions. Version and help exit without initializing a profile.
  Relative paths and `--user-data-dir=DIR` work; empty, repeated or malformed
  forms are refused before startup. The flag can also precede Linux browser-helper
  commands. Child agents still use their normal
  login/config and project files; profiler paths retain their usual locations.
  Credentials are not migrated from the default profile; if a root cannot be
  resolved when first used, its fallback keychain name may differ from a later
  resolved name.

## Settings & theming

- **Settings modal** — sections for Appearance, Editor, Behavior, Speech, Read
  Aloud, Agents, Runners, Snippets, Loops, Remotes, Identities, Projects, and
  Keybindings.
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
- **Installs tmux for you** — on the first launch without tmux (macOS/Linux) muxel
  offers to install it, showing the exact commands first. On Linux it picks the
  distro's package manager from `/etc/os-release` (apt, dnf/yum, zypper, pacman,
  apk, xbps, emerge, eopkg) and elevates only the install: none when already root
  or with passwordless sudo, else the desktop's own polkit password prompt, else a
  sudo password muxel asks for once and doesn't keep. On macOS it runs
  `brew install tmux` as you (Homebrew refuses root). NixOS, rpm-ostree systems,
  and a Mac without Homebrew get instructions and a *Check again* button instead.
  The output streams into the dialog, which can be hidden while it runs; a failure
  offers the command to paste into a terminal. Offered once — *Install tmux…*
  under Settings' tmux toggle opens it again.
- **Agents survive a stray `pkill`** — muxel starts the tmux server itself, from a
  command line naming no project, so an agent running `pkill -f <project>` (to clear
  its own dev server) can't match the *shared* server and kill every session with it.
  Its socket is named `mxl` rather than `muxel` for the same reason: a `pkill -f
  muxel` stops muxel's windows but leaves the agents running in tmux.
- **A tmux server of its own** — muxel keeps its sessions on a private tmux socket
  (`tmux -L mxl`), on this computer and on remote hosts alike, so nothing outside
  muxel that runs a bare `tmux` command can reach them: not your own tmux, and not
  another tool's `tmux capture-pane`, which on builds that abort on it (RHEL 10's
  stock tmux) used to take down the server and every agent in it. A command run
  *inside* a muxel pane is different: tmux points every pane at its own server, so a
  bare `tmux kill-server` typed there still stops muxel's. To look at a session by
  hand, use `tmux -L mxl ls` and `tmux -L mxl attach -t muxel_…`. Upgrading stops
  nothing: a pane whose session is still running on the default server from an
  earlier muxel reattaches to it there (closing the pane still stops it), and its
  next session starts on muxel's own after both servers confirm the old session is
  gone and `mxl` has no session of that exact name. A namesake on `mxl` is left alone
  and the pane keeps its default-server binding but stops, then attaches by itself
  once another muxel sharing the project's layout assigns it the new owner (or
  after you stop that session and restart the pane); retry cannot recreate a
  second agent on Default, and Restart checks for the namesake before it stops
  anything. Once moved, a pane stays on `mxl` whatever an older copy of the layout
  says. A session still running on the default server reattaches even when `mxl`
  gives an unexpected answer. An imported session also stays on its recorded owner
  while running.
  Desktop and phone save an allowed migration
  before launching; a failed probe or save cannot authorize it.
  Reconnecting also retries pending closes on their recorded named servers;
  absence from the standard-server inventory does not forget an unconfirmed close.
  Such a `pkill` reaches only that pane's tmux client: the session and the agent keep
  running, and muxel **reattaches the pane automatically** — you see it blink, not die.
  Local and remote alike (SSH panes and the iOS companion do the same), since a host
  has one tmux server shared by every session on it.
- **New panes start from muxel's environment** — a tmux pane's program starts from
  the tmux server's environment, which tmux copies from whatever started the server.
  When that isn't muxel (an agent that kills the server and then runs `tmux
  new-session` to try something, say), muxel notices before opening a pane there and
  gives the server muxel's own environment back, so new panes don't inherit that
  command's home directory, editor setting, or agent variables. The event log notes
  what was removed. Local panes, with tmux 3.1 or newer.
- **Killed tmux sessions come back** — if the tmux session (or the whole server) dies
  anyway, the pane doesn't tombstone: muxel recreates the session and relaunches the
  agent with `--resume`, so a resume-capable agent picks its conversation back up where
  it left off (tmux scrollback is the only casualty). A deliberate `tmux kill-session`,
  and an agent simply quitting, still close the pane normally. The feed says which
  happened — *reattached* (session survived) or *session restored* (agent resumed).
- **tmux lifecycle** — closing a **pane** kills its tmux session (local or
  remote), unless another pane still shows that session: then it keeps running
  for that pane, and closing the last pane on it stops it. A *dropped* SSH
  connection never auto-closes — it leaves a tombstone pane, keeping the remote
  session reconnectable. Quitting the **app** leaves
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

- **Opt-in session diagnostics** — `MUXEL_PROFILE=1` records background
  session lookup durations and outcomes, plus the remaining launch-time resume
  lookup. Records contain fixed labels, timings and short pane ids, with no
  terminal content, session ids, titles or paths.
- **Opt-in focus diagnostics** — the UI profiler correlates GPUI focus-path
  loss with redraw requests and native Windows ownership. WebView children
  must descend from a registered Muxel window; unrelated WRY apps remain
  external. Records use fixed class buckets and opaque pane/project UUIDs,
  with no terminal text, URLs, titles, or paths.
- **Cross-platform** — Linux (x86_64 + arm64), macOS (Intel + Apple Silicon), and
  Windows (x86_64 + arm64).
- **One title bar on every Linux desktop** — muxel's windows draw their own title
  bar, window controls, resize border, and shadow (client-side decorations), so an
  X11 window manager or a decorating Wayland compositor (such as KDE) doesn't add a
  second title bar above muxel's.
- **macOS menu bar** — the **muxel** application menu has Services, Hide muxel
  (`Cmd+H`), Hide Others (`Option+Cmd+H`), Show All, and Quit muxel (`Cmd+Q`).
  Quit asks for confirmation just like `Cmd+Q`, raising the window first so the
  question shows even when muxel is minimized to the tray.
- **Desktop integration** — app icon and a `.desktop` launcher entry (also the
  notification icon).
- **`muxel --version` and `--help`** — print the version (`-V`) or a short usage
  and exit, without opening a window, so asking the binary for its version never
  starts a second muxel that would take over the running one's `muxel ctl`
  endpoint and window position.
- **Linux: self-cleaning AppImage mounts** — a muxel instance run from an
  AppImage that crashes or is SIGKILLed can't unmount its squashfuse mount, and a
  dead leftover mount makes any filesystem scan (a desktop monitor's periodic
  `df`) stall in the kernel FUSE layer — a periodic Wayland cursor stutter that
  worsens the longer the machine is up. On launch muxel reaps such dead
  `/tmp/.mount_muxel-*` leftovers (leaving live mounts from other running
  instances alone), so they can't accumulate.
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
