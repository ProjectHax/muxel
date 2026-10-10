//! Team libraries: shared snippets, runners and loops read from a git
//! repository's `muxel-library.toml`.

pub mod config;
pub mod hub;
pub mod menu;
pub mod parse;
pub mod resolve;
pub mod resync;
pub mod sanitize;
pub mod state;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{LoopSchedule, PostRunAction};

// --- Fixed parameters of team libraries ---

/// File read from the root of each library clone.
pub const LIBRARY_FILE: &str = "muxel-library.toml";
/// Seconds between automatic update attempts of one library.
pub const PULL_INTERVAL_SECS: u64 = 300;
/// Time limit of a clone, and of a whole re-sync.
pub const GIT_TIMEOUT_CLONE_SECS: u64 = 120;
pub const GIT_TIMEOUT_PULL_SECS: u64 = 60;
/// Total time limit of the local-changes check before a re-sync.
pub const RESYNC_CHECK_TIMEOUT_SECS: u64 = GIT_TIMEOUT_PULL_SECS;
/// Largest fraction of the window height a drop-down list may take.
pub const MENU_LIST_MAX_FRACTION: f32 = 0.6;
/// Margin, in px, kept between a drop-down and the window edges.
pub const MENU_WINDOW_MARGIN: f32 = 8.0;

// --- Persisted types (stored in `Settings`, never in `workspace.json`) ---

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryConfig {
    /// Name of the clone directory, `LIB_DIR/<id>`.
    pub id: Uuid,
    /// Trimmed repository URL. Immutable once added.
    pub url: String,
    /// Trimmed branch; `""` = the remote's default branch.
    #[serde(default)]
    pub branch: String,
    /// Trimmed display name; `""` = derived from the URL.
    #[serde(default)]
    pub name: String,
    /// Unix seconds of the last successful clone / pull / re-sync.
    #[serde(default)]
    pub last_pull_ok: Option<u64>,
}

/// The watched fields of a shared runner: a change to any of them needs a
/// new confirmation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunnerContent {
    pub prompt: String,
    /// Sanitized literal preset name; `None` = the toolbar's agent at run time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub auto_mode_presses: u8,
}

/// The watched fields of a shared loop: a change to any of them switches the
/// loop off.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopContent {
    pub prompt: String,
    /// Sanitized literal preset name; `None` = the user pins an agent when
    /// turning the loop on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub auto_mode_presses: u8,
    pub schedule: LoopSchedule,
    pub post_run: PostRunAction,
}

/// A switched-on shared loop. It exists if and only if the loop is on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SharedLoopState {
    pub library: Uuid,
    pub name: String,
    /// Stable id used in `running_loops`, so runs do not stack.
    pub run_id: Uuid,
    pub project_id: Uuid,
    #[serde(default)]
    pub last_run: Option<u64>,
    pub approved: LoopContent,
    /// The local preset picked in the turn-on dialog of a loop without
    /// `preset`. Not part of the approved content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_preset_id: Option<Uuid>,
}

/// A shared runner the user confirmed, with the content confirmed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SharedRunnerConfirmation {
    pub library: Uuid,
    pub name: String,
    pub confirmed: RunnerContent,
}

// --- Domain types (not persisted) ---

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LibKind {
    Snippet,
    Runner,
    Loop,
}

/// Identity of a library item: library, table and sanitized name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LibItemKey {
    pub library: Uuid,
    pub kind: LibKind,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibSnippet {
    pub name: String,
    pub text: String,
    pub submit: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LibRunner {
    pub name: String,
    pub content: RunnerContent,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LibLoop {
    pub name: String,
    pub content: LoopContent,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedLibrary {
    pub snippets: Vec<LibSnippet>,
    pub runners: Vec<LibRunner>,
    pub loops: Vec<LibLoop>,
    pub discarded: usize,
    /// The first discarded item, in report order (see [`parse::parse_library`]).
    pub first_discard: Option<ItemIssue>,
    pub warnings: usize,
    pub first_warning: Option<LibWarning>,
}

/// Why one item (or a whole table key) was discarded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemIssue {
    pub table: LibKind,
    /// 1-based position in its array; `0` for [`IssueKind::TableNotArray`].
    pub position: usize,
    /// The item's name, when it has a usable one; `None` for `TableNotArray`.
    pub name: Option<String>,
    pub problem: IssueKind,
}

/// The problem that discarded an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueKind {
    /// The whole key is not an array of tables (any non-table element included).
    TableNotArray,
    MissingName,
    WrongType {
        field: String,
        expected: &'static str,
    },
    OutOfRange {
        field: String,
    },
    UnknownScheduleKind(String),
    UnknownPostRun(String),
    DuplicateName,
}

/// A non-fatal finding while reading a library file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibWarning {
    UnknownField {
        table: LibKind,
        position: usize,
        field: String,
    },
    UnknownTable(String),
}

/// Why the library file could not be read at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileError {
    Missing,
    NotUtf8,
    Unreadable(String),
    Syntax { line: Option<usize>, detail: String },
}

/// Why a git operation on a library failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitFailure {
    /// The `git` executable could not be found.
    NotFound,
    /// The operation exceeded its time limit.
    TimedOut { secs: u64 },
    /// git ran and failed.
    Failed { detail: String },
    /// A filesystem operation around git failed.
    Io { detail: String },
    /// The library's clone folder exists but is not a git clone (no `.git`):
    /// not pulled, and not deleted to clone it again.
    NotAClone,
    /// A re-sync could neither move the new clone into place nor move the
    /// previous clone back; the previous clone is left as `leftover`.
    ResyncRestoreFailed {
        replace: String,
        restore: String,
        leftover: String,
    },
}

