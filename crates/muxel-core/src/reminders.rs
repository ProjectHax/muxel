//! Pure model + maintenance for muxel's per-project **reminders**:
//! `.muxel/REMINDERS.md`, a list of things to tell the user about at a set time.
//!
//! Like the accomplishment log, the file is **append-only for agents**: when the
//! user asks to be reminded of something, the agent appends one line,
//! `- YYYY-MM-DD HH:MM ±HHMM | message | agent | #key`, with a shell append, never
//! reading the file. A later line with the same `#key` replaces the earlier one
//! (to move a reminder), and `- cancel | #key` removes it. muxel owns everything
//! else: it folds those, keys unkeyed lines, drops exact repeats, writes every time
//! with an explicit offset, keeps the lines in due order, and removes a reminder
//! once the user dismisses it.
//!
//! A bullet line whose time can't be read is **kept verbatim** (and reported), so
//! muxel never throws away text it doesn't understand.
//!
//! This module is pure apart from [`LocalZone`], which reads the system time zone.
//! Times are unix seconds (UTC); the [`Zone`] passed in decides how a time written
//! without an offset is read, and how times are written back.

use std::collections::{HashMap, HashSet};

use crate::accomplishments::normalize_key;
use crate::memory::{DAY_SECS, fmt_date, parse_date};

/// When a reminder written with a date but no time of day goes off.
pub const DATE_ONLY_HOUR: i64 = 9;

/// How local wall-clock times map to UTC.
pub trait Zone {
    /// The UTC offset, in seconds, in effect at `utc`.
    fn offset_at(&self, utc: i64) -> i32;
    /// UTC seconds for a local wall-clock time, given as seconds since the epoch as
    /// if that wall-clock time were UTC.
    fn utc_of_local(&self, local: i64) -> i64 {
        let guess = local - i64::from(self.offset_at(local));
        local - i64::from(self.offset_at(guess))
    }
}

/// A zone with one fixed offset (tests, and times written with an offset).
#[derive(Clone, Copy, Debug)]
pub struct FixedZone(pub i32);

impl Zone for FixedZone {
    fn offset_at(&self, _utc: i64) -> i32 {
        self.0
    }
}

/// The system's local time zone.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalZone;

impl Zone for LocalZone {
    fn offset_at(&self, utc: i64) -> i32 {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_opt(utc, 0)
            .earliest()
            .map_or(0, |d| d.offset().local_minus_utc())
    }

    fn utc_of_local(&self, local: i64) -> i64 {
        use chrono::TimeZone;
        let naive = chrono::DateTime::from_timestamp(local, 0).map(|d| d.naive_utc());
        naive
            .and_then(|n| chrono::Local.from_local_datetime(&n).earliest())
            .map(|d| d.timestamp())
            // A wall-clock time skipped by a DST jump: fall back to the offset math.
            .unwrap_or_else(|| {
                let guess = local - i64::from(self.offset_at(local));
                local - i64::from(self.offset_at(guess))
            })
    }
}

/// One reminder in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reminder {
    /// When it goes off (unix seconds). `None` when the line's time couldn't be read.
    pub due: Option<i64>,
    pub message: String,
    pub agent: Option<String>,
    pub key: Option<String>,
    /// The original line, kept verbatim, when `due` is `None`.
    pub raw: Option<String>,
}

impl Reminder {
    /// A new reminder the user added themselves.
    pub fn new(due: i64, message: &str, key: Option<String>) -> Self {
        Self {
            due: Some(due),
            message: clean_message(message),
            agent: None,
            key,
            raw: None,
        }
    }

    /// A stable identity for this reminder within its file: its key, or for a line
    /// muxel couldn't read, the line itself.
    pub fn id(&self) -> String {
        match (&self.key, &self.raw) {
            (Some(key), _) => format!("#{key}"),
            (None, Some(raw)) => raw.clone(),
            (None, None) => self.message.clone(),
        }
    }

