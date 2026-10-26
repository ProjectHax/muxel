//! Program Status Protocol (OSC 7501), revision 0.4.
//!
//! The reader frames and validates reports before the UI sees the PTY bytes.
//! Only the root record drives a pane badge; child ids are valid protocol input
//! but describe work inside the pane, not the pane's own lifecycle.

use alacritty_terminal::vte::{Parser, Perform};
use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use std::sync::Arc;

pub(crate) const CAPABILITY_REPLY: &str =
    "\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";
const MAX_SEQUENCE: usize = 4096;
const TEXT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramStatusKind {
    Permission,
    Question,
    Auth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub state: ProgramState,
    pub id: Option<String>,
    pub kind: Option<ProgramStatusKind>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Query,
    Report(Report),
    Reset,
    ShellPrompt,
    Title(String),
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte))
}

fn valid_id(id: &str) -> bool {
    let mut count = 0;
    for segment in id.split('/') {
        count += 1;
        if count > 8 || segment.len() > 32 || !valid_token(segment) {
            return false;
        }
    }
    count > 0
}

fn decode_text(encoded: &str, decoded_limit: usize) -> Option<String> {
    let bytes = TEXT_BASE64.decode(encoded).ok()?;
    if bytes.len() > decoded_limit {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text
        .chars()
        .any(|ch| matches!(ch as u32, 0..=31 | 127..=159))
    {
        return None;
    }
    Some(text)
}

fn parse_report(body: &[u8]) -> Option<Report> {
    let mut state = None;
    let mut id = None;
    let mut kind = None;
    let mut progress = None;
    let mut app = None;
    let mut title = None;
    let mut msg = None;
    for pair in body.split(|byte| *byte == b':') {
        let Some(equal) = pair.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let key = pair[..equal].trim_ascii();
        let value = pair[equal + 1..].trim_ascii();
        if key.len() > 16 {
            return None;
        }
        if key.is_empty() || !key.iter().all(u8::is_ascii_lowercase) {
            continue;
        }
        let limit = match key {
            b"msg" => 2732,
            b"title" => 256,
            b"app" => 32,
            b"id" => 128,
            _ => MAX_SEQUENCE,
        };
        if value.len() > limit {
            return None;
        }
        // A present but malformed address must never become a root report.
        if key == b"id" {
            let value = std::str::from_utf8(value).ok()?;
            if !valid_id(value) {
                return None;
            }
            id = Some(value.to_owned());
            continue;
        }
        if !value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.,+/=-".contains(byte))
        {
            continue;
        }
        let value = std::str::from_utf8(value).ok()?;
        match key {
            b"state" => {
                state = match value {
                    "idle" => Some(ProgramState::Idle),
                    "working" => Some(ProgramState::Working),
                    "done" => Some(ProgramState::Done),
                    "blocked" => Some(ProgramState::Blocked),
                    "error" => Some(ProgramState::Error),
                    "clear" => Some(ProgramState::Clear),
                    _ => None,
                }
            }
            b"kind" => {
                kind = match value {
                    "permission" => Some(ProgramStatusKind::Permission),
                    "question" => Some(ProgramStatusKind::Question),
                    "auth" => Some(ProgramStatusKind::Auth),
                    _ => None,
                }
            }
            b"progress" => progress = value.parse::<u8>().ok().filter(|n| *n <= 100),
            b"app" => app = valid_token(value).then(|| value.to_owned()),
            b"title" => title = Some(decode_text(value, 192)?),
            b"msg" => msg = Some(decode_text(value, 2048)?),
            _ => {}
        }
    }
    let state = state?;
    if state != ProgramState::Blocked {
        kind = None;
    }
    if !matches!(state, ProgramState::Working | ProgramState::Blocked) {
        progress = None;
    }
    Some(Report {
        state,
        id,
        kind,
        progress,
        app,
        title,
        msg,
    })
}

#[derive(Default)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
    OtherString,
    OtherEscape,
}

