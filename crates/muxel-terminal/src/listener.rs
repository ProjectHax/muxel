//! `alacritty_terminal` event listener: handles title/bell, queues OSC-52
//! copies, and writes the replies only the emulator can give (cursor-position
//! and mode reports) back to the child. Every other query is answered on the
//! PTY reader thread — see `replies`.

use crate::replies::Replies;
use crate::session::LaunchBinding;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::ClipboardType;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Shared, thread-safe handle to the PTY's input side.
pub(crate) type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

const MAX_TITLE_EVENTS: usize = 64;
const MAX_TITLE_EVENT_BYTES: usize = 16 * 1024;

/// Attribution allowance around a provider switch reported out of band.
/// The reporter's time trails the switch:
/// Claude's hook stamps its record once Claude has spawned the hook process and
/// it has started, and Grok's registry time may be whole seconds. PTY output
/// carries when the reader received it, which trails the provider's write by
/// the ConPTY or tmux relay, the reader's scheduling, and up to the 150 ms vte
/// holds a synchronized update. Two seconds is an allowance, not a bound on
/// either delay; older output read beyond the window can appear fresh. A turn
/// starting and finishing inside the window shows no Done.
pub(crate) const SWITCH_SLACK: Duration = Duration::from_secs(2);

/// How long after a switch window closes the reducer waits for fresh evidence
/// before it takes the level state it shows as the new conversation's. An idle
/// provider may print nothing more after `/clear` or `/resume`; without a bound
/// its status would stay uncertain, and `ctl` would treat it as Working, until
/// something else printed. Settling this way forms no completion edge.
pub(crate) const SWITCH_SETTLE: Duration = Duration::from_secs(2);

/// Where one piece of lifecycle evidence (an OSC title, a bell, a submitted
/// turn) falls in this PTY's evidence order. The sequence orders evidence
/// exactly; the wall-clock time places it against a provider switch that is
/// reported out of band, such as a hook's record read on a later tick. For PTY
/// output, the time is when the reader received the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EvidenceMark {
    seq: u64,
    at: SystemTime,
}

struct TitleEvent {
    mark: EvidenceMark,
    title: Option<String>,
}

/// Recent OSC title edges, oldest first, bounded by count and bytes. Reading
/// does not consume them: the reader keeps a cursor, so it can read a new
/// binding's edges from its start.
#[derive(Default)]
struct TitleEventBuffer {
    events: VecDeque<TitleEvent>,
    bytes: usize,
    /// The newest edge dropped to stay within bounds. A reader whose cursor is
    /// behind it has missed an edge.
    evicted: Option<EvidenceMark>,
}

impl TitleEventBuffer {
    fn push(&mut self, mark: EvidenceMark, title: Option<String>) {
        let bytes = title.as_ref().map_or(0, String::len);
        if bytes > MAX_TITLE_EVENT_BYTES {
            self.events.clear();
            self.bytes = 0;
            self.evicted = Some(mark);
            return;
        }
        while self.events.len() >= MAX_TITLE_EVENTS
            || self.bytes.saturating_add(bytes) > MAX_TITLE_EVENT_BYTES
        {
            let Some(dropped) = self.events.pop_front() else {
                break;
            };
            self.bytes -= dropped.title.as_ref().map_or(0, String::len);
            self.evicted = Some(dropped.mark);
        }
        self.bytes += bytes;
        self.events.push_back(TitleEvent { mark, title });
    }

    /// The edges after `cursor` that were read after `fresh_after` (every one,
    /// when `None`), the cursor past all edges after `cursor`, and whether an
    /// edge after `cursor` was evicted.
    fn after(
        &self,
        cursor: u64,
        fresh_after: Option<SystemTime>,
    ) -> (Vec<Option<String>>, u64, bool) {
        let titles = self
            .events
            .iter()
            .filter(|event| event.mark.seq > cursor)
            .filter(|event| fresh_after.is_none_or(|after| event.mark.at > after))
            .map(|event| event.title.clone())
            .collect();
        let evicted = self.evicted.map_or(0, |mark| mark.seq);
        let last = self.events.back().map_or(0, |event| event.mark.seq);
        (titles, cursor.max(last).max(evicted), evicted > cursor)
    }
}

