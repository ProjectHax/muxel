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

Fix: `spawn_present_pump` in `crates/muxel/src/main.rs` — a watchdog thread
calling `RedrawWindow(RDW_INVALIDATE | RDW_UPDATENOW)` on the UI thread's
windows every 8ms. `RDW_UPDATENOW` delivers `WM_PAINT` through the
sent-message channel, which bypasses posted-queue priority. When nothing is
dirty, gpui's request-frame handler is a no-op, so the idle cost is small.
This is a gpui bug worth upstreaming (any gpui app on Windows with background
entity notifies during sustained typing hits it).

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

## Diagnostics (opt-in, `MUXEL_PROFILE_TERMINAL=1`)

`crates/muxel-terminal/src/profile.rs` logs 500ms interval stats to
`term-prof.log`. Run any build with `MUXEL_PROFILE_TERMINAL=1`; set
`MUXEL_PROFILE_LOG` for the log path and `XDG_CONFIG_HOME`/`XDG_DATA_HOME`
to sandbox away from the real workspace. Logged: key/notify/process/paint
rates, paint phase splits
(build/shape/submit) with shape-reuse %, felt-latency samples (`key→echo` =
agent+ConPTY side, `echo→paint` = muxel side), sync-expiry count, and a
focused-pane cursor-row probe that shows whether typed bytes reached the grid.
`env_logger` is initialized at `warn` so gpui render errors (present failures,
device loss) are visible on stderr.

## Ruled out on the way (kept for the next archaeologist)

Agent-side echo throttling as the freeze cause (grid advanced at full key rate
throughout), ConPTY implementation (sideloading Windows Terminal's
`conpty.dll`/`OpenConsole.exe` changed nothing), MPO/hybrid-GPU composition
(froze on both GPUs; PresentMon showed `Composed: Flip` throughout), DWM
compositor starvation (mouse-wiggle forcing compositions changed nothing),
GPU device loss (event log and gpui logs clean). The agent's ~50ms echo
latency for rapid printable input (vs ~0.3ms for backspace) is real but
theirs — likely paste-coalescing — and muxel can't fix it.
