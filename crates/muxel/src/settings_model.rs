//! Pure decision logic for the Settings window: search matching, how
//! keybindings are named, grouped, folded and checked for conflicts, and the
//! colour math that keeps the window readable in every theme. No gpui here, so
//! `#[test]` stays the built-in one.

use std::collections::HashMap;

// ===== Search =====

/// The lower-cased words of a search query.
pub fn query_words(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|word| word.to_lowercase())
        .collect()
}

/// Whether every query word appears somewhere in `haystacks` (case-insensitive).
/// An empty query matches nothing: there is nothing to look for.
pub fn matches(words: &[String], haystacks: &[&str]) -> bool {
    if words.is_empty() {
        return false;
    }
    let text = haystacks.join("\n").to_lowercase();
    words.iter().all(|word| text.contains(word.as_str()))
}

/// The byte ranges of `text` that a query word matched, sorted and without
/// overlaps, for highlighting. Compared case-insensitively, a character at a
/// time, so the ranges always fall on character boundaries of `text` itself.
pub fn highlight_ranges(text: &str, words: &[String]) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for word in words.iter().filter(|word| !word.is_empty()) {
        for (start, _) in text.char_indices() {
            let mut lowered = String::new();
            let mut end = start;
            for c in text[start..].chars() {
                if lowered.len() >= word.len() {
                    break;
                }
                lowered.extend(c.to_lowercase());
                end += c.len_utf8();
            }
            if lowered == *word {
                ranges.push(start..end);
            }
        }
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

// ===== Keybindings =====

/// One row of Settings → Keybindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRow {
    /// Heading the row sits under.
    pub group: &'static str,
    /// The action name, or for a range the prefix its nine actions share
    /// (`JumpToTab` covers `JumpToTab1` to `JumpToTab9`).
    pub id: &'static str,
    /// What the row is called (English; translated where it is shown).
    pub label: &'static str,
    /// Nine actions bound to the same modifiers plus a digit, shown as one row.
    pub range: bool,
}

