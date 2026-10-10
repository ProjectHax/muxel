//! Parser for `muxel-library.toml`.

use std::collections::HashSet;

use toml::{Table, Value};

use super::sanitize::sanitize;
use super::{
    FileError, IssueKind, ItemIssue, LibKind, LibLoop, LibRunner, LibSnippet, LibWarning,
    LoopContent, ParsedLibrary, RunnerContent,
};
use crate::{LoopSchedule, PostRunAction};

/// The three item tables, in the order their issues are reported.
const ITEM_TABLES: [(&str, LibKind); 3] = [
    ("snippets", LibKind::Snippet),
    ("runners", LibKind::Runner),
    ("loops", LibKind::Loop),
];

/// Fields accepted with any type and without effect.
const IGNORED_FIELDS: [&str; 5] = ["id", "preset_id", "project_id", "enabled", "last_run"];

fn schema_fields(kind: LibKind) -> &'static [&'static str] {
    match kind {
        LibKind::Snippet => &["name", "text", "submit"],
        LibKind::Runner => &["name", "preset", "auto_mode_presses", "prompt"],
        LibKind::Loop => &[
            "name",
            "preset",
            "auto_mode_presses",
            "prompt",
            "schedule",
            "post_run",
        ],
    }
}

/// Reads the text of a library file into its items, discards and warnings.
///
/// - A top-level item table that is not an array of tables is one
///   [`IssueKind::TableNotArray`] discard and loads nothing of that kind.
/// - Text fields are sanitized before identity and duplicate detection.
/// - Among items sharing a name, the first valid one loads; an invalid item
///   does not take the name.
/// - Unknown item fields and top-level keys are warnings.
///
/// Issues are reported in a stable order: tables in `ITEM_TABLES` order, each
/// in file order, then unknown top-level keys, sorted.
pub fn parse_library(text: &str) -> Result<ParsedLibrary, FileError> {
    let root: Table = text
        .parse()
        .map_err(|e: toml::de::Error| syntax_error(text, &e))?;

    let mut out = ParsedLibrary::default();
    let mut discards: Vec<ItemIssue> = Vec::new();
    let mut warnings: Vec<LibWarning> = Vec::new();

    for (key, kind) in ITEM_TABLES {
        let Some(value) = root.get(key) else {
            continue;
        };
        let Some(items) = array_of_tables(value) else {
            discards.push(ItemIssue {
                table: kind,
                position: 0,
                name: None,
                problem: IssueKind::TableNotArray,
            });
            continue;
        };
        let mut loaded: HashSet<String> = HashSet::new();
        for (index, item) in items.into_iter().enumerate() {
            let position = index + 1;
            warnings.extend(unknown_fields(kind, item).into_iter().map(|field| {
                LibWarning::UnknownField {
                    table: kind,
                    position,
                    field,
                }
            }));
            match parse_item(kind, item) {
                Err((name, problem)) => discards.push(ItemIssue {
                    table: kind,
                    position,
                    name,
                    problem,
                }),
                Ok(parsed) => {
                    let name = parsed.name().to_string();
                    if loaded.insert(name.clone()) {
                        match parsed {
                            Parsed::Snippet(s) => out.snippets.push(s),
                            Parsed::Runner(r) => out.runners.push(r),
                            Parsed::Loop(l) => out.loops.push(l),
                        }
                    } else {
                        discards.push(ItemIssue {
                            table: kind,
                            position,
                            name: Some(name),
                            problem: IssueKind::DuplicateName,
                        });
                    }
                }
            }
        }
    }

    let mut unknown_tables: Vec<&String> = root
        .keys()
        .filter(|k| !ITEM_TABLES.iter().any(|(known, _)| known == k))
        .collect();
    unknown_tables.sort();
    warnings.extend(
        unknown_tables
            .into_iter()
            .map(|k| LibWarning::UnknownTable(k.clone())),
    );

    out.discarded = discards.len();
    out.first_discard = discards.into_iter().next();
    out.warnings = warnings.len();
    out.first_warning = warnings.into_iter().next();
    Ok(out)
}

