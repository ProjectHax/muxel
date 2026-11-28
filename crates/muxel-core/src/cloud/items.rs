//! Settings ⇄ sync items (protocol §4.4).
//!
//! Each runner, loop, snippet, host and identity is one item keyed by its id.
//! Appearance and keybindings are one item each with a fixed id. Machine-local
//! fields are dropped on the way out: presets travel by **name** (their ids are
//! per install), loops by **project key** (see [`super::project_key`]), and a
//! loop's `enabled` / `last_run` never leave the device.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    AgentPreset, Identity, KeyBindingCfg, Loop, LoopSchedule, PostRunAction, RemoteHost, Runner,
    Settings, Snippet,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Collection {
    Runners,
    Loops,
    Snippets,
    Hosts,
    Identities,
    Appearance,
    Keybindings,
    Tasks,
}

impl Collection {
    pub const SETTINGS: [Collection; 7] = [
        Collection::Runners,
        Collection::Loops,
        Collection::Snippets,
        Collection::Hosts,
        Collection::Identities,
        Collection::Appearance,
        Collection::Keybindings,
    ];

    /// Collections a team's `settings` scope may carry (appearance and
    /// keybindings stay personal).
    pub const TEAM_SETTINGS: [Collection; 5] = [
        Collection::Runners,
        Collection::Loops,
        Collection::Snippets,
        Collection::Hosts,
        Collection::Identities,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Collection::Runners => "runners",
            Collection::Loops => "loops",
            Collection::Snippets => "snippets",
            Collection::Hosts => "hosts",
            Collection::Identities => "identities",
            Collection::Appearance => "appearance",
            Collection::Keybindings => "keybindings",
            Collection::Tasks => "tasks",
        }
    }

    pub fn parse(s: &str) -> Option<Collection> {
        Some(match s {
            "runners" => Collection::Runners,
            "loops" => Collection::Loops,
            "snippets" => Collection::Snippets,
            "hosts" => Collection::Hosts,
            "identities" => Collection::Identities,
            "appearance" => Collection::Appearance,
            "keybindings" => Collection::Keybindings,
            "tasks" => Collection::Tasks,
            _ => return None,
        })
    }

    /// Collections whose items execute something (they need the strictest
    /// author checks and arrive switched off from others).
    pub fn executes(self) -> bool {
        matches!(self, Collection::Runners | Collection::Loops)
    }
}

