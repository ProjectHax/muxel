//! Turn a unified `git diff` into side-by-side (split) rows for a GitHub-style
//! two-column diff view. Pure + unit-tested; the GUI just renders the rows.

/// One row of a side-by-side diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SplitRow {
    /// A `@@ … @@` hunk header (spans both columns).
    Hunk(String),
    /// A content row. Each side is `(line_number, text)` or `None` (blank
    /// padding). `changed` marks a removed/added row (color the present cells);
    /// unchanged context rows have it `false`.
    Line {
        left: Option<(u32, String)>,
        right: Option<(u32, String)>,
        changed: bool,
    },
}

/// Parse a unified diff (the output of `git diff … -- <file>`) into split rows.
/// File headers (`diff --git`, `index`, `---`, `+++`) are skipped; within a hunk,
/// runs of removed/added lines are paired row-by-row (padding the shorter side).
pub fn split_diff(unified: &str) -> Vec<SplitRow> {
    let mut rows = Vec::new();
    let mut old_no = 0u32;
    let mut new_no = 0u32;
    let mut in_hunk = false;
    let mut removed: Vec<(u32, String)> = Vec::new();
    let mut added: Vec<(u32, String)> = Vec::new();

    fn flush(
        removed: &mut Vec<(u32, String)>,
        added: &mut Vec<(u32, String)>,
        rows: &mut Vec<SplitRow>,
    ) {
        let n = removed.len().max(added.len());
        for i in 0..n {
            rows.push(SplitRow::Line {
                left: removed.get(i).cloned(),
                right: added.get(i).cloned(),
                changed: true,
            });
        }
        removed.clear();
        added.clear();
    }

    for line in unified.lines() {
        if line.starts_with("@@") {
            flush(&mut removed, &mut added, &mut rows);
            if let Some((o, n)) = parse_hunk_starts(line) {
                old_no = o;
                new_no = n;
            }
            in_hunk = true;
            rows.push(SplitRow::Hunk(line.to_string()));
            continue;
        }
        if !in_hunk {
            continue; // file headers before the first hunk
        }
        // The first byte is the change tag (' ', '-', '+'), always ASCII.
        match line.as_bytes().first() {
            Some(b' ') => {
                flush(&mut removed, &mut added, &mut rows);
                let text = line[1..].to_string();
                rows.push(SplitRow::Line {
                    left: Some((old_no, text.clone())),
                    right: Some((new_no, text)),
                    changed: false,
                });
                old_no += 1;
                new_no += 1;
            }
            Some(b'-') => {
                removed.push((old_no, line[1..].to_string()));
                old_no += 1;
            }
            Some(b'+') => {
                added.push((new_no, line[1..].to_string()));
                new_no += 1;
            }
            // "\ No newline at end of file" and blank/other lines: ignore.
            _ => {}
        }
    }
    flush(&mut removed, &mut added, &mut rows);
    rows
}

/// Parse `@@ -oldStart,oldCount +newStart,newCount @@ …` → `(oldStart, newStart)`.
fn parse_hunk_starts(line: &str) -> Option<(u32, u32)> {
    let mut old = None;
    let mut new = None;
    for tok in line.split_whitespace() {
        if let Some(s) = tok.strip_prefix('-') {
            old = s.split(',').next().and_then(|n| n.parse().ok());
        } else if let Some(s) = tok.strip_prefix('+') {
            new = s.split(',').next().and_then(|n| n.parse().ok());
        }
    }
    Some((old?, new?))
}

#[cfg(test)]
mod tests {
    use super::{SplitRow, split_diff};

    fn line(l: Option<(u32, &str)>, r: Option<(u32, &str)>, changed: bool) -> SplitRow {
        SplitRow::Line {
            left: l.map(|(n, t)| (n, t.to_string())),
            right: r.map(|(n, t)| (n, t.to_string())),
            changed,
        }
    }

    #[test]
    fn pairs_replacements_and_keeps_context() {
        let unified = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n\
                       @@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let rows = split_diff(unified);
        assert!(matches!(&rows[0], SplitRow::Hunk(h) if h.starts_with("@@")));
        assert_eq!(rows[1], line(Some((1, "one")), Some((1, "one")), false));
        assert_eq!(rows[2], line(Some((2, "two")), Some((2, "TWO")), true));
        assert_eq!(rows[3], line(Some((3, "three")), Some((3, "three")), false));
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn pure_add_and_delete_pad_the_other_side() {
        // One line removed, two added.
        let unified = "--- a/f\n+++ b/f\n@@ -1,2 +1,3 @@\n ctx\n-gone\n+new1\n+new2\n";
        let rows = split_diff(unified);
        assert_eq!(rows[1], line(Some((1, "ctx")), Some((1, "ctx")), false));
        // Removed paired with first add; second add has a blank left side.
        assert_eq!(rows[2], line(Some((2, "gone")), Some((2, "new1")), true));
        assert_eq!(rows[3], line(None, Some((3, "new2")), true));
    }

    #[test]
    fn non_diff_text_yields_no_rows() {
        assert!(split_diff("# f\n\nNo diff available.\n").is_empty());
    }
}
