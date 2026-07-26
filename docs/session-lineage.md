# Session identity and lineage

This document is the source for the session-name/rename PR and the planned
session-lineage follow-up.

## Incident

Two Claude panes showed the same auto-title. After muxel restarted, only one
appeared to resume.

The saved muxel instances explained it:

- one pane owned a valid Claude session ID;
- the other pane recorded an ID for which Claude had no transcript;
- both visible branches were stored in the first transcript.

The second pane did restore, but muxel correctly rejected its nonexistent
resume ID and started a blank Claude. Because muxel did not persist Claude's
auto-title, the blank pane appeared as “Claude Code.” The combination looked
like a lost pane.

The transcript is intact. Its branches do not have separate session IDs, so
Claude's CLI can resume the selected/latest leaf but cannot address the older
leaf as a separate conversation. That is an addressability failure, not proof
that the older reasoning was deleted. The JSONL retains message UUIDs and
`parentUuid` links; a recovery tool can walk one leaf back to the root and write
that branch as a new, independently resumable transcript.

## Rules

Three identifiers must stay separate:

1. `instance_id`: muxel's pane/process identity.
2. `session_id`: the harness's resumable root conversation.
3. `parent_session_id`: the source conversation when muxel deliberately creates
   a fork.

One muxel instance owns one harness session ID. Two panes must never resume the
same session ID concurrently. A manual display name is not session identity.

Subagents, side chats, remote-control clients, share URLs, PR associations, and
worktrees are links to a session, not replacement session IDs. Muxel should not
turn an internal harness thread into a pane unless the harness exposes a durable
root session ID for it.

## Current PR

The current PR stays narrow:

- persist the latest non-empty terminal auto-title in a three-second coalescing
  window;
- render `custom_name ?? auto_name ?? preset_name` everywhere, including after
  restart;
- keep a manual name as a separate override; clearing it reveals the auto-name;
- commit rename through the input's Enter/blur events, removing duplicate
  mouse-out handlers that could save the opening click;
- open rename at full width with the current value selected;
- render the shared rename input only in the view that opened it, since GPUI
  entities cannot own two sets of bounds in one frame;
- reject a harness session UUID as an auto-title;
- ignore transient startup titles such as `cmd.exe` and the agent executable
  name, leaving the saved/preset fallback visible until the agent reports a
  real title;
- clear copied session state when the existing Duplicate action creates another
  pane, so resume-capable harnesses start without deliberately sharing the
  source ID. Durable ownership for agent-minted IDs such as Codex remains part
  of the lineage follow-up.

The persisted `auto_name` field is backward compatible through
`#[serde(default)]`.

The implementation also:

- extract `Instance::display_name()` in `muxel-core` and use it everywhere
  instead of copying the fallback chain through the UI;
- filter empty live OSC titles and normalize persisted shell titles with the
  same `user@host:` stripping used for live titles;
- port `auto_name` to `ios/Muxel/Models/Instance.swift`, including encode,
  decode, and the display fallback, because iOS writes the shared remote layout;
- exclude `auto_name` from `RemoteLayout::content_key()`. An agent retitling
  itself is local display state and must not schedule a remote layout push.
- preserve a local `auto_name` when applying a peer's structural layout update,
  and flush a pending title before workspace switch or application quit.

The field remains in remote JSON for lossless iOS round-trips. Auto-name-only
differences do not count as layout changes or trigger a push; a later structural
layout write can carry the latest fallback title with the rest of the instance.

## Deferred

First-class fork UI, harness-specific lineage tracking, and Claude orphan-branch
recovery are separate work. They are deliberately excluded from this PR. The
working design and the validated one-off recovery are tracked outside this repo
in `orez-desk/designs/muxel-session-lineage-recovery.md`.

## Automated verification

- Old workspace JSON without `auto_name` loads.
- `Instance::display_name()` covers manual, auto, preset, and empty-value cases.
- An empty/reset OSC title does not erase the last useful persisted name.
- Manual rename wins; clearing it restores the auto-name.
- A bare session UUID falls back to the preset name.
- Duplicate panes clear copied session state and auto-name.
- Auto-title changes do not change `RemoteLayout::content_key()`.
- Swift round-trips preserve `auto_name`.

## Manual verification

- Auto-title changes coalesce into one save within three seconds of the first
  unsaved change, even if titles keep changing.
- A pending title survives application quit and workspace switching.
- Restored shell auto-titles receive the same prefix normalization as live ones.
- Enter and blur each commit once; the click that opens rename cannot commit it.
- Rename shows and selects the complete current value before typing.
- Claude and Grok duplicates start without the source conversation ID. Codex
  agent-minted session ownership remains deferred.