/// Fixed item ids for the singleton collections.
pub const APPEARANCE_ID: Uuid = Uuid::from_u128(0x6d75_7865_6c2d_6170_7065_6172_616e_6365);
pub const KEYBINDINGS_ID: Uuid = Uuid::from_u128(0x6d75_7865_6c2d_6b65_7962_696e_6469_6e67);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunnerItem {
    pub name: String,
    #[serde(default)]
    pub preset_name: Option<String>,
    #[serde(default)]
    pub auto_mode_presses: u8,
    #[serde(default)]
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SnippetItem {
    pub name: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub submit: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopItem {
    pub name: String,
    #[serde(default)]
    pub preset_name: Option<String>,
    #[serde(default)]
    pub project_key: Option<String>,
    #[serde(default)]
    pub auto_mode_presses: u8,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub schedule: LoopSchedule,
    #[serde(default)]
    pub post_run: PostRunAction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Appearance {
    pub theme: String,
    pub theme_mode: String,
    #[serde(default)]
    pub language: Option<String>,
    pub font_family: String,
    pub font_size: f32,
    pub ui_font_size: f32,
    pub zoom: f32,
    pub pane_border: String,
    #[serde(default = "crate::default_cursor_shape")]
    pub cursor_shape: String,
    #[serde(default)]
    pub diff_split_view: bool,
    pub editor_font_family: String,
    pub editor_font_size: f32,
    pub editor_tab_size: u32,
    #[serde(default)]
    pub editor_soft_wrap: bool,
    #[serde(default)]
    pub editor_line_numbers: bool,
    #[serde(default)]
    pub editor_indent_guides: bool,
}

impl Appearance {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            theme: s.theme.clone(),
            theme_mode: s.theme_mode.clone(),
            language: s.language.clone(),
            font_family: s.font_family.clone(),
            font_size: s.font_size,
            ui_font_size: s.ui_font_size,
            zoom: s.zoom,
            pane_border: s.pane_border.clone(),
            cursor_shape: s.cursor_shape.clone(),
            diff_split_view: s.diff_split_view,
            editor_font_family: s.editor_font_family.clone(),
            editor_font_size: s.editor_font_size,
            editor_tab_size: s.editor_tab_size,
            editor_soft_wrap: s.editor_soft_wrap,
            editor_line_numbers: s.editor_line_numbers,
            editor_indent_guides: s.editor_indent_guides,
        }
    }

    pub fn apply_to(&self, s: &mut Settings) {
        s.theme = self.theme.clone();
        s.theme_mode = self.theme_mode.clone();
        s.language = self.language.clone();
        s.font_family = self.font_family.clone();
        s.font_size = self.font_size;
        s.ui_font_size = self.ui_font_size;
        s.zoom = self.zoom;
        s.pane_border = self.pane_border.clone();
        s.cursor_shape = if self.cursor_shape.is_empty() {
            crate::default_cursor_shape()
        } else {
            self.cursor_shape.clone()
        };
        s.diff_split_view = self.diff_split_view;
        s.editor_font_family = self.editor_font_family.clone();
        s.editor_font_size = self.editor_font_size;
        s.editor_tab_size = self.editor_tab_size;
        s.editor_soft_wrap = self.editor_soft_wrap;
        s.editor_line_numbers = self.editor_line_numbers;
        s.editor_indent_guides = self.editor_indent_guides;
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Keybindings {
    pub keybindings: Vec<KeyBindingCfg>,
    #[serde(default)]
    pub terminal_passthrough_keys: Vec<String>,
}

/// Names that a pulled item referred to but this device couldn't resolve. They
/// are re-emitted on push so one device lacking a preset or project never
/// erases the reference for everyone else. Persisted in the sync state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Carry {
    #[serde(default)]
    pub preset_name: Option<String>,
    #[serde(default)]
    pub project_key: Option<String>,
}

/// Local lookups the conversion needs.
pub struct Resolver<'a> {
    pub presets: &'a [AgentPreset],
    /// Every local project as `(id, portable key)`, across all workspaces.
    pub projects: &'a [(Uuid, String)],
    pub carry: &'a HashMap<Uuid, Carry>,
}

/// A preset id that resolves to nothing: the app reports it as "preset was
/// deleted" instead of silently running the toolbar's agent.
pub const UNRESOLVED_PRESET: Uuid = Uuid::nil();

impl Resolver<'_> {
    fn preset_name(&self, item: Uuid, id: Option<Uuid>) -> Option<String> {
        let id = id?;
        self.presets
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .or_else(|| self.carry.get(&item).and_then(|c| c.preset_name.clone()))
    }

    fn preset_id(&self, name: Option<&str>) -> Option<Uuid> {
        let name = name?;
        Some(
            self.presets
                .iter()
                .find(|p| p.name == name)
                .or_else(|| {
                    self.presets
                        .iter()
                        .find(|p| p.name.eq_ignore_ascii_case(name))
                })
                .map_or(UNRESOLVED_PRESET, |p| p.id),
        )
    }

    fn project_key(&self, item: Uuid, id: Uuid) -> Option<String> {
        self.projects
            .iter()
            .find(|(pid, _)| *pid == id)
            .map(|(_, k)| k.clone())
            .or_else(|| self.carry.get(&item).and_then(|c| c.project_key.clone()))
    }

    fn project_id(&self, key: Option<&str>) -> Option<Uuid> {
        let key = key?;
        self.projects
            .iter()
            .find(|(_, k)| k == key)
            .map(|(id, _)| *id)
    }
}

/// One item as extracted from local settings.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalItem {
    pub collection: Collection,
    pub id: Uuid,
    pub data: Value,
}

fn to_value<T: Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

