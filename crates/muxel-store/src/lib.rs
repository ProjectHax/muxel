//! Persistence for muxel.
//!
//! The whole [`Workspace`] (projects, pane layouts, instance metadata) is saved
//! as a single JSON document under the platform data dir, written atomically
//! (temp file + rename). The dataset is small and loaded/saved wholesale, so a
//! file is simpler and just as durable as a database here; this module is the
//! seam to swap in SQLite later if querying/scale ever demands it.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use muxel_core::{ProfileMeta, ProfilesIndex, Settings, Uuid, WindowGeom, Workspace};
use std::path::{Path, PathBuf};

/// The muxel config directory (e.g. `~/.config/muxel` on Linux).
pub fn config_dir() -> Option<PathBuf> {
    ProjectDirs::from("dev", "muxel", "muxel").map(|d| d.config_dir().to_path_buf())
}

/// Path to the hand-editable settings file.
pub fn settings_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

/// Load settings from the default location (defaults if missing or invalid).
pub fn load_settings() -> Settings {
    settings_path()
        .map(|p| load_settings_from(&p))
        .unwrap_or_default()
}

/// Save settings to the default location as TOML.
pub fn save_settings(settings: &Settings) -> Result<()> {
    let path = settings_path().context("could not determine config directory")?;
    save_settings_to(&path, settings)
}

/// Load settings from an explicit path (defaults on missing/invalid).
pub fn load_settings_from(path: &Path) -> Settings {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            log::warn!("ignoring invalid config at {}: {e}", path.display());
            Settings::default()
        }),
        Err(_) => Settings::default(),
    }
}

/// Save settings to an explicit path as TOML.
pub fn save_settings_to(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(settings).context("serializing settings")?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The muxel data directory (e.g. `~/.local/share/muxel` on Linux).
pub fn data_dir() -> Option<PathBuf> {
    ProjectDirs::from("dev", "muxel", "muxel").map(|d| d.data_dir().to_path_buf())
}

/// Path to the persisted workspace document.
pub fn workspace_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("workspace.json"))
}

/// Load the workspace from the default location, if present and valid.
pub fn load_workspace() -> Option<Workspace> {
    load_workspace_from(&workspace_path()?)
}

/// Save the workspace to the default location.
pub fn save_workspace(workspace: &Workspace) -> Result<()> {
    let path = workspace_path().context("could not determine data directory")?;
    save_workspace_to(&path, workspace)
}

/// Load a workspace from an explicit path (returns `None` on missing/invalid).
pub fn load_workspace_from(path: &Path) -> Option<Workspace> {
    let data = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&data) {
        Ok(workspace) => Some(workspace),
        Err(e) => {
            log::warn!("ignoring unreadable workspace at {}: {e}", path.display());
            None
        }
    }
}

/// Save a workspace to an explicit path, atomically (temp file + rename).
pub fn save_workspace_to(path: &Path, workspace: &Workspace) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating data dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(workspace).context("serializing workspace")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Path to the persisted main-window geometry.
pub fn window_geom_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("window.json"))
}

/// Load the saved window geometry, if present and valid.
pub fn load_window_geom() -> Option<WindowGeom> {
    let path = window_geom_path()?;
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Save the window geometry (best-effort).
pub fn save_window_geom(geom: &WindowGeom) -> Result<()> {
    let path = window_geom_path().context("could not determine data directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating data dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(geom).context("serializing window geometry")?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

// ---- Profiles: each profile owns a workspace; UI settings stay global. ----

/// The profiles directory (`<data_dir>/profiles`).
pub fn profiles_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("profiles"))
}

/// Path to a profile's workspace document.
pub fn profile_workspace_path(id: Uuid) -> Option<PathBuf> {
    profiles_dir().map(|d| d.join(id.to_string()).join("workspace.json"))
}

/// Path to the profiles index document.
pub fn profiles_index_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("profiles.json"))
}

