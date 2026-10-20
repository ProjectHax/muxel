//! Pure model + maintenance for muxel's per-project **accomplishment log**:
//! `.muxel/ACCOMPLISHMENTS.md`, a record of finished work (features added, bugs
//! fixed, …) that agents append to as they complete it.
//!
//! The file is **append-only for agents**: one line per accomplishment,
//! `- YYYY-MM-DD | type | summary | agent | #key`, written with a shell append so an
//! agent never has to read the file (it would cost tokens for nothing it needs).
//! The `#key` is a short name the agent picks; to correct an entry later it appends
//! a new line with the same key, and muxel folds it into the original (which keeps
//! its date and gains an `updated` date). muxel owns everything else: it stamps
//! undated lines, keys unkeyed ones, merges corrections, dedupes repeats, keeps the
//! lines in date order, and prunes entries older than the retention window —
//! rewriting the file only when one of those actually changes something.
//!
//! This module is pure — no I/O. Callers pass `now` as **local wall-clock seconds**
//! (unix seconds plus the local UTC offset), so "today" matches the date an agent
//! writes and the day a person reads it, and everything stays deterministic.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::memory::{DAY_SECS, fmt_date, parse_date};

/// Days of history kept when the user hasn't chosen otherwise.
pub const DEFAULT_RETENTION_DAYS: u32 = 14;
/// Longest retention window the setting allows.
pub const MAX_RETENTION_DAYS: u32 = 90;

/// Clamp a retention setting to the supported `1..=MAX_RETENTION_DAYS` range.
pub fn clamp_retention(days: u32) -> u32 {
    days.clamp(1, MAX_RETENTION_DAYS)
}

/// What kind of work an accomplishment was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AccomplishmentKind {
    Feature,
    Fix,
    Refactor,
    Perf,
    Docs,
    Test,
    Chore,
    Other,
}

impl AccomplishmentKind {
    /// Every kind, in display order.
    pub const ALL: [Self; 8] = [
        Self::Feature,
        Self::Fix,
        Self::Refactor,
        Self::Perf,
        Self::Docs,
        Self::Test,
        Self::Chore,
        Self::Other,
    ];

    /// Recognize a type word, tolerating the synonyms and decoration agents tend to
    /// use (`feat`, `bugfix`, `[fix]`, `**docs**`). `None` for anything else.
    pub fn parse(word: &str) -> Option<Self> {
        let w = word
            .trim()
            .trim_matches(|c: char| matches!(c, '[' | ']' | '(' | ')' | '*' | '`' | ':' | '_'))
            .to_ascii_lowercase();
        Some(match w.as_str() {
            "feature" | "features" | "feat" | "add" | "added" | "new" => Self::Feature,
            "fix" | "fixed" | "fixes" | "bug" | "bugfix" | "hotfix" => Self::Fix,
            "refactor" | "refactored" | "refactoring" | "cleanup" => Self::Refactor,
            "perf" | "performance" | "optimize" | "optimization" | "speed" => Self::Perf,
            "docs" | "doc" | "documentation" => Self::Docs,
            "test" | "tests" | "testing" => Self::Test,
            "chore" | "build" | "ci" | "deps" => Self::Chore,
            "other" | "misc" => Self::Other,
            _ => return None,
        })
    }

    /// The canonical word written to the file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Feature => "feature",
            Self::Fix => "fix",
            Self::Refactor => "refactor",
            Self::Perf => "perf",
            Self::Docs => "docs",
            Self::Test => "test",
            Self::Chore => "chore",
            Self::Other => "other",
        }
    }
}

/// One finished piece of work — a line in `ACCOMPLISHMENTS.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accomplishment {
    /// Local day it was accomplished, as seconds at that day's start (`0` = the line
    /// carried no date yet; [`maintain`] stamps it).
    pub date: i64,
    pub kind: AccomplishmentKind,
    /// One-sentence description of the outcome.
    pub summary: String,
    /// Which agent did it (`claude`, `codex`, …), when the line says.
    pub agent: Option<String>,
    /// Short kebab-case name for the result (`#tmux-restart-race` in the file). A
    /// later line with the same key corrects this entry. [`maintain`] gives every
    /// entry one, so any of them can be corrected.
    #[serde(default)]
    pub key: Option<String>,
    /// Local day of the latest correction, when it came on a later day than `date`.
    #[serde(default)]
    pub updated: Option<i64>,
}

