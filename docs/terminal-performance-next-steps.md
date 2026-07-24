# Terminal performance: next experiments

Status: design only. Start here if damage tracking, paint priority, and the
Windows present pump do not make terminal interaction consistently fast.

Reader: muxel maintainers diagnosing terminal latency on Windows.

Related:

- `docs/terminal-paint-under-load.md` — fixed freezes: blocking ConPTY writes
  and missing presents under sustained input
- `docs/terminal-paint-architecture.md` — implemented damage tracking,
  paint priority, and trailing-edge paints

## Decision

Do not move the terminal parser to another thread. The 2026-07-24 ETW capture
put the saturated UI thread in Taffy layout and GPUI scene construction, not
terminal parsing. Plain Cargo dev mode made that work much worse: even primitive
`f32` arithmetic remained as out-of-line calls.

The first fix is an optimized, instrumented dev profile. The next structural
change should reduce how much stable UI is laid out and submitted after a
terminal update. Move parsing only if a later trace shows parsing has become
material.

The likely end state is:

1. bounded PTY queues;
2. a scheduler that always paints the newest dirty generation;
3. parsing outside the UI thread only if parsing or UI queue wait is material;
4. retained rows or a terminal texture only if scene submission remains the
   dominant cost.

That list is a set of hypotheses, not a roadmap.

## 2026-07-24 baseline and finding

Workload: sustained Codex output in a terminal pane while typing, with a browser
pane open. The running binary was a plain `cargo build -p muxel` debug build.

| Signal | Result |
|---|---:|
| Muxel CPU | 116–120% of one core |
| Hot UI thread CPU | 9.92–9.97 s per 10 s |
| Present rate | 11.3 fps |
| Present gap p50 / p95 / max | 88.8 / 221.6 / 255.3 ms |
| Average CPU busy per frame | 107.1 ms |
| ETW samples on hot thread | 34,295 |

Manual symbolization of a one-second interval resolved the largest named groups
to Taffy layout, GPUI scene/layout, and allocation/container work. Primitive
`f32::add` alone held 81 of 963 instruction-pointer samples. Terminal parsing
did not appear as a major exclusive hotspot.

This proves two problems:

1. terminal updates trigger broad layout/scene work on the UI thread;
2. unoptimized dependencies multiply that cost enough to starve input.

`Cargo.toml` now builds local crates at optimization level 1 and dependencies
at level 3 while retaining debug assertions and PDBs. Re-run this workload
before changing invalidation. Optimization is the baseline; it does not excuse
the broad layout work.

## Terms

- **Input-to-grid**: key handler start until the corresponding TUI reaction is
  visible in the parsed terminal grid. This includes the agent and ConPTY.
- **Grid-to-paint**: grid change until muxel finishes submitting the terminal
  scene to GPUI.
- **Paint-to-present**: paint completion until the swap chain presents the
  frame. PresentMon measures this boundary on Windows.
- **UI queue wait**: time work spends waiting for GPUI's UI thread before
  `view.update` begins.
- **Generation**: a monotonic number identifying terminal content. A dirty
  generation has changed; a painted generation has reached `TerminalElement`.
- **High-water mark**: the largest observed queue size during a run.

## Performance contract

Use these as initial gates on the same machine and workload. Change a threshold
only with recorded evidence.

| Signal | Gate |
|---|---:|
| Key handler p99 | under 0.25 ms |
| UI queue wait p95 / p99 | under 4 ms / 12 ms |
| Grid-to-paint p95 / p99 | under 16 ms / 33 ms |
| Focused interaction present gap p99 | under 33 ms |
| Focused stream paint rate | 25–35 Hz |
| Background paint rate | at most 10 Hz per visible pane |
| Final dirty generation after output stops | painted within 120 ms |
| Paints over 8 ms | under 1% |
| Queue size after a 30 s steady stream | bounded; no upward trend |
| Lost or reordered PTY bytes | zero |

Input-to-grid depends on the child. Report it, but do not assign it to muxel
unless a deterministic child shows the same regression.

## Fix the profiler before changing architecture

The current `term-prof[v5]` names two samples too strongly:

- `key→echo` closes on the next focused PTY batch. During streaming, that batch
  may predate the key. Rename it `key→next-output`.