    /// Whether it's due at `now`.
    pub fn is_due(&self, now: i64) -> bool {
        self.due.is_some_and(|d| d <= now)
    }
}

/// One parsed line: a reminder, or a request to cancel one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Reminder(Reminder),
    Cancel(String),
}

/// One line, no `|` (the field separator), no surrounding whitespace.
fn clean_message(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "/")
}

/// A key for a reminder that arrived without one: its message's first few words.
fn key_from_message(message: &str) -> String {
    let words: Vec<&str> = message.split_whitespace().take(6).collect();
    normalize_key(&words.join(" ")).unwrap_or_else(|| "reminder".to_string())
}

/// A strict `YYYY-MM-DD`, as seconds at that day's start (UTC).
fn parse_day(field: &str) -> Option<i64> {
    let b = field.as_bytes();
    let shaped = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    shaped.then(|| parse_date(field)).filter(|&d| d != 0)
}

/// `HH:MM` or `HH:MM:SS` as seconds into the day.
fn parse_clock(s: &str) -> Option<i64> {
    let mut it = s.split(':');
    let h: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let sec: i64 = match it.next() {
        Some(v) => v.parse().ok()?,
        None => 0,
    };
    if it.next().is_some() || !(0..24).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    if !(0..61).contains(&sec) {
        return None;
    }
    Some(h * 3600 + m * 60 + sec)
}

/// `Z`, `UTC`, `+HHMM`, `-HH:MM`, or `+HH` as an offset in seconds.
fn parse_offset(s: &str) -> Option<i32> {
    if s.eq_ignore_ascii_case("z") || s.eq_ignore_ascii_case("utc") {
        return Some(0);
    }
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (h, m) = match digits.len() {
        2 => (digits.parse::<i32>().ok()?, 0),
        4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?),
        _ => return None,
    };
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// Parse a time field: `YYYY-MM-DD`, then optionally a time of day (` HH:MM` or
/// `THH:MM[:SS]`), then optionally an offset (` -0500`, `Z`, `+05:30`, glued on or
/// after a space). Without an offset the time is read in `zone`.
pub fn parse_time(field: &str, zone: &impl Zone) -> Option<i64> {
    let field = field.trim();
    let day = parse_day(field.get(..10)?)?;
    let rest = field[10..].trim_start_matches(['T', 't', ' ']).trim();
    if rest.is_empty() {
        return Some(zone.utc_of_local(day + DATE_ONLY_HOUR * 3600));
    }
    // Split the clock from an offset, which may be glued on (`15:00-05:00`, `15:00Z`)
    // or follow a space.
    let (clock, offset) = match rest.split_once(char::is_whitespace) {
        Some((clock, offset)) => (clock, Some(offset.trim())),
        None => match rest.find(['+', '-', 'Z', 'z']) {
            Some(i) => (&rest[..i], Some(&rest[i..])),
            None => (rest, None),
        },
    };
    let local = day + parse_clock(clock)?;
    match offset {
        Some(o) => Some(local - i64::from(parse_offset(o)?)),
        None => Some(zone.utc_of_local(local)),
    }
}

/// Format `utc` as the file writes it: `YYYY-MM-DD HH:MM ±HHMM`, in `zone`.
pub fn fmt_time(utc: i64, zone: &impl Zone) -> String {
    let offset = zone.offset_at(utc);
    let local = utc + i64::from(offset);
    let secs = local.rem_euclid(DAY_SECS);
    let sign = if offset < 0 { '-' } else { '+' };
    let off = offset.unsigned_abs();
    format!(
        "{} {:02}:{:02} {sign}{:02}{:02}",
        fmt_date(local),
        secs / 3600,
        secs % 3600 / 60,
        off / 3600,
        off % 3600 / 60
    )
}