impl Accomplishment {
    pub fn new(
        kind: AccomplishmentKind,
        summary: impl AsRef<str>,
        agent: Option<String>,
        now: i64,
    ) -> Self {
        Self {
            date: day_start(now),
            kind,
            summary: clean_summary(summary.as_ref()),
            agent: agent
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty()),
            key: None,
            updated: None,
        }
    }

    /// The day this entry last changed: its latest correction, else its own date.
    /// Retention counts from here, so a recently corrected entry isn't pruned.
    pub fn last_activity(&self) -> i64 {
        self.updated.map_or(self.date, |u| u.max(self.date))
    }

    /// Case-insensitive substring match on summary, agent, or type. `query` must
    /// already be lowercased.
    pub fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.summary.to_lowercase().contains(query)
            || self.kind.as_str().contains(query)
            || self
                .agent
                .as_deref()
                .is_some_and(|a| a.to_lowercase().contains(query))
            || self.key.as_deref().is_some_and(|k| k.contains(query))
    }
}

/// Seconds at the start of the day containing `secs`.
pub fn day_start(secs: i64) -> i64 {
    secs.div_euclid(DAY_SECS) * DAY_SECS
}

/// One line, no `|` (the field separator), no surrounding whitespace.
fn clean_summary(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "/")
}

/// Longest key muxel keeps (agents' keys are trimmed to it).
const MAX_KEY_LEN: usize = 48;

/// Normalize a key as written (`#Tmux_Restart race`) to kebab-case
/// (`tmux-restart-race`). `None` when nothing usable is left.
pub fn normalize_key(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.trim().trim_start_matches('#').chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(MAX_KEY_LEN);
    let out = out.trim_matches('-').to_string();
    (!out.is_empty()).then_some(out)
}

/// A key for an entry that arrived without one: its summary's first few words.
fn key_from_summary(summary: &str) -> String {
    let words: Vec<&str> = summary.split_whitespace().take(6).collect();
    normalize_key(&words.join(" ")).unwrap_or_else(|| "entry".to_string())
}

/// A strict `YYYY-MM-DD` field, as seconds at that day's start.
fn parse_day(field: &str) -> Option<i64> {
    let b = field.as_bytes();
    let shaped = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    shaped.then(|| parse_date(field))
}

/// Parse one log line. Only bullet lines (`- ` / `* `) are entries; the fields are
/// `date | type | summary | agent | #key | updated YYYY-MM-DD`, but any field other
/// than the summary may be missing, and an unknown type reads as
/// [`AccomplishmentKind::Other`]. The trailing `#key` / `updated` fields are known by
/// their prefix. Extra `|`s stay in the summary.
pub fn parse_line(line: &str) -> Option<Accomplishment> {
    let t = line.trim();
    let body = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))?
        .trim();
    let mut parts: Vec<&str> = body.split('|').map(str::trim).collect();

    let mut date = 0;
    if let Some(day) = parts.first().and_then(|f| parse_day(f)) {
        date = day;
        parts.remove(0);
    }
    let mut kind = AccomplishmentKind::Other;
    if parts.len() > 1
        && let Some(k) = AccomplishmentKind::parse(parts[0])
    {
        kind = k;
        parts.remove(0);
    }
    let mut key = None;
    let mut updated = None;
    while parts.len() > 1 {
        let last = parts[parts.len() - 1];
        if key.is_none() && last.starts_with('#') {
            key = normalize_key(last);
        } else if let Some(day) = last
            .strip_prefix("updated ")
            .and_then(|d| parse_day(d.trim()))
            .filter(|_| updated.is_none())
        {
            updated = Some(day);
        } else {
            break;
        }
        parts.pop();
    }
    let agent = if parts.len() > 1 {
        parts.pop().filter(|a| !a.is_empty()).map(str::to_string)
    } else {
        None
    };
    let summary = clean_summary(&parts.join(" | "));
    if summary.is_empty() {
        return None;
    }
    Some(Accomplishment {
        date,
        kind,
        summary,
        agent,
        key,
        updated: updated.filter(|&u| u > 0),
    })
}

