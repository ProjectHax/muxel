//! Persistence for muxel.
//!
//! The whole [`Workspace`] (projects, pane layouts, instance metadata) is saved
//! as a single JSON document under the platform data dir, written atomically
//! (temp file + rename). The dataset is small and loaded/saved wholesale, so a
//! file is simpler and just as durable as a database here; this module is the
//! seam to swap in SQLite later if querying/scale ever demands it.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use muxel_core::{Settings, Uuid, WindowGeom, Workspace, WorkspaceMeta, WorkspacesIndex};
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

/// Load settings from the default location: `Ok(None)` if missing (or the
/// config dir can't be resolved), `Err` if unreadable or invalid.
pub fn try_load_settings() -> Result<Option<Settings>> {
    match settings_path() {
        Some(path) => try_load_settings_from(&path),
        None => Ok(None),
    }
}

/// [`try_load_settings`] from an explicit path.
pub fn try_load_settings_from(path: &Path) -> Result<Option<Settings>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let settings = parse_settings(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(settings))
}

pub fn parse_settings(text: &str) -> Result<Settings> {
    Ok(toml::from_str(text)?)
}

/// Load settings from an explicit path (defaults on missing/invalid).
pub fn load_settings_from(path: &Path) -> Settings {
    match try_load_settings_from(path) {
        Ok(Some(settings)) => settings,
        Ok(None) => Settings::default(),
        Err(e) => {
            log::warn!("ignoring invalid config at {}: {e:#}", path.display());
            Settings::default()
        }
    }
}

/// Save settings to an explicit path as TOML, atomically: written and flushed
/// to a temporary sibling, then renamed over the target, so a reader never sees
/// a half-written file. A symlink (even broken) is written through to its real
/// file; on Unix the old permissions are kept. On failure the target is untouched.
pub fn save_settings_to(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(settings).context("serializing settings")?;
    // Write through a symlink to its real file instead of replacing the link.
    let target = resolve_symlinks(path);
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Unique per save, so two processes saving at once never share it.
    let tmp = target.with_file_name(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let written = write_synced(&tmp, text.as_bytes(), &target)
        .and_then(|()| rename_with_retry(&tmp, &target))
        .with_context(|| format!("writing {}", path.display()));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Most links followed when resolving a broken symlink chain.
const MAX_SYMLINK_HOPS: u32 = 40;

/// The real file behind `path`. A broken chain is followed with `read_link`, so
/// the save creates the missing file instead of replacing the link.
fn resolve_symlinks(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        let is_link = std::fs::symlink_metadata(&current)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_link {
            break;
        }
        let Ok(next) = std::fs::read_link(&current) else {
            break;
        };
        current = match current.parent() {
            Some(parent) if next.is_relative() => parent.join(next),
            _ => next,
        };
    }
    current
}

/// Write `bytes` to a new file at `tmp` and flush it. On Unix it takes `old`'s
/// permissions before any byte is written.
fn write_synced(tmp: &Path, bytes: &[u8], old: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(tmp)?;
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(old) {
        file.set_permissions(meta.permissions())?;
    }
    #[cfg(not(unix))]
    let _ = old;
    file.write_all(bytes)?;
    file.sync_all()
}

/// How many times a save retries a rename refused with `PermissionDenied`
/// (on Windows, a file briefly held open by an antivirus or an editor).
const RENAME_RETRIES: u32 = 3;
/// Pause between those retries.
const RENAME_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut retries = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied && retries < RENAME_RETRIES =>
            {
                retries += 1;
                std::thread::sleep(RENAME_RETRY_DELAY);
            }
            res => return res,
        }
    }
}

/// The muxel data directory (e.g. `~/.local/share/muxel` on Linux).
pub fn data_dir() -> Option<PathBuf> {
    ProjectDirs::from("dev", "muxel", "muxel").map(|d| d.data_dir().to_path_buf())
}

/// `LIB_DIR` = `<data_dir>/libraries`; each library is cloned into
/// `LIB_DIR/<library id>`. Not created here.
pub fn libraries_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("libraries"))
}