/// Reader-thread framing for immediate capability replies. PSP state and
/// binding attribution use `Observer` on the UI drain. DCS and other strings
/// stay opaque here, so a passthrough wrapper cannot forge a local query.
#[derive(Default)]
pub(crate) struct Scanner {
    state: ScanState,
    raw: Vec<u8>,
    bytes: usize,
    oversized: bool,
}

impl Scanner {
    fn start(&mut self) {
        self.state = ScanState::Osc;
        self.raw.clear();
        self.bytes = 2;
        self.oversized = false;
    }

    fn count_byte(&mut self) {
        self.bytes += 1;
        if self.bytes > MAX_SEQUENCE {
            self.raw.clear();
            self.oversized = true;
        }
    }

    fn finish(&mut self) -> Option<Event> {
        self.state = ScanState::Ground;
        if self.oversized {
            self.raw.clear();
            return None;
        }
        let event = if self.raw == b"7501;?" {
            Some(Event::Query)
        } else if self.raw == b"133;A" || self.raw.starts_with(b"133;A;") {
            Some(Event::ShellPrompt)
        } else if self.raw.starts_with(b"0;") || self.raw.starts_with(b"2;") {
            // Match VTE's OSC 0/2 title decoding (16 params including the
            // selector). Binding titles must be observed in wire order even
            // when VTE buffers synchronized UI.
            let title = self.raw[2..]
                .split(|byte| *byte == b';')
                .take(15)
                .filter_map(|part| std::str::from_utf8(part).ok())
                .collect::<Vec<_>>()
                .join(";")
                .trim()
                .to_owned();
            Some(Event::Title(title))
        } else {
            self.raw
                .strip_prefix(b"7501;")
                .and_then(parse_report)
                .map(Event::Report)
        };
        self.raw.clear();
        event
    }

    pub(crate) fn advance(&mut self, byte: u8) -> Option<Event> {
        match self.state {
            ScanState::Ground => {
                if byte == 0x1b {
                    self.state = ScanState::Escape;
                }
            }
            ScanState::Escape => match byte {
                b']' => self.start(),
                b'P' | b'X' | b'^' | b'_' => self.state = ScanState::OtherString,
                b'c' => {
                    self.state = ScanState::Ground;
                    return Some(Event::Reset);
                }
                0x1b => {}
                _ => self.state = ScanState::Ground,
            },
            ScanState::OtherString => match byte {
                0x1b => self.state = ScanState::OtherEscape,
                0x18 | 0x1a => self.state = ScanState::Ground,
                _ => {}
            },
            ScanState::OtherEscape => match byte {
                b'\\' | 0x18 | 0x1a => self.state = ScanState::Ground,
                b'c' => {
                    self.state = ScanState::Ground;
                    return Some(Event::Reset);
                }
                0x1b => {}
                _ => self.state = ScanState::OtherString,
            },
            ScanState::Osc => {
                self.count_byte();
                match byte {
                    0x07 => return self.finish(),
                    0x1b => self.state = ScanState::OscEscape,
                    0x18 | 0x1a => {
                        self.state = ScanState::Ground;
                        self.raw.clear();
                    }
                    _ if !self.oversized => self.raw.push(byte),
                    _ => {}
                }
            }
            ScanState::OscEscape => {
                self.count_byte();
                if byte == b'\\' {
                    return self.finish();
                }
                self.state = ScanState::Escape;
                self.raw.clear();
                if byte == b']' {
                    self.start();
                } else if byte == b'c' {
                    self.state = ScanState::Ground;
                    return Some(Event::Reset);
                } else if matches!(byte, b'P' | b'X' | b'^' | b'_') {
                    self.state = ScanState::OtherString;
                } else if byte != 0x1b {
                    self.state = ScanState::Ground;
                }
            }
        }
        None
    }
}

/// Passive VTE observer for the PTY output stream. Alacritty keeps its own
/// processor for the grid and UI title; this parser observes protocol facts
/// before synchronized updates can defer the grid's callbacks.
#[derive(Default)]
pub(crate) struct Observer {
    parser: Parser,
    escape: bool,
    escape_fresh: bool,
    escape_generation: u64,
    osc_len: usize,
    osc_fresh: bool,
    osc_generation: u64,
    osc_invalid_control: bool,
    discard_osc: bool,
    pending_st: Option<PendingSt>,
}

