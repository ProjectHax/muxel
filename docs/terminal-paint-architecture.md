# Terminal paint architecture — damage, priority, echo

Status: **implemented (damage + priority scheduler); local echo deferred (settings later)**  
Related: `docs/terminal-paint-under-load.md` (Windows present pump / multi-agent freezes)

## Problem (validated 2026-07-23)

After pump-v2, multi-second freezes and present-queue collapse are largely gone.
Remaining lag: **paste into a busy agent + type in the same pane while it streams**.

Measured (focused Claude, stream + steer typing):

| Stage | Time | Notes |
|--------|------|--------|
| Key handler | ~20 µs | fine |
| key→echo | ~3–11 ms | agent not the main stall |
| Terminal paint | **~8–9 ms** | ~all `submit`; shape reuse **99%** |
| echo→paint | **~55 ms** | felt mush |
| PresentMon p50 | ~84 ms | ~12 fps |

Shape work is already solved. We still **re-submit every visible run on every paint** whenever `content_gen` advances — and on master, **focused panes notified on every PTY batch**, so stream thrashing equals full-viewport submit thrashing on the UI thread.

## Doctrine

1. **Grid generation advanced ≠ redraw the universe.** Use alacritty’s damage.
2. **One paint policy, two priorities** — user-facing feedback beats stream fidelity.
3. **No barnacles** — not “if paste and batch > 4k and focused…”. Prefer damage + priority.
4. **Questionable feel hacks stay opt-in** (settings / later PR).

## What gpui forces (honest constraint)

`TerminalElement::paint` rebuilds the scene for the whole pane. There is no free
“leave clean rows on the GPU from last frame” path without **row-level views**
or a retained texture. So:

| Technique | Saves build/shape? | Saves submit? |
|-----------|--------------------|---------------|
| Damage-guided draw-list patch | yes (partial rebuild) | **no** (still paint all runs) |
| Paint priority / coalesce | n/a | **yes** (fewer paints) |
| Row `AnyView::cached` children | yes | **yes** (only dirty rows re-enter paint) |
| Local echo | n/a | feels faster; same paint when stream runs |

Phase 1–2 ship damage + priority (correct architecture, real win on notify rate).
Phase 3 (local echo) is optional. Phase 4 (row views) is the submit endgame if
submit stays ~7 ms after 1–2.

## Phase 1 — Damage tracking (alacritty `TermDamage`)

**Source of truth:** `Term::damage()` / `Term::reset_damage()` (alacritty_terminal 0.25).

After each `process_output` batch:

1. `processor.advance`
2. Read `term.damage()` → `Full` or `Partial(lines)`
3. `term.reset_damage()`
4. Store a `ContentDamage` summary on the session for the next paint
5. `bump_content()` as today (list invalidation)

Paint path:

- **Full** (or no previous list / metrics change / damage covering most of the viewport): existing full `build_draw_list`
- **Partial**: strip runs/rects for damaged visual lines from the previous list; rebuild **only those rows**; merge; retain shapes; shape missing; **paint full list** (submit still full — see constraint)

This is not a special case for paste: scroll → Full; typed char → Partial one line; stream → Partial many lines or Full.

### Validation

- Profile under paste+stream: `build` time should drop when damage is a few lines
- Correctness: no ghost cells after scroll, resize, clear, alt-screen
- Unit tests: `process_output` of printable input → partial damage includes cursor line; newline flood → Full

## Phase 2 — Paint priority scheduler

Replace “focused ⇒ notify every batch” with:

| Reason | When | Min interval (focused) | Unfocused |
|--------|------|------------------------|-----------|
| **UserEcho** | `write_input` set `expect_echo`; next `process_output` consumes it | **8 ms** (~120 Hz cap) | same as stream bg |
| **Stream** | PTY output without pending echo | **33 ms** (~30 Hz) | **100 ms** (status) |
| **Structure** | resize, selection, search, scroll-from-UI | immediate | immediate |
| **Exit** | process exit | immediate | immediate |

`write_input` always arms `expect_echo`. Multi-key before echo keeps the flag set
so key feedback stays high-priority when the agent is also streaming.

No per-paste special cases: large stream without echo naturally uses Stream 30 Hz.

### Validation

- Stream-only focused pane: notify ≤ ~30 Hz
- Type while stream: echo paints stay ≤ ~8 ms cadence; stream doesn’t force 60+ full paints/s
- PresentMon + term-prof: lower paint count while typing under load; echo→paint drops

## Phase 3 — Local echo (optional, settings-gated later)

**Idea:** for simple printable insert, paint the cell before ConPTY/agent echo.

**Risk:** full-screen Ink/TUIs (Claude) can desync if we guess wrong.

**Recommendation:** **not in the first PR.** Ship as a later opt-in:

- Settings → Behavior → “Local echo for typing (experimental)” default **off**
- Gate: only when not alt-screen / or only for raw mode off — TBD with experiments
- Always reconcile on real echo

Do **not** enable by default until Ink-heavy agents are tested.

## Phase 4 — Row-level retained views (future, if needed)

If submit stays ~7 ms after Phases 1–2: split the grid into per-row cached
views (or a strip atlas). Only dirty rows re-paint. That’s the real submit win
under gpui’s model. Larger refactor; design only until metrics demand it.

## PR plan

| PR | Contents | Base | Settings? |
|----|----------|------|-----------|
| **This PR** | Design doc + Phase 1 damage + Phase 2 scheduler | `master` | no — correct defaults |
| **Later** | Phase 3 local echo | `master` | **yes, experimental, default off** |
| **Later** | Phase 4 row views | `master` | no (architecture) |

Why not two PRs for 1 and 2: damage without scheduler leaves master notifying every
focused batch (worst of both worlds). Scheduler without damage still full-rebuilds
every paint. Ship together; one review, one validation story.

Soft-lag present pump (#14) is orthogonal: that PR is Windows present path; this
is terminal paint path. Either order is fine.

## Profiler hooks (opt-in, off by default)

Existing `MUXEL_PROFILE=1` / `MUXEL_PROFILE_TERMINAL=1`. Extend term-prof lines when
enabled with:

- `dmg=full|partial:N` for last batch
- `reason=echo|stream|structure` for last notify

No always-on logging.

## Non-goals

- More present-pump special cases for paste
- Moving `Term` off the UI thread in this PR
- Changing agent presets or Claude integration

## Success criteria (paste + stream + type into same pane)

- Paint rate under load: stream-dominated ≤ ~30 Hz; with typing, echo still crisp
- `echo→paint` p50 clearly under previous ~55 ms on the same machine
- No correctness regressions (scrollback, selection, search, resize)
- With profilers off, zero new background threads or log files