/// Marks a string for the translation catalog (the extractor looks for
/// `t("…")`). It is translated where it is shown, not here.
const fn t(text: &'static str) -> &'static str {
    text
}

const fn key(group: &'static str, id: &'static str, label: &'static str) -> KeyRow {
    KeyRow {
        group,
        id,
        label,
        range: false,
    }
}

const fn range(group: &'static str, id: &'static str, label: &'static str) -> KeyRow {
    KeyRow {
        group,
        id,
        label,
        range: true,
    }
}

/// Every rebindable action, in the order Settings shows them.
pub const KEY_ROWS: &[KeyRow] = &[
    key(t("Panes and tabs"), "NewPane", t("New pane")),
    key(t("Panes and tabs"), "NewTab", t("New tab")),
    key(t("Panes and tabs"), "TabNext", t("Next tab")),
    key(t("Panes and tabs"), "TabPrev", t("Previous tab")),
    key(t("Panes and tabs"), "SplitRight", t("Split right")),
    key(t("Panes and tabs"), "SplitDown", t("Split down")),
    key(t("Panes and tabs"), "ClosePane", t("Close pane")),
    key(t("Panes and tabs"), "UndoClose", t("Reopen closed pane")),
    key(t("Focus"), "FocusNext", t("Next pane")),
    key(t("Focus"), "FocusPrev", t("Previous pane")),
    key(t("Focus"), "FocusLeft", t("Pane to the left")),
    key(t("Focus"), "FocusRight", t("Pane to the right")),
    key(t("Focus"), "FocusUp", t("Pane above")),
    key(t("Focus"), "FocusDown", t("Pane below")),
    key(t("Focus"), "FocusAttention", t("Pane that needs attention")),
    range(t("Jump by number"), "JumpToTab", t("Tab 1 to 9")),
    range(t("Jump by number"), "JumpToProject", t("Project 1 to 9")),
    range(
        t("Jump by number"),
        "NewAgent",
        t("New agent from preset 1 to 9"),
    ),
    key(t("Search"), "GlobalSearch", t("Command palette")),
    key(t("Search"), "FindInProject", t("Find in project")),
    key(t("Search"), "SearchTerminal", t("Search the terminal")),
    key(t("Search"), "ClearTerminal", t("Clear the terminal")),
    key(t("View"), "ZoomIn", t("Zoom in")),
    key(t("View"), "ZoomOut", t("Zoom out")),
    key(t("View"), "ToggleSidebar", t("Show or hide the sidebar")),
    key(t("View"), "ToggleDashboard", t("Dashboard")),
    key(t("View"), "ToggleSettings", t("Settings")),
    key(t("View"), "ShowKeys", t("Keyboard shortcuts")),
    key(t("View"), "ToggleFullScreen", t("Full screen")),
    key(t("View"), "NewWindow", t("New window")),
    key(t("View"), "ToggleDevConsole", t("Developer console")),
    key(
        t("Agents"),
        "ToggleBroadcast",
        t("Broadcast typing to every pane"),
    ),
    key(
        t("Agents"),
        "ToggleWorktree",
        t("Give new agents a git worktree"),
    ),
    key(
        t("Voice"),
        "ToggleSpeechToText",
        t("Start or stop dictation"),
    ),
    key(t("Voice"), "HoldSpeechToText", t("Dictate while held")),
    key(t("Voice"), "ReadAloud", t("Read the last reply aloud")),
    key(
        t("Voice"),
        "ReadAloudRestart",
        t("Read aloud from the start"),
    ),
    key(t("Voice"), "ReadAloudStop", t("Stop reading")),
    key(t("Editor"), "SaveFile", t("Save file")),
    key(t("Editor"), "SaveFileAs", t("Save file as")),
];

/// The actions a row stands for: itself, or `<id>1` to `<id>9` for a range.
pub fn row_actions(row: &KeyRow) -> Vec<String> {
    if row.range {
        (1..=9).map(|n| format!("{}{n}", row.id)).collect()
    } else {
        vec![row.id.to_string()]
    }
}

/// The row an action is shown on, with its digit when the row is a range.
pub fn row_for_action(action: &str) -> Option<(&'static KeyRow, Option<u8>)> {
    KEY_ROWS.iter().find_map(|row| {
        if !row.range {
            return (row.id == action).then_some((row, None));
        }
        let digit = action.strip_prefix(row.id)?.parse::<u8>().ok()?;
        (1..=9).contains(&digit).then_some((row, Some(digit)))
    })
}

/// What an action is called, e.g. `JumpToTab3` → "Tab 3". Falls back to the
/// action name for one with no row.
pub fn action_label(action: &str) -> String {
    match row_for_action(action) {
        Some((row, None)) => row.label.to_string(),
        Some((row, Some(digit))) => match row.id {
            "JumpToTab" => format!("Tab {digit}"),
            "JumpToProject" => format!("Project {digit}"),
            _ => format!("New agent from preset {digit}"),
        },
        None => action.to_string(),
    }
}

const MODIFIERS: &[&str] = &[
    "ctrl",
    "control",
    "alt",
    "shift",
    "cmd",
    "super",
    "win",
    "platform",
    "secondary",
    "fn",
    "function",
];

/// Split one chord into its modifiers and its key. The key may itself be `-`
/// (`ctrl--`), so modifiers are peeled off the front rather than split on `-`.
fn split_chord(chord: &str) -> (Vec<&str>, &str) {
    let mut rest = chord;
    let mut mods = Vec::new();
    while let Some((head, tail)) = rest.split_once('-') {
        if tail.is_empty() || !MODIFIERS.contains(&head.to_ascii_lowercase().as_str()) {
            break;
        }
        mods.push(head);
        rest = tail;
    }
    (mods, rest)
}

/// A keystroke in one canonical spelling, so `Shift-Ctrl-T` and `ctrl-shift-t`
/// compare equal.
pub fn normalize_keystroke(keystroke: &str) -> String {
    keystroke
        .split_whitespace()
        .map(|chord| {
            let (mods, key) = split_chord(chord);
            let mut mods: Vec<String> = mods
                .into_iter()
                .map(|m| match m.to_ascii_lowercase().as_str() {
                    "control" => "ctrl".to_string(),
                    "super" | "win" | "platform" => "cmd".to_string(),
                    "function" => "fn".to_string(),
                    other => other.to_string(),
                })
                .collect();
            // The order people say them in, so a derived keystroke reads
            // naturally when it is shown (Ctrl+Alt, not Alt+Ctrl).
            let rank = |m: &String| {
                ["ctrl", "alt", "shift", "cmd", "secondary", "fn"]
                    .iter()
                    .position(|known| known == m)
                    .unwrap_or(usize::MAX)
            };
            mods.sort_by_key(rank);
            mods.dedup();
            mods.push(key.to_lowercase());
            mods.join("-")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn modifier_cap(modifier: &str) -> &'static str {
    match modifier.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => "Ctrl",
        "alt" => {
            if cfg!(target_os = "macos") {
                "Option"
            } else {
                "Alt"
            }
        }
        "shift" => "Shift",
        "fn" | "function" => "Fn",
        "secondary" => {
            if cfg!(target_os = "macos") {
                "Cmd"
            } else {
                "Ctrl"
            }
        }
        _ => {
            if cfg!(target_os = "macos") {
                "Cmd"
            } else if cfg!(target_os = "windows") {
                "Win"
            } else {
                "Super"
            }
        }
    }
}

fn key_cap(key: &str) -> String {
    match key.to_lowercase().as_str() {
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "escape" => "Esc".to_string(),
        "delete" => "Del".to_string(),
        "pageup" => "PgUp".to_string(),
        "pagedown" => "PgDn".to_string(),
        "space" => "Space".to_string(),
        lower => {
            let mut chars = lower.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// The key caps a keystroke is drawn as: `ctrl-shift-t` → `Ctrl`, `Shift`, `T`.
/// A sequence of chords is joined by a `then` cap.
pub fn key_caps(keystroke: &str) -> Vec<String> {
    let mut caps = Vec::new();
    for (ix, chord) in keystroke.split_whitespace().enumerate() {
        if ix > 0 {
            caps.push("then".to_string());
        }
        let (mods, key) = split_chord(chord);
        caps.extend(mods.into_iter().map(|m| modifier_cap(m).to_string()));
        caps.push(key_cap(key));
    }
    caps
}

/// `alt-3` → (`alt`, 3): the modifiers and digit of a chord that ends in 1–9.
pub fn digit_chord(keystroke: &str) -> Option<(String, u8)> {
    let keystroke = keystroke.trim();
    if keystroke.contains(' ') {
        return None;
    }
    let (mods, key) = split_chord(keystroke);
    let digit = key.parse::<u8>().ok()?;
    (key.len() == 1 && (1..=9).contains(&digit)).then(|| (mods.join("-"), digit))
}

/// The modifiers nine keystrokes share when the Nth is `<modifiers>-N`; `None`
/// when they don't follow that pattern, so the row can't be shown folded.
pub fn range_modifiers(keystrokes: &[String]) -> Option<String> {
    if keystrokes.len() != 9 {
        return None;
    }
    let mut shared: Option<String> = None;
    for (ix, keystroke) in keystrokes.iter().enumerate() {
        let (mods, digit) = digit_chord(keystroke)?;
        if usize::from(digit) != ix + 1 {
            return None;
        }
        let mods = normalize_keystroke(&format!("{mods}-x"));
        match &shared {
            Some(first) if *first != mods => return None,
            Some(_) => {}
            None => shared = Some(mods),
        }
    }
    // Drop the placeholder key again; a bare digit has no modifiers.
    shared.map(|mods| mods.strip_suffix("-x").unwrap_or("").to_string())
}

/// `alt` + 3 → `alt-3`; bare digits when there are no modifiers.
pub fn range_keystroke(modifiers: &str, digit: u8) -> String {
    if modifiers.is_empty() {
        digit.to_string()
    } else {
        format!("{modifiers}-{digit}")
    }
}

/// A key that is only a modifier: pressing it alone can't be a shortcut.
pub fn is_modifier_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "shift"
            | "control"
            | "ctrl"
            | "alt"
            | "platform"
            | "cmd"
            | "super"
            | "win"
            | "function"
            | "fn"
    )
}

/// An action as it is bound: its keystroke and the key context it applies in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub action: String,
    pub keystroke: String,
    pub context: Option<String>,
}

/// Whether two bindings on the same keystroke can both be in play. A binding
/// scoped to `Terminal` deliberately shadows a global one there, so that pair
/// is not a collision; `Terminal` and `!Terminal` never overlap.
fn contexts_collide(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        (None, Some(scoped)) | (Some(scoped), None) => scoped.starts_with('!'),
    }
}

/// For each action, the other actions bound to the same keystroke where both
/// would apply. Only one of them fires, so Settings warns about it.
pub fn conflicts(bound: &[Bound]) -> HashMap<String, Vec<String>> {
    let mut by_keystroke: HashMap<String, Vec<&Bound>> = HashMap::new();
    for binding in bound {
        by_keystroke
            .entry(normalize_keystroke(&binding.keystroke))
            .or_default()
            .push(binding);
    }
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for group in by_keystroke.values() {
        for a in group {
            for b in group {
                if a.action != b.action
                    && contexts_collide(a.context.as_deref(), b.context.as_deref())
                {
                    out.entry(a.action.clone())
                        .or_default()
                        .push(b.action.clone());
                }
            }
        }
    }
    out
}

/// The bindings in `old` that `new` no longer has. Their keystrokes have to be
/// unbound, or a shortcut the user just moved would keep firing.
pub fn stale_bindings(old: &[Bound], new: &[Bound]) -> Vec<Bound> {
    old.iter()
        .filter(|binding| {
            !new.iter().any(|kept| {
                kept.action == binding.action
                    && kept.context == binding.context
                    && normalize_keystroke(&kept.keystroke)
                        == normalize_keystroke(&binding.keystroke)
            })
        })
        .cloned()
        .collect()
}

// ===== Colour =====

/// An opaque sRGB colour, each channel 0–1.
pub type Rgb = [f32; 3];

const BLACK: Rgb = [0.0, 0.0, 0.0];
const WHITE: Rgb = [1.0, 1.0, 1.0];

/// WCAG relative luminance.
pub fn luminance(color: Rgb) -> f32 {
    let linear = |c: f32| {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color[0]) + 0.7152 * linear(color[1]) + 0.0722 * linear(color[2])
}

/// WCAG contrast ratio between two colours, 1 (identical) to 21.
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// `a` moved `t` of the way toward `b`.
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn worst_contrast(color: Rgb, surfaces: &[Rgb]) -> f32 {
    surfaces
        .iter()
        .map(|surface| contrast(color, *surface))
        .fold(f32::INFINITY, f32::min)
}

/// `preferred` if it reads at `min` contrast on every surface; otherwise the
/// first step toward `fallback` that does, keeping as much of the theme's own
/// colour as legibility allows. `fallback` itself when nothing reaches `min`.
pub fn readable(preferred: Rgb, fallback: Rgb, surfaces: &[Rgb], min: f32) -> Rgb {
    for step in 0..=10 {
        let candidate = mix(preferred, fallback, step as f32 / 10.0);
        if worst_contrast(candidate, surfaces) >= min {
            return candidate;
        }
    }
    fallback
}

fn stronger(a: Rgb, b: Rgb, against: Rgb) -> Rgb {
    if contrast(a, against) >= contrast(b, against) {
        a
    } else {
        b
    }
}

/// The theme colours the Settings window is built from.
#[derive(Clone, Copy, Debug)]
pub struct ThemeInputs {
    pub background: Rgb,
    pub foreground: Rgb,
    pub muted_foreground: Rgb,
    pub border: Rgb,
    pub sidebar: Rgb,
    pub sidebar_accent: Rgb,
    pub sidebar_accent_foreground: Rgb,
    pub primary: Rgb,
    pub warning: Rgb,
    pub danger: Rgb,
    pub success: Rgb,
}

/// Every colour the Settings window paints with, derived from a theme so each
/// stays legible whatever the theme chose for its own tokens.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub background: Rgb,
    /// The section list on the left.
    pub nav: Rgb,
    /// A group of setting rows.
    pub card: Rgb,
    pub card_border: Rgb,
    /// Hairline between rows of a card.
    pub divider: Rgb,
    /// Background of a segmented control, stepper or key cap.
    pub track: Rgb,
    /// The chosen segment of a segmented control.
    pub track_selected: Rgb,
    pub text: Rgb,
    /// Descriptions, group headings and other secondary text.
    pub dim: Rgb,
    pub nav_selected: Rgb,
    pub nav_selected_text: Rgb,
    /// A switch that is on, the ring around the chosen theme, a key being
    /// recorded.
    pub accent: Rgb,
    /// What is drawn on top of `accent`.
    pub on_accent: Rgb,
    pub switch_off: Rgb,
    pub switch_off_thumb: Rgb,
    pub warning: Rgb,
    pub danger: Rgb,
    pub success: Rgb,
    /// Behind the part of a label that matched the search.
    pub highlight: Rgb,
}