/// Every item of `collections` currently in `settings`.
pub fn extract(settings: &Settings, r: &Resolver, collections: &[Collection]) -> Vec<LocalItem> {
    let mut out = Vec::new();
    for &c in collections {
        match c {
            Collection::Runners => out.extend(settings.runners.iter().map(|x| LocalItem {
                collection: c,
                id: x.id,
                data: to_value(&RunnerItem {
                    name: x.name.clone(),
                    preset_name: r.preset_name(x.id, x.preset_id),
                    auto_mode_presses: x.auto_mode_presses,
                    prompt: x.prompt.clone(),
                }),
            })),
            Collection::Snippets => out.extend(settings.snippets.iter().map(|x| LocalItem {
                collection: c,
                id: x.id,
                data: to_value(&SnippetItem {
                    name: x.name.clone(),
                    text: x.text.clone(),
                    submit: x.submit,
                }),
            })),
            Collection::Loops => out.extend(settings.loops.iter().map(|x| LocalItem {
                collection: c,
                id: x.id,
                data: to_value(&LoopItem {
                    name: x.name.clone(),
                    preset_name: r.preset_name(x.id, x.preset_id),
                    project_key: r.project_key(x.id, x.project_id),
                    auto_mode_presses: x.auto_mode_presses,
                    prompt: x.prompt.clone(),
                    schedule: x.schedule,
                    post_run: x.post_run,
                }),
            })),
            Collection::Hosts => out.extend(settings.remotes.iter().map(|x| LocalItem {
                collection: c,
                id: x.id,
                data: to_value(x),
            })),
            Collection::Identities => out.extend(settings.identities.iter().map(|x| LocalItem {
                collection: c,
                id: x.id,
                data: to_value(x),
            })),
            Collection::Appearance => out.push(LocalItem {
                collection: c,
                id: APPEARANCE_ID,
                data: to_value(&Appearance::from_settings(settings)),
            }),
            Collection::Keybindings => out.push(LocalItem {
                collection: c,
                id: KEYBINDINGS_ID,
                data: to_value(&Keybindings {
                    keybindings: settings.keybindings.clone(),
                    terminal_passthrough_keys: settings.terminal_passthrough_keys.clone(),
                }),
            }),
            Collection::Tasks => {}
        }
    }
    out
}

/// What applying a remote item did, for the app's feed and the sync state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Settings changed.
    pub changed: bool,
    /// References that didn't resolve here and must be carried (see [`Carry`]).
    pub carry: Option<Carry>,
    /// A loop arrived new on this device and was left switched off.
    pub new_loop_off: bool,
    /// The local item this one replaced by adoption (same name, never synced).
    pub adopted_from: Option<Uuid>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ApplyError(pub String);

fn from_value<T: for<'de> Deserialize<'de>>(v: &Value) -> Result<T, ApplyError> {
    serde_json::from_value(v.clone()).map_err(|e| ApplyError(e.to_string()))
}

fn upsert<T: Clone>(list: &mut Vec<T>, id_of: impl Fn(&T) -> Uuid, id: Uuid, item: T) -> bool {
    if let Some(slot) = list.iter_mut().find(|x| id_of(x) == id) {
        *slot = item;
    } else {
        list.push(item);
    }
    true
}