/// One read of the evidence for the runtime reducer.
pub(crate) struct EvidenceRead {
    /// Title edges after the reader's cursor, in order, except those read
    /// inside a switch window: those can't form a completion edge.
    pub titles: Vec<Option<String>>,
    /// The cursor for the next read.
    pub cursor: u64,
    /// An edge after the cursor was evicted unread.
    pub overflowed: bool,
    /// A turn was submitted in the current binding: after the title that began
    /// it, or after the latest instant a switch reported out of band can have
    /// happened.
    pub submitted: bool,
    /// The binding began at a switch reported out of band and has no fresh
    /// evidence yet, so the level state may still be the old conversation's.
    pub settling: bool,
    /// While settling: when it ends without fresh evidence (see
    /// [`SWITCH_SETTLE`]).
    pub settles_at: Option<SystemTime>,
    /// The current title was read after the binding's switch window (or no
    /// window began it). Settling at the deadline doesn't make an older title
    /// fresh: it can't confirm a held screen marker.
    pub title_fresh: bool,
    /// The binding began at a switch reported out of band.
    pub windowed: bool,
}

/// Which current evidence a binding switch retired.
pub(crate) struct BindingSwitch {
    pub title: bool,
    pub bell: bool,
}

/// Where a switch reported out of band stops being ambiguous. Output read
/// before the switch could have happened, less [`SWITCH_SLACK`], is the old
/// conversation's. Evidence after these points is the new one's. Evidence in
/// between could be either: it shows as level state and decides nothing.
#[derive(Clone, Copy, Debug)]
struct SwitchWindow {
    /// The latest the switch can have happened. Input written after it went to
    /// the new conversation; input has no read lag.
    input_after: SystemTime,
    /// When muxel learned of the switch, or `input_after` plus the slack if
    /// that is later. Output read after it is the new conversation's.
    output_after: SystemTime,
}

impl SwitchWindow {
    /// When the level state settles if no fresh evidence has arrived.
    fn settles_at(&self) -> SystemTime {
        self.output_after
            .checked_add(SWITCH_SETTLE)
            .unwrap_or(self.output_after)
    }
}

/// A PTY launch's lifecycle evidence, in one order, and the provider binding
/// (conversation) it belongs to. A binding begins at a sequence number;
/// evidence recorded before it belongs to the conversation it replaced. A
/// binding begun by a switch reported out of band also has a window: evidence
/// inside it is ambiguous.
#[derive(Default)]
pub(crate) struct LifecycleEvidence {
    seq: u64,
    titles: TitleEventBuffer,
    /// The edge that set (or reset) the current title.
    title: Option<EvidenceMark>,
    /// The latest bell. Acknowledging it clears the session's flag, not this.
    bell: Option<EvidenceMark>,
    submitted: Option<EvidenceMark>,
    /// The first submitted turn owns an unattended loop run. Later input must
    /// never transfer that ownership to a replacement conversation. Revision
    /// zero means its recipient was unknown and stays unknown for this run.
    first_submission: Option<u64>,
    /// First admitted submission in this launch, captured with its evidence.
    first_submission_at: Option<SystemTime>,
    /// What muxel knew at spawn about the conversation the launch opened. A
    /// known one is revision 1 from the start.
    launch: LaunchBinding,
    binding_rev: u64,
    binding_since: u64,
    /// The window of the switch reported out of band that began the current
    /// binding, if one did.
    window: Option<SwitchWindow>,
    /// When the reader received the output being parsed.
    reading_at: Option<SystemTime>,
    /// Output read after the window has been parsed.
    fresh_output: bool,
}

impl LifecycleEvidence {
    /// Evidence for a launch that opens `launch`. A conversation known at spawn
    /// is the launch's first binding (revision 1), so a turn submitted before
    /// any title names it still has a known recipient. An unknown one begins
    /// at revision 0, which the first UUID title advances.
    pub(crate) fn for_launch(launch: &LaunchBinding) -> Self {
        Self {
            binding_rev: match launch {
                LaunchBinding::Unknown => 0,
                LaunchBinding::Known(_) => 1,
            },
            launch: launch.clone(),
            ..Self::default()
        }
    }

    /// Whether the launch's first UUID title, `thread`, begins a new binding.
    /// It does when the conversation was unknown at spawn, or after a switch
    /// reported out of band. Otherwise the launch is still in the conversation
    /// it opened, which the title names unless muxel pinned a different id.
    pub(crate) fn first_thread_switches(&self, thread: &str) -> bool {
        match &self.launch {
            LaunchBinding::Known(pinned) if self.binding_rev == 1 => pinned
                .as_deref()
                .is_some_and(|id| !thread_names(id, thread)),
            _ => true,
        }
    }

    /// The parser is about to apply output the reader received at `at`.
    pub(crate) fn begin_read(&mut self, at: SystemTime) {
        self.reading_at = Some(at);
        self.fresh_output |= self.output_is_fresh(at);
    }

    pub(crate) fn end_read(&mut self) {
        self.reading_at = None;
    }

    /// When the output raising an event was read: the chunk being parsed, or
    /// now for an event raised outside one.
    pub(crate) fn output_time(&self) -> SystemTime {
        self.reading_at.unwrap_or_else(SystemTime::now)
    }