fn syntax_error(text: &str, e: &toml::de::Error) -> FileError {
    let line = e.span().map(|span| {
        let before = text.get(..span.start).unwrap_or(text);
        1 + before.matches('\n').count()
    });
    FileError::Syntax {
        line,
        detail: e.message().to_string(),
    }
}

/// `Some` only for an array whose elements are all tables (including `[]`).
fn array_of_tables(value: &Value) -> Option<Vec<&Table>> {
    let Value::Array(items) = value else {
        return None;
    };
    items.iter().map(Value::as_table).collect()
}

/// Unknown fields of one item (`schedule.<key>` inside a schedule), sorted.
fn unknown_fields(kind: LibKind, item: &Table) -> Vec<String> {
    let known = schema_fields(kind);
    let mut fields: Vec<String> = item
        .keys()
        .filter(|k| !known.contains(&k.as_str()) && !IGNORED_FIELDS.contains(&k.as_str()))
        .cloned()
        .collect();
    if kind == LibKind::Loop
        && let Some(schedule) = item.get("schedule").and_then(Value::as_table)
        && let Some(kind_name) = schedule.get("kind").and_then(Value::as_str)
        && let Some(allowed) = schedule_fields(kind_name)
    {
        fields.extend(
            schedule
                .keys()
                .filter(|k| k.as_str() != "kind" && !allowed.contains(&k.as_str()))
                .map(|k| format!("schedule.{k}")),
        );
    }
    fields.sort();
    fields
}

/// The value fields of each `schedule.kind`; `None` for an unknown kind.
fn schedule_fields(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "every_minutes" => Some(&["minutes"]),
        "every_hours" => Some(&["hours"]),
        "daily_at" => Some(&["hour", "minute"]),
        _ => None,
    }
}

enum Parsed {
    Snippet(LibSnippet),
    Runner(LibRunner),
    Loop(LibLoop),
}

impl Parsed {
    fn name(&self) -> &str {
        match self {
            Parsed::Snippet(s) => &s.name,
            Parsed::Runner(r) => &r.name,
            Parsed::Loop(l) => &l.name,
        }
    }
}

/// Validates one item, `name` first. The error carries the name if any.
fn parse_item(kind: LibKind, item: &Table) -> Result<Parsed, (Option<String>, IssueKind)> {
    let name = match item.get("name") {
        None => return Err((None, IssueKind::MissingName)),
        Some(Value::String(raw)) => sanitize(raw),
        Some(_) => return Err((None, wrong_type("name", "string"))),
    };
    if name.is_empty() {
        return Err((None, IssueKind::MissingName));
    }
    let fail = |problem: IssueKind| (Some(name.clone()), problem);

    match kind {
        LibKind::Snippet => {
            let text = opt_string(item, "text").map_err(fail)?.unwrap_or_default();
            let submit = opt_bool(item, "submit").map_err(fail)?.unwrap_or(false);
            Ok(Parsed::Snippet(LibSnippet { name, text, submit }))
        }
        LibKind::Runner => {
            let preset = opt_string(item, "preset").map_err(fail)?;
            let auto_mode_presses = presses(item).map_err(fail)?;
            let prompt = opt_string(item, "prompt")
                .map_err(fail)?
                .unwrap_or_default();
            Ok(Parsed::Runner(LibRunner {
                name,
                content: RunnerContent {
                    prompt,
                    preset,
                    auto_mode_presses,
                },
            }))
        }
        LibKind::Loop => {
            let preset = opt_string(item, "preset").map_err(fail)?;
            let auto_mode_presses = presses(item).map_err(fail)?;
            let prompt = opt_string(item, "prompt")
                .map_err(fail)?
                .unwrap_or_default();
            let schedule = schedule(item.get("schedule")).map_err(fail)?;
            let post_run = post_run(item.get("post_run")).map_err(fail)?;
            Ok(Parsed::Loop(LibLoop {
                name,
                content: LoopContent {
                    prompt,
                    preset,
                    auto_mode_presses,
                    schedule,
                    post_run,
                },
            }))
        }
    }
}

fn wrong_type(field: &str, expected: &'static str) -> IssueKind {
    IssueKind::WrongType {
        field: field.to_string(),
        expected,
    }
}