/// Apply one remote item (`data = None` = deleted) to `settings`.
/// `is_synced(id)` says whether a local id has ever been synced; an unsynced
/// local runner or snippet with the same name as a new remote one is replaced
/// by it rather than duplicated (fresh installs seed identical built-ins with
/// different ids).
pub fn apply(
    settings: &mut Settings,
    r: &Resolver,
    collection: Collection,
    id: Uuid,
    data: Option<&Value>,
    is_synced: &dyn Fn(Uuid) -> bool,
) -> Result<Applied, ApplyError> {
    let mut applied = Applied::default();
    let Some(data) = data else {
        let before = count(settings, collection);
        match collection {
            Collection::Runners => settings.runners.retain(|x| x.id != id),
            Collection::Snippets => settings.snippets.retain(|x| x.id != id),
            Collection::Loops => settings.loops.retain(|x| x.id != id),
            Collection::Hosts => settings.remotes.retain(|x| x.id != id),
            Collection::Identities => settings.identities.retain(|x| x.id != id),
            Collection::Appearance | Collection::Keybindings | Collection::Tasks => {}
        }
        applied.changed = before != count(settings, collection);
        return Ok(applied);
    };
    match collection {
        Collection::Runners => {
            let item: RunnerItem = from_value(data)?;
            let preset_id = r.preset_id(item.preset_name.as_deref());
            if preset_id == Some(UNRESOLVED_PRESET) {
                applied.carry = Some(Carry {
                    preset_name: item.preset_name.clone(),
                    project_key: None,
                });
            }
            // Adopt only an identical copy: a customized local "Review" is kept
            // beside the remote one rather than silently replaced.
            applied.adopted_from = adopt(&mut settings.runners, id, is_synced, |x| {
                (
                    x.id,
                    x.name == item.name
                        && x.prompt == item.prompt
                        && x.auto_mode_presses == item.auto_mode_presses,
                )
            });
            if let Some(old) = applied.adopted_from
                && let Some(x) = settings.runners.iter_mut().find(|x| x.id == old)
            {
                x.id = id;
            }
            let runner = Runner {
                id,
                name: item.name,
                preset_id,
                auto_mode_presses: item.auto_mode_presses,
                prompt: item.prompt,
            };
            applied.changed = upsert(&mut settings.runners, |x| x.id, id, runner);
        }
        Collection::Snippets => {
            let item: SnippetItem = from_value(data)?;
            applied.adopted_from = adopt(&mut settings.snippets, id, is_synced, |x| {
                (
                    x.id,
                    x.name == item.name && x.text == item.text && x.submit == item.submit,
                )
            });
            if let Some(old) = applied.adopted_from
                && let Some(x) = settings.snippets.iter_mut().find(|x| x.id == old)
            {
                x.id = id;
            }
            let snippet = Snippet {
                id,
                name: item.name,
                text: item.text,
                submit: item.submit,
            };
            applied.changed = upsert(&mut settings.snippets, |x| x.id, id, snippet);
        }
        Collection::Loops => {
            let item: LoopItem = from_value(data)?;
            let preset_id = r.preset_id(item.preset_name.as_deref());
            let project_id = r.project_id(item.project_key.as_deref());
            let mut carry = Carry::default();
            if preset_id == Some(UNRESOLVED_PRESET) {
                carry.preset_name = item.preset_name.clone();
            }
            if project_id.is_none() && item.project_key.is_some() {
                carry.project_key = item.project_key.clone();
            }
            if carry != Carry::default() {
                applied.carry = Some(carry);
            }
            let existing = settings.loops.iter().find(|x| x.id == id).cloned();
            let (enabled, last_run, local_project) = match &existing {
                Some(x) => (x.enabled, x.last_run, Some(x.project_id)),
                None => {
                    applied.new_loop_off = true;
                    (false, None, None)
                }
            };
            let l = Loop {
                id,
                name: item.name,
                preset_id,
                // An unresolved project keeps whatever the device had (or nil,
                // which never fires); the loop can't run until one is picked.
                project_id: project_id.or(local_project).unwrap_or(Uuid::nil()),
                auto_mode_presses: item.auto_mode_presses,
                prompt: item.prompt,
                schedule: item.schedule,
                post_run: item.post_run,
                // Never armed by a pull: a loop arriving on a device is off,
                // and an update keeps the device's own choice.
                enabled: enabled && project_id.or(local_project).is_some(),
                last_run,
            };
            applied.changed = upsert(&mut settings.loops, |x| x.id, id, l);
        }
        Collection::Hosts => {
            let mut host: RemoteHost = from_value(data)?;
            host.id = id;
            applied.changed = upsert(&mut settings.remotes, |x| x.id, id, host);
        }
        Collection::Identities => {
            let mut ident: Identity = from_value(data)?;
            ident.id = id;
            applied.changed = upsert(&mut settings.identities, |x| x.id, id, ident);
        }
        Collection::Appearance => {
            let a: Appearance = from_value(data)?;
            applied.changed = Appearance::from_settings(settings) != a;
            a.apply_to(settings);
        }
        Collection::Keybindings => {
            let k: Keybindings = from_value(data)?;
            settings.keybindings = k.keybindings;
            settings.terminal_passthrough_keys = k.terminal_passthrough_keys;
            applied.changed = true;
        }
        Collection::Tasks => return Err(ApplyError("tasks are not settings".into())),
    }
    Ok(applied)
}

/// The id of an unsynced local item that a new remote item `id` should adopt:
/// none when `id` already exists locally.
fn adopt<T>(
    list: &mut [T],
    id: Uuid,
    is_synced: &dyn Fn(Uuid) -> bool,
    probe: impl Fn(&T) -> (Uuid, bool),
) -> Option<Uuid> {
    if list.iter().any(|x| probe(x).0 == id) {
        return None;
    }
    list.iter()
        .map(&probe)
        .find(|(local, same)| *same && !is_synced(*local))
        .map(|(local, _)| local)
}