    fn output_is_fresh(&self, at: SystemTime) -> bool {
        self.window.is_none_or(|window| at > window.output_after)
    }

    fn mark(&mut self, at: SystemTime) -> EvidenceMark {
        self.seq += 1;
        EvidenceMark { seq: self.seq, at }
    }

    /// Record an OSC title (or its reset); returns its sequence number.
    pub(crate) fn record_title(&mut self, title: Option<String>, at: SystemTime) -> u64 {
        let mark = self.mark(at);
        self.titles.push(mark, title);
        self.title = Some(mark);
        mark.seq
    }

    /// Record a bell; returns whether it rings for the current binding. One
    /// read inside a switch window may be the old conversation's last turn.
    pub(crate) fn record_bell(&mut self, at: SystemTime) -> bool {
        self.bell = Some(self.mark(at));
        self.output_is_fresh(at)
    }

    pub(crate) fn record_submission(&mut self, at: SystemTime) {
        self.first_submission_at.get_or_insert(at);
        let mark = self.mark(at);
        self.submitted = Some(mark);
        self.first_submission.get_or_insert(self.binding_rev);
    }

    pub(crate) fn first_submission_binding(&self) -> Option<u64> {
        self.first_submission.filter(|rev| *rev != 0)
    }

    pub(crate) fn first_submission_at(&self) -> Option<SystemTime> {
        self.first_submission_at
    }

    pub(crate) fn binding_rev(&self) -> u64 {
        self.binding_rev
    }

    /// Begin a new binding at sequence `since`, a provider title in the output
    /// itself. The order is exact: evidence before it belongs to the retired
    /// conversation, evidence from `since` on is the new one's, and no window
    /// is needed.
    pub(crate) fn begin_binding(&mut self, since: u64) -> BindingSwitch {
        self.window = None;
        self.advance_binding(since)
    }

    fn advance_binding(&mut self, since: u64) -> BindingSwitch {
        let since = since.max(self.binding_since);
        self.binding_rev += 1;
        self.binding_since = since;
        let retired = |mark: Option<EvidenceMark>| mark.is_some_and(|mark| mark.seq < since);
        let switch = BindingSwitch {
            title: retired(self.title),
            bell: retired(self.bell),
        };
        if switch.bell {
            self.bell = None;
        }
        switch
    }

    /// Begin a new binding for a switch reported out of band: the provider
    /// switched at some instant in `[from, to]`, and muxel learned of it at
    /// `learned_at`. The current title is retired if it was read before `from`
    /// less the slack, and any bell is. Everything else recorded so far, and
    /// output read until the window closes, is ambiguous.
    pub(crate) fn switch_binding(
        &mut self,
        from: SystemTime,
        to: SystemTime,
        learned_at: SystemTime,
    ) -> BindingSwitch {
        let window = SwitchWindow {
            input_after: to,
            output_after: learned_at.max(to.checked_add(SWITCH_SLACK).unwrap_or(to)),
        };
        // A later report never shortens an earlier window.
        self.window = Some(match self.window {
            Some(open) => SwitchWindow {
                input_after: open.input_after.max(window.input_after),
                output_after: open.output_after.max(window.output_after),
            },
            None => window,
        });
        self.fresh_output = false;
        let old_before = from
            .checked_sub(SWITCH_SLACK)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let old_title = self.title.is_some_and(|mark| mark.at < old_before);
        let mut switch = self.advance_binding(self.seq + 1);
        switch.title = old_title;
        if old_title {
            self.title = None;
        }
        switch
    }

    #[cfg(test)]
    pub(crate) fn read(&self, cursor: Option<u64>) -> EvidenceRead {
        self.read_at(cursor, SystemTime::now())
    }

    /// Read for the reducer at wall-clock time `now`. `cursor: None` reads the
    /// current binding's title edges from its start; a cursor never reaches
    /// back into a retired binding.
    pub(crate) fn read_at(&self, cursor: Option<u64>, now: SystemTime) -> EvidenceRead {
        let start = self.binding_since.saturating_sub(1);
        let cursor = cursor.map_or(start, |cursor| cursor.max(start));
        let window = self.window;
        let (titles, cursor, overflowed) = self
            .titles
            .after(cursor, window.map(|window| window.output_after));
        EvidenceRead {
            titles,
            cursor,
            overflowed,
            submitted: self.submitted.is_some_and(|mark| match window {
                // Input written after the switch went to the new conversation,
                // even before muxel learned of the switch.
                Some(window) => mark.at > window.input_after,
                None => mark.seq >= self.binding_since,
            }),
            settling: self.settling(now),
            settles_at: window
                .filter(|_| self.settling(now))
                .map(|window| window.settles_at()),
            title_fresh: window
                .is_none_or(|window| self.title.is_some_and(|mark| mark.at > window.output_after)),
            windowed: window.is_some(),
        }
    }