struct PendingSt {
    event: Event,
    generation: u64,
}

#[derive(Default)]
struct ObservedEvent {
    event: Option<Event>,
    cancelled: bool,
}

impl Perform for ObservedEvent {
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if self.cancelled || params.is_empty() {
            return;
        }
        self.event = match params[0] {
            b"0" | b"2" if params.len() >= 2 => {
                let title = params[1..]
                    .iter()
                    .filter_map(|part| std::str::from_utf8(part).ok())
                    .collect::<Vec<_>>()
                    .join(";")
                    .trim()
                    .to_owned();
                Some(Event::Title(title))
            }
            b"7501" if params.len() == 2 && params[1] == b"?" => Some(Event::Query),
            b"7501" if params.len() == 2 => parse_report(params[1]).map(Event::Report),
            b"133" if params.get(1) == Some(&b"A".as_slice()) => Some(Event::ShellPrompt),
            _ => None,
        };
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.event = Some(Event::Reset);
        }
    }
}

impl Observer {
    #[cfg(test)]
    fn advance(&mut self, byte: u8) -> Option<Event> {
        self.advance_at(byte, true, 0)
    }

    pub(crate) fn advance_at(&mut self, byte: u8, fresh: bool, generation: u64) -> Option<Event> {
        let confirmed_st = self.pending_st.take().and_then(|pending| {
            (byte == b'\\' && fresh && pending.generation == generation).then_some(pending.event)
        });
        let was_osc = self.osc_len > 0;
        // vte's std build grows its OSC Vec without a limit. This watchdog
        // follows only OSC entry and termination; it does not decode a second
        // grammar. On overflow, reset the parser and discard until termination.
        if self.discard_osc {
            if byte == 0x1b {
                self.discard_osc = false;
                self.escape = true;
                self.escape_fresh = fresh;
                self.escape_generation = generation;
                self.parser = Parser::new();
                self.parser.advance(&mut ObservedEvent::default(), &[byte]);
            } else if matches!(byte, 0x07 | 0x18 | 0x1a) {
                self.discard_osc = false;
                self.parser = Parser::new();
            }
            return confirmed_st;
        }
        if self.osc_len > 0 {
            self.osc_len += 1;
            self.osc_invalid_control |=
                matches!(byte, 0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f);
            if self.osc_len > MAX_SEQUENCE {
                self.osc_len = 0;
                self.parser = Parser::new();
                self.discard_osc = byte != 0x1b;
                self.escape = byte == 0x1b;
                self.escape_fresh = fresh;
                self.escape_generation = generation;
                if byte == 0x1b {
                    self.parser.advance(&mut ObservedEvent::default(), &[byte]);
                }
                return confirmed_st;
            }
            if matches!(byte, 0x1b | 0x07 | 0x18 | 0x1a) {
                self.osc_len = 0;
            }
        } else if self.escape && byte == b']' {
            self.osc_len = 2;
            self.osc_fresh = self.escape_fresh && fresh;
            self.osc_generation = self.escape_generation;
            self.osc_invalid_control = false;
        }
        let completing_osc = was_osc && matches!(byte, 0x1b | 0x07 | 0x18 | 0x1a);
        let osc_attributable = self.osc_fresh && fresh && self.osc_generation == generation;
        let escape_attributable =
            self.escape_fresh && fresh && self.escape_generation == generation;
        // VTE stays in Escape across C0 (apart from CAN/SUB), DEL, and
        // unsupported high bytes. Keep that entry provenance so an OSC after
        // ESC NUL ] still receives the same budget and binding boundary.
        let escape = byte == 0x1b
            || (self.escape && matches!(byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f..=0xff));
        self.escape = escape;
        if byte == 0x1b {
            self.escape_fresh = fresh;
            self.escape_generation = generation;
        }
        let mut sink = ObservedEvent {
            cancelled: matches!(byte, 0x18 | 0x1a),
            ..ObservedEvent::default()
        };
        self.parser.advance(&mut sink, &[byte]);
        let current = match sink.event {
            Some(Event::Title(title)) if completing_osc && osc_attributable => {
                Some(Event::Title(title))
            }
            Some(Event::Reset) if escape_attributable => Some(Event::Reset),
            Some(Event::Query | Event::Report(_)) if self.osc_invalid_control => None,
            Some(event @ (Event::Report(_) | Event::ShellPrompt))
                if completing_osc && osc_attributable && byte == 0x1b =>
            {
                self.pending_st = Some(PendingSt { event, generation });
                None
            }
            Some(event) if completing_osc && osc_attributable => Some(event),
            _ => None,
        };
        confirmed_st.or(current)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RootStatus {
    pub state: ProgramState,
    pub kind: Option<ProgramStatusKind>,
    pub title: Option<Arc<str>>,
}

/// The exact UUID-shaped title field accepted by #91. Codex also publishes
/// abbreviated fields ending in `...`; a full UUID parser would miss those
/// boundaries. Fixed storage bounds UUID comparison without hashing titles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ThreadKey {
    bytes: [u8; 36],
    len: u8,
}

impl ThreadKey {
    pub(crate) fn from_field(field: &str) -> Option<Self> {
        let bytes = field.as_bytes();
        if bytes.len() > 36 {
            return None;
        }
        let mut key = Self {
            bytes: [0; 36],
            len: bytes.len() as u8,
        };
        key.bytes[..bytes.len()].copy_from_slice(bytes);
        Some(key)
    }
}

/// One pane's root record and ordered transition facts. `revision` also lets a
/// lagging UI notice a clear that was followed by another report in one read.
/// A Working turn stays active through Blocked until it becomes Idle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProgramStatusSnapshot {
    pub root: Option<RootStatus>,
    pub binding_revision: u64,
    pub revision: u64,
    pub clear_revision: u64,
    pub working_to_idle_revision: u64,
    pub idle_completions: u64,
    pub explicit_completions: u64,
    working_active: bool,
    thread: Option<ThreadKey>,
}