fn count(s: &Settings, c: Collection) -> usize {
    match c {
        Collection::Runners => s.runners.len(),
        Collection::Snippets => s.snippets.len(),
        Collection::Loops => s.loops.len(),
        Collection::Hosts => s.remotes.len(),
        Collection::Identities => s.identities.len(),
        _ => 0,
    }
}

/// A team's shared runners, snippets and loops as a library section (see
/// `LibraryHub::set_cloud_library`), cleaned like a git library's text.
/// Sorted by name; among items sharing a name the one with the lowest id is
/// kept, so every device shows the same one. Unreadable or nameless items are
/// dropped. `items` is collection name → `(item id, data)`.
pub fn team_library(items: &BTreeMap<String, Vec<(Uuid, Value)>>) -> crate::library::ParsedLibrary {
    use crate::library::sanitize::sanitize;
    use crate::library::{
        LibLoop, LibRunner, LibSnippet, LoopContent, ParsedLibrary, RunnerContent,
    };
    fn read<T: serde::de::DeserializeOwned>(
        items: &BTreeMap<String, Vec<(Uuid, Value)>>,
        collection: Collection,
        name: impl Fn(&T) -> &str,
    ) -> Vec<(String, T)> {
        let mut out: Vec<(String, Uuid, T)> = items
            .get(collection.as_str())
            .into_iter()
            .flatten()
            .filter_map(|(id, v)| {
                serde_json::from_value::<T>(v.clone())
                    .ok()
                    .map(|t| (*id, t))
            })
            .map(|(id, t)| (sanitize(name(&t)).trim().to_string(), id, t))
            .filter(|(n, _, _)| !n.is_empty())
            .collect();
        out.sort_by(|a, b| {
            a.0.to_lowercase()
                .cmp(&b.0.to_lowercase())
                .then(a.1.cmp(&b.1))
        });
        let mut seen = HashSet::new();
        out.into_iter()
            .filter(|(n, _, _)| seen.insert(n.clone()))
            .map(|(n, _, t)| (n, t))
            .collect()
    }
    let preset = |p: &Option<String>| {
        p.as_deref()
            .map(|p| sanitize(p).trim().to_string())
            .filter(|p| !p.is_empty())
    };
    ParsedLibrary {
        snippets: read::<SnippetItem>(items, Collection::Snippets, |s| &s.name)
            .into_iter()
            .map(|(name, s)| LibSnippet {
                name,
                text: sanitize(&s.text),
                submit: s.submit,
            })
            .collect(),
        runners: read::<RunnerItem>(items, Collection::Runners, |r| &r.name)
            .into_iter()
            .map(|(name, r)| LibRunner {
                name,
                content: RunnerContent {
                    prompt: sanitize(&r.prompt),
                    preset: preset(&r.preset_name),
                    auto_mode_presses: r.auto_mode_presses,
                },
            })
            .collect(),
        loops: read::<LoopItem>(items, Collection::Loops, |l| &l.name)
            .into_iter()
            .map(|(name, l)| LibLoop {
                name,
                content: LoopContent {
                    prompt: sanitize(&l.prompt),
                    preset: preset(&l.preset_name),
                    auto_mode_presses: l.auto_mode_presses,
                    schedule: l.schedule,
                    post_run: l.post_run,
                },
            })
            .collect(),
        ..ParsedLibrary::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        APPEARANCE_ID, Carry, Collection, LoopItem, Resolver, RunnerItem, UNRESOLVED_PRESET, apply,
        extract, team_library,
    };
    use crate::{AgentPreset, Loop, Settings};
    use std::collections::HashMap;
    use uuid::Uuid;

    fn preset(name: &str, id: u128) -> AgentPreset {
        let mut p = AgentPreset::claude();
        p.name = name.into();
        p.id = Uuid::from_u128(id);
        p
    }

    #[test]
    fn runner_travels_by_preset_name() {
        let presets_a = [preset("Claude", 1)];
        let presets_b = [preset("Claude", 99)];
        let carry = HashMap::new();
        let mut a = Settings::default();
        a.runners.truncate(0);
        let mut r = crate::Runner::defaults().remove(0);
        r.preset_id = Some(Uuid::from_u128(1));
        a.runners.push(r.clone());
        let ra = Resolver {
            presets: &presets_a,
            projects: &[],
            carry: &carry,
        };
        let items = extract(&a, &ra, &[Collection::Runners]);
        let item: RunnerItem = serde_json::from_value(items[0].data.clone()).unwrap();
        assert_eq!(item.preset_name.as_deref(), Some("Claude"));

        let mut b = Settings::default();
        b.runners.clear();
        let rb = Resolver {
            presets: &presets_b,
            projects: &[],
            carry: &carry,
        };
        apply(
            &mut b,
            &rb,
            Collection::Runners,
            r.id,
            Some(&items[0].data),
            &|_| false,
        )
        .unwrap();
        assert_eq!(b.runners[0].preset_id, Some(Uuid::from_u128(99)));
    }

    #[test]
    fn unresolved_preset_is_flagged_and_carried() {
        let carry = HashMap::new();
        let r = Resolver {
            presets: &[],
            projects: &[],
            carry: &carry,
        };
        let mut s = Settings::default();
        s.runners.clear();
        let data = serde_json::to_value(RunnerItem {
            name: "x".into(),
            preset_name: Some("Gone".into()),
            auto_mode_presses: 0,
            prompt: String::new(),
        })
        .unwrap();
        let id = Uuid::from_u128(5);
        let out = apply(&mut s, &r, Collection::Runners, id, Some(&data), &|_| false).unwrap();
        assert_eq!(s.runners[0].preset_id, Some(UNRESOLVED_PRESET));
        let carry_map = HashMap::from([(id, out.carry.clone().unwrap())]);
        let r = Resolver {
            presets: &[],
            projects: &[],
            carry: &carry_map,
        };
        // Pushing it back re-emits the name rather than erasing it.
        let back = extract(&s, &r, &[Collection::Runners]);
        let item: RunnerItem = serde_json::from_value(back[0].data.clone()).unwrap();
        assert_eq!(item.preset_name.as_deref(), Some("Gone"));
    }

    #[test]
    fn pulled_loop_arrives_off_and_updates_keep_local_choice() {
        let presets = [preset("Claude", 1)];
        let project = Uuid::from_u128(42);
        let projects = [(project, "git:github.com/o/r".to_string())];
        let carry = HashMap::new();
        let r = Resolver {
            presets: &presets,
            projects: &projects,
            carry: &carry,
        };
        let id = Uuid::from_u128(9);
        let data = serde_json::to_value(LoopItem {
            name: "nightly".into(),
            preset_name: Some("Claude".into()),
            project_key: Some("git:github.com/o/r".into()),
            auto_mode_presses: 0,
            prompt: "p".into(),
            schedule: Default::default(),
            post_run: Default::default(),
        })
        .unwrap();
        let mut s = Settings::default();
        let out = apply(&mut s, &r, Collection::Loops, id, Some(&data), &|_| false).unwrap();
        assert!(out.new_loop_off);
        assert!(!s.loops[0].enabled);
        assert_eq!(s.loops[0].project_id, project);
        s.loops[0].enabled = true;
        s.loops[0].last_run = Some(123);
        let out = apply(&mut s, &r, Collection::Loops, id, Some(&data), &|_| true).unwrap();
        assert!(!out.new_loop_off);
        assert!(s.loops[0].enabled);
        assert_eq!(s.loops[0].last_run, Some(123));
    }

    #[test]
    fn loop_with_unknown_project_never_arms_and_carries_key() {
        let carry = HashMap::new();
        let presets = [preset("Claude", 1)];
        let r = Resolver {
            presets: &presets,
            projects: &[],
            carry: &carry,
        };
        let id = Uuid::from_u128(9);
        let data = serde_json::to_value(LoopItem {
            name: "n".into(),
            preset_name: Some("Claude".into()),
            project_key: Some("git:example.com/x".into()),
            auto_mode_presses: 0,
            prompt: String::new(),
            schedule: Default::default(),
            post_run: Default::default(),
        })
        .unwrap();
        let mut s = Settings::default();
        let out = apply(&mut s, &r, Collection::Loops, id, Some(&data), &|_| false).unwrap();
        assert_eq!(s.loops[0].project_id, Uuid::nil());
        assert_eq!(
            out.carry,
            Some(Carry {
                preset_name: None,
                project_key: Some("git:example.com/x".into())
            })
        );
    }

    #[test]
    fn seeded_builtins_are_adopted_not_duplicated() {
        let carry = HashMap::new();
        let r = Resolver {
            presets: &[],
            projects: &[],
            carry: &carry,
        };
        let a = Settings::default();
        let mut b = Settings::default();
        assert!(!b.snippets.is_empty());
        let items = extract(&a, &r, &[Collection::Snippets]);
        for it in &items {
            apply(
                &mut b,
                &r,
                Collection::Snippets,
                it.id,
                Some(&it.data),
                &|_| false,
            )
            .unwrap();
        }
        assert_eq!(a.snippets.len(), b.snippets.len());
        let ids_a: Vec<_> = a.snippets.iter().map(|s| s.id).collect();
        let ids_b: Vec<_> = b.snippets.iter().map(|s| s.id).collect();
        assert_eq!(ids_a, ids_b);

        // An item that was already synced is never adopted away: it stays and
        // the remote one is added beside it.
        let mut c = Settings::default();
        let before = c.snippets.len();
        let it = &items[0];
        apply(
            &mut c,
            &r,
            Collection::Snippets,
            it.id,
            Some(&it.data),
            &|_| true,
        )
        .unwrap();
        assert_eq!(c.snippets.len(), before + 1);

        // A customized copy with the same name is kept, not replaced.
        let mut d = Settings::default();
        d.snippets[0].text = "my own words".into();
        let n = d.snippets.len();
        apply(
            &mut d,
            &r,
            Collection::Snippets,
            items[0].id,
            Some(&items[0].data),
            &|_| false,
        )
        .unwrap();
        assert_eq!(d.snippets.len(), n + 1);
        assert!(d.snippets.iter().any(|s| s.text == "my own words"));
    }

    #[test]
    fn delete_and_appearance() {
        let carry = HashMap::new();
        let r = Resolver {
            presets: &[],
            projects: &[],
            carry: &carry,
        };
        let mut s = Settings::default();
        let l = Loop::new("x", Uuid::nil());
        let id = l.id;
        s.loops.push(l);
        let out = apply(&mut s, &r, Collection::Loops, id, None, &|_| true).unwrap();
        assert!(out.changed && s.loops.is_empty());

        let other = Settings {
            theme: "Nord".into(),
            font_size: 17.0,
            ..Default::default()
        };
        let it = extract(&other, &r, &[Collection::Appearance]);
        assert_eq!(it[0].id, APPEARANCE_ID);
        apply(
            &mut s,
            &r,
            Collection::Appearance,
            APPEARANCE_ID,
            Some(&it[0].data),
            &|_| true,
        )
        .unwrap();
        assert_eq!(s.theme, "Nord");
        assert_eq!(s.font_size, 17.0);
    }

    #[test]
    fn team_items_become_a_clean_library_section() {
        use serde_json::json;
        use std::collections::BTreeMap;
        let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let mut items = BTreeMap::new();
        items.insert(
            "snippets".to_string(),
            vec![
                (b, json!({"name": "deploy", "text": "make deploy-b"})),
                (a, json!({"name": "deploy", "text": "make deploy-a\r\n"})),
                (c, json!({"name": " \u{0007} ", "text": "nameless"})),
            ],
        );
        items.insert(
            "runners".to_string(),
            vec![(
                a,
                json!({"name": "Review", "preset_name": "  ", "prompt": "review\u{200B}"}),
            )],
        );
        items.insert(
            "loops".to_string(),
            vec![
                (
                    a,
                    json!({"name": "zeta", "preset_name": "Claude", "prompt": "p"}),
                ),
                (b, json!({"name": "Alpha", "prompt": "q"})),
                (c, json!({"no_name": true})),
            ],
        );
        let lib = team_library(&items);
        assert_eq!(lib.snippets.len(), 1);
        assert_eq!(lib.snippets[0].text, "make deploy-a\n", "lowest id wins");
        assert_eq!(
            lib.runners[0].content.preset, None,
            "blank preset = toolbar's"
        );
        assert_eq!(lib.runners[0].content.prompt, "review");
        let names: Vec<_> = lib.loops.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "zeta"]);
        assert_eq!(lib.loops[1].content.preset.as_deref(), Some("Claude"));
    }
}