/// Load the profiles index (empty if missing/invalid).
pub fn load_profiles_index() -> ProfilesIndex {
    profiles_index_path()
        .map(|p| load_profiles_index_from(&p))
        .unwrap_or_default()
}

/// Load a profiles index from an explicit path.
pub fn load_profiles_index_from(path: &Path) -> ProfilesIndex {
    match std::fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
            log::warn!("ignoring invalid profiles index at {}: {e}", path.display());
            ProfilesIndex::default()
        }),
        Err(_) => ProfilesIndex::default(),
    }
}

/// Save the profiles index to the default location.
pub fn save_profiles_index(index: &ProfilesIndex) -> Result<()> {
    let path = profiles_index_path().context("could not determine data directory")?;
    save_profiles_index_to(&path, index)
}

/// Save a profiles index to an explicit path, atomically (temp file + rename).
pub fn save_profiles_index_to(path: &Path, index: &ProfilesIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating data dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(index).context("serializing profiles index")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Ensure a profiles index exists. On first run, migrate a legacy single
/// `workspace.json` into a "Default" profile (or seed an empty one).
pub fn migrate_to_profiles() -> ProfilesIndex {
    match data_dir() {
        Some(base) => migrate_profiles_at(&base),
        None => ProfilesIndex::default(),
    }
}

/// Migration core, parameterized by the data dir for testability.
fn migrate_profiles_at(base: &Path) -> ProfilesIndex {
    let index_path = base.join("profiles.json");
    if index_path.exists() {
        return load_profiles_index_from(&index_path);
    }
    let id = Uuid::new_v4();
    let index = ProfilesIndex {
        profiles: vec![ProfileMeta {
            id,
            name: "Default".to_string(),
        }],
        current: Some(id),
    };
    // Move the legacy single workspace.json into the new profile, if present.
    let legacy = base.join("workspace.json");
    let dest = base
        .join("profiles")
        .join(id.to_string())
        .join("workspace.json");
    if legacy.exists() {
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::rename(&legacy, &dest).is_err() {
            let _ = std::fs::copy(&legacy, &dest);
            let _ = std::fs::remove_file(&legacy);
        }
    }
    let _ = save_profiles_index_to(&index_path, &index);
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxel_core::{
        AgentPreset, EnvVar, Instance, InstanceKind, PaneNode, ProfileMeta, ProfilesIndex, Project,
        Settings, Uuid, WindowGeom,
    };

    #[test]
    fn window_geom_round_trips() {
        let geom = WindowGeom {
            x: 10.0,
            y: 20.0,
            width: 800.0,
            height: 600.0,
            maximized: true,
        };
        let json = serde_json::to_string_pretty(&geom).expect("serialize");
        let loaded: WindowGeom = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(loaded.width, 800.0);
        assert_eq!(loaded.x, 10.0);
        assert!(loaded.maximized);
    }

    #[test]
    fn editor_instance_round_trips_through_disk() {
        let dir = std::env::temp_dir().join("muxel-store-test-editor");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("workspace.json");

        let mut workspace = Workspace::default();
        let mut project = Project::new("demo", "/tmp/demo");
        let editor = Instance::editor(project.id, Some("/tmp/demo/src/main.rs".into()));
        let iid = editor.id;
        project.layout = Some(PaneNode::leaf(iid));
        workspace.add_instance(editor);
        workspace.add_project(project);

        save_workspace_to(&path, &workspace).expect("save");
        let loaded = load_workspace_from(&path).expect("load");
        let inst = loaded.instance(iid).expect("editor instance present");
        assert_eq!(inst.kind, InstanceKind::Editor);
        assert_eq!(
            inst.editor_path.as_deref(),
            Some(std::path::Path::new("/tmp/demo/src/main.rs"))
        );
        // A diff instance round-trips with its target directory in editor_path.
        let diff = Instance::diff(loaded.projects[0].id, "/tmp/demo/.wt/agent".into());
        let did = diff.id;
        let mut ws2 = loaded;
        ws2.add_instance(diff);
        save_workspace_to(&path, &ws2).expect("save diff");
        let reloaded = load_workspace_from(&path).expect("load diff");
        let dinst = reloaded.instance(did).expect("diff instance present");
        assert_eq!(dinst.kind, InstanceKind::Diff);
        assert_eq!(
            dinst.editor_path.as_deref(),
            Some(std::path::Path::new("/tmp/demo/.wt/agent"))
        );

        // A legacy instance with no `kind` field defaults to Terminal.
        let legacy: Instance =
            serde_json::from_str(r#"{"id":"00000000-0000-0000-0000-000000000001","project_id":"00000000-0000-0000-0000-000000000002","title":"old"}"#)
                .expect("legacy deserialize");
        assert_eq!(legacy.kind, InstanceKind::Terminal);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_with_populated_preset_round_trips() {
        let dir = std::env::temp_dir().join("muxel-store-test-preset-rt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");

        let mut s = Settings::default();
        let mut p = AgentPreset::claude();
        p.model = Some("claude-opus-4-8".to_string());
        p.effort = Some("high".to_string());
        p.effort_flag = Some("--effort".to_string());
        p.system_prompt = Some("line one\nline two with \"quotes\" and = signs\ttab".to_string());
        p.env = vec![EnvVar {
            key: "FOO".to_string(),
            value: "bar = baz \"q\"".to_string(),
        }];
        s.presets.push(p);
        s.font_size = 18.0;
        s.zoom = 1.5;

        save_settings_to(&path, &s).expect("save populated settings");
        let loaded = load_settings_from(&path);
        assert_eq!(loaded.font_size, 18.0);
        assert_eq!(loaded.zoom, 1.5);
        let edited = loaded
            .presets
            .iter()
            .find(|p| p.model.as_deref() == Some("claude-opus-4-8"))
            .expect("custom preset present");
        assert_eq!(edited.env.len(), 1);
        assert_eq!(edited.effort.as_deref(), Some("high"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_round_trips_through_disk() {
        let dir = std::env::temp_dir().join("muxel-store-test-rt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("workspace.json");

        let mut workspace = Workspace::default();
        let mut project = Project::new("demo", "/tmp/demo");
        let instance = Instance::shell(project.id);
        let iid = instance.id;
        project.layout = Some(PaneNode::leaf(iid));
        workspace.add_instance(instance);
        let pid = workspace.add_project(project);

        save_workspace_to(&path, &workspace).expect("save");
        let loaded = load_workspace_from(&path).expect("load");

        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.active_project, Some(pid));
        assert_eq!(loaded.project(pid).unwrap().first_instance(), Some(iid));
        assert_eq!(loaded.instances.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_workspace_with_instance_leaves_still_loads() {
        // A workspace.json written before tabs existed: leaves use the single
        // `"instance"` field, nested in a split. It must still deserialize, with
        // each legacy leaf becoming a one-tab group, so saved layouts survive the
        // upgrade. Mirrors the real on-disk shape: split[ leaf, split[leaf,leaf] ].
        let dir = std::env::temp_dir().join("muxel-store-test-legacy");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("workspace.json");

        let (pid, a, b, c) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let json = format!(
            r#"{{
              "projects": [{{
                "id": "{pid}", "name": "demo", "root_path": "/tmp/demo",
                "layout": {{
                  "kind": "split", "direction": "horizontal", "sizes": [1.0, 1.0],
                  "children": [
                    {{"kind": "leaf", "instance": "{a}"}},
                    {{"kind": "split", "direction": "vertical", "sizes": [1.0, 1.0],
                      "children": [
                        {{"kind": "leaf", "instance": "{b}"}},
                        {{"kind": "leaf", "instance": "{c}"}}]}}]}}
              }}],
              "active_project": "{pid}",
              "instances": [
                {{"id": "{a}", "project_id": "{pid}", "title": "one"}},
                {{"id": "{b}", "project_id": "{pid}", "title": "two"}},
                {{"id": "{c}", "project_id": "{pid}", "title": "three"}}]
            }}"#
        );
        std::fs::write(&path, json).expect("write legacy json");

        let loaded = load_workspace_from(&path).expect("legacy workspace loads");
        let project = loaded.project(pid).expect("project");
        let layout = project.layout.as_ref().expect("layout");
        // All three instances are present, in reading order, each its own tab.
        assert_eq!(layout.collect_instances(), vec![a, b, c]);
        // A legacy leaf is now a single-tab group with active index 0.
        let path_a = layout.find_path(a).unwrap();
        assert_eq!(
            layout.get_at_path(&path_a).unwrap().tabs(),
            Some((&[a][..], 0))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_none() {
        let path = std::env::temp_dir().join("muxel-store-test-does-not-exist.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_workspace_from(&path).is_none());
    }

    #[test]
    fn settings_round_trip() {
        let dir = std::env::temp_dir().join("muxel-store-test-settings");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        let settings = Settings {
            default_use_tmux: true,
            notifications_enabled: false,
            default_preset: "Claude".to_string(),
            theme: "Tokyo Night".to_string(),
            theme_mode: "dark".to_string(),
            font_size: 16.0,
            ..Settings::default()
        };
        save_settings_to(&path, &settings).expect("save");
        let loaded = load_settings_from(&path);
        assert!(loaded.default_use_tmux);
        assert!(!loaded.notifications_enabled);
        assert_eq!(loaded.default_preset, "Claude");
        assert_eq!(loaded.theme, "Tokyo Night");
        assert_eq!(loaded.font_size, 16.0);
        // Presets default-seed and round-trip.
        assert!(!loaded.presets.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn accepted_terms_version_round_trips() {
        let dir = std::env::temp_dir().join("muxel-store-test-terms");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        // Defaults to 0 (never accepted).
        assert_eq!(Settings::default().accepted_terms_version, 0);
        let settings = Settings {
            accepted_terms_version: 7,
            ..Settings::default()
        };
        save_settings_to(&path, &settings).expect("save");
        assert_eq!(load_settings_from(&path).accepted_terms_version, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_settings_is_default() {
        let path = std::env::temp_dir().join("muxel-store-test-no-config.toml");
        let _ = std::fs::remove_file(&path);
        assert!(load_settings_from(&path).notifications_enabled);
    }

    #[test]
    fn profiles_index_round_trips() {
        let dir = std::env::temp_dir().join("muxel-store-test-profiles-rt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("profiles.json");
        let id = Uuid::new_v4();
        let index = ProfilesIndex {
            profiles: vec![ProfileMeta {
                id,
                name: "Work".to_string(),
            }],
            current: Some(id),
        };
        save_profiles_index_to(&path, &index).expect("save");
        let loaded = load_profiles_index_from(&path);
        assert_eq!(loaded.profiles.len(), 1);
        assert_eq!(loaded.profiles[0].name, "Work");
        assert_eq!(loaded.current, Some(id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_creates_default_from_legacy_workspace() {
        let base = std::env::temp_dir().join("muxel-store-test-migrate");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::create_dir_all(&base);

        let mut ws = Workspace::default();
        ws.add_project(Project::new("legacy", "/tmp/legacy"));
        save_workspace_to(&base.join("workspace.json"), &ws).expect("seed legacy");

        let index = migrate_profiles_at(&base);
        assert_eq!(index.profiles.len(), 1);
        assert_eq!(index.profiles[0].name, "Default");
        let id = index.profiles[0].id;
        assert_eq!(index.current, Some(id));

        // Legacy file moved into the profile directory.
        assert!(!base.join("workspace.json").exists());
        let moved = base
            .join("profiles")
            .join(id.to_string())
            .join("workspace.json");
        assert_eq!(load_workspace_from(&moved).unwrap().projects.len(), 1);

        // Re-running is idempotent (index already exists).
        assert_eq!(migrate_profiles_at(&base).profiles[0].id, id);
        let _ = std::fs::remove_dir_all(&base);
    }
}