/// Parse a whole `ACCOMPLISHMENTS.md`, in file order. The heading, the header
/// comment, and anything that isn't a bullet line are ignored, so a hand-edited or
/// half-written file still yields every well-formed entry.
pub fn parse_document(text: &str) -> Vec<Accomplishment> {
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
        if let Some(e) = parse_line(t) {
            out.push(e);
        }
    }
    out
}

/// Render one entry as its log line (no trailing newline).
pub fn render_line(e: &Accomplishment) -> String {
    let mut s = String::from("- ");
    if e.date > 0 {
        s.push_str(&fmt_date(e.date));
        s.push_str(" | ");
    }
    s.push_str(e.kind.as_str());
    s.push_str(" | ");
    s.push_str(&clean_summary(&e.summary));
    if let Some(agent) = e.agent.as_deref().filter(|a| !a.is_empty()) {
        s.push_str(" | ");
        s.push_str(&agent.replace('|', "/"));
    }
    if let Some(key) = e.key.as_deref().and_then(normalize_key) {
        s.push_str(" | #");
        s.push_str(&key);
    }
    if let Some(updated) = e.updated.filter(|&u| u > 0) {
        s.push_str(" | updated ");
        s.push_str(&fmt_date(updated));
    }
    s
}

/// The heading + format comment at the top of the file. It ends in a newline so a
/// shell append (`echo … >> file`) always starts a fresh line.
pub fn document_header() -> &'static str {
    "# Accomplishments\n\n\
<!-- muxel-maintained log of finished work in this project, oldest first. Agents:
     don't read this file. Append one line per accomplishment with a shell append,
     in the form `- YYYY-MM-DD | type | summary | agent | #key`, where type is one
     of feature, fix, refactor, perf, docs, test, chore and #key is a short
     kebab-case name for the result. To correct an entry, append a new line with
     its #key and muxel replaces the old one. Don't edit or reorder lines: muxel
     stamps, merges, dedupes, and prunes old entries automatically. -->\n"
}

/// Render the full document (header + one line per entry, in the given order).
pub fn render_document(entries: &[Accomplishment]) -> String {
    let mut s = String::from(document_header());
    for e in entries {
        s.push_str(&render_line(e));
        s.push('\n');
    }
    s
}

/// Result of [`maintain`]: the entries that stay, oldest first, and whether anything
/// differs from the input (so the caller knows whether the file needs rewriting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Maintenance {
    pub kept: Vec<Accomplishment>,
    pub changed: bool,
}

/// Bring a parsed log into its maintained form. In order:
///
/// 1. stamp undated entries with today;
/// 2. fold each correction (a later line with an earlier line's key) into the
///    original: it takes the new type, summary, and agent if it had none, keeps
///    its date, and records the correction's day as `updated` when that's later;
/// 3. sort by date (stably, so append order holds within a day);
/// 4. prune entries whose last activity is older than `retention_days` — today
///    counts as day one — and drop exact repeats (same day, type, and summary,
///    ignoring case);
/// 5. key every entry still without a key, uniquely.
///
/// `now` is local wall-clock seconds.
pub fn maintain(mut entries: Vec<Accomplishment>, now: i64, retention_days: u32) -> Maintenance {
    let today = day_start(now);
    let days = i64::from(clamp_retention(retention_days));
    let cutoff = today - (days - 1) * DAY_SECS;
    let mut changed = false;

    for e in &mut entries {
        let day = if e.date <= 0 {
            today
        } else {
            day_start(e.date)
        };
        if day != e.date {
            e.date = day;
            changed = true;
        }
    }

    let mut merged: Vec<Accomplishment> = Vec::with_capacity(entries.len());
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for e in entries {
        let Some(key) = e.key.clone() else {
            merged.push(e);
            continue;
        };
        let Some(&i) = by_key.get(&key) else {
            by_key.insert(key, merged.len());
            merged.push(e);
            continue;
        };
        let original = &mut merged[i];
        let when = e.last_activity();
        original.kind = e.kind;
        original.summary = e.summary;
        if original.agent.is_none() {
            original.agent = e.agent;
        }
        if when > original.date {
            original.updated = Some(original.updated.map_or(when, |u| u.max(when)));
        }
        changed = true;
    }
    let mut entries = merged;

    if !entries.windows(2).all(|w| w[0].date <= w[1].date) {
        entries.sort_by_key(|e| e.date);
        changed = true;
    }

    let mut seen = HashSet::new();
    let mut kept = Vec::with_capacity(entries.len());
    for e in entries {
        if e.last_activity() < cutoff || !seen.insert((e.date, e.kind, e.summary.to_lowercase())) {
            changed = true;
            continue;
        }
        kept.push(e);
    }

    let mut used: HashSet<String> = kept.iter().filter_map(|e| e.key.clone()).collect();
    for e in kept.iter_mut().filter(|e| e.key.is_none()) {
        let base = key_from_summary(&e.summary);
        let mut key = base.clone();
        let mut n = 2;
        while used.contains(&key) {
            key = format!("{base}-{n}");
            n += 1;
        }
        used.insert(key.clone());
        e.key = Some(key);
        changed = true;
    }
    Maintenance { kept, changed }
}