impl ProgramStatusSnapshot {
    pub fn observe_title(
        &mut self,
        thread: ThreadKey,
        carry_initial_working: bool,
        confirms_launch: bool,
    ) {
        // The listener can defer title callbacks until a synchronized frame
        // ends. Consume this exception here, at the first title the observer
        // accepts, so another title in that frame opens its own binding.
        if self.thread.is_none() && confirms_launch {
            self.thread = Some(thread);
            return;
        }
        if self.thread != Some(thread) {
            let working =
                self.binding_revision == 0 && carry_initial_working && self.working_active;
            self.thread = Some(thread);
            self.binding_revision = self.binding_revision.wrapping_add(1);
            self.clear_binding();
            self.working_active = working;
        }
    }

    pub fn adopt_binding(&mut self, revision: u64) {
        self.binding_revision = revision;
        self.clear_binding();
    }

    pub fn recover_binding(&mut self, revision: u64, thread: Option<ThreadKey>) {
        self.thread = thread;
        self.adopt_binding(revision);
    }

    pub fn apply(&mut self, report: Report) {
        if report.id.is_some() {
            return;
        }
        if report.state == ProgramState::Clear {
            self.clear();
            return;
        }
        self.revision = self.revision.wrapping_add(1);
        if self.working_active && report.state == ProgramState::Idle {
            self.working_to_idle_revision = self.revision;
            self.idle_completions = self.idle_completions.wrapping_add(1);
        }
        if (self.working_active
            || self
                .root
                .as_ref()
                .is_some_and(|root| root.state == ProgramState::Blocked))
            && matches!(report.state, ProgramState::Done | ProgramState::Error)
        {
            self.explicit_completions = self.explicit_completions.wrapping_add(1);
        }
        match report.state {
            ProgramState::Working => self.working_active = true,
            ProgramState::Blocked => {}
            ProgramState::Idle | ProgramState::Done | ProgramState::Error | ProgramState::Clear => {
                self.working_active = false;
            }
        }
        self.root = Some(RootStatus {
            state: report.state,
            kind: report.kind,
            title: report.title.map(Arc::from),
        });
    }

    pub fn clear(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.clear_revision = self.revision;
        self.root = None;
        self.working_active = false;
    }

