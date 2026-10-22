# Terminal runtime facts

`TerminalView` keeps one runtime snapshot for each live PTY launch. Its readers
(`status()`, `status_attributable()`, `completion_ready()`, `runtime_snapshot()`)
return the stored snapshot; they never parse terminal rows or advance status.
Attribution also checks the session's current binding revision, so a switch
invalidates cached facts before its queued reconciliation runs.

## Reconciliation edges

The view reconciles after drained PTY output, process exit, a settled resize,
a submission, a provider binding switch, an acknowledged bell, or the earliest
due quiet/provider debounce or switch settle deadline. A one-entry runtime trigger coalesces
redundant invalidations. Timers carry a generation and become no-ops after a
newer deadline replaces them.

Output reconciliation captures the latest visible screen when configured markers
or the provider adapter needs it. Active output is coalesced to at most one scan
per 50 ms with one exact trailing deadline; unchanged generations and ordinary
reads never scan the grid. Render therefore cannot manufacture a lifecycle
transition by polling more often.

## Snapshot fields

`TerminalRuntimeSnapshot` holds:

- `launch_id`: a fresh UUID minted by each spawned `TerminalSession`;
- `binding_rev`: advances when the launch switches provider conversations (see
  below). A provider that switches without telling muxel leaves it unchanged.
  A launch into a conversation muxel knows at spawn begins at revision 1: one
  it pinned with a session flag (`--session-id`, `--resume`), or the one a
  program that reports no id of its own is in. A launch whose provider names
  its conversation once running (a fresh Codex) begins at 0, and its first UUID
  title begins revision 1. A known launch's first UUID title is no switch
  unless it names a different id than the pinned one;
- `snapshot_seq`: advances when the status, its reason, whether it is
  attributable (see below), the binding, or completion readiness changes;
- `input_rev`: advances before ordinary non-empty input writes reach
  the PTY writer; fallible control submissions commit it after successful writer
  admission. Keys, pastes, sends, and mouse and wheel reports count. Focus
  reports and the terminal's own replies to the program don't count;
- `draft_latched`: set by typed input and cleared by a submission (see below);
  mouse and wheel reports leave it as it was;
- the status, whether it is attributable, its strongest visible reason,
  completion readiness, and evidence coverage.

## Provider bindings

OSC title edges, bells, and submissions are recorded in one order per launch,
each with its time. PTY output carries when the reader received it, not when
the drain parsed it: a batch can wait in the queue and in the background
coalescing delay. A binding begins at a point in that order; evidence before it
belongs to the conversation it replaced. When the binding changes, the reducer
resets its latches and reads the new binding's title edges from its start. If
more title edges arrived than the 64-edge buffer keeps, it rebuilds from the
current title without a completion edge.

- A Codex title with a different UUID thread begins the binding at that title,
  in exact order. Earlier frames, and a bell that rang before it, are retired;
  a bell after it, even in the same drained batch, survives.
