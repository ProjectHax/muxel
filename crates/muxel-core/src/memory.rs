//! Pure model + maintenance for muxel's per-project **memory**: `.muxel/MEMORY.md`,
//! a single greppable markdown file of timestamped facts an agent (or the user)
//! accumulates about a project.
//!
//! Each fact is one `##` section carrying a machine-readable meta line (id, created,
//! accessed, pinned, tags). muxel keeps the file **LRU-ordered** (most-recently-
//! accessed first, pinned on top), **auto-purged** by age, and **capped** in count,
//! with pinned entries exempt from both. Keeping everything in one file (rather than
//! an index + per-entry files) means a remote project syncs with a single SSH file
//! read/write, and `grep -i <term> MEMORY.md` finds an entry directly.
//!
//! This module is pure — no I/O. `muxel`/`muxel-store` read and write the file; here
//! we only parse a document, render one, and run maintenance. Callers pass `now`
//! (unix seconds) so it stays deterministic and unit-testable.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Un-pinned entries not accessed within this many days are purged on maintenance.
pub const PURGE_AFTER_DAYS: i64 = 30;
/// Max un-pinned entries kept; the least-recently-accessed excess is evicted.
pub const MAX_ENTRIES: usize = 40;
const DAY_SECS: i64 = 86_400;

/// One remembered fact — a `##` section in `MEMORY.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: Uuid,
    /// Short human title (the `##` heading).
    pub title: String,
    /// Markdown body. Its first non-empty line doubles as the list summary.
    #[serde(default)]
    pub content: String,
    /// Keywords for grep: synonyms, filenames, error strings, tools.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Pinned entries are exempt from purge and the count cap, and sort first.
    #[serde(default)]
    pub pinned: bool,
    /// Unix seconds when first created (rendered as `YYYY-MM-DD`).
    pub created: i64,
    /// Unix seconds of last access/update — drives LRU ordering and purge.
    pub accessed: i64,
}

impl MemoryEntry {
    /// A fresh entry created/accessed at `now`.
    pub fn new(
        title: impl Into<String>,
        content: impl Into<String>,
        tags: Vec<String>,
        now: i64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            content: content.into(),
            tags: clean_tags(tags),
            pinned: false,
            created: now,
            accessed: now,
        }
    }

    /// Mark the entry used now (bumps it up the LRU order, resets its purge clock).
    pub fn touch(&mut self, now: i64) {
        self.accessed = now.max(self.accessed);
    }

    /// First non-empty line of the content, for the list view.
    pub fn summary(&self) -> &str {
        self.content
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
    }

    /// Render this entry as a `MEMORY.md` section.
    fn render_section(&self) -> String {
        let pin = if self.pinned { "📌 " } else { "" };
        format!(
            "## {pin}{title}\n<!-- muxel: id={id}; created={created}; accessed={accessed}; \
             pinned={pinned}; tags={tags} -->\n\n{content}\n",
            title = self.title,
            id = self.id,
            created = fmt_date(self.created),
            accessed = fmt_date(self.accessed),
            pinned = self.pinned,
            tags = self.tags.join(", "),
            content = self.content.trim(),
        )
    }
}

fn clean_tags(tags: Vec<String>) -> Vec<String> {
    tags.into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Parse a `MEMORY.md` document into its entries. Tolerant of hand edits: a section
/// missing its meta line still parses (with a fresh id and zeroed dates, which
/// [`maintain`] then treats as "now"). Any prose *before* the first `##` section
/// that isn't just the boilerplate header is captured as a single "Imported notes"
/// entry, so upgrading a legacy flat `MEMORY.md` never loses content.
pub fn parse_document(text: &str) -> Vec<MemoryEntry> {
    let mut entries = Vec::new();

    // Everything up to the first "## " heading is the preamble (header + any legacy
    // freeform notes). Split on lines that start a section.
    let mut preamble = String::new();
    let mut sections: Vec<String> = Vec::new();
    let mut cur: Option<String> = None;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("## ") {
            if let Some(prev) = cur.take() {
                sections.push(prev);
            }
            cur = Some(line.to_string());
        } else if let Some(c) = cur.as_mut() {
            c.push_str(line);
        } else {
            preamble.push_str(line);
        }
    }
    if let Some(prev) = cur.take() {
        sections.push(prev);
    }

    if let Some(legacy) = legacy_notes(&preamble) {
        entries.push(MemoryEntry {
            id: Uuid::new_v4(),
            title: "Imported notes".to_string(),
            content: legacy,
            tags: Vec::new(),
            pinned: false,
            created: 0,
            accessed: 0,
        });
    }

    for sec in sections {
        if let Some(e) = parse_section(&sec) {
            entries.push(e);
        }
    }
    entries
}