    /// A title or out-of-band switch starts a different provider conversation.
    /// Transition counts and the root belong only to their original binding.
    pub fn clear_binding(&mut self) {
        self.clear();
        self.working_to_idle_revision = 0;
        self.idle_completions = 0;
        self.explicit_completions = 0;
    }

    /// A new shell prompt retires an unfinished reporter. Completed reports
    /// remain visible; an unsampled Working-to-Idle edge cannot finish there.
    pub fn shell_prompt(&mut self) {
        if self
            .root
            .as_ref()
            .is_some_and(|root| matches!(root.state, ProgramState::Working | ProgramState::Blocked))
        {
            self.clear();
        } else if self
            .root
            .as_ref()
            .is_some_and(|root| root.state == ProgramState::Idle)
        {
            self.revision = self.revision.wrapping_add(1);
            self.clear_revision = self.revision;
        }
        self.working_active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, Observer, ProgramState, ProgramStatusKind, ProgramStatusSnapshot, Scanner};

    fn observed(bytes: &[u8]) -> Vec<Event> {
        let mut observer = Observer::default();
        bytes
            .iter()
            .filter_map(|byte| observer.advance(*byte))
            .collect()
    }

    #[test]
    fn observer_uses_vte_title_framing_and_normalization() {
        let wire = b"\x1b]0;Topic\x01; Ready\x1b\\\x1b]7501;state=working\x07";
        let expected = observed(wire);
        assert_eq!(expected.len(), 2);
        assert_eq!(expected[0], Event::Title("Topic; Ready".to_string()));
        assert!(
            matches!(&expected[1], Event::Report(report) if report.state == ProgramState::Working)
        );
        for split in 0..=wire.len() {
            let mut observer = Observer::default();
            let mut actual = Vec::new();
            for part in [&wire[..split], &wire[split..]] {
                actual.extend(part.iter().filter_map(|byte| observer.advance(*byte)));
            }
            assert_eq!(actual, expected, "split at {split}");
        }
        // VTE completes OSC at ESC, before the slash half of ST arrives.
        let mut observer = Observer::default();
        let first = b"\x1b]0;Topic\x1b";
        let events = first
            .iter()
            .filter_map(|byte| observer.advance(*byte))
            .collect::<Vec<_>>();
        assert_eq!(events, vec![Event::Title("Topic".to_string())]);
        assert_eq!(observer.advance(b'\\'), None);
    }

    #[test]
    fn observer_rejects_stale_and_cross_boundary_titles() {
        let title = b"\x1b]0;019f95d7-db31-7db0-904d-9e08330e0015 | Old\x07";
        let mut observer = Observer::default();
        assert!(
            title
                .iter()
                .all(|byte| observer.advance_at(*byte, false, 1).is_none())
        );

        let mut observer = Observer::default();
        let start = b"\x1b]0;019f95d7-db31-7db0-904d-9e08330e0015 | Old";
        assert!(
            start
                .iter()
                .all(|byte| observer.advance_at(*byte, true, 1).is_none())
        );
        assert_eq!(observer.advance_at(0x07, true, 2), None);
    }

    #[test]
    fn observer_bounds_osc_and_recovers_from_opaque_strings() {
        let mut wire = b"\x1b]7501;state=working:msg=".to_vec();
        wire.extend(vec![b'A'; 5000]);
        wire.extend_from_slice(b"\x07\x1b]7501;state=idle\x07");
        assert_eq!(observed(&wire).len(), 1);
        assert!(
            matches!(&observed(&wire)[0], Event::Report(report) if report.state == ProgramState::Idle)
        );
        assert_eq!(
            observed(b"\x1bPopaque\x1bc\x1b]133;A\x07"),
            vec![Event::Reset, Event::ShellPrompt]
        );
        // vte exposes at most 16 OSC parameters. Reject extra semicolons so
        // a malformed address hidden past that limit cannot become root.
        assert!(
            observed(b"\x1b]7501;state=working;x;x;x;x;x;x;x;x;x;x;x;x;x;x;id=worker!\x07")
                .is_empty()
        );
        assert!(observed(b"\x1b]7501;sta\x01te=working\x07").is_empty());
    }