    /// After a switch reported out of band, until fresh evidence sets the
    /// level state: the current title was read inside the window, or there is
    /// none and no output read after the window has been parsed. Without
    /// fresh evidence, it ends [`SWITCH_SETTLE`] after the window closes.
    fn settling(&self, now: SystemTime) -> bool {
        let Some(window) = self.window else {
            return false;
        };
        if now >= window.settles_at() {
            return false;
        }
        match self.title {
            Some(mark) => mark.at <= window.output_after,
            None => !self.fresh_output,
        }
    }
}

/// Whether a title's thread field names conversation `id`: its full UUID, or
/// Codex's abbreviation of it (the first 29 characters and `...`).
fn thread_names(id: &str, thread: &str) -> bool {
    match thread.strip_suffix("...") {
        Some(prefix) => id
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix)),
        None => id.eq_ignore_ascii_case(thread),
    }
}

// Forwards terminal events to shared state. Lives inside the `Term`, so its
/// `send_event` runs wherever the VTE processor is advanced (the GPUI thread) —
/// and while the `Term` mutex is held, so it must never call back into the
/// session; it only touches its own shared state and the PTY writer.
pub(crate) struct MuxelListener {
    pub replies: Arc<Replies>,
    pub title: Arc<Mutex<Option<String>>>,
    /// Title edges, bells and submissions in one order, and the provider
    /// binding they belong to. The binding advances only when a provider
    /// publishes a different UUID-shaped thread (or the app reports a switch);
    /// ordinary spinner/title frames for the same conversation do not churn it.
    pub evidence: Arc<Mutex<LifecycleEvidence>>,
    /// Monotonic sequence for OSC title changes, including ResetTitle. Consumers
    /// use it to distinguish a repeated title event from an unchanged snapshot.
    pub title_generation: Arc<AtomicU64>,
    pub title_changed_at: Arc<Mutex<Option<Instant>>>,
    /// Latest OSC title whose thread field is a full UUID or Codex's validated
    /// 32-character `thread-id` abbreviation. The app resolves and validates the
    /// provider's full title contract before adopting it.
    pub session_id_hint: Arc<Mutex<Option<String>>>,
    pub bell: Arc<AtomicBool>,
    /// OSC-52 copies from the child, drained by the view onto the system
    /// clipboard (a clipboard write needs a gpui context this thread lacks).
    pub clipboard_store: Arc<Mutex<Vec<(ClipboardType, String)>>>,
}

fn session_thread_field(title: &str) -> Option<&str> {
    let title = title.trim();
    let title = title
        .strip_prefix("[ ! ] Action Required | ")
        .or_else(|| title.strip_prefix("[ . ] Action Required | "))
        .unwrap_or(title);
    let thread = title
        .split_once(" | ")
        .map_or(title, |(thread, _)| thread.trim());
    if uuid::Uuid::parse_str(thread).is_ok() {
        return Some(thread);
    }
    let prefix = thread.strip_suffix("...")?;
    (prefix.len() == 29 && uuid::Uuid::parse_str(&format!("{prefix}0000000")).is_ok())
        .then_some(thread)
}