- `echo→paint` starts at that same arbitrary batch. Rename it
  `output→paint`.

Those samples remain useful as upper-level symptoms. They do not identify the
slow stage.

### Add a v6 event path

Give each processed output batch a sequence number and timestamps for:

1. PTY reader received bytes;
2. bytes entered the output queue;
3. drain task received the first chunk;
4. `view.update` was requested;
5. `view.update` began;
6. VTE parse began and ended;
7. dirty generation advanced;
8. paint notify was requested;
9. `TerminalElement::paint` began and ended;
10. generation was marked painted.

Log interval aggregates, not one line per event. Per-event logging changes the
workload.

Add these fields:

```text
read_bytes
output_queue_bytes_current / output_queue_bytes_high
writer_queue_bytes_current / writer_queue_bytes_high
reader→drain p50/p95/p99/max
ui_queue_wait p50/p95/p99/max
parse p50/p95/p99/max
dirty→notify p50/p95/p99/max
notify→paint p50/p95/p99/max
dirty_gen / painted_gen / skipped_gens
trailing_timer_armed / fired / superseded
paint reason: interaction / stream / structure / exit
visibility: focused / visible-background / hidden
```

Use fixed-size histograms or logarithmic buckets. Averages hide the stalls this
work is meant to find.

### Measure presents outside the process

Capture PresentMon data for the isolated test process, not a working session.
Correlate its CSV timestamps with the epoch timestamps already written by
`term-prof`.

Report:

- present count and frames per second;
- p50/p95/p99/max gap between presents;
- time from the nearest completed paint to the next present;
- any interval with paints but no presents.

If paints complete on time but presents stop, terminal architecture is not the
next problem. The Windows/GPUI present path is.

## Reproducible workloads

Use a separate `target/debug/muxel.exe` with isolated config and data for
repeatable experiments. If a severe failure exists only in a long-running
session, preserve that process and take a read-only ETW/PresentMon capture
before restarting it. Never replace a live binary as part of profiling.

Run synthetic and real workloads. Synthetic runs identify muxel cost. Real
agents catch behavior the synthetic child missed.

### Synthetic cases

1. **Idle**: one focused terminal, no output, 30 s.
2. **Type only**: deterministic child echoes one byte per key.
3. **Stream only**: deterministic child writes fixed-size ANSI frames at a
   controlled byte rate.
4. **Stream and type**: the same stream while typing a fixed 10 s input script.
5. **Burst then stop**: output lands inside each throttle interval, then stops.
   The last generation must paint.
6. **Many panes**: 1 focused stream plus 7, 15, and 23 background streams.
7. **Backpressure**: child stops reading stdin while key repeat and a large
   paste are sent.
8. **Correctness**: scrollback, resize, alternate screen, selection, search,
   font zoom, synchronized updates, and process exit.

The stream child should accept byte rate, frame size, ANSI density, and duration
as arguments. Commit the generator once it becomes part of a performance claim.

### Real case

Use one agent and one prompt that produces at least 30 s of steady output. Paste
a follow-up and type while it is still streaming. Record the agent version,
terminal size, pane count, font, GPU, monitor refresh rate, and whether the
browser pane is enabled.

Real-agent results are validation. They are not a stable benchmark.

## Experiment protocol

1. Build one binary containing runtime flags for both paths. This avoids
   comparing unrelated builds.
2. Warm up for 10 s.
3. Capture 30 s.
4. Run five trials in `A B B A A` order.
5. Keep terminal dimensions and pane layout fixed.
6. Save the binary commit, flags, term-prof log, and PresentMon CSV together.
7. Compare medians and tails. Do not call a win from one run.

A change wins only when:

- its target metric improves by at least 20% or crosses a failed contract gate;
- no p95 or p99 interaction metric regresses by more than 10%;
- CPU and memory do not regress materially;
- all terminal correctness tests pass;
- the result repeats in at least four of five trials.

Delete an experiment flag after the decision. Permanent dormant branches make
later profiling ambiguous.

## Experiment 1: prove whether damage patching pays

### Hypothesis

Partial row rebuilds reduce grid-walk time, but cloning, filtering, hashing, and
sorting the old draw list may cost as much as rebuilding it. Full scene
submission still happens either way.