/// Parse one line. Only bullet lines (`- ` / `* `) count. `- cancel | #key` cancels;
/// anything else is a reminder whose first field is its time, then the message, then
/// optionally the agent and a `#key`. A bullet whose first field isn't a time is
/// kept as a reminder with no due time and its text in `raw`.
pub fn parse_line(line: &str, zone: &impl Zone) -> Option<Line> {
    let t = line.trim();
    let body = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))?
        .trim();
    let mut parts: Vec<&str> = body.split('|').map(str::trim).collect();
    let mut key = None;
    if parts.len() > 1 && parts[parts.len() - 1].starts_with('#') {
        key = normalize_key(parts.pop().unwrap_or_default());
    }
    if parts
        .first()
        .is_some_and(|f| f.eq_ignore_ascii_case("cancel"))
    {
        return key.map(Line::Cancel);
    }
    let Some(due) = parts.first().and_then(|f| parse_time(f, zone)) else {
        return (!body.is_empty()).then(|| {
            Line::Reminder(Reminder {
                due: None,
                message: clean_message(body),
                agent: None,
                key: None,
                raw: Some(t.to_string()),
            })
        });
    };
    parts.remove(0);
    let agent = if parts.len() > 1 {
        parts.pop().filter(|a| !a.is_empty()).map(str::to_string)
    } else {
        None
    };
    let message = clean_message(&parts.join(" | "));
    if message.is_empty() {
        return None;
    }
    Some(Line::Reminder(Reminder {
        due: Some(due),
        message,
        agent,
        key,
        raw: None,
    }))
}

/// Parse a whole `REMINDERS.md`, in file order, skipping the heading, the header
/// comment, and anything that isn't a bullet line.
pub fn parse_document(text: &str, zone: &impl Zone) -> Vec<Line> {
    let mut out = Vec::new();
    let mut in_comment = false;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let t = line.trim();
        if in_comment {
            in_comment = !t.contains("-->");
            continue;
        }
        if t.starts_with("<!--") {
            in_comment = !t.contains("-->");
            continue;
        }
        if let Some(l) = parse_line(t, zone) {
            out.push(l);
        }
    }
    out
}

/// The reminders in `text`, maintained (see [`maintain`]).
pub fn reminders_in(text: &str, zone: &impl Zone) -> Vec<Reminder> {
    maintain(parse_document(text, zone))
}

/// Render one reminder as its line (no trailing newline).
pub fn render_line(r: &Reminder, zone: &impl Zone) -> String {
    let Some(due) = r.due else {
        return r
            .raw
            .clone()
            .unwrap_or_else(|| format!("- {}", clean_message(&r.message)));
    };
    let mut s = format!("- {} | {}", fmt_time(due, zone), clean_message(&r.message));
    if let Some(agent) = r.agent.as_deref().filter(|a| !a.is_empty()) {
        s.push_str(" | ");
        s.push_str(&agent.replace('|', "/"));
    }
    if let Some(key) = r.key.as_deref().and_then(normalize_key) {
        s.push_str(" | #");
        s.push_str(&key);
    }
    s
}

/// The heading + format comment at the top of the file. It ends in a newline so a
/// shell append (`echo … >> file`) always starts a fresh line.
pub fn document_header() -> &'static str {
    "# Reminders\n\n\
<!-- muxel-maintained reminders for this project, soonest first. muxel alerts the
     user when each one is due and removes it once they dismiss it. Agents: don't
     read this file. Append one line per reminder with a shell append, in the form
     `- YYYY-MM-DD HH:MM +HHMM | message | agent | #key` (the user's local time,
     with its UTC offset). To move a reminder, append a new line with its #key; to
     cancel one, append `- cancel | #key`. -->\n"
}

/// Render the full document (header + one line per reminder, in the given order).
pub fn render_document(reminders: &[Reminder], zone: &impl Zone) -> String {
    let mut s = String::from(document_header());
    for r in reminders {
        s.push_str(&render_line(r, zone));
        s.push('\n');
    }
    s
}