/// The maintained document for `text`, or `None` when maintenance changes nothing —
/// so muxel only rewrites the file (and only risks racing an agent's append) when
/// there's something to stamp, dedupe, or prune.
pub fn plan_rewrite(text: &str, now: i64, retention_days: u32) -> Option<String> {
    let m = maintain(parse_document(text), now, retention_days);
    m.changed.then(|| render_document(&m.kept))
}

/// Group items by day, newest day first and newest-appended first within a day.
/// `items` must be in file (oldest-first) order.
pub fn group_by_day<T>(mut items: Vec<T>, date_of: impl Fn(&T) -> i64) -> Vec<(i64, Vec<T>)> {
    items.reverse();
    items.sort_by_key(|item| std::cmp::Reverse(date_of(item)));
    let mut groups: Vec<(i64, Vec<T>)> = Vec::new();
    for item in items {
        let day = date_of(&item);
        match groups.last_mut() {
            Some((d, group)) if *d == day => group.push(item),
            _ => groups.push((day, vec![item])),
        }
    }
    groups
}

/// How many entries of each kind, in [`AccomplishmentKind::ALL`] order, omitting
/// kinds with none.
pub fn kind_counts<'a>(
    entries: impl IntoIterator<Item = &'a Accomplishment>,
) -> Vec<(AccomplishmentKind, usize)> {
    let mut counts = [0usize; AccomplishmentKind::ALL.len()];
    for e in entries {
        counts[e.kind as usize] += 1;
    }
    AccomplishmentKind::ALL
        .iter()
        .zip(counts)
        .filter(|(_, n)| *n > 0)
        .map(|(k, n)| (*k, n))
        .collect()
}

/// Parse the panel's quick-add box: `fix: what was done` takes the type from the
/// prefix; without a recognized prefix the whole text is the summary. `None` when
/// there's no summary.
pub fn parse_user_input(input: &str) -> Option<(AccomplishmentKind, String)> {
    let input = input.trim();
    let (kind, summary) = match input.split_once(':') {
        Some((prefix, rest)) => match AccomplishmentKind::parse(prefix) {
            Some(kind) if !prefix.trim().contains(' ') => (kind, rest),
            _ => (AccomplishmentKind::Other, input),
        },
        None => (AccomplishmentKind::Other, input),
    };
    let summary = clean_summary(summary);
    (!summary.is_empty()).then_some((kind, summary))
}