    #[test]
    fn ignored_bytes_in_escape_cannot_bypass_the_osc_budget() {
        for ignored in [0x00, 0x7f, 0x80] {
            let mut observer = Observer::default();
            for byte in [0x1b, ignored, b']', b'0', b';'] {
                assert_eq!(observer.advance(byte), None);
            }
            assert!(observer.osc_len > 0, "ignored={ignored:#x}");
            for byte in std::iter::repeat_n(b'A', 5000) {
                assert_eq!(observer.advance(byte), None);
            }
            assert!(observer.discard_osc, "ignored={ignored:#x}");
            assert_eq!(observer.advance(0x07), None);
            let recovered = b"\x1b]7501;state=idle\x07"
                .iter()
                .filter_map(|byte| observer.advance(*byte))
                .collect::<Vec<_>>();
            assert!(
                matches!(recovered.as_slice(), [Event::Report(report)] if report.state == ProgramState::Idle)
            );
        }
    }

    #[test]
    fn reports_wait_for_the_second_byte_of_st() {
        let mut observer = Observer::default();
        for byte in b"\x1b]7501;state=done\x1b" {
            assert_eq!(observer.advance(*byte), None);
        }
        for byte in b"[2J" {
            assert_eq!(observer.advance(*byte), None);
        }
        for byte in b"\x1b]7501;state=blocked\x1b" {
            assert_eq!(observer.advance(*byte), None);
        }
        assert!(
            matches!(observer.advance(b'\\'), Some(Event::Report(report)) if report.state == ProgramState::Blocked)
        );

        let mut observer = Observer::default();
        for byte in b"\x1b]7501;state=done\x1b" {
            assert_eq!(observer.advance_at(*byte, true, 1), None);
        }
        assert_eq!(observer.advance_at(b'\\', true, 2), None);
        assert_eq!(observed(b"\x1b]7501;state=done\x07").len(), 1);
    }

    fn events(bytes: &[u8]) -> Vec<Event> {
        let mut scanner = Scanner::default();
        bytes
            .iter()
            .filter_map(|byte| scanner.advance(*byte))
            .collect()
    }

    #[test]
    fn split_frames_and_both_terminators() {
        for terminator in [b"\x07".as_slice(), b"\x1b\\".as_slice()] {
            let mut wire = b"\x1b]7501;state=blocked:kind=question:progress=50:app=claude-code:id=build/test:title=VGVzdA==:msg=T0s=".to_vec();
            wire.extend_from_slice(terminator);
            let expected = events(&wire);
            assert_eq!(expected.len(), 1);
            for split in 0..wire.len() {
                let mut scanner = Scanner::default();
                let mut found = Vec::new();
                for part in [&wire[..split], &wire[split..]] {
                    found.extend(part.iter().filter_map(|byte| scanner.advance(*byte)));
                }
                assert_eq!(found, expected, "split at {split}");
            }
            let Event::Report(report) = &expected[0] else {
                panic!("expected report");
            };
            assert_eq!(report.state, ProgramState::Blocked);
            assert_eq!(report.kind, Some(ProgramStatusKind::Question));
            assert_eq!(report.progress, Some(50));
            assert_eq!(report.id.as_deref(), Some("build/test"));
            assert_eq!(report.app.as_deref(), Some("claude-code"));
            assert_eq!(report.title.as_deref(), Some("Test"));
            assert_eq!(report.msg.as_deref(), Some("OK"));
        }
    }