/// Bring parsed lines into their maintained form:
///
/// 1. a later reminder with an earlier one's key replaces it (its time and message;
///    its agent too, if it names one);
/// 2. `cancel` removes the reminder with that key written before it;
/// 3. exact repeats (same time and message, ignoring case) are dropped;
/// 4. reminders are sorted by due time (stably; unreadable lines go last);
/// 5. every readable reminder still without a key gets a unique one.
pub fn maintain(lines: Vec<Line>) -> Vec<Reminder> {
    let mut kept: Vec<Option<Reminder>> = Vec::with_capacity(lines.len());
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for line in lines {
        match line {
            Line::Cancel(key) => {
                if let Some(i) = by_key.remove(&key) {
                    kept[i] = None;
                }
            }
            Line::Reminder(r) => {
                let Some(key) = r.key.clone() else {
                    kept.push(Some(r));
                    continue;
                };
                match by_key.get(&key).and_then(|&i| kept[i].as_mut()) {
                    Some(original) => {
                        original.due = r.due;
                        original.message = r.message;
                        original.raw = r.raw;
                        if r.agent.is_some() {
                            original.agent = r.agent;
                        }
                    }
                    None => {
                        by_key.insert(key, kept.len());
                        kept.push(Some(r));
                    }
                }
            }
        }
    }
    let mut seen = HashSet::new();
    let mut out: Vec<Reminder> = kept
        .into_iter()
        .flatten()
        .filter(|r| r.due.is_none() || seen.insert((r.due, r.message.to_lowercase())))
        .collect();
    out.sort_by_key(|r| r.due.unwrap_or(i64::MAX));

    let mut used: HashSet<String> = out.iter().filter_map(|r| r.key.clone()).collect();
    for r in out
        .iter_mut()
        .filter(|r| r.key.is_none() && r.due.is_some())
    {
        let base = key_from_message(&r.message);
        let mut key = base.clone();
        let mut n = 2;
        while used.contains(&key) {
            key = format!("{base}-{n}");
            n += 1;
        }
        used.insert(key.clone());
        r.key = Some(key);
    }
    out
}

/// The maintained document for `text`, or `None` when it's already in that form —
/// so muxel only rewrites the file (and only risks racing an agent's append) when
/// there's something to fold, key, sort, or normalize.
pub fn plan_rewrite(text: &str, zone: &impl Zone) -> Option<String> {
    let rendered = render_document(&reminders_in(text, zone), zone);
    (rendered != text).then_some(rendered)
}

/// The earliest due time among `reminders`.
pub fn next_due<'a>(reminders: impl IntoIterator<Item = &'a Reminder>) -> Option<i64> {
    reminders.into_iter().filter_map(|r| r.due).min()
}

/// Remove the reminder with identity `id` (see [`Reminder::id`]). Whether one went.
pub fn remove(reminders: &mut Vec<Reminder>, id: &str) -> bool {
    let before = reminders.len();
    reminders.retain(|r| r.id() != id);
    reminders.len() != before
}

/// Move the reminder with identity `id` to `due`. Whether it was found.
pub fn reschedule(reminders: &mut [Reminder], id: &str, due: i64) -> bool {
    match reminders.iter_mut().find(|r| r.id() == id) {
        Some(r) => {
            r.due = Some(due);
            r.raw = None;
            true
        }
        None => false,
    }
}

/// A key not yet used in `reminders`, derived from `message`.
pub fn fresh_key(reminders: &[Reminder], message: &str) -> String {
    let base = key_from_message(message);
    let used: HashSet<&str> = reminders.iter().filter_map(|r| r.key.as_deref()).collect();
    let mut key = base.clone();
    let mut n = 2;
    while used.contains(key.as_str()) {
        key = format!("{base}-{n}");
        n += 1;
    }
    key
}