/// Non-boilerplate leftover prose from a document preamble, or `None` if there's
/// nothing worth keeping (fresh/seeded file).
fn legacy_notes(preamble: &str) -> Option<String> {
    let mut kept = String::new();
    let mut in_comment = false;
    for line in preamble.lines() {
        let t = line.trim();
        if in_comment {
            if t.contains("-->") {
                in_comment = false;
            }
            continue;
        }
        if t.starts_with("<!--") {
            if !t.contains("-->") {
                in_comment = true;
            }
            continue;
        }
        // Drop the top-level title and known seed boilerplate.
        if t.starts_with("# ") || t.is_empty() {
            continue;
        }
        if t.starts_with("Shared notes for agents")
            || t.starts_with("Append durable lessons")
            || t == "_No memories yet._"
        {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    let kept = kept.trim().to_string();
    if kept.is_empty() { None } else { Some(kept) }
}

fn parse_section(sec: &str) -> Option<MemoryEntry> {
    let mut lines = sec.lines();
    let heading = lines.next()?; // "## [📌 ]Title"
    let title = heading
        .trim_start()
        .strip_prefix("## ")?
        .trim()
        .strip_prefix("📌")
        .map(str::trim)
        .unwrap_or_else(|| heading.trim_start().strip_prefix("## ").unwrap().trim())
        .to_string();
    if title.is_empty() {
        return None;
    }

    // Defaults for a hand-written section with no meta line.
    let mut id = Uuid::new_v4();
    let mut created = 0i64;
    let mut accessed = 0i64;
    let mut pinned = heading.contains("📌");
    let mut tags = Vec::new();

    let mut body = String::new();
    for line in lines {
        let t = line.trim();
        if let Some(meta) = t
            .strip_prefix("<!-- muxel:")
            .and_then(|m| m.strip_suffix("-->"))
        {
            for field in meta.split(';') {
                let Some((k, v)) = field.split_once('=') else {
                    continue;
                };
                let v = v.trim();
                match k.trim() {
                    "id" => {
                        if let Ok(u) = Uuid::parse_str(v) {
                            id = u;
                        }
                    }
                    "created" => created = parse_date(v),
                    "accessed" => accessed = parse_date(v),
                    "pinned" => pinned = v == "true",
                    "tags" => tags = clean_tags(v.split(',').map(str::to_string).collect()),
                    _ => {}
                }
            }
            continue;
        }
        body.push_str(line);
        body.push('\n');
    }

    Some(MemoryEntry {
        id,
        title,
        content: body.trim().to_string(),
        tags,
        pinned,
        created,
        accessed,
    })
}

/// Result of [`maintain`]: the entries that stay (in display order) and the count
/// dropped (purged + capped), so the caller can report what was pruned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Maintenance {
    pub kept: Vec<MemoryEntry>,
    pub removed: usize,
}

/// Order (LRU, pinned first), purge stale un-pinned entries, then cap the count.
/// `now` is unix seconds. Entries with a non-positive `accessed` (a hand-authored
/// or freshly-imported section) are treated as accessed `now` so they aren't purged
/// immediately.
pub fn maintain(mut entries: Vec<MemoryEntry>, now: i64) -> Maintenance {
    for e in &mut entries {
        if e.accessed <= 0 {
            e.accessed = now;
        }
        if e.created <= 0 {
            e.created = e.accessed;
        }
    }
    order(&mut entries);

    let cutoff = now - PURGE_AFTER_DAYS * DAY_SECS;
    let mut kept = Vec::with_capacity(entries.len());
    let mut removed = 0usize;
    let mut unpinned_kept = 0usize;
    for e in entries {
        if !e.pinned && e.accessed < cutoff {
            removed += 1; // purged: stale + un-pinned
            continue;
        }
        if !e.pinned {
            if unpinned_kept >= MAX_ENTRIES {
                removed += 1; // capped: least-recently-accessed excess
                continue;
            }
            unpinned_kept += 1;
        }
        kept.push(e);
    }
    Maintenance { kept, removed }
}

/// Sort in place: pinned first, then most-recently-accessed, with created date and
/// title as stable tie-breakers.
pub fn order(entries: &mut [MemoryEntry]) {
    entries.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(b.accessed.cmp(&a.accessed))
            .then(b.created.cmp(&a.created))
            .then(a.title.cmp(&b.title))
            .then(a.id.cmp(&b.id))
    });
}