### A/B

- A: full draw-list rebuild with content-keyed shape reuse.
- B: current damage-guided `patch_draw_list`.

### Evidence

Compare build, shape, submit, total paint, allocation count, and paints over
8 ms. Split results by one damaged row, quarter screen, half screen, and scroll
(`Full` damage).

### Decision

Keep damage patching only if it removes at least 0.5 ms from p95 total paint or
reduces total paint by 10% on the common one-row case. If it only improves the
already-small build phase, remove it.

## Experiment 2: make scheduling observable

### Hypothesis

Some latency remains because `last_paint_notify` measures a request, not an
actual paint. GPUI may coalesce or delay a notify while the scheduler believes
the frame happened.

### Change

Track `dirty_generation`, `notified_generation`, and `painted_generation`.
Trailing timers compare dirty with painted. A timer does nothing when the
newest dirty generation is already painted.

Do not retry at 8 ms forever. One retry after the deadline is enough for
measurement; repeated failure belongs to the present path.

### Evidence

Count:

- notifies that produced no paint;
- multiple notifies for one painted generation;
- generations skipped because a newer state replaced them;
- final dirty generations left unpainted after 120 ms.

Skipping intermediate visual generations is correct. Leaving the newest
generation unpainted is not.

### Decision

Keep generation-aware scheduling if it eliminates stale final generations and
reduces redundant notifies without increasing grid-to-paint p99. If the counts
show every notify already paints, retain the counters and skip the scheduler
change.

## Experiment 3: time-budget PTY drain work

### Hypothesis

`MAX_BYTES_PER_TURN = 256 KiB` bounds bytes but not UI-thread time. ANSI-heavy
input can hold the UI thread much longer than plain text of the same size.

### A/B

- A: current byte cap.
- B: stop parsing after 1 ms or 2 ms of UI-thread work, then yield and continue.

Preserve byte order. Do not notify between slices unless the paint scheduler
admits a frame.

### Evidence

Compare UI queue wait, parse max, key-handler latency, output throughput, queue
high-water mark, and total time to drain a fixed 10 MiB stream.

### Decision

Keep the time budget if UI queue p99 improves without reducing sustained parse
throughput by more than 10%. Tune one budget; do not add pane-count exceptions.

## Experiment 4: bound both PTY queues

### Hypothesis

The reader and writer channels can grow without limit. The current design stays
responsive by moving waiting into memory.

### Change

Measure queue depth in bytes first. Then add byte budgets.

- Output queue: block the PTY reader at the budget and let OS/PTY backpressure
  propagate.
- Writer queue: chunk large paste input. Reserve capacity for control replies
  and ordinary keys so they do not wait behind a paste.
- Never reorder bytes within one PTY stream.

Define overflow behavior before implementation. Silently dropping input is not
allowed.

### Evidence

Run the 30 s backpressure case. Record memory, queue high-water marks, writer
stall duration, input-to-grid latency after the child resumes, and byte-for-byte
output correctness.

### Decision

Memory must plateau. Recovery after the child resumes must not lose or reorder
bytes. If a bounded writer cannot meet both constraints, keep the writer
unbounded temporarily but ship high-water telemetry and a documented limit
decision.

## Experiment 5: parse outside the UI thread

Do this only if v6 shows either:

- VTE parsing consumes more than 20% of the UI thread during the failing case;
  or
- UI queue wait remains above contract after time-budgeted draining.

### Candidate design

A per-terminal runtime owns `Term` and `Processor`. It consumes PTY bytes in
order and publishes the newest immutable render snapshot plus generation.

The UI:

- never parses raw PTY bytes;
- never blocks on the live parser;
- consumes the newest complete snapshot;
- may skip superseded snapshots;
- sends input, resize, selection, search, and scroll commands back to the
  runtime in order.

Start with a full visible-grid snapshot. A 200×60 grid is small enough to
measure before inventing a diff protocol.

### Risks

- selection and search currently expect synchronous terminal access;
- VTE listener callbacks write replies to the PTY;
- resize ordering must remain exact;
- snapshot publication can copy more than current parsing costs;
- a mutex around the live `Term` merely moves UI blocking; it is not this
  design.

### Evidence