    #[test]
    fn rejects_bad_or_oversize_reports_and_resynchronizes() {
        for body in [
            "state=unknown",
            "state=working:id=bad//id",
            "state=blocked:id=worker!",
            "state=clear:id=worker!",
            "state=working:id=",
            "state=working:msg=abcde",
            "state=working:msg=SGVsbG8K",
            "state=working:title=/w==",
            "state=working:msg=//8=",
        ] {
            let wire = format!("\x1b]7501;{body}\x07");
            assert!(events(wire.as_bytes()).is_empty(), "{body}");
        }
        let mut wire = format!("\x1b]7501;state=working:msg={}\x07", "A".repeat(4100)).into_bytes();
        wire.extend_from_slice(b"\x1b]7501;?\x07");
        assert_eq!(events(&wire), vec![Event::Query]);
        assert_eq!(
            events(b"\x1b]7501;?\x1bq\x1b]7501;?\x07"),
            vec![Event::Query]
        );
        assert!(events(b"\x1b]7501;?\x1bX\x1b]7501;?\x07\x1b\\").is_empty());
        assert!(events(b"\x1bPopaque\x1b]7501;?\x07\x1b\\").is_empty());
        assert_eq!(events(b"\x1bc"), vec![Event::Reset]);
        for split in 0..b"\x1bPunterminated\x1bc\x1b]7501;state=working\x07".len() {
            let wire = b"\x1bPunterminated\x1bc\x1b]7501;state=working\x07";
            let mut scanner = Scanner::default();
            let mut found = Vec::new();
            for part in [&wire[..split], &wire[split..]] {
                found.extend(part.iter().filter_map(|byte| scanner.advance(*byte)));
            }
            assert!(matches!(found.as_slice(), [Event::Reset, Event::Report(_)]));
        }
    }

    #[test]
    fn last_duplicate_wins_and_optional_values_are_ignored() {
        let found = events(b"\x1b]7501;state=working:state=blocked:kind=unknown:progress=101:app=bad/name:unknown=x:msg=T0s:ignored=@@@\x07");
        let [Event::Report(report)] = found.as_slice() else {
            panic!("expected one report");
        };
        assert_eq!(report.state, ProgramState::Blocked);
        assert_eq!(report.kind, None);
        assert_eq!(report.progress, None);
        assert_eq!(report.app, None);
        assert_eq!(report.msg.as_deref(), Some("OK"));
    }

    #[test]
    fn root_clear_and_child_reports_do_not_override_the_pane() {
        let mut status = ProgramStatusSnapshot::default();
        for event in events(b"\x1b]7501;state=working\x07\x1b]7501;state=blocked:id=worker:kind=permission\x07\x1b]7501;state=blocked:kind=permission\x07\x1b]7501;state=idle\x07") {
            if let Event::Report(report) = event {
                status.apply(report);
            }
        }
        assert_eq!(status.root.as_ref().unwrap().state, ProgramState::Idle);
        assert_eq!(status.working_to_idle_revision, status.revision);
        let clear = events(b"\x1b]7501;state=clear\x07").pop().unwrap();
        if let Event::Report(report) = clear {
            status.apply(report);
        }
        assert_eq!(status.root, None);
        assert_eq!(status.clear_revision, status.revision);
    }

    #[test]
    fn shell_prompt_retires_busy_records_but_preserves_finished_records() {
        let mut status = ProgramStatusSnapshot::default();
        for state in ["working", "blocked"] {
            let wire = format!("\x1b]7501;state={state}\x07\x1b]133;A\x07");
            for event in events(wire.as_bytes()) {
                match event {
                    Event::Report(report) => status.apply(report),
                    Event::ShellPrompt => status.shell_prompt(),
                    _ => {}
                }
            }
            assert_eq!(status.root, None, "{state}");
            assert_eq!(status.clear_revision, status.revision);
        }
        for state in ["done", "error"] {
            let wire = format!("\x1b]7501;state={state}\x07\x1b]133;A\x1b\\");
            for event in events(wire.as_bytes()) {
                match event {
                    Event::Report(report) => status.apply(report),
                    Event::ShellPrompt => status.shell_prompt(),
                    _ => {}
                }
            }
            assert!(matches!(
                status.root.as_ref().unwrap().state,
                ProgramState::Done | ProgramState::Error
            ));
        }
        let wire = b"\x1b]7501;state=working\x07\x1b]7501;state=idle\x07\x1b]133;A\x07";
        for event in events(wire) {
            match event {
                Event::Report(report) => status.apply(report),
                Event::ShellPrompt => status.shell_prompt(),
                _ => {}
            }
        }
        assert!(status.working_to_idle_revision < status.clear_revision);
    }
}