/// Render the full `MEMORY.md` document from the (already maintained) entries.
/// Call [`order`]/[`maintain`] first so it's in display order.
pub fn render_document(entries: &[MemoryEntry]) -> String {
    let mut s = String::from(document_header());
    if entries.is_empty() {
        s.push_str("\n_No memories yet._\n");
        return s;
    }
    for e in entries {
        s.push('\n');
        s.push_str(&e.render_section());
    }
    s
}

/// The heading + policy/how-to-grep comment at the top of `MEMORY.md`. muxel keeps
/// the file maintained automatically; the comment documents the format so the file
/// is self-explanatory to an agent or human reading it.
pub fn document_header() -> &'static str {
    "# Project memory\n\n\
<!-- muxel-maintained memory for this project. Most-relevant-first: recently-used
     entries rise to the top; 📌 pinned ones stay first. Each entry is a `##` section
     with a meta line (id, dates, tags), so `grep -i <term> MEMORY.md` finds it —
     grep the file, then read that section. To remember something, add a `## Title`
     section with a short note (a `- tags:` hint helps grep); muxel stamps, orders,
     and dedupes it on the next run. Un-pinned entries unused for 30 days are purged
     and at most 40 are kept, so don't hand-maintain ordering or delete others. -->\n"
}

/// kebab-case a title (used for stable slugs / anchors if a caller needs one).
pub fn slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        "memory".to_string()
    } else {
        s
    }
}

/// Format unix seconds as `YYYY-MM-DD` (UTC). Pure — Howard Hinnant's days↔civil.
pub fn fmt_date(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(DAY_SECS));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parse a `YYYY-MM-DD` date to unix seconds (UTC midnight), or 0 if malformed.
fn parse_date(s: &str) -> i64 {
    // Also accept a bare unix-seconds integer (forward/back compatible).
    if let Ok(secs) = s.parse::<i64>() {
        return secs;
    }
    let mut it = s.splitn(3, '-');
    let y: i64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let m: i64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let d: i64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return 0;
    }
    days_from_civil(y, m as u32, d as u32) * DAY_SECS
}

/// Convert a day number (days since 1970-01-01) to `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d)
}