/// Parse the panel's "when" box, relative to `now` (unix seconds):
///
/// - `15m`, `2h`, `1d`, `90 min`, `in 2 hours` — that long from now;
/// - `14:30` — today at that time, or tomorrow if it has passed;
/// - `tomorrow`, `tomorrow 14:30` — tomorrow at 09:00 or that time;
/// - `YYYY-MM-DD [HH:MM] [±HHMM]` — as the file reads it.
pub fn parse_when(input: &str, now: i64, zone: &impl Zone) -> Option<i64> {
    let input = input.trim().to_ascii_lowercase();
    let input = input.strip_prefix("in ").unwrap_or(&input).trim();
    if input.is_empty() {
        return None;
    }
    if let Some(secs) = parse_duration(input) {
        return Some(now + secs);
    }
    let local_now = now + i64::from(zone.offset_at(now));
    let today = local_now.div_euclid(DAY_SECS) * DAY_SECS;
    if let Some(rest) = input.strip_prefix("tomorrow") {
        let rest = rest.trim().trim_start_matches("at ").trim();
        let clock = if rest.is_empty() {
            DATE_ONLY_HOUR * 3600
        } else {
            parse_clock_12(rest)?
        };
        return Some(zone.utc_of_local(today + DAY_SECS + clock));
    }
    if let Some(clock) = parse_clock_12(input.trim_start_matches("at ").trim()) {
        let at = zone.utc_of_local(today + clock);
        return Some(if at > now {
            at
        } else {
            zone.utc_of_local(today + DAY_SECS + clock)
        });
    }
    parse_time(input, zone)
}

/// `15m`, `2 h`, `90 minutes`, `1d` as seconds.
fn parse_duration(s: &str) -> Option<i64> {
    let digits_end = s.find(|c: char| !c.is_ascii_digit())?;
    let n: i64 = s[..digits_end].parse().ok()?;
    let unit = s[digits_end..].trim();
    let per = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => DAY_SECS,
        "w" | "wk" | "week" | "weeks" => 7 * DAY_SECS,
        _ => return None,
    };
    (n > 0).then_some(n * per)
}