/// How an item's `preset` resolves against the local presets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetResolution {
    /// The file names no preset. What that means is up to the caller: a
    /// runner uses the toolbar's selection, a loop the agent pinned for it.
    Unnamed,
    Preset(Uuid),
    NotFound(String),
}

/// Why a switched-on shared loop's agent does not resolve when it is about
/// to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopOffReason {
    PresetNotFound(String),
    /// No `preset`, and the pinned preset is no longer in `Settings.presets`.
    PinnedPresetDeleted,
    /// No `preset` and no pinned preset (an older saved state).
    NoAgentPinned,
}

#[cfg(test)]
mod tests {
    use super::{LibraryConfig, LoopContent, SharedLoopState};
    use crate::{LoopSchedule, PostRunAction, Settings};
    use uuid::Uuid;

    /// A `config.toml` as written before team libraries existed.
    const OLD_SETTINGS_TOML: &str = r#"
default_use_tmux = false
theme = "One Dark"
font_size = 15.0

[[snippets]]
name = "Continue"
text = "continue"
submit = true

[[runners]]
name = "Review"
prompt = "Review {{input}}"
"#;

    #[test]
    fn old_settings_without_library_fields_load_with_empty_vectors() {
        let s: Settings = toml::from_str(OLD_SETTINGS_TOML).expect("old config parses");
        assert!(s.libraries.is_empty());
        assert!(s.shared_loops.is_empty());
        assert!(s.shared_runner_confirmations.is_empty());
        assert!(!s.default_use_tmux);
        assert_eq!(s.theme, "One Dark");
        assert_eq!(s.snippets.len(), 1);
        assert_eq!(s.runners.len(), 1);
    }

    #[test]
    fn settings_default_has_no_libraries() {
        let s = Settings::default();
        assert!(s.libraries.is_empty());
        assert!(s.shared_loops.is_empty());
        assert!(s.shared_runner_confirmations.is_empty());
    }

    #[test]
    fn library_config_toml_roundtrip() {
        let cfg = LibraryConfig {
            id: Uuid::from_u128(0x1234),
            url: "https://github.com/acme/team-lib.git".to_string(),
            branch: "team".to_string(),
            name: "Acme".to_string(),
            last_pull_ok: Some(1_700_000_000),
        };
        let text = toml::to_string(&cfg).expect("serialize");
        let back: LibraryConfig = toml::from_str(&text).expect("deserialize");
        assert_eq!(back, cfg);
    }

    #[test]
    fn library_config_optional_fields_default() {
        let text = "id = \"00000000-0000-0000-0000-000000001234\"\nurl = \"file:///r\"\n";
        let cfg: LibraryConfig = toml::from_str(text).expect("deserialize");
        assert_eq!(cfg.url, "file:///r");
        assert_eq!(cfg.branch, "");
        assert_eq!(cfg.name, "");
        assert_eq!(cfg.last_pull_ok, None);
    }

    fn shared_loop(preset: Option<&str>) -> SharedLoopState {
        SharedLoopState {
            library: Uuid::from_u128(1),
            name: "Nightly".to_string(),
            run_id: Uuid::from_u128(2),
            project_id: Uuid::from_u128(3),
            last_run: Some(1000),
            approved: LoopContent {
                prompt: "go".to_string(),
                preset: preset.map(str::to_string),
                auto_mode_presses: 2,
                schedule: LoopSchedule::DailyAt {
                    hour: 9,
                    minute: 30,
                },
                post_run: PostRunAction::Exit,
            },
            pinned_preset_id: None,
        }
    }

    #[test]
    fn pinned_preset_round_trips_and_old_state_loads_without_it() {
        let c = Uuid::from_u128(0xC1);
        let mut pinned = shared_loop(None);
        pinned.pinned_preset_id = Some(c);
        let settings = Settings {
            shared_loops: vec![pinned.clone()],
            ..Default::default()
        };
        let text = toml::to_string(&settings).expect("serialize");
        assert!(
            text.contains("pinned_preset_id = \"00000000-0000-0000-0000-0000000000c1\""),
            "{text}"
        );
        let back: Settings = toml::from_str(&text).expect("reload");
        assert_eq!(back.shared_loops, vec![pinned]);
        assert_eq!(back.shared_loops[0].pinned_preset_id, Some(c));

        let unpinned = shared_loop(None);
        let text = toml::to_string(&unpinned).expect("serialize");
        assert!(!text.contains("pinned_preset_id"), "{text}");
        let back: SharedLoopState = toml::from_str(&text).expect("old state loads");
        assert_eq!(back.pinned_preset_id, None);
        assert_eq!(back, unpinned);
    }

    #[test]
    fn shared_loop_preset_none_and_empty_roundtrip_distinctly() {
        let absent = shared_loop(None);
        let empty = shared_loop(Some(""));

        let absent_text = toml::to_string(&absent).expect("serialize None");
        let empty_text = toml::to_string(&empty).expect("serialize Some(\"\")");
        // `None` is omitted; `Some("")` is written as an explicit empty string.
        assert!(!absent_text.contains("preset"), "{absent_text}");
        assert!(empty_text.contains("preset = \"\""), "{empty_text}");

        let absent_back: SharedLoopState = toml::from_str(&absent_text).expect("reload None");
        let empty_back: SharedLoopState = toml::from_str(&empty_text).expect("reload Some");
        assert_eq!(absent_back.approved.preset, None);
        assert_eq!(empty_back.approved.preset, Some(String::new()));
        assert_eq!(absent_back, absent);
        assert_eq!(empty_back, empty);
        assert_ne!(absent_back, empty_back);
    }
}