/// Path to the persisted workspace document.
pub fn workspace_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("workspace.json"))
}

/// Try to take the single-instance lock for one **workspace** (keyed by its id).
/// Returns the locked file on success — **keep it alive while that workspace is
/// open** (the OS releases the advisory lock when the file is dropped or the
/// process exits, even on a crash, so no stale lock survives). `None` means
/// another muxel already has *this* workspace open, so the caller should refuse
/// to open it and not clobber its `workspace.json`. Locking is per-workspace, so
/// two muxel processes can run side by side on different workspaces. A lock the
/// platform can't support is treated as "available" rather than blocking the user.
pub fn try_lock_workspace(id: Uuid) -> Option<std::fs::File> {
    let dir = workspaces_dir()?.join(id.to_string());
    try_lock_dir(&dir)
}

/// Lock core, parameterized by the workspace dir for testability. Creates the dir
/// and takes an advisory lock on its `instance.lock`; `None` if another open file
/// handle already holds it.
fn try_lock_dir(dir: &Path) -> Option<std::fs::File> {
    let _ = std::fs::create_dir_all(dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("instance.lock"))
        .ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(std::fs::TryLockError::WouldBlock) => None,
        Err(std::fs::TryLockError::Error(_)) => Some(file),
    }
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

/// Path to the append-only event log (pane exits, closes, recoveries). The GUI
/// often runs with stderr discarded (AppImage/desktop launch), so lifecycle
/// events that matter after the fact are recorded here.
pub fn event_log_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("muxel.log"))
}

/// Rotation threshold for the event log: past this size it is renamed to
/// `muxel.log.1` (replacing the previous rotation) and a fresh file starts.
const EVENT_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Append a timestamped line to the event log at the default location.
/// Best-effort by design: diagnostics must never break the app, so failures
/// are ignored (there is nowhere better to report them).
pub fn append_event_log(line: &str) {
    if let Some(path) = event_log_path() {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z");
        let _ = append_event_log_to(&path, &format!("{stamp} {line}"));
    }
}

/// Append one line to the log at `path`, rotating first when it has grown past
/// [`EVENT_LOG_MAX_BYTES`].
pub fn append_event_log_to(path: &Path, line: &str) -> Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating log dir {}", parent.display()))?;
    }
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() >= EVENT_LOG_MAX_BYTES
    {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, PathBuf::from(rotated));
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(file, "{line}").with_context(|| format!("appending to {}", path.display()))?;
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

// ---- Workspaces: each owns its own projects + layout; settings stay global. ----

/// The workspaces directory (`<data_dir>/workspaces`).
pub fn workspaces_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("workspaces"))
}

/// Directory holding downloaded speech-to-text models (`<data_dir>/models`),
/// created on demand. `None` if the platform data dir can't be resolved.
pub fn models_dir() -> Option<PathBuf> {
    let dir = data_dir()?.join("models");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Path to a workspace's saved document.
pub fn workspace_doc_path(id: Uuid) -> Option<PathBuf> {
    workspaces_dir().map(|d| d.join(id.to_string()).join("workspace.json"))
}

/// Path to the workspaces index document.
pub fn workspaces_index_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("workspaces.json"))
}

/// Load the workspaces index (empty if missing/invalid).
pub fn load_workspaces_index() -> WorkspacesIndex {
    workspaces_index_path()
        .map(|p| load_workspaces_index_from(&p))
        .unwrap_or_default()
}

/// Load a workspaces index from an explicit path.
pub fn load_workspaces_index_from(path: &Path) -> WorkspacesIndex {
    match std::fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
            log::warn!(
                "ignoring invalid workspaces index at {}: {e}",
                path.display()
            );
            WorkspacesIndex::default()
        }),
        Err(_) => WorkspacesIndex::default(),
    }
}

/// Save the workspaces index to the default location.
pub fn save_workspaces_index(index: &WorkspacesIndex) -> Result<()> {
    let path = workspaces_index_path().context("could not determine data directory")?;
    save_workspaces_index_to(&path, index)
}