/// An optional string field, sanitized; empty stays `Some("")`.
fn opt_string(item: &Table, field: &str) -> Result<Option<String>, IssueKind> {
    match item.get(field) {
        None => Ok(None),
        Some(Value::String(raw)) => Ok(Some(sanitize(raw))),
        Some(_) => Err(wrong_type(field, "string")),
    }
}

fn opt_bool(item: &Table, field: &str) -> Result<Option<bool>, IssueKind> {
    match item.get(field) {
        None => Ok(None),
        Some(Value::Boolean(b)) => Ok(Some(*b)),
        Some(_) => Err(wrong_type(field, "bool")),
    }
}

/// An integer in `min..=max`; out of range is an error, never clamped.
fn int_in_range(value: &Value, label: &str, min: i64, max: i64) -> Result<i64, IssueKind> {
    match value {
        Value::Integer(n) if (min..=max).contains(n) => Ok(*n),
        Value::Integer(_) => Err(IssueKind::OutOfRange {
            field: label.to_string(),
        }),
        _ => Err(wrong_type(label, "integer")),
    }
}

fn presses(item: &Table) -> Result<u8, IssueKind> {
    match item.get("auto_mode_presses") {
        None => Ok(0),
        Some(v) => int_in_range(v, "auto_mode_presses", 0, 9).map(|n| n as u8),
    }
}

/// A required integer field of a schedule table; absent counts as a wrong type.
fn schedule_int(schedule: &Table, field: &str, min: i64, max: i64) -> Result<i64, IssueKind> {
    let label = format!("schedule.{field}");
    match schedule.get(field) {
        None => Err(wrong_type(&label, "integer")),
        Some(v) => int_in_range(v, &label, min, max),
    }
}

fn schedule(value: Option<&Value>) -> Result<LoopSchedule, IssueKind> {
    let Some(value) = value else {
        return Ok(LoopSchedule::EveryHours { hours: 1 });
    };
    let Some(table) = value.as_table() else {
        return Err(wrong_type("schedule", "table"));
    };
    let Some(kind) = table.get("kind").and_then(Value::as_str) else {
        return Err(wrong_type("schedule.kind", "string"));
    };
    let max_u32 = i64::from(u32::MAX);
    match kind {
        "every_minutes" => Ok(LoopSchedule::EveryMinutes {
            minutes: schedule_int(table, "minutes", 1, max_u32)? as u32,
        }),
        "every_hours" => Ok(LoopSchedule::EveryHours {
            hours: schedule_int(table, "hours", 1, max_u32)? as u32,
        }),
        "daily_at" => Ok(LoopSchedule::DailyAt {
            hour: schedule_int(table, "hour", 0, 23)? as u8,
            minute: schedule_int(table, "minute", 0, 59)? as u8,
        }),
        other => Err(IssueKind::UnknownScheduleKind(other.to_string())),
    }
}