/// Convert `(year, month, day)` to a day number (days since 1970-01-01).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_ENTRIES, MemoryEntry, fmt_date, maintain, order, parse_date, parse_document,
        render_document, slug,
    };

    const DAY: i64 = 86_400;
    const NOW: i64 = 1_800_057_600; // fixed, day-aligned "now" (20834 * 86400)

    fn entry(title: &str, accessed_days_ago: i64) -> MemoryEntry {
        let mut e = MemoryEntry::new(title, format!("summary of {title}"), vec!["t".into()], NOW);
        e.accessed = NOW - accessed_days_ago * DAY;
        e.created = e.accessed;
        e
    }

    #[test]
    fn slug_is_kebab_and_safe() {
        assert_eq!(slug("macOS Build Setup!"), "macos-build-setup");
        assert_eq!(slug("  a  b  "), "a-b");
        assert_eq!(slug("///"), "memory");
    }

    #[test]
    fn date_round_trips() {
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_date(1_719_705_600), "2024-06-30");
        assert_eq!(parse_date("2024-06-30"), 1_719_705_600);
        assert_eq!(parse_date("1970-01-01"), 0);
        // A bare integer is accepted too.
        assert_eq!(parse_date("1719705600"), 1_719_705_600);
        assert_eq!(parse_date("not-a-date"), 0);
    }

    #[test]
    fn order_puts_pinned_then_recent_first() {
        let mut es = vec![entry("old", 10), entry("new", 1), entry("mid", 5)];
        es[0].pinned = true; // "old" pinned
        order(&mut es);
        assert_eq!(es[0].title, "old", "pinned first");
        assert_eq!(es[1].title, "new", "then most-recent");
        assert_eq!(es[2].title, "mid");
    }

    #[test]
    fn maintain_purges_stale_unpinned_but_keeps_pinned() {
        let mut stale_pinned = entry("keep-me", 100);
        stale_pinned.pinned = true;
        let es = vec![entry("fresh", 2), entry("stale", 45), stale_pinned];
        let m = maintain(es, NOW);
        let kept: Vec<_> = m.kept.iter().map(|e| e.title.as_str()).collect();
        assert!(kept.contains(&"fresh"));
        assert!(kept.contains(&"keep-me"), "pinned survives past cutoff");
        assert!(!kept.contains(&"stale"));
        assert_eq!(m.removed, 1);
    }

    #[test]
    fn maintain_caps_unpinned_count_evicting_least_recent() {
        // MAX_ENTRIES + 3 un-pinned entries, all fresh (hours apart) so the age
        // purge doesn't fire and only the count cap trims them.
        let mut es: Vec<_> = (0..(MAX_ENTRIES as i64 + 3))
            .map(|i| {
                let mut e = entry(&format!("e{i:03}"), 0);
                e.accessed = NOW - i * 3600;
                e
            })
            .collect();
        let mut p = entry("pinned-old", 90);
        p.pinned = true;
        es.push(p);
        let m = maintain(es, NOW);
        let unpinned_kept = m.kept.iter().filter(|e| !e.pinned).count();
        assert_eq!(
            unpinned_kept, MAX_ENTRIES,
            "capped to MAX_ENTRIES un-pinned"
        );
        assert!(
            m.kept.iter().any(|e| e.pinned),
            "pinned kept regardless of cap"
        );
        assert_eq!(m.removed, 3);
    }

    #[test]
    fn maintain_rescues_missing_timestamps() {
        let mut e = entry("legacy", 0);
        e.accessed = 0;
        e.created = 0;
        let m = maintain(vec![e], NOW);
        assert_eq!(
            m.kept.len(),
            1,
            "missing accessed treated as now, not purged"
        );
        assert_eq!(m.kept[0].accessed, NOW);
    }

    #[test]
    fn document_round_trips() {
        let mut a = MemoryEntry::new("Alpha", "first line\nmore body", vec!["ssh".into()], NOW);
        a.pinned = true;
        let b = MemoryEntry::new(
            "Beta",
            "beta body",
            vec!["tmux".into(), "git".into()],
            NOW - DAY,
        );
        let m = maintain(vec![a.clone(), b.clone()], NOW);
        let doc = render_document(&m.kept);
        let parsed = parse_document(&doc);
        assert_eq!(parsed.len(), 2);
        // Pinned Alpha renders first; round-trip preserves fields.
        assert_eq!(parsed[0], a);
        assert_eq!(parsed[1], b);
    }

    #[test]
    fn document_is_greppable_and_ordered() {
        let mut a = entry("Alpha thing", 1);
        a.tags = vec!["metal".into(), "build".into()];
        let mut b = entry("Beta thing", 3);
        b.pinned = true;
        b.tags = vec!["tmux".into()];
        let m = maintain(vec![a, b], NOW);
        let doc = render_document(&m.kept);
        assert!(doc.find("Beta thing").unwrap() < doc.find("Alpha thing").unwrap());
        assert!(doc.contains("📌"));
        assert!(doc.contains("tags=metal, build"));
    }

    #[test]
    fn legacy_flat_file_is_imported_not_lost() {
        let legacy = "# Project memory\n\nShared notes for agents working on this project. \
                      Append durable lessons, decisions, and gotchas below.\n\n\
                      - Always run the metal toolchain download first.\n\
                      - The staging DB creds are in vault.\n";
        let entries = parse_document(legacy);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Imported notes");
        assert!(entries[0].content.contains("metal toolchain"));
        assert!(entries[0].content.contains("staging DB"));
    }

    #[test]
    fn seeded_empty_file_has_no_entries() {
        let seeded = render_document(&[]);
        assert!(
            parse_document(&seeded).is_empty(),
            "boilerplate isn't imported"
        );
    }

    #[test]
    fn section_without_meta_line_still_parses() {
        let doc = "# Project memory\n\n## Hand written\nsome note a human typed\n";
        let e = parse_document(doc);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].title, "Hand written");
        assert_eq!(e[0].content, "some note a human typed");
        assert_eq!(e[0].accessed, 0, "no date yet; maintain() will stamp it");
    }
}