/// Save a workspaces index to an explicit path, atomically (temp file + rename).
pub fn save_workspaces_index_to(path: &Path, index: &WorkspacesIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating data dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(index).context("serializing workspaces index")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Ensure a workspaces index exists. On first run, migrate a legacy single
/// `workspace.json` into a "Default" workspace (or seed an empty one).
pub fn migrate_to_workspaces() -> WorkspacesIndex {
    match data_dir() {
        Some(base) => migrate_workspaces_at(&base),
        None => WorkspacesIndex::default(),
    }
}

/// Migration core, parameterized by the data dir for testability.
fn migrate_workspaces_at(base: &Path) -> WorkspacesIndex {
    // Carry the pre-rename on-disk layout (`profiles.json` + `profiles/`) over to
    // the new names, once, so existing setups aren't lost. Each is independent in
    // case only one moved last time.
    if base.join("profiles.json").exists() && !base.join("workspaces.json").exists() {
        let _ = std::fs::rename(base.join("profiles.json"), base.join("workspaces.json"));
    }
    if base.join("profiles").is_dir() && !base.join("workspaces").exists() {
        let _ = std::fs::rename(base.join("profiles"), base.join("workspaces"));
    }
    let index_path = base.join("workspaces.json");
    if index_path.exists() {
        return load_workspaces_index_from(&index_path);
    }
    let id = Uuid::new_v4();
    let index = WorkspacesIndex {
        workspaces: vec![WorkspaceMeta {
            id,
            name: "Default".to_string(),
        }],
        current: Some(id),
    };
    // Move the legacy single workspace.json into the new workspace, if present.
    let legacy = base.join("workspace.json");
    let dest = base
        .join("workspaces")
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
    let _ = save_workspaces_index_to(&index_path, &index);
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxel_core::{
        AgentPreset, EnvVar, Instance, InstanceKind, PaneNode, Project, Settings, Uuid, WindowGeom,
        WorkspaceMeta, WorkspacesIndex,
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
    fn workspaces_index_round_trips() {
        let dir = std::env::temp_dir().join("muxel-store-test-workspaces-rt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("workspaces.json");
        let id = Uuid::new_v4();
        let index = WorkspacesIndex {
            workspaces: vec![WorkspaceMeta {
                id,
                name: "Work".to_string(),
            }],
            current: Some(id),
        };
        save_workspaces_index_to(&path, &index).expect("save");
        let loaded = load_workspaces_index_from(&path);
        assert_eq!(loaded.workspaces.len(), 1);
        assert_eq!(loaded.workspaces[0].name, "Work");
        assert_eq!(loaded.current, Some(id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_lock_is_per_workspace() {
        let base = std::env::temp_dir().join("muxel-store-test-lock");
        let _ = std::fs::remove_dir_all(&base);
        let a = base.join("ws-a");
        let b = base.join("ws-b");

        // Hold workspace A's lock.
        let lock_a = try_lock_dir(&a).expect("first lock on A succeeds");
        // A second muxel can't take A while it's held...
        assert!(try_lock_dir(&a).is_none(), "A is already locked");
        // ...but a *different* workspace locks independently.
        let lock_b = try_lock_dir(&b).expect("B locks despite A being held");

        // Releasing A (process exit / workspace switch drops the handle) frees it.
        drop(lock_a);
        let lock_a2 = try_lock_dir(&a).expect("A is lockable again once released");

        drop(lock_b);
        drop(lock_a2);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migration_renames_legacy_profiles_dir_and_index() {
        let base = std::env::temp_dir().join("muxel-store-test-migrate-rename");
        let _ = std::fs::remove_dir_all(&base);
        let id = Uuid::new_v4();
        // Lay down the pre-rename on-disk shape (profiles.json + profiles/<id>/).
        std::fs::create_dir_all(base.join("profiles").join(id.to_string())).unwrap();
        std::fs::write(
            base.join("profiles.json"),
            format!(r#"{{"profiles":[{{"id":"{id}","name":"Legacy"}}],"current":"{id}"}}"#),
        )
        .unwrap();
        std::fs::write(
            base.join("profiles")
                .join(id.to_string())
                .join("workspace.json"),
            "{}",
        )
        .unwrap();

        let index = migrate_workspaces_at(&base);

        // Renamed on disk; the old paths are gone.
        assert!(base.join("workspaces.json").exists());
        assert!(
            base.join("workspaces")
                .join(id.to_string())
                .join("workspace.json")
                .exists()
        );
        assert!(!base.join("profiles.json").exists());
        assert!(!base.join("profiles").exists());
        // The legacy "profiles" key was read through the alias — list preserved.
        assert_eq!(index.workspaces.len(), 1);
        assert_eq!(index.workspaces[0].name, "Legacy");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migration_creates_default_from_legacy_workspace() {
        let base = std::env::temp_dir().join("muxel-store-test-migrate");
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::create_dir_all(&base);

        let mut ws = Workspace::default();
        ws.add_project(Project::new("legacy", "/tmp/legacy"));
        save_workspace_to(&base.join("workspace.json"), &ws).expect("seed legacy");

        let index = migrate_workspaces_at(&base);
        assert_eq!(index.workspaces.len(), 1);
        assert_eq!(index.workspaces[0].name, "Default");
        let id = index.workspaces[0].id;
        assert_eq!(index.current, Some(id));

        // Legacy file moved into the workspace directory.
        assert!(!base.join("workspace.json").exists());
        let moved = base
            .join("workspaces")
            .join(id.to_string())
            .join("workspace.json");
        assert_eq!(load_workspace_from(&moved).unwrap().projects.len(), 1);

        // Re-running is idempotent (index already exists).
        assert_eq!(migrate_workspaces_at(&base).workspaces[0].id, id);
        let _ = std::fs::remove_dir_all(&base);
    }

    use muxel_core::library::hub::LibraryHub;
    use muxel_core::library::{
        LibraryConfig, LoopContent, RunnerContent, SharedLoopState, SharedRunnerConfirmation,
    };
    use muxel_core::{LoopSchedule, PostRunAction};

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn persist_roundtrip(hub: &LibraryHub, settings: &mut Settings, path: &Path) -> LibraryHub {
        hub.write_into(settings);
        save_settings_to(path, settings).expect("save settings");
        let mut loaded = try_load_settings_from(path)
            .expect("config parses")
            .expect("config exists");
        LibraryHub::take_from(&mut loaded)
    }

    /// A `config.toml` as written before team libraries existed.
    const PRE_LIBRARIES_CONFIG: &str = r#"
theme = "Tokyo Night"
font_size = 15.0

[[runners]]
id = "11111111-1111-1111-1111-111111111111"
name = "Review"
preset_id = "22222222-2222-2222-2222-222222222222"
auto_mode_presses = 2
prompt = "Review the diff\n\n{{input}}"

[[runners]]
id = "11111111-1111-1111-1111-111111111112"
name = "Plain"
auto_mode_presses = 0
prompt = "just do it"

[[snippets]]
id = "33333333-3333-3333-3333-333333333331"
name = "Continue"
text = "continue"
submit = true

[[snippets]]
id = "33333333-3333-3333-3333-333333333332"
name = "Plan"
text = "outline first"
submit = false

[[loops]]
id = "44444444-4444-4444-4444-444444444441"
name = "Nightly"
project_id = "55555555-5555-5555-5555-555555555555"
auto_mode_presses = 1
prompt = "run the nightly checks"
post_run = "exit"
enabled = false
last_run = 1700000000

[loops.schedule]
kind = "daily_at"
hour = 3
minute = 30
"#;

    fn assert_pre_libraries_contents(s: &Settings) {
        let uuid = |t: &str| Uuid::parse_str(t).unwrap();
        assert_eq!(s.runners.len(), 2);
        let r = &s.runners[0];
        assert_eq!(r.id, uuid("11111111-1111-1111-1111-111111111111"));
        assert_eq!(r.name, "Review");
        assert_eq!(
            r.preset_id,
            Some(uuid("22222222-2222-2222-2222-222222222222"))
        );
        assert_eq!(r.auto_mode_presses, 2);
        assert_eq!(r.prompt, "Review the diff\n\n{{input}}");
        let r = &s.runners[1];
        assert_eq!(r.id, uuid("11111111-1111-1111-1111-111111111112"));
        assert_eq!(r.name, "Plain");
        assert_eq!(r.preset_id, None);
        assert_eq!(r.auto_mode_presses, 0);
        assert_eq!(r.prompt, "just do it");
        assert_eq!(s.snippets.len(), 2);
        let sn = &s.snippets[0];
        assert_eq!(sn.id, uuid("33333333-3333-3333-3333-333333333331"));
        assert_eq!(sn.name, "Continue");
        assert_eq!(sn.text, "continue");
        assert!(sn.submit);
        let sn = &s.snippets[1];
        assert_eq!(sn.id, uuid("33333333-3333-3333-3333-333333333332"));
        assert_eq!(sn.name, "Plan");
        assert_eq!(sn.text, "outline first");
        assert!(!sn.submit);
        assert_eq!(s.loops.len(), 1);
        let l = &s.loops[0];
        assert_eq!(l.id, uuid("44444444-4444-4444-4444-444444444441"));
        assert_eq!(l.name, "Nightly");
        assert_eq!(l.preset_id, None);
        assert_eq!(l.project_id, uuid("55555555-5555-5555-5555-555555555555"));
        assert_eq!(l.auto_mode_presses, 1);
        assert_eq!(l.prompt, "run the nightly checks");
        assert_eq!(
            l.schedule,
            LoopSchedule::DailyAt {
                hour: 3,
                minute: 30
            }
        );
        assert_eq!(l.post_run, PostRunAction::Exit);
        assert!(!l.enabled);
        assert_eq!(l.last_run, Some(1_700_000_000));
        assert!(s.libraries.is_empty());
        assert!(s.shared_loops.is_empty());
        assert!(s.shared_runner_confirmations.is_empty());
    }

    #[test]
    fn pre_libraries_config_loads_with_load_settings_from() {
        let dir = fresh_dir("muxel-store-test-pre-libraries-load");
        let path = dir.join("config.toml");
        std::fs::write(&path, PRE_LIBRARIES_CONFIG).unwrap();
        let s = load_settings_from(&path);
        assert_eq!(s.theme, "Tokyo Night");
        assert_eq!(s.font_size, 15.0);
        assert_pre_libraries_contents(&s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pre_libraries_config_loads_with_try_load_settings_from() {
        let dir = fresh_dir("muxel-store-test-pre-libraries-try");
        let path = dir.join("config.toml");
        std::fs::write(&path, PRE_LIBRARIES_CONFIG).unwrap();
        let mut s = try_load_settings_from(&path)
            .expect("no error")
            .expect("file present");
        assert_pre_libraries_contents(&s);
        let hub = LibraryHub::take_from(&mut s);
        assert!(hub.configs.is_empty());
        assert!(hub.shared_loops.is_empty());
        assert!(hub.runner_confirmations.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second hard link to the old file keeps the old contents, which an
    /// in-place write would change.
    #[test]
    fn save_settings_to_replaces_the_file_atomically() {
        let dir = fresh_dir("muxel-store-test-atomic-settings");
        let path = dir.join("config.toml");
        const OLD: &str = "theme = \"Old theme with a long name\"\nfont_size = 13.0\n";
        std::fs::write(&path, OLD).unwrap();
        let link = dir.join("old-link.toml");
        std::fs::hard_link(&path, &link).unwrap();

        let settings = Settings {
            theme: "New".to_string(),
            font_size: 19.0,
            snippets: vec![muxel_core::Snippet {
                id: Uuid::from_u128(7),
                name: "S".to_string(),
                text: "x".repeat(10_000),
                submit: false,
            }],
            ..Settings::default()
        };
        save_settings_to(&path, &settings).expect("save");

        let old_link = std::fs::read_to_string(&link).unwrap();
        assert!(old_link == OLD, "the old file was overwritten in place");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("theme = \"New\""), "{text}");
        let loaded = try_load_settings_from(&path).unwrap().unwrap();
        assert_eq!(loaded.theme, "New");
        assert_eq!(loaded.font_size, 19.0);
        assert_eq!(loaded.snippets.len(), 1);
        assert_eq!(loaded.snippets[0].text.len(), 10_000);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["config.toml", "old-link.toml"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_settings_to_failure_is_err_and_leaves_no_temp_file() {
        let dir = fresh_dir("muxel-store-test-atomic-settings-err");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&path).unwrap();
        assert!(save_settings_to(&path, &Settings::default()).is_err());
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["config.toml"]);
        assert!(path.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_settings_to_writes_through_a_symlink() {
        let dir = fresh_dir("muxel-store-test-atomic-settings-symlink");
        let real_dir = dir.join("dotfiles");
        let link_dir = dir.join("config");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::create_dir_all(&link_dir).unwrap();
        let real = real_dir.join("muxel.toml");
        std::fs::write(&real, "theme = \"Old\"\n").unwrap();
        let link = link_dir.join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let settings = Settings {
            theme: "New".to_string(),
            ..Settings::default()
        };
        save_settings_to(&link, &settings).expect("save");

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_link(&link).unwrap(), real);
        let loaded = try_load_settings_from(&real).unwrap().unwrap();
        assert_eq!(loaded.theme, "New");
        let names = |d: &Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        assert_eq!(names(&real_dir), vec!["muxel.toml"]);
        assert_eq!(names(&link_dir), vec!["config.toml"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_settings_to_keeps_the_old_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = fresh_dir("muxel-store-test-atomic-settings-perms");
        // The old mode is chosen to differ from a fresh save's, whatever the umask.
        let fresh = dir.join("fresh.toml");
        save_settings_to(&fresh, &Settings::default()).expect("save fresh");
        let fresh_mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        let old_mode = if fresh_mode == 0o600 { 0o640 } else { 0o600 };

        let path = dir.join("config.toml");
        std::fs::write(&path, "theme = \"Old\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(old_mode)).unwrap();

        save_settings_to(&path, &Settings::default()).expect("save");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, old_mode);
        assert_ne!(mode, fresh_mode);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_settings_to_writes_through_a_broken_symlink() {
        let dir = fresh_dir("muxel-store-test-atomic-settings-broken-symlink");
        let real_dir = dir.join("dotfiles");
        let link_dir = dir.join("config");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("config.toml");
        let relative = Path::new("../dotfiles/muxel.toml");
        std::os::unix::fs::symlink(relative, &link).unwrap();
        let real = real_dir.join("muxel.toml");
        assert!(!real.exists());

        let settings = Settings {
            theme: "New".to_string(),
            ..Settings::default()
        };
        save_settings_to(&link, &settings).expect("save");

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_link(&link).unwrap(), relative);
        assert_eq!(try_load_settings_from(&real).unwrap().unwrap().theme, "New");
        let names = |d: &Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        assert_eq!(names(&real_dir), vec!["muxel.toml"]);
        assert_eq!(names(&link_dir), vec!["config.toml"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Open `path` with no sharing, so a rename over it fails while the handle lives.
    #[cfg(windows)]
    fn lock_exclusively(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap()
    }

    /// The lock is released at about 20 ms, well inside the retry window.
    #[cfg(windows)]
    #[test]
    fn save_settings_to_retries_a_briefly_locked_target() {
        let dir = fresh_dir("muxel-store-test-atomic-settings-locked-brief");
        let path = dir.join("config.toml");
        std::fs::write(&path, "theme = \"Old\"\n").unwrap();
        let lock = lock_exclusively(&path);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            drop(lock);
        });
        let settings = Settings {
            theme: "New".to_string(),
            ..Settings::default()
        };
        let res = save_settings_to(&path, &settings);
        release.join().unwrap();
        res.expect("save after the lock is released");
        assert_eq!(try_load_settings_from(&path).unwrap().unwrap().theme, "New");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn save_settings_to_gives_up_on_a_locked_target() {
        let dir = fresh_dir("muxel-store-test-atomic-settings-locked");
        let path = dir.join("config.toml");
        const OLD: &str = "theme = \"Old\"\n";
        std::fs::write(&path, OLD).unwrap();
        let lock = lock_exclusively(&path);
        let res = save_settings_to(&path, &Settings::default());
        drop(lock);
        assert!(res.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), OLD);
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["config.toml"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_load_settings_from_missing_is_ok_none() {
        let dir = fresh_dir("muxel-store-test-try-missing");
        let path = dir.join("config.toml");
        assert!(matches!(try_load_settings_from(&path), Ok(None)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_load_settings_from_invalid_is_err_and_load_still_defaults() {
        let dir = fresh_dir("muxel-store-test-try-invalid");
        let path = dir.join("config.toml");
        std::fs::write(&path, "font_size = [this is not toml").unwrap();
        assert!(try_load_settings_from(&path).is_err());
        let s = load_settings_from(&path);
        assert!(s.notifications_enabled);
        assert!(s.libraries.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_load_settings_from_valid_is_ok_some() {
        let dir = fresh_dir("muxel-store-test-try-valid");
        let path = dir.join("config.toml");
        std::fs::write(&path, "font_size = 21.0\n").unwrap();
        let s = try_load_settings_from(&path)
            .expect("valid")
            .expect("present");
        assert_eq!(s.font_size, 21.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn libraries_dir_is_data_dir_libraries() {
        let lib = libraries_dir().expect("platform data dir");
        assert_eq!(lib.file_name().and_then(|n| n.to_str()), Some("libraries"));
        assert_eq!(lib.parent().map(Path::to_path_buf), data_dir());
    }

    const APPROVED_LOOP_PROMPT: &str = "approved loop prompt 7f3a-unique";
    const CONFIRMED_RUNNER_PROMPT: &str = "confirmed runner prompt 9c1b-unique";

    #[test]
    fn shared_state_round_trips_through_config_not_workspace() {
        let dir = fresh_dir("muxel-store-test-shared-state");
        let path = dir.join("config.toml");

        let lib_id = Uuid::from_u128(0x11B);
        let project_p = Uuid::from_u128(0xBEEF);
        let run_id = Uuid::from_u128(0x5EED);
        const L: u64 = 1_750_000_123;
        const T: u64 = 1_760_000_456;

        let mut hub = LibraryHub::default();
        hub.configs.push(LibraryConfig {
            id: lib_id,
            url: "file:///tmp/R".to_string(),
            branch: "main".to_string(),
            name: "Team".to_string(),
            last_pull_ok: Some(T),
        });
        hub.shared_loops.push(SharedLoopState {
            library: lib_id,
            name: "Nightly".to_string(),
            run_id,
            project_id: project_p,
            last_run: Some(L),
            approved: LoopContent {
                prompt: APPROVED_LOOP_PROMPT.to_string(),
                preset: Some("Claude".to_string()),
                auto_mode_presses: 3,
                schedule: LoopSchedule::EveryMinutes { minutes: 15 },
                post_run: PostRunAction::Exit,
            },
            pinned_preset_id: None,
        });
        let pinned_c = Uuid::from_u128(0xC1);
        hub.shared_loops.push(SharedLoopState {
            library: lib_id,
            name: "Hourly".to_string(),
            run_id: Uuid::from_u128(0x5EEE),
            project_id: project_p,
            last_run: Some(L),
            approved: LoopContent {
                prompt: "hourly".to_string(),
                preset: None,
                auto_mode_presses: 0,
                schedule: LoopSchedule::EveryHours { hours: 1 },
                post_run: PostRunAction::Leave,
            },
            pinned_preset_id: Some(pinned_c),
        });
        hub.runner_confirmations.push(SharedRunnerConfirmation {
            library: lib_id,
            name: "Review".to_string(),
            confirmed: RunnerContent {
                prompt: CONFIRMED_RUNNER_PROMPT.to_string(),
                preset: None,
                auto_mode_presses: 1,
            },
        });

        let mut settings = Settings::default();
        let loaded = persist_roundtrip(&hub, &mut settings, &path);

        assert_eq!(loaded.configs, hub.configs);
        assert_eq!(loaded.shared_loops, hub.shared_loops);
        assert_eq!(loaded.runner_confirmations, hub.runner_confirmations);
        assert_eq!(loaded.shared_loops.len(), 2);
        assert_eq!(loaded.shared_loops[0].pinned_preset_id, None);
        assert_eq!(loaded.shared_loops[1].pinned_preset_id, Some(pinned_c));
        let lp = &loaded.shared_loops[0];
        assert_eq!(lp.project_id, project_p);
        assert_eq!(lp.last_run, Some(L));
        assert_eq!(lp.approved.prompt, APPROVED_LOOP_PROMPT);
        assert_eq!(
            lp.approved.schedule,
            LoopSchedule::EveryMinutes { minutes: 15 }
        );
        assert_eq!(
            loaded.runner_confirmations[0].confirmed.prompt,
            CONFIRMED_RUNNER_PROMPT
        );
        assert_eq!(loaded.configs[0].last_pull_ok, Some(T));

        let toml_text = std::fs::read_to_string(&path).unwrap();
        assert!(toml_text.contains(APPROVED_LOOP_PROMPT));
        assert!(toml_text.contains(CONFIRMED_RUNNER_PROMPT));
        assert!(toml_text.contains(&format!("pinned_preset_id = \"{pinned_c}\"")));
        let mut workspace = Workspace::default();
        let mut project = Project::new("demo", "/tmp/demo");
        project.id = project_p;
        workspace.add_project(project);
        let ws_json = serde_json::to_string(&workspace).unwrap();
        assert!(!ws_json.contains(APPROVED_LOOP_PROMPT));
        assert!(!ws_json.contains(CONFIRMED_RUNNER_PROMPT));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_hub_overwrites_previous_shared_state() {
        // A previous `[[shared_loops]]` left in the settings does not survive.
        let dir = fresh_dir("muxel-store-test-shared-state-off");
        let path = dir.join("config.toml");
        let mut settings = Settings::default();
        settings.shared_loops.push(SharedLoopState {
            library: Uuid::from_u128(1),
            name: "Old".to_string(),
            run_id: Uuid::from_u128(2),
            project_id: Uuid::from_u128(3),
            last_run: None,
            approved: LoopContent {
                prompt: "old".to_string(),
                preset: None,
                auto_mode_presses: 0,
                schedule: LoopSchedule::EveryHours { hours: 1 },
                post_run: PostRunAction::Leave,
            },
            pinned_preset_id: None,
        });
        let loaded = persist_roundtrip(&LibraryHub::default(), &mut settings, &path);
        assert!(loaded.configs.is_empty());
        assert!(loaded.shared_loops.is_empty());
        assert!(loaded.runner_confirmations.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn event_log_appends_and_rotates() {
        let dir = std::env::temp_dir().join("muxel-store-test-eventlog");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("muxel.log");

        append_event_log_to(&path, "first").expect("append creates the file");
        append_event_log_to(&path, "second").expect("append");
        let text = std::fs::read_to_string(&path).expect("read log");
        assert_eq!(text, "first\nsecond\n");

        // Grow past the rotation threshold: the next append moves the old file
        // to `.1` and starts fresh.
        std::fs::write(&path, "x".repeat(EVENT_LOG_MAX_BYTES as usize)).expect("grow");
        append_event_log_to(&path, "fresh").expect("append after rotation");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "fresh\n");
        let rotated =
            std::fs::read_to_string(dir.join("muxel.log.1")).expect("rotated file exists");
        assert!(rotated.starts_with('x'));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