fn post_run(value: Option<&Value>) -> Result<PostRunAction, IssueKind> {
    match value {
        None => Ok(PostRunAction::Leave),
        Some(Value::String(s)) => match s.as_str() {
            "leave" => Ok(PostRunAction::Leave),
            "exit" => Ok(PostRunAction::Exit),
            other => Err(IssueKind::UnknownPostRun(other.to_string())),
        },
        Some(_) => Err(wrong_type("post_run", "string")),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_library;
    use crate::library::{
        FileError, IssueKind, ItemIssue, LibKind, LibWarning, LoopContent, ParsedLibrary,
        RunnerContent,
    };
    use crate::{LoopSchedule, PostRunAction};

    fn parse_ok(text: &str) -> ParsedLibrary {
        parse_library(text).expect("file parses")
    }

    fn snippet_names(p: &ParsedLibrary) -> Vec<&str> {
        p.snippets.iter().map(|s| s.name.as_str()).collect()
    }

    fn runner_names(p: &ParsedLibrary) -> Vec<&str> {
        p.runners.iter().map(|r| r.name.as_str()).collect()
    }

    fn loop_names(p: &ParsedLibrary) -> Vec<&str> {
        p.loops.iter().map(|l| l.name.as_str()).collect()
    }

    const REFERENCE_FILE: &str = r#"
[[snippets]]
name = "Plan first"
text = "Before you start, outline your plan and wait for my confirmation."
submit = false

[[runners]]
name = "Team review"
preset = "Claude"
auto_mode_presses = 3
prompt = "Review the current changes against docs/STYLE.md.\n\n{{input}}"

[[loops]]
name = "Nightly deps check"
preset = "Claude"
prompt = "Check for outdated dependencies and summarize."
schedule = { kind = "daily_at", hour = 2, minute = 30 }
post_run = "exit"
"#;

    #[test]
    fn reference_file_loads_exact_values() {
        let p = parse_ok(REFERENCE_FILE);
        assert_eq!(p.snippets.len(), 1);
        assert_eq!(p.runners.len(), 1);
        assert_eq!(p.loops.len(), 1);
        assert_eq!(p.snippets[0].name, "Plan first");
        assert_eq!(
            p.snippets[0].text,
            "Before you start, outline your plan and wait for my confirmation."
        );
        assert!(!p.snippets[0].submit);
        assert_eq!(p.runners[0].name, "Team review");
        assert_eq!(
            p.runners[0].content,
            RunnerContent {
                prompt: "Review the current changes against docs/STYLE.md.\n\n{{input}}"
                    .to_string(),
                preset: Some("Claude".to_string()),
                auto_mode_presses: 3,
            }
        );
        assert_eq!(p.loops[0].name, "Nightly deps check");
        assert_eq!(
            p.loops[0].content,
            LoopContent {
                prompt: "Check for outdated dependencies and summarize.".to_string(),
                preset: Some("Claude".to_string()),
                auto_mode_presses: 0,
                schedule: LoopSchedule::DailyAt {
                    hour: 2,
                    minute: 30
                },
                post_run: PostRunAction::Exit,
            }
        );
        assert_eq!(p.discarded, 0);
        assert_eq!(p.first_discard, None);
        assert_eq!(p.warnings, 0);
        assert_eq!(p.first_warning, None);
    }

    #[test]
    fn defaults_when_optional_fields_absent() {
        let p = parse_ok(
            "[[snippets]]\nname = \"S\"\n[[runners]]\nname = \"R\"\n[[loops]]\nname = \"L\"\n",
        );
        assert_eq!(p.snippets[0].text, "");
        assert!(!p.snippets[0].submit);
        assert_eq!(
            p.runners[0].content,
            RunnerContent {
                prompt: String::new(),
                preset: None,
                auto_mode_presses: 0,
            }
        );
        assert_eq!(
            p.loops[0].content,
            LoopContent {
                prompt: String::new(),
                preset: None,
                auto_mode_presses: 0,
                schedule: LoopSchedule::EveryHours { hours: 1 },
                post_run: PostRunAction::Leave,
            }
        );
        assert_eq!(p.discarded, 0);
        assert_eq!(p.warnings, 0);
    }

    #[test]
    fn every_schedule_kind_and_post_run_leave() {
        let p = parse_ok(
            r#"
[[loops]]
name = "M"
schedule = { kind = "every_minutes", minutes = 5 }
post_run = "leave"
[[loops]]
name = "H"
schedule = { kind = "every_hours", hours = 4294967295 }
[[loops]]
name = "D"
schedule = { kind = "daily_at", hour = 23, minute = 59 }
auto_mode_presses = 9
"#,
        );
        assert_eq!(p.discarded, 0);
        assert_eq!(
            p.loops[0].content.schedule,
            LoopSchedule::EveryMinutes { minutes: 5 }
        );
        assert_eq!(p.loops[0].content.post_run, PostRunAction::Leave);
        assert_eq!(
            p.loops[1].content.schedule,
            LoopSchedule::EveryHours {
                hours: 4_294_967_295
            }
        );
        assert_eq!(
            p.loops[2].content.schedule,
            LoopSchedule::DailyAt {
                hour: 23,
                minute: 59
            }
        );
        assert_eq!(p.loops[2].content.auto_mode_presses, 9);
    }

    #[test]
    fn out_of_range_is_discarded_never_clamped() {
        let p = parse_ok(
            r#"
[[loops]]
name = "Bad"
schedule = { kind = "every_minutes", minutes = 0 }
[[loops]]
prompt = "no name"
[[loops]]
name = "Good"
schedule = { kind = "every_minutes", minutes = 7 }
"#,
        );
        assert_eq!(loop_names(&p), vec!["Good"]);
        assert_eq!(p.discarded, 2);
        assert_eq!(
            p.first_discard,
            Some(ItemIssue {
                table: LibKind::Loop,
                position: 1,
                name: Some("Bad".to_string()),
                problem: IssueKind::OutOfRange {
                    field: "schedule.minutes".to_string()
                },
            })
        );
        assert!(
            p.loops
                .iter()
                .all(|l| l.content.schedule != LoopSchedule::EveryMinutes { minutes: 1 })
        );
    }

    #[test]
    fn missing_name_is_reported_with_its_position() {
        let p = parse_ok("[[loops]]\nprompt = \"x\"\n");
        assert_eq!(p.discarded, 1);
        assert_eq!(
            p.first_discard,
            Some(ItemIssue {
                table: LibKind::Loop,
                position: 1,
                name: None,
                problem: IssueKind::MissingName,
            })
        );
    }

    #[test]
    fn auto_mode_presses_10_is_discarded() {
        let p = parse_ok(
            "[[runners]]\nname = \"R\"\nauto_mode_presses = 10\n[[runners]]\nname = \"N\"\nauto_mode_presses = -1\n",
        );
        assert!(p.runners.is_empty());
        assert_eq!(p.discarded, 2);
        assert_eq!(
            p.first_discard.unwrap().problem,
            IssueKind::OutOfRange {
                field: "auto_mode_presses".to_string()
            }
        );
    }

    #[test]
    fn minutes_above_u32_is_discarded() {
        let p = parse_ok(
            "[[loops]]\nname = \"L\"\nschedule = { kind = \"every_minutes\", minutes = 4294967296 }\n",
        );
        assert!(p.loops.is_empty());
        assert_eq!(p.discarded, 1);
        assert_eq!(
            p.first_discard.unwrap().problem,
            IssueKind::OutOfRange {
                field: "schedule.minutes".to_string()
            }
        );
    }

    #[test]
    fn daily_at_hour_and_minute_bounds() {
        let p = parse_ok(
            r#"
[[loops]]
name = "H"
schedule = { kind = "daily_at", hour = 24, minute = 0 }
[[loops]]
name = "M"
schedule = { kind = "daily_at", hour = 0, minute = 60 }
[[loops]]
name = "Z"
schedule = { kind = "every_hours", hours = 0 }
"#,
        );
        assert!(p.loops.is_empty());
        assert_eq!(p.discarded, 3);
        assert_eq!(
            p.first_discard.unwrap().problem,
            IssueKind::OutOfRange {
                field: "schedule.hour".to_string()
            }
        );
    }

    #[test]
    fn wrong_types_unknown_kind_and_post_run() {
        let p = parse_ok(
            r#"
[[snippets]]
name = 5
[[snippets]]
name = "T"
text = 1
[[runners]]
name = "P"
preset = 1
[[loops]]
name = "K"
schedule = { kind = "weekly" }
[[loops]]
name = "R"
post_run = "close"
[[loops]]
name = "S"
schedule = "daily"
"#,
        );
        assert!(p.snippets.is_empty());
        assert!(p.runners.is_empty());
        assert!(p.loops.is_empty());
        assert_eq!(p.discarded, 6);
        assert_eq!(
            p.first_discard,
            Some(ItemIssue {
                table: LibKind::Snippet,
                position: 1,
                name: None,
                problem: IssueKind::WrongType {
                    field: "name".to_string(),
                    expected: "string"
                },
            })
        );
        let one = |t: &str| parse_ok(t).first_discard.unwrap().problem;
        assert_eq!(
            one("[[snippets]]\nname = \"T\"\ntext = 1\n"),
            IssueKind::WrongType {
                field: "text".to_string(),
                expected: "string"
            }
        );
        assert_eq!(
            one("[[loops]]\nname = \"K\"\nschedule = { kind = \"weekly\" }\n"),
            IssueKind::UnknownScheduleKind("weekly".to_string())
        );
        assert_eq!(
            one("[[loops]]\nname = \"R\"\npost_run = \"close\"\n"),
            IssueKind::UnknownPostRun("close".to_string())
        );
        assert_eq!(
            one("[[loops]]\nname = \"S\"\nschedule = \"daily\"\n"),
            IssueKind::WrongType {
                field: "schedule".to_string(),
                expected: "table"
            }
        );
    }

    #[test]
    fn syntax_error_reports_line() {
        assert!(matches!(
            parse_library("[[snippets"),
            Err(FileError::Syntax { line: Some(1), .. })
        ));
        assert!(matches!(
            parse_library("[[snippets]]\nname=\"a\"\n[[snippets"),
            Err(FileError::Syntax { line: Some(3), .. })
        ));
    }

    #[test]
    fn ignored_fields_are_accepted_with_any_type_and_not_warned() {
        let p = parse_ok(
            r#"
[[loops]]
name = "L"
enabled = true
project_id = "00000000-0000-0000-0000-000000000001"
last_run = 0
id = 3
preset_id = [1]
schedule = { kind = "every_minutes", minutes = 1 }
"#,
        );
        assert_eq!(loop_names(&p), vec!["L"]);
        assert_eq!(
            p.loops[0].content.schedule,
            LoopSchedule::EveryMinutes { minutes: 1 }
        );
        assert_eq!(p.discarded, 0);
        assert_eq!(p.warnings, 0);
    }

    #[test]
    fn duplicate_name_keeps_first() {
        let p = parse_ok(
            "[[snippets]]\nname = \"Yes\"\ntext = \"a\"\n[[snippets]]\nname = \"Yes\"\ntext = \"b\"\n",
        );
        assert_eq!(snippet_names(&p), vec!["Yes"]);
        assert_eq!(p.snippets[0].text, "a");
        assert_eq!(p.discarded, 1);
        assert_eq!(
            p.first_discard,
            Some(ItemIssue {
                table: LibKind::Snippet,
                position: 2,
                name: Some("Yes".to_string()),
                problem: IssueKind::DuplicateName,
            })
        );
    }

    #[test]
    fn invalid_item_does_not_occupy_the_name() {
        let p = parse_ok(
            r#"
[[snippets]]
name = "X"
text = "1"
submit = "yes"
[[snippets]]
name = "X"
text = "2"
[[snippets]]
name = "X"
text = "3"
[[loops]]
name = "L"
prompt = "a"
schedule = { kind = "every_minutes", minutes = 0 }
[[loops]]
name = "L"
prompt = "b"
"#,
        );
        assert_eq!(snippet_names(&p), vec!["X"]);
        assert_eq!(p.snippets[0].text, "2");
        assert_eq!(loop_names(&p), vec!["L"]);
        assert_eq!(p.loops[0].content.prompt, "b");
        assert_eq!(p.discarded, 3);
        assert_eq!(
            p.first_discard,
            Some(ItemIssue {
                table: LibKind::Snippet,
                position: 1,
                name: Some("X".to_string()),
                problem: IssueKind::WrongType {
                    field: "submit".to_string(),
                    expected: "bool"
                },
            })
        );
    }

    #[test]
    fn same_name_in_different_tables_is_not_a_duplicate() {
        let p = parse_ok(
            "[[snippets]]\nname = \"A\"\n[[runners]]\nname = \"A\"\n[[loops]]\nname = \"A\"\n",
        );
        assert_eq!(p.snippets.len() + p.runners.len() + p.loops.len(), 3);
        assert_eq!(p.discarded, 0);
    }

    const DIRTY_AC070: &str = r#"a\u001b[31mb\u0007c\r\nd\te\rf\u0085g\u009bh\u007f"#;
    const DIRTY_AC090: &str =
        r#"x\U000E0041y\u202Ez\U000E0100w\u200Dv\uFE0Fu\u200Ct\u200Bs\u2066r\uFEFFq"#;

    fn three_items_with(text: &str) -> String {
        format!(
            "[[snippets]]\nname = \"S\"\ntext = \"{text}\"\n[[runners]]\nname = \"R\"\nprompt = \"{text}\"\n[[loops]]\nname = \"L\"\nprompt = \"{text}\"\n"
        )
    }

    #[test]
    fn text_and_prompts_are_sanitized() {
        let p = parse_ok(&three_items_with(DIRTY_AC070));
        assert_eq!(p.discarded, 0);
        assert_eq!(p.snippets[0].text, "a[31mbc\nd\tefgh");
        assert_eq!(p.runners[0].content.prompt, "a[31mbc\nd\tefgh");
        assert_eq!(p.loops[0].content.prompt, "a[31mbc\nd\tefgh");
    }

    #[test]
    fn format_characters_are_sanitized() {
        let p = parse_ok(&three_items_with(DIRTY_AC090));
        assert_eq!(p.discarded, 0);
        let clean = "xyzw\u{200D}v\u{FE0F}u\u{200C}tsrq";
        assert_eq!(p.snippets[0].text, clean);
        assert_eq!(p.runners[0].content.prompt, clean);
        assert_eq!(p.loops[0].content.prompt, clean);
    }

    #[test]
    fn names_and_presets_sanitized_before_identity() {
        let p = parse_ok(
            r#"
[[snippets]]
name = "Go\u202E"
text = "1"
[[snippets]]
name = "Go"
text = "2"
[[snippets]]
name = "\u2066\u2069"
text = "3"
[[runners]]
name = "A"
preset = "Cla\u200Bude"
[[runners]]
name = "B"
preset = "\u202E"
[[loops]]
name = "C\u200B"
preset = "Claude\u200B"
"#,
        );
        assert_eq!(snippet_names(&p), vec!["Go"]);
        assert_eq!(p.snippets[0].text, "1");
        assert_eq!(p.discarded, 2);
        assert_eq!(
            p.first_discard,
            Some(ItemIssue {
                table: LibKind::Snippet,
                position: 2,
                name: Some("Go".to_string()),
                problem: IssueKind::DuplicateName,
            })
        );
        assert_eq!(runner_names(&p), vec!["A", "B"]);
        assert_eq!(p.runners[0].content.preset, Some("Claude".to_string()));
        assert_eq!(p.runners[1].content.preset, Some(String::new()));
        assert_eq!(loop_names(&p), vec!["C"]);
        assert_eq!(p.loops[0].content.preset, Some("Claude".to_string()));
    }

    #[test]
    fn name_empty_after_sanitize_is_missing_name() {
        let p = parse_ok("[[runners]]\nname = \"\\u2066\\u2069\"\n[[runners]]\nname = \"\"\n");
        assert!(p.runners.is_empty());
        assert_eq!(p.discarded, 2);
        assert_eq!(
            p.first_discard,
            Some(ItemIssue {
                table: LibKind::Runner,
                position: 1,
                name: None,
                problem: IssueKind::MissingName,
            })
        );
    }

    #[test]
    fn warnings_follow_first_order() {
        let p = parse_ok(
            r#"
[[runners]]
name = "R1"
promt = "x"
[[macros]]
name = "M"
[[snippets]]
name = "S"
color = "red"
[[runners]]
name = "R2"
colour = "blue"
"#,
        );
        assert_eq!(runner_names(&p), vec!["R1", "R2"]);
        assert_eq!(p.runners[0].content.prompt, "");
        assert_eq!(snippet_names(&p), vec!["S"]);
        assert_eq!(p.discarded, 0);
        assert_eq!(p.warnings, 4);
        assert_eq!(
            p.first_warning,
            Some(LibWarning::UnknownField {
                table: LibKind::Snippet,
                position: 1,
                field: "color".to_string(),
            })
        );

        let p = parse_ok(
            "[[runners]]\nname = \"R1\"\npromt = \"x\"\n[[runners]]\nname = \"R2\"\ncolour = \"blue\"\n",
        );
        assert_eq!(p.warnings, 2);
        assert_eq!(
            p.first_warning,
            Some(LibWarning::UnknownField {
                table: LibKind::Runner,
                position: 1,
                field: "promt".to_string(),
            })
        );
    }

    #[test]
    fn unknown_fields_alphabetical_within_item_and_tables_last() {
        let p = parse_ok("zeta = 1\n[alpha]\nx = 1\n[[runners]]\nname = \"R\"\nzz = 1\nbb = 2\n");
        assert_eq!(p.warnings, 4);
        assert_eq!(
            p.first_warning,
            Some(LibWarning::UnknownField {
                table: LibKind::Runner,
                position: 1,
                field: "bb".to_string(),
            })
        );
        let p = parse_ok("zeta = 1\n[alpha]\nx = 1\n");
        assert_eq!(p.warnings, 2);
        assert_eq!(
            p.first_warning,
            Some(LibWarning::UnknownTable("alpha".to_string()))
        );
    }

    #[test]
    fn unknown_field_inside_schedule_is_warned() {
        let p = parse_ok(
            "[[loops]]\nname = \"L\"\nschedule = { kind = \"every_minutes\", minutes = 5, foo = 1 }\n",
        );
        assert_eq!(loop_names(&p), vec!["L"]);
        assert_eq!(
            p.loops[0].content.schedule,
            LoopSchedule::EveryMinutes { minutes: 5 }
        );
        assert_eq!(p.discarded, 0);
        assert_eq!(p.warnings, 1);
        assert_eq!(
            p.first_warning,
            Some(LibWarning::UnknownField {
                table: LibKind::Loop,
                position: 1,
                field: "schedule.foo".to_string(),
            })
        );
    }

    fn table_not_array(table: LibKind) -> Option<ItemIssue> {
        Some(ItemIssue {
            table,
            position: 0,
            name: None,
            problem: IssueKind::TableNotArray,
        })
    }

    #[test]
    fn string_key_discards_one_and_loads_the_rest() {
        let p =
            parse_ok("snippets = \"x\"\n[[runners]]\nname = \"R1\"\n[[loops]]\nname = \"L1\"\n");
        assert!(p.snippets.is_empty());
        assert_eq!(runner_names(&p), vec!["R1"]);
        assert_eq!(loop_names(&p), vec!["L1"]);
        assert_eq!(p.discarded, 1);
        assert_eq!(p.first_discard, table_not_array(LibKind::Snippet));
        assert_eq!(p.warnings, 0);
    }

    #[test]
    fn one_discard_per_key_not_per_element() {
        let p = parse_ok("runners = 3\nloops = [1, 2]\n[[snippets]]\nname = \"S\"\n");
        assert_eq!(snippet_names(&p), vec!["S"]);
        assert!(p.runners.is_empty());
        assert!(p.loops.is_empty());
        assert_eq!(p.discarded, 2);
        assert_eq!(p.first_discard, table_not_array(LibKind::Runner));
        assert_eq!(p.warnings, 0);
    }

    #[test]
    fn empty_array_is_valid() {
        let p = parse_ok("snippets = []\n[[runners]]\nname = \"R1\"\n");
        assert!(p.snippets.is_empty());
        assert_eq!(runner_names(&p), vec!["R1"]);
        assert_eq!(p.discarded, 0);
        assert_eq!(p.first_discard, None);
        assert_eq!(p.warnings, 0);
    }

    #[test]
    fn mixed_array_loads_nothing_and_does_not_warn() {
        let p = parse_ok("snippets = [{ name = \"a\", foo = 1 }, 1]\n");
        assert!(p.snippets.is_empty());
        assert_eq!(p.discarded, 1);
        assert_eq!(p.first_discard, table_not_array(LibKind::Snippet));
        assert_eq!(p.warnings, 0);
        assert_eq!(p.first_warning, None);
    }

    #[test]
    fn plain_table_key_is_not_an_array_and_takes_its_table_place() {
        // The runner is reported first: `runners` precedes `loops`.
        let p = parse_ok("[loops]\nname = \"L\"\n[[runners]]\nauto_mode_presses = 1\n");
        assert_eq!(p.discarded, 2);
        assert_eq!(
            p.first_discard,
            Some(ItemIssue {
                table: LibKind::Runner,
                position: 1,
                name: None,
                problem: IssueKind::MissingName,
            })
        );
        assert_eq!(p.warnings, 0);
    }
}