/// The short agent name muxel bakes into an agent's log-line template: the program's
/// file stem (`claude`, `codex`), else the preset name, reduced to `[a-z0-9._-]` so
/// it's safe inside the single-quoted shell command the agent runs.
pub fn agent_label(program: Option<&str>, preset: &str) -> String {
    fn sanitize(s: &str) -> String {
        let mut out = String::new();
        for c in s.trim().chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                out.push(c);
            } else if c.is_whitespace() && !out.ends_with('-') {
                out.push('-');
            }
        }
        out.trim_matches('-').to_string()
    }
    let stem = program
        .map(|p| p.trim().rsplit(['/', '\\']).next().unwrap_or_default())
        .map(|leaf| {
            let lower = leaf.to_ascii_lowercase();
            [".exe", ".cmd", ".bat", ".ps1"]
                .iter()
                .find_map(|ext| lower.strip_suffix(ext).map(str::to_string))
                .unwrap_or(lower)
        })
        .map(|stem| sanitize(&stem))
        .filter(|s| !s.is_empty());
    stem.or_else(|| Some(sanitize(preset)).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "agent".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        Accomplishment, AccomplishmentKind, DAY_SECS, agent_label, clamp_retention, group_by_day,
        kind_counts, maintain, normalize_key, parse_document, parse_line, parse_user_input,
        plan_rewrite, render_document, render_line,
    };

    const DAY: i64 = DAY_SECS;
    const NOW: i64 = 1_800_057_600 + 15 * 3600; // 2027-01-16, mid-afternoon
    const TODAY: i64 = 1_800_057_600;

    fn entry(summary: &str, days_ago: i64) -> Accomplishment {
        let mut e = Accomplishment::new(AccomplishmentKind::Fix, summary, None, NOW);
        e.date = TODAY - days_ago * DAY;
        e
    }

    #[test]
    fn parses_the_full_form() {
        let e = parse_line("- 2027-01-16 | fix | Fixed the restart race | claude").unwrap();
        assert_eq!(e.date, TODAY);
        assert_eq!(e.kind, AccomplishmentKind::Fix);
        assert_eq!(e.summary, "Fixed the restart race");
        assert_eq!(e.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn parsing_tolerates_missing_and_odd_fields() {
        let e = parse_line("* feat | Added the log").unwrap();
        assert_eq!((e.date, e.kind), (0, AccomplishmentKind::Feature));
        assert_eq!(e.summary, "Added the log");
        assert_eq!(e.agent, None);

        let e = parse_line("- did a thing").unwrap();
        assert_eq!(e.kind, AccomplishmentKind::Other);
        assert_eq!(e.summary, "did a thing");

        let e = parse_line("- 2027-01-16 | Tidied things | codex").unwrap();
        assert_eq!(e.kind, AccomplishmentKind::Other, "unknown type word");
        assert_eq!(e.summary, "Tidied things");
        assert_eq!(e.agent.as_deref(), Some("codex"));

        let e = parse_line("- 2027-01-16 | [bugfix] | a | b | amp").unwrap();
        assert_eq!(e.kind, AccomplishmentKind::Fix);
        assert_eq!(e.summary, "a / b", "extra separators stay in the summary");
        assert_eq!(e.agent.as_deref(), Some("amp"));

        let e = parse_line("- 2027-01-16 | fix | trailing |").unwrap();
        assert_eq!(e.summary, "trailing");
        assert_eq!(e.agent, None);

        assert!(parse_line("not a bullet").is_none());
        assert!(parse_line("- ").is_none());
        assert!(parse_line("- 2027-01-16 |  |").is_none());
    }

    #[test]
    fn the_header_and_comments_are_not_entries() {
        assert!(parse_document(&render_document(&[])).is_empty());
        let doc = "\u{feff}# Accomplishments\r\n<!--\r\n- not an entry\r\n-->\r\n\
                   - 2027-01-16 | docs | Wrote the guide\r\n";
        let es = parse_document(doc);
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].summary, "Wrote the guide");
        assert_eq!(es[0].kind, AccomplishmentKind::Docs);
    }

    #[test]
    fn document_round_trips() {
        let mut a = Accomplishment::new(
            AccomplishmentKind::Feature,
            "Added X",
            Some("claude".into()),
            NOW - DAY,
        );
        a.date = TODAY - DAY;
        let b = Accomplishment::new(AccomplishmentKind::Perf, "Faster Y", None, NOW);
        let doc = render_document(&[a.clone(), b.clone()]);
        assert!(doc.ends_with('\n'), "an append always starts a fresh line");
        assert_eq!(parse_document(&doc), vec![a, b]);
    }

    #[test]
    fn a_summary_with_separators_survives_rendering() {
        let e = Accomplishment::new(AccomplishmentKind::Fix, "a | b\nc", None, NOW);
        assert_eq!(e.summary, "a / b c");
        assert_eq!(parse_line(&render_line(&e)).unwrap(), e);
    }

    #[test]
    fn retention_counts_today_as_day_one() {
        for days in [1u32, 14, 90] {
            let span = i64::from(days);
            let es = vec![entry("oldest kept", span - 1), entry("just expired", span)];
            let m = maintain(es, NOW, days);
            let kept: Vec<_> = m.kept.iter().map(|e| e.summary.as_str()).collect();
            assert_eq!(kept, ["oldest kept"], "{days}-day window");
            assert!(m.changed);
        }
    }

    #[test]
    fn retention_is_clamped() {
        assert_eq!(clamp_retention(0), 1);
        assert_eq!(clamp_retention(14), 14);
        assert_eq!(clamp_retention(1000), 90);
    }

    #[test]
    fn maintain_stamps_sorts_and_dedupes() {
        let mut undated = entry("new work", 0);
        undated.date = 0;
        let mut dup = entry("Fixed it", 2);
        dup.summary = "fixed IT".into();
        let es = vec![entry("Fixed it", 2), entry("older", 3), dup, undated];
        let m = maintain(es, NOW, 14);
        assert!(m.changed);
        let kept: Vec<_> = m
            .kept
            .iter()
            .map(|e| (e.summary.as_str(), (TODAY - e.date) / DAY))
            .collect();
        assert_eq!(kept, [("older", 3), ("Fixed it", 2), ("new work", 0)]);
    }

    #[test]
    fn a_clean_file_needs_no_rewrite() {
        let doc = render_document(&maintain(vec![entry("a", 3), entry("b", 1)], NOW, 14).kept);
        assert_eq!(plan_rewrite(&doc, NOW, 14), None);

        let appended = format!("{doc}- fix | appended without a date | claude\n");
        let rewritten = plan_rewrite(&appended, NOW, 14).expect("stamps the new line");
        assert!(rewritten.contains("- 2027-01-16 | fix | appended without a date | claude"));
        assert_eq!(
            plan_rewrite(&rewritten, NOW, 14),
            None,
            "stable once maintained"
        );
    }

    #[test]
    fn groups_newest_day_and_entry_first() {
        let es = vec![entry("d3", 3), entry("d1-a", 1), entry("d1-b", 1)];
        let groups = group_by_day(es, |e| e.date);
        let shape: Vec<(i64, Vec<&str>)> = groups
            .iter()
            .map(|(d, g)| {
                (
                    (TODAY - d) / DAY,
                    g.iter().map(|e| e.summary.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(shape, [(1, vec!["d1-b", "d1-a"]), (3, vec!["d3"])]);
    }

    #[test]
    fn counts_kinds_in_display_order() {
        let mut feat = entry("f", 0);
        feat.kind = AccomplishmentKind::Feature;
        let es = [entry("a", 0), feat, entry("b", 0)];
        assert_eq!(
            kind_counts(&es),
            [
                (AccomplishmentKind::Feature, 1),
                (AccomplishmentKind::Fix, 2)
            ]
        );
    }

    #[test]
    fn quick_add_takes_a_type_prefix() {
        assert_eq!(
            parse_user_input("fix: the thing"),
            Some((AccomplishmentKind::Fix, "the thing".into()))
        );
        assert_eq!(
            parse_user_input("Note: not a type"),
            Some((AccomplishmentKind::Other, "Note: not a type".into()))
        );
        assert_eq!(
            parse_user_input("plain words"),
            Some((AccomplishmentKind::Other, "plain words".into()))
        );
        assert_eq!(parse_user_input("docs:  "), None);
    }

    #[test]
    fn agent_label_is_a_safe_short_name() {
        assert_eq!(agent_label(Some("claude"), "Claude Code"), "claude");
        assert_eq!(agent_label(Some(r"C:\tools\Codex.cmd"), "Codex"), "codex");
        assert_eq!(agent_label(Some("/usr/bin/open'code"), ""), "opencode");
        assert_eq!(agent_label(None, "My Agent!"), "my-agent");
        assert_eq!(agent_label(None, "  "), "agent");
    }

    fn keyed(summary: &str, days_ago: i64, key: &str) -> Accomplishment {
        let mut e = entry(summary, days_ago);
        e.key = Some(key.into());
        e
    }

    #[test]
    fn parses_key_and_updated_fields() {
        let e = parse_line(
            "- 2027-01-10 | fix | Fixed it | claude | #Tmux_Restart race | updated 2027-01-16",
        )
        .unwrap();
        assert_eq!(e.agent.as_deref(), Some("claude"));
        assert_eq!(e.key.as_deref(), Some("tmux-restart-race"));
        assert_eq!(e.updated, Some(TODAY));
        assert_eq!(parse_line(&render_line(&e)).unwrap(), e, "round-trips");

        // A key without an agent, and a `#` inside the summary that isn't a key.
        let e = parse_line("- fix | Closed #123 for good | #issue-123").unwrap();
        assert_eq!(e.summary, "Closed #123 for good");
        assert_eq!((e.agent, e.key.as_deref()), (None, Some("issue-123")));

        assert_eq!(normalize_key("#"), None);
        assert_eq!(normalize_key("  #a  b "), Some("a-b".into()));
    }

    #[test]
    fn a_correction_replaces_the_original_and_keeps_its_date() {
        let mut original = keyed("Fixed the crash", 3, "startup-crash");
        original.agent = Some("claude".into());
        let mut fix = keyed(
            "Partly fixed the crash (Windows still affected)",
            0,
            "startup-crash",
        );
        fix.agent = Some("codex".into());
        fix.kind = AccomplishmentKind::Other;
        let m = maintain(vec![original, entry("unrelated", 2), fix], NOW, 14);
        assert!(m.changed);
        assert_eq!(m.kept.len(), 2, "folded into one entry");
        let e = &m.kept[0];
        assert_eq!(e.summary, "Partly fixed the crash (Windows still affected)");
        assert_eq!(e.kind, AccomplishmentKind::Other, "type corrected too");
        assert_eq!(
            e.date,
            TODAY - 3 * DAY,
            "still dated when the work was done"
        );
        assert_eq!(e.updated, Some(TODAY));
        assert_eq!(
            e.agent.as_deref(),
            Some("claude"),
            "keeps the original's attribution"
        );

        // Stable once folded.
        let doc = render_document(&m.kept);
        assert!(doc.contains("| #startup-crash | updated 2027-01-16"));
        assert_eq!(plan_rewrite(&doc, NOW, 14), None);
    }

    #[test]
    fn a_same_day_correction_just_replaces() {
        let m = maintain(
            vec![keyed("first take", 0, "k"), keyed("second take", 0, "k")],
            NOW,
            14,
        );
        assert_eq!(m.kept.len(), 1);
        assert_eq!(m.kept[0].summary, "second take");
        assert_eq!(m.kept[0].updated, None);
    }

    #[test]
    fn a_recent_correction_keeps_an_aging_entry() {
        // The original is past the window, but it was corrected today.
        let m = maintain(
            vec![
                keyed("old work", 20, "k"),
                keyed("old work, revised", 0, "k"),
            ],
            NOW,
            14,
        );
        assert_eq!(m.kept.len(), 1);
        assert_eq!(m.kept[0].summary, "old work, revised");

        // A correction whose original is already gone simply stands alone.
        let m = maintain(vec![keyed("revised", 0, "gone")], NOW, 14);
        assert_eq!((m.kept[0].date, m.kept[0].updated), (TODAY, None));
    }

    #[test]
    fn unkeyed_entries_get_unique_stable_keys() {
        let mut keyed_one = entry("Added dark mode", 2);
        keyed_one.key = Some("added-dark-mode".into());
        let m = maintain(
            vec![keyed_one, entry("Added dark mode", 1), entry("!!!", 0)],
            NOW,
            14,
        );
        let keys: Vec<_> = m.kept.iter().map(|e| e.key.as_deref().unwrap()).collect();
        assert_eq!(keys, ["added-dark-mode", "added-dark-mode-2", "entry"]);
        assert!(m.changed);
        let again = maintain(m.kept.clone(), NOW, 14);
        assert!(!again.changed, "keys stick once assigned");
    }
}