/// Contrast secondary text keeps on every surface, wherever the theme's own
/// foreground can reach it.
pub const DIM_CONTRAST: f32 = 4.5;
/// Contrast a label keeps on the search highlight behind it.
pub const HIGHLIGHT_CONTRAST: f32 = 4.0;
/// Contrast a control's fill keeps against the surface it sits on.
pub const CONTROL_CONTRAST: f32 = 3.0;

/// Derive the Settings palette from a theme.
pub fn palette(theme: &ThemeInputs) -> Palette {
    let background = theme.background;
    // A theme whose text barely shows on its own background can't be trusted
    // for anything derived from it; fall back to black or white.
    let text = if contrast(theme.foreground, background) < CONTROL_CONTRAST {
        stronger(BLACK, WHITE, background)
    } else {
        theme.foreground
    };
    let card = mix(background, text, 0.045);
    let nav = theme.sidebar;
    let track = mix(background, text, 0.09);
    let surfaces = [background, card, nav, track];

    let card_border = if contrast(theme.border, background) >= 1.25 {
        theme.border
    } else {
        mix(background, text, 0.2)
    };

    let (nav_selected, nav_selected_text) = {
        let (bg, fg) = (theme.sidebar_accent, theme.sidebar_accent_foreground);
        if contrast(fg, bg) >= DIM_CONTRAST && contrast(bg, nav) >= 1.12 {
            (bg, fg)
        } else {
            let bg = mix(nav, text, 0.16);
            (
                bg,
                readable(text, stronger(BLACK, WHITE, bg), &[bg], DIM_CONTRAST),
            )
        }
    };

    let accent = if worst_contrast(theme.primary, &[card, background]) >= CONTROL_CONTRAST {
        theme.primary
    } else {
        text
    };
    let switch_off = mix(background, text, 0.3);
    // As much of the accent as leaves the label on top of it easy to read.
    let highlight = [0.35, 0.28, 0.21, 0.14, 0.08]
        .into_iter()
        .map(|share| mix(card, accent, share))
        .find(|tint| contrast(text, *tint) >= HIGHLIGHT_CONTRAST)
        .unwrap_or(card);

    Palette {
        background,
        nav,
        card,
        card_border,
        divider: mix(card, text, 0.09),
        track,
        track_selected: mix(background, text, 0.22),
        text,
        dim: readable(theme.muted_foreground, text, &surfaces, DIM_CONTRAST),
        nav_selected,
        nav_selected_text,
        accent,
        on_accent: stronger(background, text, accent),
        switch_off,
        switch_off_thumb: stronger(background, text, switch_off),
        warning: readable(theme.warning, text, &[card, background], DIM_CONTRAST),
        danger: readable(theme.danger, text, &[card, background], DIM_CONTRAST),
        success: readable(theme.success, text, &[card, background], DIM_CONTRAST),
        highlight,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Bound, KEY_ROWS, ThemeInputs, action_label, conflicts, contrast, digit_chord,
        highlight_ranges, is_modifier_key, key_caps, matches, normalize_keystroke, palette,
        query_words, range_keystroke, range_modifiers, readable, row_actions, row_for_action,
        stale_bindings,
    };
    use crate::settings_view::DEFAULT_KEYBINDINGS;

    fn bound(action: &str, keystroke: &str, context: Option<&str>) -> Bound {
        Bound {
            action: action.to_string(),
            keystroke: keystroke.to_string(),
            context: context.map(str::to_string),
        }
    }

    #[test]
    fn search_needs_every_word_somewhere() {
        let words = query_words("  Terminal FONT ");
        assert!(matches(&words, &["Terminal font", "Appearance"]));
        assert!(matches(&words, &["Font size", "Terminal"]));
        assert!(!matches(&words, &["Terminal text"]));
        assert!(!matches(&query_words("   "), &["anything"]));
    }

    #[test]
    fn highlights_fall_on_the_original_text() {
        let words = query_words("term é");
        let text = "Terminal café — TERMINAL";
        let found: Vec<&str> = highlight_ranges(text, &words)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(found, ["Term", "é", "TERM"]);
        // Overlapping matches merge into one range.
        let merged = highlight_ranges("aaaa", &query_words("aa aaa"));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0], 0..4);
        assert!(highlight_ranges("anything", &[]).is_empty());
    }

    #[test]
    fn every_default_action_has_exactly_one_row() {
        for (action, _, _) in DEFAULT_KEYBINDINGS {
            assert!(
                row_for_action(action).is_some(),
                "{action} is rebindable but has no row in KEY_ROWS"
            );
        }
        let mut covered: Vec<String> = KEY_ROWS.iter().flat_map(row_actions).collect();
        let total = covered.len();
        covered.sort();
        covered.dedup();
        assert_eq!(covered.len(), total, "an action is listed twice");
        for action in &covered {
            assert!(
                DEFAULT_KEYBINDINGS
                    .iter()
                    .any(|(name, _, _)| name == action),
                "{action} has a row but is not a rebindable action"
            );
        }
    }

    #[test]
    fn range_actions_are_named_by_their_digit() {
        assert_eq!(action_label("JumpToTab3"), "Tab 3");
        assert_eq!(action_label("NewAgent9"), "New agent from preset 9");
        assert_eq!(action_label("SplitRight"), "Split right");
        assert_eq!(action_label("Unknown"), "Unknown");
        // `JumpToTab10` is not one of the nine.
        assert!(row_for_action("JumpToTab10").is_none());
    }

    #[test]
    fn key_caps_keep_a_dash_or_bracket_as_the_key() {
        assert_eq!(key_caps("ctrl-shift-t"), ["Ctrl", "Shift", "T"]);
        assert_eq!(key_caps("ctrl--"), ["Ctrl", "-"]);
        assert_eq!(key_caps("ctrl-shift-]"), ["Ctrl", "Shift", "]"]);
        assert_eq!(key_caps("f11"), ["F11"]);
        assert_eq!(key_caps("ctrl-shift-up"), ["Ctrl", "Shift", "↑"]);
        assert_eq!(
            key_caps("ctrl-k ctrl-s"),
            ["Ctrl", "K", "then", "Ctrl", "S"]
        );
    }

    #[test]
    fn keystrokes_compare_whatever_the_modifier_order() {
        assert_eq!(
            normalize_keystroke("Shift-Ctrl-T"),
            normalize_keystroke("ctrl-shift-t")
        );
        assert_ne!(
            normalize_keystroke("ctrl-t"),
            normalize_keystroke("ctrl-shift-t")
        );
        assert_eq!(normalize_keystroke("ctrl--"), "ctrl--");
    }

    #[test]
    fn nine_digits_on_shared_modifiers_fold_into_one_row() {
        let keys =
            |mods: &str| -> Vec<String> { (1..=9).map(|n| range_keystroke(mods, n)).collect() };
        assert_eq!(range_modifiers(&keys("alt")), Some("alt".to_string()));
        assert_eq!(
            range_modifiers(&keys("ctrl-alt")),
            Some("ctrl-alt".to_string())
        );
        assert_eq!(range_modifiers(&keys("")), Some(String::new()));

        let mut odd = keys("alt");
        odd[2] = "ctrl-3".to_string();
        assert_eq!(range_modifiers(&odd), None, "one differs");
        let mut shuffled = keys("alt");
        shuffled.swap(0, 1);
        assert_eq!(range_modifiers(&shuffled), None, "digits out of order");
        let mut letter = keys("alt");
        letter[8] = "alt-x".to_string();
        assert_eq!(range_modifiers(&letter), None);
    }

    #[test]
    fn digit_chords_are_one_to_nine_only() {
        assert_eq!(digit_chord("ctrl-alt-4"), Some(("ctrl-alt".to_string(), 4)));
        assert_eq!(digit_chord("7"), Some((String::new(), 7)));
        assert_eq!(digit_chord("alt-0"), None);
        assert_eq!(digit_chord("alt-12"), None);
        assert_eq!(digit_chord("alt-a"), None);
        assert_eq!(digit_chord("ctrl-k alt-1"), None);
    }

    #[test]
    fn a_lone_modifier_is_not_a_shortcut() {
        for modifier in ["shift", "control", "alt", "platform", "function"] {
            assert!(is_modifier_key(modifier), "{modifier}");
        }
        assert!(!is_modifier_key("a"));
        assert!(!is_modifier_key("escape"));
    }

    #[test]
    fn same_keystroke_conflicts_only_where_both_apply() {
        let found = conflicts(&[
            bound("SplitRight", "ctrl-shift-l", None),
            bound("ClearTerminal", "Shift-Ctrl-L", None),
            // Scoped to the terminal on purpose: shadows, doesn't collide.
            bound("FindInProject", "ctrl-shift-f", None),
            bound("SearchTerminal", "ctrl-shift-f", Some("Terminal")),
            // Outside a terminal both would fire.
            bound("NewTab", "ctrl-t", None),
            bound("SaveFile", "ctrl-t", Some("!Terminal")),
            // Never in play together.
            bound("A", "ctrl-b", Some("Terminal")),
            bound("B", "ctrl-b", Some("!Terminal")),
        ]);
        assert_eq!(found["SplitRight"], ["ClearTerminal"]);
        assert_eq!(found["ClearTerminal"], ["SplitRight"]);
        assert_eq!(found["NewTab"], ["SaveFile"]);
        for free in ["FindInProject", "SearchTerminal", "A", "B"] {
            assert!(!found.contains_key(free), "{free}");
        }
    }

    #[test]
    fn a_moved_shortcut_leaves_its_old_keystroke_stale() {
        let old = [
            bound("NewPane", "ctrl-shift-t", None),
            bound("SaveFile", "ctrl-s", Some("!Terminal")),
            bound("ClosePane", "ctrl-shift-w", None),
        ];
        let new = [
            bound("NewPane", "ctrl-shift-y", None),
            // Same keystroke, respelled: not stale.
            bound("SaveFile", "Ctrl-S", Some("!Terminal")),
            // Same keystroke, now in another context: the old one is stale.
            bound("ClosePane", "ctrl-shift-w", Some("!Terminal")),
        ];
        let stale = stale_bindings(&old, &new);
        assert_eq!(
            stale,
            [
                bound("NewPane", "ctrl-shift-t", None),
                bound("ClosePane", "ctrl-shift-w", None)
            ]
        );
    }

    #[test]
    fn readable_keeps_the_theme_colour_when_it_already_reads() {
        let (white, black, grey) = ([1.0; 3], [0.0; 3], [0.64; 3]);
        assert_eq!(readable(grey, white, &[black], 4.5), grey);
        // Too dim on black: stepped toward white until it reads.
        let dim = [0.3; 3];
        let fixed = readable(dim, white, &[black], 4.5);
        assert!(contrast(fixed, black) >= 4.5);
        assert!(fixed[0] > dim[0] && fixed[0] < 1.0);
    }

    #[test]
    fn palette_repairs_a_theme_with_unreadable_tokens() {
        let dark = [0.04; 3];
        let theme = ThemeInputs {
            background: dark,
            foreground: [0.95; 3],
            // Secondary text nearly invisible, border invisible, selection text
            // on a selection of the same colour, primary lost in the background.
            muted_foreground: [0.2; 3],
            border: dark,
            sidebar: dark,
            sidebar_accent: [0.5; 3],
            sidebar_accent_foreground: [0.5; 3],
            primary: [0.1; 3],
            warning: [0.2, 0.2, 0.0],
            danger: [0.3, 0.0, 0.0],
            success: [0.0, 0.2, 0.0],
        };
        let p = palette(&theme);
        for surface in [p.background, p.card, p.nav, p.track] {
            assert!(contrast(p.dim, surface) >= 4.5);
            assert!(contrast(p.text, surface) >= 4.5);
        }
        assert!(contrast(p.card_border, p.background) >= 1.25);
        assert!(contrast(p.nav_selected_text, p.nav_selected) >= 4.5);
        assert!(contrast(p.accent, p.card) >= 3.0);
        assert!(contrast(p.on_accent, p.accent) >= 3.0);
        for status in [p.warning, p.danger, p.success] {
            assert!(contrast(status, p.card) >= 4.5);
        }
    }
}