- Claude's `SessionStart` hook and Grok's session registry report a switch out
  of band, on a clock that can't be ordered exactly against PTY output, and
  muxel reads them up to a tick later. The report places the switch in an
  interval: the hook's own timestamp (for an older muxel's hook, its record's
  write time); Grok's registration time through the last instant it can stand
  for (a time written in whole seconds, to the end of that second); or, for a
  Grok row without one, anywhere from the start of this pane's previous accepted
  registry observation in the same launch and process (the epoch when unknown)
  to the return of this read. Unrelated panes and unresolved polls cannot advance
  that lower bound. A two-second
  slack allows for delayed reports and reads; it is not a bound on either delay.
  Around the interval lies an ambiguity window, from its start less the slack
  to whichever is later: when muxel learned of the switch, or the interval's
  end plus the slack.
  - Output read before the window is the old conversation's; its title is
    retired.
  - Output read inside the window is ambiguous. The current title stays as
    level state (Working, Idle or Blocked), but nothing in the window forms a
    completion edge, latches Done, or rings the bell.
  - Output read after the window, and input written after the interval's end,
    are attributed to the new conversation.

  Until fresh evidence arrives (a title read after the window or, with no
  title, any output read after it), the reducer shows level state only. An
  Idle there may be the old conversation's, so it neither starts a completion
  edge nor arms one; a submission made after the switch still arms. A turn
  that starts and finishes inside the window shows no Done.

  An idle provider may print nothing more after `/clear` or `/resume`, so the
  wait is bounded: two seconds after the window closes, a deadline of its own
  reconciles the reducer, which then takes the level state it shows as the new
  conversation's. That forms no completion edge either (an Idle reached this
  way arms the next turn, as at startup) and doesn't end a screen hold: only a
  title read after the window can confirm a held marker.

  The status shown meanwhile is marked non-attributable
  (`status_attributable()`): a Blocked from an old title or a prompt still on
  screen may be the old conversation's. The app neither notifies for a
  non-attributable status nor records it as the pane's last status (unread
  state, transitions), so a Blocked or Done that fresh evidence confirms is
  still a transition and still notifies. A process exit, and a bell (which
  rings only for output read after the window), stay attributable.

  Fresh output alone doesn't make an unchanged screen marker the new
  conversation's. A switch reported out of band starts a hold on the screen;
  muxel doesn't capture the screen at the switch, so later captures decide when
  the hold ends. While it lasts, a status read from a screen marker (a provider
  prompt or a configured marker) stays non-attributable, after the window too.
  The hold ends when a capture shows no marker, or when a title read after the
  window reports the state the screen shows. Held Working evidence never enters
  completion history: removing an old marker cannot finish a newly submitted
  turn. Explicit submissions survive the hold independently.

  Retained provider evidence must be attributable at its source. Uncertain
  titles do not seed sticky semantic status or readiness. Grok's 750 ms Blocked
  title hold and two-second Working row hold start only from an attributable
  title or row; an old row cannot acquire a fresh lifetime when it disappears.

  Control commands use current runtime facts rather than the attention history
  held through uncertainty. A non-attributable status, including a cached
  snapshot from before the session's current revision, is treated as Working:
  an ordinary `send` is refused and `wait` keeps waiting, at most until the
  settle deadline unless an old prompt's screen hold persists. `send --force`
  retains its explicit override. Exit and failed/not-running launch states keep their
  existing control behavior.

  Docked and popped-out terminals use the same attribution gate for activity
  and attendance. A popout records later attendance once without delivering
  alerts or audio. Redocking always transfers the live terminal, but preserves
  attention cards until its status is attributable to the current binding.
  Scheduled-loop heartbeats also resolve the live terminal across both windows;
  moving a pane does not release its run. Existing Exit cleanup removes the
  live terminal and popout window when it closes that pane.
  Window removal runs after the app update returns so synchronous close
  observers can update the app safely. Restart redocks a popped-out terminal
  before stopping its old process and installing its replacement.

  Scheduled-loop cleanup also reads the current snapshot, never retained
  attention history. It owns the launch and the conversation that received
  the first submitted turn, only when its identity was known before that
  submission: known at spawn (a fresh Claude loop pane, launched with the
  `--session-id` muxel minted) or named by a title first. Without a prior
  identity (a fresh Codex launch whose prompt precedes its first title),
  ownership stays unknown for the whole run: a later UUID or hook cannot prove
  which conversation received the prompt.
  Later identities or submissions do not establish or transfer ownership.
  Replacement
  launches or conversations abandon the old run without completion or close.
  Completion and closing at the runtime safety cap require attributable
  evidence for the owned binding. The cap always releases tracking: if startup
  never submits or the status stays uncertain, it abandons the run without
  completion or pane close, allowing later scheduled fires. Unknown ownership
  has the same non-closing behavior. In that ordering, an Exit-policy loop
  leaves its pane open until a human closes it.

  The app records each pane's last status with its binding (launch and binding
  revision). Within one PTY launch, the first attributable status after a
  switch is a transition and a new event in the pane's activity, even when it
  repeats the old conversation's last state. Across a relaunch, equal statuses
  still compare as unchanged, so after a relaunch into a different
  conversation, an event in the state last recorded for the pane can be
  missed.