impl EventListener for MuxelListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                let mut hint = self.session_id_hint.lock();
                let mut evidence = self.evidence.lock();
                let at = evidence.output_time();
                let seq = evidence.record_title(Some(title.clone()), at);
                if evidence.output_is_fresh(at)
                    && let Some(next) = session_thread_field(&title)
                {
                    let changed = match hint.as_deref().and_then(session_thread_field) {
                        Some(previous) => previous != next,
                        None => evidence.first_thread_switches(next),
                    };
                    *hint = Some(title.clone());
                    if changed {
                        // This title opens the new provider binding. Frames
                        // before it belong to the retired conversation and
                        // cannot seed the new reducer. So does a bell that
                        // rang before it; one after it (same batch included)
                        // belongs to the new binding and survives.
                        if evidence.begin_binding(seq).bell {
                            self.bell.store(false, Ordering::Relaxed);
                        }
                    }
                }
                *self.title.lock() = Some(title);
                *self.title_changed_at.lock() = Some(Instant::now());
                self.title_generation.fetch_add(1, Ordering::Relaxed);
            }
            Event::ResetTitle => {
                {
                    let mut evidence = self.evidence.lock();
                    let at = evidence.output_time();
                    evidence.record_title(None, at);
                }
                *self.title.lock() = None;
                *self.title_changed_at.lock() = Some(Instant::now());
                self.title_generation.fetch_add(1, Ordering::Relaxed);
            }
            Event::Bell => {
                let mut evidence = self.evidence.lock();
                let at = evidence.output_time();
                if evidence.record_bell(at) {
                    self.bell.store(true, Ordering::Relaxed);
                }
            }
            // Apps query the terminal (cursor position, device attributes, …);
            // the reply must go back to the PTY's stdin. Those the reader thread
            // already answered are dropped here, the rest kept in query order.
            Event::PtyWrite(text) => self.replies.terminal_reply(&text),
            // OSC-52 copy: alacritty hands over the already-base64-decoded text;
            // queue it for the view to land on the system clipboard.
            Event::ClipboardStore(ty, text) => self.clipboard_store.lock().push((ty, text)),
            // OSC-52 reads and color queries are answered synchronously on the
            // PTY reader thread. Waiting for this listener means waiting for the
            // GPUI drain task, which is late enough for a TUI to treat the reply
            // as typed text.
            Event::ClipboardLoad(_, _) | Event::ColorRequest(_, _) => {}
            // `Wakeup` and `ChildExit` are emitted only by alacritty's own
            // EventLoop, which muxel doesn't run: repaints are driven by the
            // view's drain task, and exit (with its code) comes from the PTY
            // reader's EOF. The remaining events (cursor blink, pointer shape,
            // text-area pixel size for CSI 14t) are intentionally unhandled.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_codex_title_retains_the_immutable_resume_hint() {
        let hint = Arc::new(Mutex::new(None));
        let listener = MuxelListener {
            replies: Arc::new(Replies::new(Arc::new(Mutex::new(Box::new(
                Vec::<u8>::new(),
            ))))),
            title: Arc::new(Mutex::new(None)),
            evidence: Arc::new(Mutex::new(LifecycleEvidence::default())),
            title_generation: Arc::new(AtomicU64::new(0)),
            title_changed_at: Arc::new(Mutex::new(None)),
            session_id_hint: hint.clone(),
            bell: Arc::new(AtomicBool::new(false)),
            clipboard_store: Arc::new(Mutex::new(Vec::new())),
        };
        let first = "019f95d7-db31-7db0-904d-9e08330e0000";
        let resumed = "019f95d7-db31-7db0-904d-9e08330e0001";

        let first_frame = format!("{}... | Starting ⠹", &first[..29]);
        listener.send_event(Event::Title(first_frame.clone()));
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 1);
        assert_eq!(listener.evidence.lock().binding_rev(), 1);
        assert_eq!(hint.lock().as_deref(), Some(first_frame.as_str()));
        listener.send_event(Event::Title("Renamed conversation | Ready".to_string()));
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 2);
        assert_eq!(listener.evidence.lock().binding_rev(), 1);
        assert_eq!(hint.lock().as_deref(), Some(first_frame.as_str()));

        listener.send_event(Event::Title(format!("Review {resumed} | Ready ·")));
        assert_eq!(hint.lock().as_deref(), Some(first_frame.as_str()));
        assert_eq!(listener.evidence.lock().binding_rev(), 1);

        // The retired conversation's bell dies with its binding; a bell after
        // the new title, even in the same drained batch, is the new one's.
        listener.send_event(Event::Bell);
        listener.send_event(Event::Title(resumed.to_string()));
        assert!(!listener.bell.load(Ordering::Relaxed));
        listener.send_event(Event::Bell);
        assert!(listener.bell.load(Ordering::Relaxed));
        assert_eq!(hint.lock().as_deref(), Some(resumed));
        assert_eq!(listener.evidence.lock().binding_rev(), 2);
        // A reader that never saw the switch reads only the new binding's edges.
        let read = listener.evidence.lock().read(Some(0));
        assert!(!read.overflowed);
        assert_eq!(read.titles, vec![Some(resumed.to_string())]);
        listener.send_event(Event::ResetTitle);
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 5);
        assert_eq!(*listener.title.lock(), None);
    }

    fn listener_for(launch: LaunchBinding) -> MuxelListener {
        MuxelListener {
            replies: Arc::new(Replies::new(Arc::new(Mutex::new(Box::new(
                Vec::<u8>::new(),
            ))))),
            title: Arc::new(Mutex::new(None)),
            evidence: Arc::new(Mutex::new(LifecycleEvidence::for_launch(&launch))),
            title_generation: Arc::new(AtomicU64::new(0)),
            title_changed_at: Arc::new(Mutex::new(None)),
            session_id_hint: Arc::new(Mutex::new(None)),
            bell: Arc::new(AtomicBool::new(false)),
            clipboard_store: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn a_launch_into_a_known_conversation_owns_a_turn_before_any_title() {
        // A fresh Claude pane: muxel minted its `--session-id`. Claude puts no
        // UUID in its title, and its hook doesn't run for a fresh start.
        let at = SystemTime::now();
        let pinned = uuid::Uuid::new_v4().to_string();
        let mut evidence = LifecycleEvidence::for_launch(&LaunchBinding::Known(Some(pinned)));
        assert_eq!(evidence.binding_rev(), 1);
        evidence.record_submission(at);
        assert_eq!(evidence.first_submission_binding(), Some(1));
        assert!(evidence.read(None).submitted);
        // `/clear` reported by the hook begins another conversation; the run
        // still owns only the first.
        evidence.switch_binding(at, at, at);
        assert_eq!(evidence.binding_rev(), 2);
        assert_eq!(evidence.first_submission_binding(), Some(1));
        // An unknown launch's turn before the first identity stays unowned.
        let mut evidence = LifecycleEvidence::for_launch(&LaunchBinding::Unknown);
        assert_eq!(evidence.binding_rev(), 0);
        evidence.record_submission(at);
        assert_eq!(evidence.first_submission_binding(), None);
    }

    #[test]
    fn the_first_title_naming_a_known_conversation_is_not_a_switch() {
        let pinned = "019f95d7-db31-7db0-904d-9e08330e0000";
        let other = "8c6f2a10-5b3e-4d7a-9f21-0a1b2c3d4e5f";
        // `codex resume <pinned>` publishes its abbreviation of the id.
        let listener = listener_for(LaunchBinding::Known(Some(pinned.into())));
        listener.send_event(Event::Title(format!("{}... | Working", &pinned[..29])));
        assert_eq!(listener.evidence.lock().binding_rev(), 1);
        listener.send_event(Event::Title(format!("{}... | Ready", &pinned[..29])));
        assert_eq!(listener.evidence.lock().binding_rev(), 1);
        // A different thread after it is a switch, as for any launch.
        listener.send_event(Event::Title(format!("{other} | Ready")));
        assert_eq!(listener.evidence.lock().binding_rev(), 2);

        // A first title naming another thread than the pinned one is a switch.
        let listener = listener_for(LaunchBinding::Known(Some(pinned.into())));
        listener.send_event(Event::Title(format!("{other} | Ready")));
        assert_eq!(listener.evidence.lock().binding_rev(), 2);

        // Without a pinned id, the first title names the launch's conversation.
        let listener = listener_for(LaunchBinding::Known(None));
        listener.send_event(Event::Title(format!("{other} | Ready")));
        assert_eq!(listener.evidence.lock().binding_rev(), 1);

        // After a switch reported out of band, a title in its ambiguity
        // window stays visible but cannot begin another binding.
        let listener = listener_for(LaunchBinding::Known(Some(pinned.into())));
        let now = SystemTime::now();
        listener.evidence.lock().switch_binding(now, now, now);
        let pinned_title = format!("{pinned} | Ready");
        listener.send_event(Event::Title(pinned_title.clone()));
        assert_eq!(listener.evidence.lock().binding_rev(), 2);
        assert_eq!(
            listener.title.lock().as_deref(),
            Some(pinned_title.as_str())
        );
        assert!(listener.session_id_hint.lock().is_none());
        // Its first fresh UUID title can then establish the replacement.
        listener
            .evidence
            .lock()
            .begin_read(now + SWITCH_SLACK + Duration::from_millis(1));
        listener.send_event(Event::Title(pinned_title));
        listener.evidence.lock().end_read();
        assert_eq!(listener.evidence.lock().binding_rev(), 3);

        // An unknown launch's first title is its first identity.
        let listener = listener_for(LaunchBinding::Unknown);
        listener.send_event(Event::Title(format!("{pinned} | Ready")));
        assert_eq!(listener.evidence.lock().binding_rev(), 1);
    }

    #[test]
    fn title_edges_are_bounded_and_report_overflow() {
        let at = SystemTime::now();
        let mut evidence = LifecycleEvidence::default();
        for index in 0..=MAX_TITLE_EVENTS {
            evidence.record_title(Some(format!("title-{index}")), at);
        }
        let read = evidence.read(None);
        assert_eq!(read.titles.len(), MAX_TITLE_EVENTS);
        assert!(read.overflowed);
        assert_eq!(
            read.titles.last().and_then(Option::as_deref),
            Some("title-64")
        );
        // Reading doesn't consume; the cursor moves past what was read.
        let read = evidence.read(Some(read.cursor));
        assert!(read.titles.is_empty());
        assert!(!read.overflowed);

        let cursor = read.cursor;
        evidence.record_title(Some("x".repeat(MAX_TITLE_EVENT_BYTES + 1)), at);
        let read = evidence.read(Some(cursor));
        assert!(read.titles.is_empty());
        assert!(read.overflowed);
    }

    #[test]
    fn first_submission_ownership_survives_later_submissions_and_switches() {
        let at = SystemTime::now();
        let mut evidence = LifecycleEvidence::default();
        assert_eq!(evidence.first_submission_binding(), None);
        // Initial provider identity can arrive before startup types the task.
        let seq = evidence.record_title(Some("initial binding".into()), at);
        evidence.begin_binding(seq);
        evidence.record_submission(at + Duration::from_secs(1));
        assert_eq!(evidence.first_submission_binding(), Some(1));
        let seq = evidence.record_title(Some("replacement".into()), at + Duration::from_secs(2));
        evidence.begin_binding(seq);
        evidence.record_submission(at + Duration::from_secs(3));
        assert_eq!(evidence.binding_rev(), 2);
        assert_eq!(evidence.first_submission_binding(), Some(1));
    }

    #[test]
    fn delayed_initial_binding_cannot_establish_submission_ownership() {
        let at = SystemTime::now();
        let mut evidence = LifecycleEvidence::default();
        evidence.record_submission(at + Duration::from_secs(2));
        evidence.switch_binding(at, at, at + Duration::from_secs(3));
        assert_eq!(evidence.first_submission_binding(), None);
        // Later switches and submissions cannot establish missing ownership.
        evidence.switch_binding(
            at + Duration::from_secs(4),
            at + Duration::from_secs(4),
            at + Duration::from_secs(5),
        );
        evidence.record_submission(at + Duration::from_secs(6));
        assert_eq!(evidence.binding_rev(), 2);
        assert_eq!(evidence.first_submission_binding(), None);
        // An out-of-order later report must not establish unknown ownership,
        // even if its timestamp predates the original task submission.
        evidence.switch_binding(at, at, at + Duration::from_secs(7));
        assert_eq!(evidence.binding_rev(), 3);
        assert_eq!(evidence.first_submission_binding(), None);
    }

    #[test]
    fn unknown_submission_ownership_never_transfers_to_a_discovered_identity() {
        let at = SystemTime::now();
        let mut evidence = LifecycleEvidence::default();
        evidence.record_submission(at);
        assert_eq!(evidence.first_submission_binding(), None);
        let seq = evidence.record_title(Some("initial identity".into()), at);
        evidence.begin_binding(seq);
        assert_eq!(evidence.first_submission_binding(), None);
        assert!(!evidence.read(None).submitted);
        let seq = evidence.record_title(Some("replacement".into()), at);
        evidence.begin_binding(seq);
        assert!(!evidence.read(None).submitted);
        evidence.record_submission(at);
        assert_eq!(evidence.binding_rev(), 2);
        assert_eq!(evidence.first_submission_binding(), None);
    }

    #[test]
    fn a_switch_window_retires_old_evidence_and_holds_ambiguous_evidence() {
        let t0 = SystemTime::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let slack = SWITCH_SLACK.as_millis() as u64;
        let mut evidence = LifecycleEvidence::default();
        // Long before the hook ran at 10 s: the old conversation's.
        evidence.record_submission(at(1_000));
        evidence.record_title(Some("old idle".into()), at(1_000));
        assert!(evidence.record_bell(at(1_000)));
        // Inside the window: Claude switches before its hook runs.
        evidence.record_title(Some("working".into()), at(9_500));
        evidence.record_title(Some("idle".into()), at(9_900));
        // Input after the hook ran went to the new conversation, even before
        // muxel's tick read the record.
        evidence.record_submission(at(10_100));

        let switch = evidence.switch_binding(at(10_000), at(10_000), at(10_400));
        assert!(
            !switch.title,
            "the current title was read inside the window"
        );
        assert!(switch.bell);
        assert_eq!(evidence.binding_rev(), 1);
        let read = evidence.read(None);
        assert!(
            read.titles.is_empty(),
            "nothing recorded so far forms an edge"
        );
        assert!(read.submitted);
        assert!(read.settling);

        // Output read until the window closes is still ambiguous.
        evidence.record_title(Some("late working".into()), at(10_000 + slack));
        assert!(
            !evidence.record_bell(at(10_000 + slack)),
            "a bell in the window rings for no binding"
        );
        let read = evidence.read(None);
        assert!(read.titles.is_empty());
        assert!(read.settling);

        // Output read after it is the new conversation's.
        evidence.record_title(Some("fresh".into()), at(10_001 + slack));
        assert!(evidence.record_bell(at(10_001 + slack)));
        let read = evidence.read(None);
        assert_eq!(read.titles, vec![Some("fresh".into())]);
        assert!(!read.settling);
    }

    #[test]
    fn an_uncertain_level_state_settles_after_the_window_without_fresh_evidence() {
        let t0 = SystemTime::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let slack = SWITCH_SLACK.as_millis() as u64;
        let mut evidence = LifecycleEvidence::default();
        // `/clear` while idle: Claude redraws around its hook, then prints
        // nothing more.
        evidence.record_submission(at(9_000));
        evidence.switch_binding(at(10_000), at(10_000), at(10_400));
        evidence.record_title(Some("idle".into()), at(10_100));
        let closes = at(10_000 + slack);
        let read = evidence.read_at(None, closes);
        assert!(read.settling);
        assert_eq!(read.settles_at, Some(closes + SWITCH_SETTLE));
        let almost = closes + SWITCH_SETTLE - Duration::from_millis(1);
        assert!(evidence.read_at(None, almost).settling);
        let settled = evidence.read_at(None, closes + SWITCH_SETTLE);
        assert!(!settled.settling);
        assert_eq!(settled.settles_at, None);
        assert!(
            settled.titles.is_empty(),
            "a title read in the window still forms no edge"
        );
        assert!(!settled.submitted, "nor does the Enter before the hook arm");

        // Fresh evidence settles it sooner, with no deadline left.
        evidence.record_title(Some("fresh".into()), at(10_001 + slack));
        let read = evidence.read_at(None, closes);
        assert!(!read.settling);
        assert_eq!(read.settles_at, None);
    }

    #[test]
    fn a_switch_retires_a_title_read_before_it_could_have_happened() {
        let t0 = SystemTime::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let slack = SWITCH_SLACK.as_millis() as u64;
        let mut evidence = LifecycleEvidence::default();
        evidence.record_submission(at(9_000));
        evidence.record_title(Some("old idle".into()), at(9_999 - slack));
        let switch = evidence.switch_binding(at(10_000), at(10_000), at(10_400));
        assert!(switch.title);
        assert!(!evidence.read(None).submitted, "the Enter that picked it");
        // With no title, output read after the window ends the wait.
        assert!(evidence.read(None).settling);
        evidence.begin_read(at(10_000 + slack));
        evidence.end_read();
        assert!(evidence.read(None).settling);
        evidence.begin_read(at(10_001 + slack));
        evidence.end_read();
        assert!(!evidence.read(None).settling);
    }

    #[test]
    fn a_window_lasts_until_muxel_learns_of_the_switch() {
        let t0 = SystemTime::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut evidence = LifecycleEvidence::default();
        // A registry row with no time of its own: Grok switched somewhere
        // between two reads, the second at 1 s. Muxel applies it at 5 s.
        evidence.record_title(Some("new idle".into()), at(500));
        let switch = evidence.switch_binding(at(0), at(1_000), at(5_000));
        assert!(!switch.title, "it may be the new conversation's");
        evidence.record_title(Some("working".into()), at(4_000));
        assert!(evidence.read(None).titles.is_empty());
        evidence.record_title(Some("working".into()), at(5_001));
        assert_eq!(evidence.read(None).titles.len(), 1);
    }

    #[test]
    fn a_later_switch_never_shortens_an_earlier_window() {
        let t0 = SystemTime::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut evidence = LifecycleEvidence::default();
        evidence.switch_binding(at(10_000), at(10_000), at(20_000));
        // A second report whose times fall before the first was learned.
        evidence.switch_binding(at(1_000), at(1_000), at(1_500));
        assert_eq!(evidence.binding_rev(), 2);
        evidence.record_title(Some("queued".into()), at(15_000));
        evidence.record_submission(at(5_000));
        let read = evidence.read(None);
        assert!(read.titles.is_empty());
        assert!(!read.submitted);
    }

    #[test]
    fn a_switch_inside_lost_title_edges_reports_the_loss() {
        let t0 = SystemTime::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let slack = SWITCH_SLACK.as_millis() as u64;
        let mut evidence = LifecycleEvidence::default();
        evidence.record_title(Some("old".into()), at(1));
        evidence.switch_binding(at(5), at(5), at(10));
        // More of the new conversation's frames than the buffer holds.
        for index in 0..MAX_TITLE_EVENTS + 4 {
            evidence.record_title(Some(format!("new-{index}")), at(10 + slack + index as u64));
        }
        let read = evidence.read(None);
        assert!(read.overflowed, "edges of the new binding were evicted");
    }

    #[test]
    fn a_submission_counts_only_in_the_binding_it_was_made_in() {
        let at = SystemTime::now();
        let mut evidence = LifecycleEvidence::default();
        evidence.record_submission(at);
        assert!(evidence.read(None).submitted);
        // A provider title opens a new binding after the Enter that chose it.
        let seq = evidence.record_title(Some("resumed".into()), at);
        evidence.begin_binding(seq);
        assert!(!evidence.read(None).submitted);
        evidence.record_submission(at);
        assert!(evidence.read(None).submitted);
    }
}
