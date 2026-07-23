# Terminal input lag under multi-agent load — post-mortem

Branch: `fix-terminal-input-lag-under-load` (2026-07-22). Symptom: with several
agent panes visible, holding a key froze the focused pane for 5–15 seconds; it
caught up the instant the key was released. Typing at normal speed stuttered.

## Root causes (two, both fixed)

### 1. PTY writes blocked the UI thread

`write_all` to the ConPTY input pipe ran synchronously on the UI thread (key
handler, mouse reports, VTE query replies). When a busy agent stops draining
stdin — an Ink TUI deep in its render debounce under key-repeat echo — conhost
stops reading the pipe, the pipe fills, and `WriteFile` blocks until the agent
catches up, which only happens once input pauses. The whole window froze with
it: no draws, no input processing, invisible to in-process profiling because a
blocked thread ticks no counters.

Fix: a dedicated `muxel-pty-writer` thread. Callers queue bytes through
`ChannelWriter` (a `Write` adapter over an mpsc channel) behind the existing
`SharedWriter` type, so no call site changed. Windows Terminal threads its PTY
input for the same reason.

### 2. gpui-on-Windows starves presents under sustained input

gpui presents frames only from `WM_PAINT` — the lowest-priority Windows
message, synthesized only when the queue is idle. Under key-repeat plus PTY
notify traffic the queue never idles. Worse, gpui's `dispatch_key_event`
draws the window synchronously to refresh its dispatch tree — consuming the
dirty flag **without presenting**. Net effect: frames rendered at full rate,
none reached the screen until input stopped. Proven with PresentMon (15s of
zero `Present()` calls while element paints ticked at 20/s) and a per-paint
color-cycling beacon that froze on glass.

Fix: `present_pump` (`crates/muxel/src/present_pump.rs`) — a message-only
HWND on the UI thread plus a watchdog that `PostMessage`s when present is
needed. The wndproc (top of the message loop, `App` not borrowed) then runs
`RedrawWindow(RDW_INVALIDATE|UPDATENOW)` so `WM_PAINT` arrives via the
sent-message channel and gpui presents.

**v2 (soft-lag):** the first pump posted every 8 ms and invalidated *every*
top-level HWND. That cured freezes but under multi-agent load made typing
mushy (settings included) — pump avg ~20 ms, present queue coalesced, cursor
vanished until input stopped. v2 is dirty-gated + adaptive 16–64 ms interval
+ foreground-window-first, with terminal paint caps (focus ≤60 Hz, bg ≤4 Hz).

Do **not** call `RDW_UPDATENOW` from a background thread (cross-thread
`SendMessage` → re-enter while `App` is borrowed → `ERROR gpui::window:
already borrowed`). Do **not** post gpui's `WM_GPUI_FORCE_UPDATE_WINDOW`
either — that sets `force_render` and full-redraws under load. This is a
gpui bug worth upstreaming (any gpui app on Windows with background entity
notifies during sustained typing hits it).

**Removable when fixed:** `present_pump` + `present_flag` are temporary. Exit
criteria are in the `present_pump.rs` module docs (zed#61469 in our gpui pin +
`MUXEL_NO_PRESENT_PUMP=1` regression green).

## Fixed along the way

- **Crash:** `window.request_animation_frame()` called from mouse/modifier
  event handlers panics (gpui's `current_view()` unwraps an empty entity stack
  outside draw phases). Terminal event handlers now hold their view's
  `EntityId` and call `cx.notify(view_id)`.
- **Stale glyphs after font-size change:** shape retention reused `ShapedLine`s
  across font-metric changes indefinitely on static content. Retention is now
  gated on `PaintMetrics::same_font`.
- **~0% shape reuse while output scrolls:** retention was keyed by grid
  position, so a one-line scroll invalidated every line. Now keyed by content
  (text + style + color); streaming reuse measured 96–100%.
- **Per-run baseline drift:** gpui centers each `ShapedLine` on its own
  ascent/descent, so a run containing a fallback glyph (e.g. `❯`) sat 1–2px
  off the row baseline. Every run is now pinned to the base font's baseline.
- **Theme/font changes left idle panes stale:** terminals render through
  `AnyView::cached`; the palette/config refresh paths now `cx.notify()` each
  view.
- **DECSET 2026 contract:** vte buffers synchronized-update windows and leaves
  expiry to the embedder (alacritty services a ~150ms deadline; muxel didn't).
  `process_output` now force-applies a window whose deadline has passed.
  Never observed to fire in practice (`sync_exp=0` throughout) — this is
  latent-bug hardening, not one of the root causes.

## Diagnostics (opt-in — **off by default**)

Nothing profiles unless you set env vars. When off, call sites are a single
OnceLock check.

| Env | Meaning |
|-----|---------|
| `MUXEL_PROFILE=1` | enable **both** terminal + UI profilers |
| `MUXEL_PROFILE_TERMINAL=1` | terminal key→echo→paint only (`profile.rs`) |
| `MUXEL_PROFILE_UI=1` | UI/present-pump/probe only (`ui_profile.rs`) |
| `MUXEL_PROFILE_LOG` | terminal log path (append; rotates at 2 MB → `.1`) |
| `MUXEL_PROFILE_UI_LOG` | UI log path (default: sibling `ui-prof*.log`) |
| `MUXEL_PROFILE_STDERR=1` | also echo dump lines to stderr (default: file only) |

**Terminal log** (`term-prof`): ~500 ms intervals while interesting work
happens (keys, paint spikes, high felt latency). Fields: paint phase splits,
shape reuse, `key→echo` vs `echo→paint`, cursor-row probe.

**UI log** (`ui-prof`): present-pump cost, UI-queue probe RTT, coalesce rate,
1m / 15m / hourly snapshots (working set). Use this for settings typing and
cursor starvation (term-prof cannot see non-terminal keys). Spikes:
`pump >8ms/>30ms`, `probe >50ms/>200ms`, `timeout=`.

Example (PowerShell):

```text
$env:MUXEL_PROFILE = "1"
$env:MUXEL_PROFILE_LOG = "…\term-prof.log"
$env:MUXEL_PROFILE_UI_LOG = "…\ui-prof.log"
.\target\debug\muxel.exe
```

Attach-only PresentMon (optional external tool):

```text
presentmon --process_id <pid> --timed 20 --output_file pm.csv --terminate_after_timed
```

Logged: key/notify/process/paint rates, paint phase splits (build/shape/submit)
with shape-reuse %, felt-latency samples (`key→echo` = agent+ConPTY side,
`echo→paint` = muxel side), sync-expiry count, and a focused-pane cursor-row
probe that shows whether typed bytes reached the grid.

## Ruled out on the way (kept for the next archaeologist)

Agent-side echo throttling as the freeze cause (grid advanced at full key rate
throughout), ConPTY implementation (sideloading Windows Terminal's
`conpty.dll`/`OpenConsole.exe` changed nothing), MPO/hybrid-GPU composition
(froze on both GPUs; PresentMon showed `Composed: Flip` throughout), DWM
compositor starvation (mouse-wiggle forcing compositions changed nothing),
GPU device loss (event log and gpui logs clean). The agent's ~50ms echo
latency for rapid printable input (vs ~0.3ms for backspace) is real but
theirs — likely paste-coalescing — and muxel can't fix it.