- Claude's hook stamps each record with a nonce. A record whose nonce this
  launch hasn't seen is a switch even when it names the conversation already
  bound, so `/resume B` then `/resume A` between two reads is a switch. The
  launch's first record, and a record from an older muxel's hook, switch only
  when they name another conversation: a resumed launch's own `SessionStart`
  names the conversation muxel launched. Recurring discovery validates an
  immutable snapshot on a worker. The UI claims the current record by
  renaming it, then requires its bytes and write time to match that snapshot
  before applying or acknowledging it. Prelaunch recovery uses the same UI
  claim owner. A record the hook writes meanwhile waits at the shared path;
  a newer mismatching claim or one whose binding failed to save is retained
  and validated before the next shared record. So is a record muxel can't
  adopt yet: its transcript doesn't validate yet (`/clear` can run before
  Claude creates the cwd's project directory), or another pane owns its
  conversation. A newer shared record supersedes it, so one that never
  validates can't hold back the next; only a record that isn't a Claude
  `SessionStart` event is dropped unadopted while its writer runs. Such a
  record names a conversation of the process that wrote it, so prelaunch
  recovery keeps it only when the launch reattaches that process's live tmux
  session; a launch that starts a new process (a direct pane, an ended tmux
  session, or one tmux can't answer for) drops it, so the new process never
  adopts its predecessor's conversation. Prelaunch recovery refuses the
  launch when a pending record can't be claimed or read, or its binding can't
  be saved. Failing to acknowledge a saved binding is logged and the launch
  goes ahead, unless a newer record waits behind the stuck claim.
- An accepted Grok identity is retained separately from durable workspace
  metadata. A real identity change invalidates the live terminal before saving,
  so a failed save cannot leave an old Exit-policy loop owning the replacement.
  Metadata still rolls back on failure; retrying the same accepted identity only
  retries persistence. A new launch or child process starts with no prior Grok
  observation. An accepted identity also reserves the conversation against
  sibling discovery while its metadata save is pending. The ownership snapshot
  checks the current pane, launch and child PID and updates claims as each result
  in a batch is staged, including a switch back that leaves metadata unchanged.
  Before Restart stops the process or redocks the pane, it saves the accepted
  identity for the resume command. A failed save refuses Restart and leaves the
  current process and window owner intact.
- A submission counts only in the binding it was made in, so the Enter that
  picks a conversation in `/resume` doesn't arm the resumed conversation's
  completion, including when the resumed UUID is the first identity observed.

## Submissions

A submission is muxel's own judgement, not a provider's. A bare carriage
return typed at the keyboard marks one; `ESC CR` (Shift+Enter, Alt+Enter)
never does. Muxel's automated sends that end in Enter mark one too: a startup
prompt, auto-continue, a broadcast, a snippet, an inserted transcript, and a
`ctl` send. A provider can read the same bytes differently. OpenCode queues a
message on Alt+Enter, which muxel doesn't count as a submission.

Control Enter commits writer admission, submission evidence, draft clearing,
and runtime notification together. A refused write or flush leaves no pending
submission for the next typed character to consume.
Its input timestamp is captured before publishing bytes and committed only
after admission succeeds, so a resume hook caused by that Enter cannot make
the selector count as a submission to the replacement conversation.

## Coverage boundary

The reducer keeps the existing no-hook classifier. Provider titles, visible
rows, configured markers, bells, recent output, and process exit are valid
evidence for the fields they describe. Quiet visible output never proves that
a provider has no hidden background work, so the snapshot reports
`ActivityOnly`, `VisibleMarkers`, or `ProviderVisible` coverage rather than
claiming more than the terminal shows.