/// `14:30`, `2:30pm`, `2pm`, `9 am` as seconds into the day.
fn parse_clock_12(s: &str) -> Option<i64> {
    let s = s.trim();
    let (body, pm) = if let Some(b) = s.strip_suffix("pm") {
        (b.trim(), Some(true))
    } else if let Some(b) = s.strip_suffix("am") {
        (b.trim(), Some(false))
    } else {
        (s, None)
    };
    let secs = if body.contains(':') {
        parse_clock(body)?
    } else if pm.is_some() {
        let h: i64 = body.parse().ok()?;
        (0..24).contains(&h).then_some(h * 3600)?
    } else {
        return None;
    };
    match pm {
        None => Some(secs),
        Some(pm) => {
            let h = secs / 3600;
            if !(1..=12).contains(&h) {
                return None;
            }
            let h = match (h, pm) {
                (12, false) => 0,
                (12, true) => 12,
                (h, true) => h + 12,
                (h, false) => h,
            };
            Some(h * 3600 + secs % 3600)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FixedZone, Line, Reminder, fmt_time, maintain, next_due, parse_document, parse_line,
        parse_time, parse_when, plan_rewrite, reminders_in, remove, render_document, reschedule,
    };

    const EST: FixedZone = FixedZone(-5 * 3600);
    /// 2026-11-16 00:00 UTC.
    const DAY: i64 = 1_794_787_200;

    fn reminder(line: &str) -> Reminder {
        match parse_line(line, &EST) {
            Some(Line::Reminder(r)) => r,
            other => panic!("not a reminder: {other:?}"),
        }
    }

    #[test]
    fn day_constant_is_the_date_it_names() {
        assert_eq!(crate::memory::fmt_date(DAY), "2026-11-16");
    }

    #[test]
    fn parses_times_with_and_without_offsets() {
        let at_15_utc = DAY + 15 * 3600;
        assert_eq!(parse_time("2026-11-16 15:00 +0000", &EST), Some(at_15_utc));
        assert_eq!(parse_time("2026-11-16 15:00Z", &EST), Some(at_15_utc));
        assert_eq!(
            parse_time("2026-11-16T15:00:00+00:00", &EST),
            Some(at_15_utc)
        );
        assert_eq!(parse_time("2026-11-16 10:00 -0500", &EST), Some(at_15_utc));
        assert_eq!(parse_time("2026-11-16T10:00-05:00", &EST), Some(at_15_utc));
        // No offset: read in the zone.
        assert_eq!(parse_time("2026-11-16 10:00", &EST), Some(at_15_utc));
        // Date only: 09:00 local.
        assert_eq!(parse_time("2026-11-16", &EST), Some(DAY + 14 * 3600));
        assert_eq!(parse_time("2026-11-16 25:00", &EST), None);
        assert_eq!(parse_time("soon", &EST), None);
        assert_eq!(parse_time("2026-11-16 10:00 EST", &EST), None);
    }

    #[test]
    fn formats_in_the_zone_with_its_offset() {
        assert_eq!(fmt_time(DAY + 15 * 3600, &EST), "2026-11-16 10:00 -0500");
        assert_eq!(
            fmt_time(DAY + 15 * 3600, &FixedZone(5 * 3600 + 1800)),
            "2026-11-16 20:30 +0530"
        );
        // Crossing midnight backwards.
        assert_eq!(fmt_time(DAY + 3600, &EST), "2026-11-15 20:00 -0500");
    }

    #[test]
    fn parses_a_full_line() {
        let r = reminder("- 2026-11-16 10:00 -0500 | Check the | deploy | claude | #Check Deploy");
        assert_eq!(r.due, Some(DAY + 15 * 3600));
        assert_eq!(r.message, "Check the / deploy");
        assert_eq!(r.agent.as_deref(), Some("claude"));
        assert_eq!(r.key.as_deref(), Some("check-deploy"));
    }

    #[test]
    fn parses_minimal_and_cancel_lines() {
        let r = reminder("* 2026-11-16 10:00 | Stretch");
        assert_eq!(r.message, "Stretch");
        assert_eq!(r.agent, None);
        assert_eq!(r.key, None);
        assert_eq!(
            parse_line("- cancel | #stretch", &EST),
            Some(Line::Cancel("stretch".into()))
        );
        assert_eq!(
            parse_line("- CANCEL | #a", &EST),
            Some(Line::Cancel("a".into()))
        );
        // A cancel needs a key.
        assert_eq!(parse_line("- cancel", &EST), None);
        assert_eq!(parse_line("not a bullet", &EST), None);
        assert_eq!(parse_line("- 2026-11-16 10:00 | ", &EST), None);
    }

    #[test]
    fn keeps_unreadable_lines_verbatim() {
        let r = reminder("- next tuesday | call the bank");
        assert_eq!(r.due, None);
        assert_eq!(r.raw.as_deref(), Some("- next tuesday | call the bank"));
        let text = "# Reminders\n- next tuesday | call the bank\n";
        let kept = reminders_in(text, &EST);
        assert_eq!(kept.len(), 1);
        let out = render_document(&kept, &EST);
        assert!(out.contains("- next tuesday | call the bank\n"));
    }

    #[test]
    fn later_key_moves_and_cancel_removes() {
        let text = "\
- 2026-11-16 10:00 | Deploy check | claude | #deploy
- 2026-11-16 09:00 | Standup | #standup
- 2026-11-16 12:00 | Deploy check again | #deploy
- cancel | #standup
- cancel | #never-existed
";
        let kept = reminders_in(text, &EST);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].message, "Deploy check again");
        assert_eq!(kept[0].due, Some(DAY + 17 * 3600));
        assert_eq!(kept[0].agent.as_deref(), Some("claude"));
    }

    #[test]
    fn a_key_can_come_back_after_a_cancel() {
        let text = "\
- 2026-11-16 10:00 | A | #a
- cancel | #a
- 2026-11-16 11:00 | A again | #a
";
        let kept = reminders_in(text, &EST);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].message, "A again");
    }

    #[test]
    fn sorts_dedupes_and_keys() {
        let text = "\
- 2026-11-16 12:00 | Lunch
- 2026-11-16 08:00 | Coffee
- 2026-11-16 12:00 | lunch
- bogus line
- 2026-11-16 13:00 | Lunch
";
        let kept = reminders_in(text, &EST);
        let msgs: Vec<&str> = kept.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(msgs, ["Coffee", "Lunch", "Lunch", "bogus line"]);
        assert_eq!(kept[0].key.as_deref(), Some("coffee"));
        assert_eq!(kept[1].key.as_deref(), Some("lunch"));
        assert_eq!(kept[2].key.as_deref(), Some("lunch-2"));
        assert_eq!(kept[3].key, None);
    }

    #[test]
    fn rewrite_is_planned_once_then_stable() {
        let text = "# Reminders\n- 2026-11-16 10:00 | Stretch\n";
        let first = plan_rewrite(text, &EST).expect("normalizes");
        assert!(first.contains("- 2026-11-16 10:00 -0500 | Stretch | #stretch\n"));
        assert_eq!(plan_rewrite(&first, &EST), None);
        // A fresh header-only file is already maintained.
        assert_eq!(plan_rewrite(&render_document(&[], &EST), &EST), None);
    }

    #[test]
    fn appended_line_after_header_parses() {
        let mut text = render_document(&[], &EST);
        text.push_str("- 2026-11-16 10:00 -0500 | Ping | codex | #ping\n");
        let lines = parse_document(&text, &EST);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn due_remove_and_reschedule() {
        let mut list = maintain(parse_document(
            "- 2026-11-16 10:00 | A | #a\n- 2026-11-16 11:00 | B | #b\n",
            &EST,
        ));
        assert_eq!(next_due(&list), Some(DAY + 15 * 3600));
        assert!(list[0].is_due(DAY + 15 * 3600));
        assert!(!list[1].is_due(DAY + 15 * 3600));
        assert!(reschedule(&mut list, "#a", DAY + 20 * 3600));
        assert!(remove(&mut list, "#b"));
        assert!(!remove(&mut list, "#b"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].due, Some(DAY + 20 * 3600));
    }

    #[test]
    fn parses_quick_add_times() {
        // Now: 2026-11-16 10:00 local (15:00 UTC).
        let now = DAY + 15 * 3600;
        assert_eq!(parse_when("15m", now, &EST), Some(now + 900));
        assert_eq!(parse_when("in 2 hours", now, &EST), Some(now + 7200));
        assert_eq!(parse_when("1d", now, &EST), Some(now + 86_400));
        assert_eq!(parse_when("14:30", now, &EST), Some(DAY + 19 * 3600 + 1800));
        assert_eq!(
            parse_when("2:30pm", now, &EST),
            Some(DAY + 19 * 3600 + 1800)
        );
        assert_eq!(parse_when("3pm", now, &EST), Some(DAY + 20 * 3600));
        // Already past today: tomorrow.
        assert_eq!(
            parse_when("08:00", now, &EST),
            Some(DAY + 86_400 + 13 * 3600)
        );
        assert_eq!(
            parse_when("tomorrow", now, &EST),
            Some(DAY + 86_400 + 14 * 3600)
        );
        assert_eq!(
            parse_when("tomorrow at 7am", now, &EST),
            Some(DAY + 86_400 + 12 * 3600)
        );
        assert_eq!(
            parse_when("2026-11-20 08:00", now, &EST),
            Some(DAY + 4 * 86_400 + 13 * 3600)
        );
        assert_eq!(parse_when("", now, &EST), None);
        assert_eq!(parse_when("whenever", now, &EST), None);
        assert_eq!(parse_when("0m", now, &EST), None);
        assert_eq!(parse_when("13pm", now, &EST), None);
    }

    #[test]
    fn local_zone_round_trips() {
        use super::{LocalZone, Zone};
        let z = LocalZone;
        let utc = DAY + 15 * 3600;
        let local = utc + i64::from(z.offset_at(utc));
        assert_eq!(z.utc_of_local(local), utc);
    }
}