Compare UI queue wait, parse CPU, snapshot copy cost, memory bandwidth, and
input-to-grid. Run every terminal interaction test plus resize and alternate
screen stress.

### Decision

Keep it only if UI queue p99 falls below contract and snapshot publication costs
less than the UI time removed. Otherwise revert and keep parsing in bounded UI
turns.

## Experiment 6: retain submitted terminal rows

Do this only if submit remains over 70% of p95 paint time after earlier
experiments.

Test two prototypes:

1. cached row views;
2. one retained terminal texture or glyph instance buffer.

Row views are easier but create many entities and notifications. A retained
texture is a larger renderer boundary but makes work proportional to damaged
rows. Neither is assumed to win.

### Evidence

Measure total submit time, GPUI element count, allocations, GPU time, resize
cost, and memory across 1, 8, and 24 visible panes.

### Decision

Require at least a 30% reduction in p95 total paint and no resize or glyph
correctness regression. Prefer the smaller implementation when results are
within 10%.

## Experiment 7: gate the Windows present pump

The pump currently wakes every 8 ms and redraws every top-level GPUI window.
That fixed a real framework failure. Optimize it only after PresentMon proves
idle or multi-window cost matters.

### Candidate

Set `present_needed` when terminal work requests a frame. Post pump messages
only while that flag is set, target known GPUI windows, and stop after a
successful present or a short quiet deadline.

### Evidence

Compare idle CPU, wakeups, present gaps, and the original held-key workload.

### Decision

The demand-driven pump must preserve the original guarantee: no interval with
completed paints and zero presents under sustained input. If that recurs, keep
the unconditional pump until GPUI fixes the Windows scheduler.

## Rejected shortcuts

- **Local echo first**: it masks latency and guesses what a full-screen TUI will
  draw.
- **More threads without ownership changes**: a background parser plus a mutex
  can block the UI at paint time.
- **More fixed byte thresholds**: byte count is not elapsed UI time.
- **One benchmark run**: scheduler and compositor noise are large enough to
  manufacture wins.
- **Average-only reporting**: users feel the missing frame, not the mean frame.
- **Profiling a working session**: it risks user work and contaminates results
  with unrelated load.

## Required result note

Every performance PR should include:

```text
Machine / GPU / refresh rate:
Commit and runtime flags:
Scenario and duration:
Trials:

Before p50 / p95 / p99 / max:
After  p50 / p95 / p99 / max:
CPU before / after:
Memory high-water before / after:
Queue high-water before / after:
Present gaps before / after:
Correctness gate:

Decision:
Raw artifact paths:
```

No raw artifacts means no performance claim.

## 2026-07-24 optimized-dev check

One before/after session tested the dev-profile change under interactive
terminal output. The workloads were similar but not controlled trials, so these
numbers justify the default and set the next baseline; they do not prove that
the remaining architecture is optimal.

| Measure | Plain dev | Optimized dev | Change |
| --- | ---: | ---: | ---: |
| Approximate FPS | 11.3 | 64.6 | 5.7x |
| Present interval p50 | 88.8 ms | 15.5 ms | -82.5% |
| Present interval p95 | 221.6 ms | 133.2 ms | -40.4% |
| CPU busy per frame | 107.1 ms | 33.0 ms | -69.2% |
| Frames at least 50 ms | 100% | 27.7% | -72.3 points |
| Frames at least 100 ms | 30.4% | 11.5% | -18.9 points |
| Muxel user-code samples | 2.14% of 32 cores | 0.35% of 32 cores | -83.6% |

The optimized build removed most steady-state CPU cost. It did not remove the
tail: p95 remained 133.2 ms. Terminal parsing stayed cheap, while ETW samples
in the plain build concentrated in GPUI scene/layout work, Taffy layout,
allocation/container code, and unoptimized primitive arithmetic. That supports
keeping optimized dependencies in dev and investigating why terminal updates
still trigger broad layout and scene construction.

The UI profiler log embedded in the after-run report was stale. Future capture
scripts must record process identity and capture start time in each profiler
output, then reject files older than the process or run. Treat that as a
profiling correctness bug, not missing evidence to fill by inference.

Next: repeat controlled trials on the optimized baseline, then run Experiments
1 through 3 before changing the present pump or moving parsing across threads.
