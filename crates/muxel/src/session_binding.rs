//! Exact provider session bindings that must survive an in-process `/resume`.

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::{Hash as _, Hasher as _};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

use muxel_core::muse::HOOK_FLAG as MUSE_HOOK_FLAG;
const MUSE_BINDING_DIR: &str = "provider-session-bindings/muse";
const MUSE_HOOK_WORK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(serde::Serialize, serde::Deserialize)]
struct MuseLaunch {
    #[serde(default)]
    launch_id: Uuid,
    #[serde(default)]
    session_root: PathBuf,
    #[serde(default)]
    context: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct MuseBinding {
    #[serde(default)]
    launch_id: Uuid,
    #[serde(default)]
    metadata: muxel_core::muse::MuseMetadata,
}

fn muse_pane_dir(data_dir: &Path, instance_id: Uuid) -> PathBuf {
    data_dir
        .join(MUSE_BINDING_DIR)
        .join(instance_id.to_string())
}

fn write_muse_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let parent = path.parent().context("Muse binding has no parent")?;
    std::fs::create_dir_all(parent)?;
    replace_muse_json(path, value)
}

fn replace_muse_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    std::fs::write(&temp, serde_json::to_vec(value)?)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

fn muse_env(name: &str, env: &[(String, String)]) -> Option<OsString> {
    env.iter()
        .rev()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| OsString::from(value))
        .or_else(|| std::env::var_os(name))
        .filter(|value| !value.is_empty())
}

fn muse_home(env: &[(String, String)]) -> Option<PathBuf> {
    muse_env("HOME", env)
        .or_else(|| muse_env("USERPROFILE", env))
        .map(PathBuf::from)
}

pub(crate) fn muse_data_dir(env: &[(String, String)]) -> Option<PathBuf> {
    muse_env("XDG_DATA_HOME", env)
        .map(PathBuf::from)
        .or_else(|| muse_home(env).map(|home| home.join(".local/share")))
        .map(|root| root.join("muse"))
}

/// Read directory names and stat one exact UUID candidate per date. Never open
/// session.jsonl or pick a sibling conversation by cwd, title or timestamp.
pub(crate) fn muse_session_exists(root: &Path, session_id: &str) -> std::io::Result<bool> {
    if Uuid::parse_str(session_id).is_err() {
        return Ok(false);
    }
    fn date_dirs(root: &Path, digits: usize) -> std::io::Result<Vec<PathBuf>> {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut dirs = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_name().to_str().is_some_and(|name| {
                name.len() == digits && name.bytes().all(|b| b.is_ascii_digit())
            }) && entry.file_type()?.is_dir()
            {
                dirs.push(entry.path());
            }
        }
        Ok(dirs)
    }
    let store = root.join("sessions");
    for year in date_dirs(&store, 4)? {
        for month in date_dirs(&year, 2)? {
            for day in date_dirs(&month, 2)? {
                let candidate = day.join(session_id).join("session.jsonl");
                let exists = match std::fs::metadata(&candidate) {
                    Ok(metadata) => metadata.is_file(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(error),
                };
                if exists
                    && let Ok(relative) = candidate.strip_prefix(&store)
                    && muxel_core::muse::session_log(session_id, &[relative.to_path_buf()])
                        .is_some()
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// A separate managed source leaves native user/project hook loading intact.
/// Existing managed handlers are copied verbatim before adding our handlers.
pub(crate) fn prepare_muse_hooks(
    data_dir: &Path,
    instance_id: Uuid,
    env: &[(String, String)],
    args: &[String],
    context: Option<&str>,
    reattaching: bool,
) -> Option<String> {
    let prepared = prepare_muse_hooks_inner(data_dir, instance_id, env, args, context, reattaching);
    if !reattaching && !matches!(&prepared, Ok(Some(_))) {
        // A plain replacement process cannot produce the previous launch's
        // hooks. Do not let its old metadata masquerade as a current binding.
        invalidate_muse_launch(data_dir, instance_id);
    }
    match prepared {
        Ok(argument) => argument,
        Err(error) => {
            log::warn!("Muse pane session hooks unavailable: {error:#}");
            None
        }
    }
}

fn prepare_muse_hooks_inner(
    data_dir: &Path,
    instance_id: Uuid,
    env: &[(String, String)],
    args: &[String],
    context: Option<&str>,
    reattaching: bool,
) -> Result<Option<String>> {
    let pane_dir = muse_pane_dir(data_dir, instance_id);
    let hooks_path = pane_dir.join("hooks.json");
    let previous_nonce = reattaching
        .then(|| {
            serde_json::from_slice::<MuseLaunch>(&std::fs::read(pane_dir.join("launch.json")).ok()?)
                .ok()
                .map(|launch| launch.launch_id)
        })
        .flatten();
    match muxel_core::muse::hook_launch(reattaching, previous_nonce) {
        muxel_core::muse::HookLaunch::Reattach(_) => {
            if !hooks_path.is_file() {
                bail!(
                    "surviving Muse tmux process has no managed hook file; leaving its launch unchanged"
                );
            }
            // tmux -A ignores this command when attaching. Preserve the exact
            // launch, context, managed file and nonce already held by Muse.
            return Ok(Some(muse_hook_argument(&hooks_path)?));
        }
        muxel_core::muse::HookLaunch::Unmanaged => {
            bail!(
                "surviving Muse tmux process has no known hook launch; cannot install hooks by reattaching"
            );
        }
        muxel_core::muse::HookLaunch::Start => {}
    }
    // A custom invocation may replace the same source. Preserve its meaning
    // rather than silently winning a duplicate -c override.
    if muxel_core::muse::has_hook_override(args) {
        return Ok(None);
    }
    let config = muse_env("XDG_CONFIG_HOME", env)
        .map(PathBuf::from)
        .or_else(|| muse_home(env).map(|home| home.join(".config")))
        .context("could not resolve Muse configuration root")?;
    let settings_path = config.join("muse/settings.json");
    let settings: Value = match std::fs::read(&settings_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("parsing Muse settings")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error).context("reading Muse settings"),
    };
    let prior = match settings
        .get("managed_hooks_path")
        .filter(|value| !value.is_null())
    {
        Some(value) => {
            let path = PathBuf::from(
                value
                    .as_str()
                    .context("managed_hooks_path must be a string")?,
            );
            let path = if path.is_absolute() {
                path
            } else {
                settings_path.parent().unwrap().join(path)
            };
            serde_json::from_slice(
                &std::fs::read(path).context("reading existing managed Muse hooks")?,
            )
            .context("parsing existing managed Muse hooks")?
        }
        None => json!({}),
    };
    let executable = std::env::current_exe()?;
    let exe = executable
        .to_str()
        .context("Muse hook executable path is not UTF-8")?;
    let data = data_dir
        .to_str()
        .context("Muse hook data path is not UTF-8")?;
    let nonce = Uuid::new_v4();
    let command = muxel_core::muse::hook_command(exe, data, instance_id, nonce, cfg!(windows));
    let document = muxel_core::muse::merge_hook_document(prior, &command)
        .map_err(|error| anyhow::anyhow!(match error {
            muxel_core::muse::HookDocumentError::StrictSchema =>
                "existing managed Muse hooks use the strict name/timeout_ms schema; launching without Muxel hook integration",
            muxel_core::muse::HookDocumentError::Malformed =>
                "existing managed Muse hook document is malformed",
        }))?;
    let launch = MuseLaunch {
        launch_id: nonce,
        session_root: muse_data_dir(env).context("could not resolve Muse session root")?,
        context: context.map(str::to_string),
    };
    write_muse_json(&pane_dir.join("launch.json"), &launch)?;
    write_muse_json(&hooks_path, &document)?;
    cleanup_muse_launch_files(&pane_dir, Some(nonce))?;
    Ok(Some(muse_hook_argument(&hooks_path)?))
}

fn muse_hook_argument(path: &Path) -> Result<String> {
    Ok(format!(
        "managed_hooks_path={}",
        serde_json::to_string(path)?
    ))
}

fn cleanup_muse_launch_files(pane_dir: &Path, current: Option<Uuid>) -> Result<()> {
    for entry in std::fs::read_dir(pane_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let stale_nonce = path.extension().is_some_and(|ext| ext == "json")
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| Uuid::parse_str(stem).ok())
                .is_some_and(|nonce| Some(nonce) != current);
        if stale_nonce || path.extension().is_some_and(|ext| ext == "tmp") {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn invalidate_muse_launch(data_dir: &Path, instance_id: Uuid) {
    let pane = muse_pane_dir(data_dir, instance_id);
    // A user may explicitly reuse the generated hooks file as a managed source.
    // Declining that source must keep it intact; only launch metadata is ours
    // to invalidate here. Permanent pane closure removes the whole directory.
    let result = (|| -> Result<()> {
        match std::fs::remove_file(pane.join("launch.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        cleanup_muse_launch_files(&pane, None)
    })();
    if let Err(error) = result {
        log::warn!(
            "could not invalidate Muse pane launch {}: {error:#}",
            pane.display()
        );
    }
}

/// Remove only the permanently closed pane's provider directory.
pub(crate) fn clear_muse_binding(data_dir: &Path, instance_id: Uuid) {
    let path = muse_pane_dir(data_dir, instance_id);
    if let Err(error) = std::fs::remove_dir_all(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!(
            "could not remove closed Muse pane bindings {}: {error}",
            path.display()
        );
    }
}

/// Early CLI route; never initializes GPUI, even for malformed hook arguments.
/// The caller must exit immediately when this returns true. All hook I/O,
/// including stdin/stdout and filesystem access, runs in the bounded worker.
/// A stalled worker is abandoned by process::exit, never joined during shutdown.
pub(crate) fn run_muse_hook_from_args(args: &[OsString]) -> bool {
    if args.first().is_none_or(|arg| arg != MUSE_HOOK_FLAG) {
        return false;
    }
    // Only Unicode arguments can name the pane, data directory and launch.
    let args: Option<Vec<String>> = args
        .iter()
        .map(|arg| arg.to_str().map(str::to_string))
        .collect();
    wait_for_muse_hook(MUSE_HOOK_WORK_TIMEOUT, move || {
        // These observation/context hooks must not turn provider I/O errors
        // into a failed hook or add unbounded stderr writes to the exit path.
        if let Some(args) = args {
            let _ = run_muse_hook(&args);
        }
    });
    true
}

fn wait_for_muse_hook(
    timeout: std::time::Duration,
    action: impl FnOnce() + Send + 'static,
) -> bool {
    let (finished, completion) = std::sync::mpsc::channel();
    if std::thread::Builder::new()
        .name("muse-hook".into())
        .spawn(move || {
            action();
            let _ = finished.send(());
        })
        .is_err()
    {
        return false;
    }
    completion.recv_timeout(timeout).is_ok()
}

fn run_muse_hook(args: &[String]) -> Result<()> {
    if args.len() != 4 {
        bail!("{MUSE_HOOK_FLAG} requires pane UUID, data directory and launch UUID");
    }
    let instance = Uuid::parse_str(&args[1])?;
    let data = Path::new(&args[2]);
    if !data.is_absolute() {
        bail!("Muse hook data directory must be absolute");
    }
    let nonce = Uuid::parse_str(&args[3])?;
    let Some(root) = std::env::var_os("MUSE_SESSION_ID").and_then(|s| s.into_string().ok()) else {
        return Ok(());
    };
    if let Some(context) =
        write_muse_binding_from_reader(data, instance, nonce, &root, std::io::stdin().lock())?
    {
        println!(
            "{}",
            json!({"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":context}})
        );
    }
    Ok(())
}

pub(crate) fn write_muse_binding_from_reader(
    data: &Path,
    instance: Uuid,
    nonce: Uuid,
    root: &str,
    reader: impl std::io::Read,
) -> Result<Option<String>> {
    let pane = muse_pane_dir(data, instance);
    let launch: MuseLaunch = serde_json::from_slice(&std::fs::read(pane.join("launch.json"))?)?;
    if launch.launch_id != nonce {
        return Ok(None);
    }
    let event: muxel_core::muse::MuseEvent = serde_json::from_reader(reader.take(1024 * 1024))?;
    let Some(metadata) = muxel_core::muse::metadata(event, root) else {
        return Ok(None);
    };
    if !metadata.cwd.is_absolute() {
        bail!("Muse hook cwd must be absolute");
    }
    let start = metadata.state == muxel_core::muse::MuseState::Idle;
    // A late old process writes only its own launch file, never the replacement's.
    // Only allowlisted metadata is serialized, never raw hook input.
    // A pane can close while the helper reads stdin. Do not recreate its
    // removed directory when that late handler finishes.
    replace_muse_json(
        &pane.join(format!("{nonce}.json")),
        &MuseBinding {
            launch_id: nonce,
            metadata,
        },
    )?;
    Ok(if start { launch.context } else { None })
}

/// The current launch's latest hook metadata and when it was recorded. A new
/// conversation must exist in the launch's session store.
pub(crate) fn read_muse_binding(
    data_dir: &Path,
    instance_id: Uuid,
    current: Option<&str>,
) -> Option<(muxel_core::muse::MuseMetadata, Option<SystemTime>)> {
    let pane = muse_pane_dir(data_dir, instance_id);
    let launch: MuseLaunch =
        serde_json::from_slice(&std::fs::read(pane.join("launch.json")).ok()?).ok()?;
    let mut record = std::fs::File::open(pane.join(format!("{}.json", launch.launch_id))).ok()?;
    let modified = record.metadata().and_then(|meta| meta.modified()).ok();
    let mut bytes = Vec::new();
    record.read_to_end(&mut bytes).ok()?;
    let binding: MuseBinding = serde_json::from_slice(&bytes).ok()?;
    (launch.launch_id == binding.launch_id
        && !launch.launch_id.is_nil()
        && Uuid::parse_str(&binding.metadata.session_id).is_ok()
        && (current == Some(binding.metadata.session_id.as_str())
            || muse_session_exists(&launch.session_root, &binding.metadata.session_id).ok()?))
    .then_some((binding.metadata, modified))
}

#[cfg(test)]
mod muse_tests {
    use super::{
        MuseBinding, MuseLaunch, clear_muse_binding, muse_pane_dir, muse_session_exists,
        prepare_muse_hooks, prepare_muse_hooks_inner, read_muse_binding, wait_for_muse_hook,
        write_muse_binding_from_reader, write_muse_json,
    };
    use muxel_core::muse::MuseState;
    use serde_json::json;
    use std::io::Cursor;
    use uuid::Uuid;

    fn hook_fixture() -> (std::path::PathBuf, Uuid, Vec<(String, String)>) {
        let root = std::env::temp_dir().join(format!("muxel-muse-lifecycle-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("config/muse")).unwrap();
        let env = vec![
            (
                "XDG_CONFIG_HOME".into(),
                root.join("config").to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME".into(),
                root.join("data").to_string_lossy().into_owned(),
            ),
        ];
        (root, Uuid::new_v4(), env)
    }

    #[test]
    fn muse_strict_managed_schema_launches_plainly_without_changing_sources() {
        for marker in [json!({"name":"policy"}), json!({"timeout_ms":5000})] {
            let (root, pane, env) = hook_fixture();
            let settings_path = root.join("config/muse/settings.json");
            let managed_path = root.join("config/muse/managed.json");
            let settings =
                json!({"managed_hooks_path":"managed.json", "hooks":{"Stop":[]}}).to_string();
            std::fs::write(&settings_path, &settings).unwrap();
            std::fs::write(&managed_path, "{}").unwrap();
            assert!(prepare_muse_hooks(&root, pane, &env, &[], None, false).is_some());
            let mut handler = json!({"type":"command", "command":"policy-hook"});
            handler
                .as_object_mut()
                .unwrap()
                .extend(marker.as_object().unwrap().clone());
            let strict = json!({"hooks":{"SessionStart":[{"hooks":[handler]}]}}).to_string();
            std::fs::write(&managed_path, &strict).unwrap();
            let error = prepare_muse_hooks_inner(&root, pane, &env, &[], None, false).unwrap_err();
            assert!(error.to_string().contains("strict name/timeout_ms schema"));
            assert!(prepare_muse_hooks(&root, pane, &env, &[], None, false).is_none());
            assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), settings);
            assert_eq!(std::fs::read_to_string(&managed_path).unwrap(), strict);
            assert!(
                !muse_pane_dir(&root, pane).join("launch.json").exists(),
                "plain launch retained a stale binding"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn muse_strict_hook_source_in_its_provider_directory_is_preserved_when_declined() {
        let (root, pane, env) = hook_fixture();
        prepare_muse_hooks(&root, pane, &env, &[], None, false).unwrap();
        let dir = muse_pane_dir(&root, pane);
        let source = dir.join("hooks.json");
        let strict = json!({"hooks":{"SessionStart":[{"hooks":[{"name":"policy", "timeout_ms":5000, "command":"policy-hook"}]}]}}).to_string();
        std::fs::write(&source, &strict).unwrap();
        let settings = json!({"managed_hooks_path":source}).to_string();
        std::fs::write(root.join("config/muse/settings.json"), &settings).unwrap();
        assert!(prepare_muse_hooks(&root, pane, &env, &[], None, false).is_none());
        assert_eq!(std::fs::read_to_string(&source).unwrap(), strict);
        assert!(!dir.join("launch.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn muse_tmux_reattach_keeps_nonce_context_and_accepts_surviving_hooks() {
        let (root, pane, env) = hook_fixture();
        let argument =
            prepare_muse_hooks(&root, pane, &env, &[], Some("original context"), false).unwrap();
        let dir = muse_pane_dir(&root, pane);
        let launch_bytes = std::fs::read(dir.join("launch.json")).unwrap();
        let hooks_bytes = std::fs::read(dir.join("hooks.json")).unwrap();
        let launch: MuseLaunch = serde_json::from_slice(&launch_bytes).unwrap();
        // A reattach must not reinterpret settings changed since Muse started.
        std::fs::write(root.join("config/muse/settings.json"), "not JSON").unwrap();
        assert_eq!(
            prepare_muse_hooks(&root, pane, &env, &[], Some("replacement context"), true),
            Some(argument)
        );
        assert_eq!(
            std::fs::read(dir.join("launch.json")).unwrap(),
            launch_bytes
        );
        assert_eq!(std::fs::read(dir.join("hooks.json")).unwrap(), hooks_bytes);
        let session = Uuid::new_v4().to_string();
        let event = json!({"session_id":session, "cwd":root, "hook_event_name":"Notification", "notification_type":"permission_prompt"});
        write_muse_binding_from_reader(
            &root,
            pane,
            launch.launch_id,
            &session,
            Cursor::new(event.to_string()),
        )
        .unwrap();
        assert_eq!(
            read_muse_binding(&root, pane, Some(&session))
                .unwrap()
                .0
                .state,
            MuseState::Blocked
        );
        let unknown = Uuid::new_v4();
        assert!(prepare_muse_hooks(&root, unknown, &env, &[], None, true).is_none());
        assert!(!muse_pane_dir(&root, unknown).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn muse_new_process_rotates_nonce_and_cleans_only_its_owned_records() {
        let (root, pane, env) = hook_fixture();
        prepare_muse_hooks(&root, pane, &env, &[], None, false).unwrap();
        let dir = muse_pane_dir(&root, pane);
        let old: MuseLaunch =
            serde_json::from_slice(&std::fs::read(dir.join("launch.json")).unwrap()).unwrap();
        let old_record = dir.join(format!("{}.json", old.launch_id));
        std::fs::write(&old_record, "{}").unwrap();
        std::fs::write(dir.join("interrupted.tmp"), "{}").unwrap();
        let sibling = Uuid::new_v4();
        prepare_muse_hooks(&root, sibling, &env, &[], None, false).unwrap();
        let sibling_hooks = muse_pane_dir(&root, sibling).join("hooks.json");
        let sibling_bytes = std::fs::read(&sibling_hooks).unwrap();
        prepare_muse_hooks(&root, pane, &env, &[], None, false).unwrap();
        let new: MuseLaunch =
            serde_json::from_slice(&std::fs::read(dir.join("launch.json")).unwrap()).unwrap();
        assert_ne!(old.launch_id, new.launch_id);
        assert!(!old_record.exists());
        assert!(!dir.join("interrupted.tmp").exists());
        assert!(dir.join("hooks.json").is_file());
        assert_eq!(std::fs::read(&sibling_hooks).unwrap(), sibling_bytes);
        clear_muse_binding(&root, pane);
        assert!(!dir.exists());
        assert_eq!(std::fs::read(&sibling_hooks).unwrap(), sibling_bytes);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn muse_late_hook_cannot_recreate_a_permanently_closed_pane_directory() {
        struct CloseOnRead {
            pane: Option<std::path::PathBuf>,
            input: Cursor<String>,
        }
        impl std::io::Read for CloseOnRead {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                if let Some(pane) = self.pane.take() {
                    std::fs::remove_dir_all(pane)?;
                }
                std::io::Read::read(&mut self.input, bytes)
            }
        }
        let (root, pane, env) = hook_fixture();
        prepare_muse_hooks(&root, pane, &env, &[], None, false).unwrap();
        let dir = muse_pane_dir(&root, pane);
        let launch: MuseLaunch =
            serde_json::from_slice(&std::fs::read(dir.join("launch.json")).unwrap()).unwrap();
        let session = Uuid::new_v4().to_string();
        let event = json!({"session_id":session, "cwd":root, "hook_event_name":"Stop"});
        let reader = CloseOnRead {
            pane: Some(dir.clone()),
            input: Cursor::new(event.to_string()),
        };
        assert!(
            write_muse_binding_from_reader(&root, pane, launch.launch_id, &session, reader)
                .is_err()
        );
        assert!(!dir.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn muse_hook_deadline_returns_before_a_blocked_worker_finishes() {
        let (release, blocked) = std::sync::mpsc::channel();
        let (finished, completion) = std::sync::mpsc::channel();
        assert!(!wait_for_muse_hook(
            std::time::Duration::from_millis(20),
            move || {
                blocked.recv().unwrap();
                finished.send(()).unwrap();
            }
        ));
        assert!(completion.try_recv().is_err());
        release.send(()).unwrap();
        completion
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }

    #[test]
    fn muse_fixture_store_binds_two_panes_and_ignores_transcript_contents() {
        let root = std::env::temp_dir().join(format!("muxel-muse-test-{}", Uuid::new_v4()));
        let cwd = root.join("same-folder");
        std::fs::create_dir_all(&cwd).unwrap();
        for _ in 0..2 {
            let pane = Uuid::new_v4();
            let session = Uuid::new_v4().to_string();
            let nonce = Uuid::new_v4();
            let pane_dir = muse_pane_dir(&root, pane);
            let store = root.join("muse");
            write_muse_json(
                &pane_dir.join("launch.json"),
                &MuseLaunch {
                    launch_id: nonce,
                    session_root: store.clone(),
                    context: Some("launch context".into()),
                },
            )
            .unwrap();
            let transcript = store
                .join("sessions/2026/10/15")
                .join(&session)
                .join("session.jsonl");
            std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            std::fs::write(
                &transcript,
                "deliberately not JSON; only the filename matters",
            )
            .unwrap();
            let event = json!({"hook_event_name":"SessionStart","session_id":session,"cwd":cwd,"prompt":"do not retain this content"});
            let context = write_muse_binding_from_reader(
                &root,
                pane,
                nonce,
                &session,
                Cursor::new(event.to_string()),
            )
            .unwrap();
            assert_eq!(context.as_deref(), Some("launch context"));
            let (binding, recorded) = read_muse_binding(&root, pane, None).unwrap();
            assert!(recorded.is_some());
            assert_eq!(binding.session_id, session);
            assert_eq!(binding.state, MuseState::Idle);
            let stored = std::fs::read_to_string(pane_dir.join(format!("{nonce}.json"))).unwrap();
            assert!(!stored.contains("do not retain"));
            assert!(!stored.contains("prompt"));
            assert!(muse_session_exists(&store, &session).unwrap());
            std::fs::remove_file(transcript).unwrap();
            assert!(read_muse_binding(&root, pane, None).is_none());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn muse_replaced_launch_rejects_delayed_old_hook_and_binding() {
        let root = std::env::temp_dir().join(format!("muxel-muse-stale-{}", Uuid::new_v4()));
        let pane = Uuid::new_v4();
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        let dir = muse_pane_dir(&root, pane);
        write_muse_json(
            &dir.join("launch.json"),
            &MuseLaunch {
                launch_id: new,
                session_root: root.join("muse"),
                context: None,
            },
        )
        .unwrap();
        assert!(
            write_muse_binding_from_reader(
                &root,
                pane,
                old,
                "invalid",
                Cursor::new("must not even parse this")
            )
            .unwrap()
            .is_none()
        );
        write_muse_json(
            &dir.join(format!("{old}.json")),
            &MuseBinding {
                launch_id: old,
                metadata: Default::default(),
            },
        )
        .unwrap();
        assert!(read_muse_binding(&root, pane, None).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn muse_per_run_file_preserves_user_and_managed_hooks_without_global_writes() {
        let root = std::env::temp_dir().join(format!("muxel-muse-hooks-{}", Uuid::new_v4()));
        let config = root.join("config");
        std::fs::create_dir_all(config.join("muse")).unwrap();
        let managed = config.join("muse/managed.json");
        let prior =
            json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"existing-managed"}]}]}});
        std::fs::write(&managed, prior.to_string()).unwrap();
        let settings=json!({"managed_hooks_path":"managed.json","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"existing-user"}]}]}}).to_string();
        let settings_path = config.join("muse/settings.json");
        std::fs::write(&settings_path, &settings).unwrap();
        let pane = Uuid::new_v4();
        let env = vec![
            (
                "XDG_CONFIG_HOME".into(),
                config.to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME".into(),
                root.join("data").to_string_lossy().into_owned(),
            ),
        ];
        let argument = prepare_muse_hooks(&root, pane, &env, &[], Some("context"), false).unwrap();
        assert!(argument.starts_with("managed_hooks_path="));
        let generated: serde_json::Value = serde_json::from_slice(
            &std::fs::read(muse_pane_dir(&root, pane).join("hooks.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(generated["hooks"]["Stop"][0], prior["hooks"]["Stop"][0]);
        assert_eq!(std::fs::read_to_string(settings_path).unwrap(), settings);
        assert_eq!(std::fs::read_to_string(managed).unwrap(), prior.to_string());
        assert!(
            prepare_muse_hooks(
                &root,
                pane,
                &env,
                &["-c".into(), "hooks={}".into()],
                None,
                false
            )
            .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

const CLAUDE_BINDING_DIR: &str = "provider-session-bindings/claude";
const CLAUDE_SETTINGS_DIR: &str = "provider-session-bindings/claude/settings";
const CLAUDE_HOOK_FLAG: &str = "--claude-session-hook";
const MAX_HOOK_INPUT_BYTES: u64 = 64 * 1024;
/// The field the hook adds to Claude's event: see [`ClaudeHookStamp`].
const HOOK_STAMP_FIELD: &str = "muxel_hook";
pub(crate) const MUXEL_INSTANCE_ID_ENV: &str = muxel_core::control::INSTANCE_ID_ENV;

/// The process muxel starts for a pane, for [`pane_env`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneProcess {
    /// The pane's program itself, on this computer.
    Program,
    /// A tmux client; the program runs in a tmux session on this computer.
    TmuxClient,
    /// An ssh client; the program runs on another computer.
    SshClient,
}

/// The environment a pane's process starts with, on top of muxel's own: the
/// variables to set (what the preset and instance set, then `launch`, what the
/// launch itself needs, such as ssh's password), and the ones to remove.
///
/// The pane's own program also gets `MUXEL_INSTANCE_ID`, the id of the pane it
/// runs in, which `muxel ctl` takes as AGENT and Claude's session hook reads to
/// say which pane switched conversations. It replaces any value set for it,
/// since only muxel knows which pane that is.
///
/// A tmux or ssh client has none at all, not even one muxel itself inherited
/// (from a pane it was started in) or one set for it, since each can pass its
/// environment on to the program. A tmux pane's program starts from the tmux
/// server's environment and gets its id through [`tmux_pane_args`], but tmux
/// copies the variables its `update-environment` option names from the client
/// into a new session, so a user who names this one would hand the session the
/// client's. An ssh `SendEnv` naming this variable could forward the client's
/// local control id to the remote program, where it does not identify that
/// program.
///
/// Under an isolated root, configured profile variables are never trusted,
/// including case variants on Windows. Direct programs and ssh clients inherit
/// the exact startup root (which need not be Unicode) and have no control
/// override. Only the tmux launch supplies its constructed matching endpoint.
pub(crate) fn pane_env(
    configured: &[(String, String)],
    launch: Vec<(String, String)>,
    instance_id: Uuid,
    process: PaneProcess,
    isolated: bool,
) -> (Vec<(String, String)>, Vec<String>) {
    let profile_key = |key: &str| {
        key.eq_ignore_ascii_case(muxel_store::USER_DATA_DIR_ENV)
            || key.eq_ignore_ascii_case(crate::control::CONTROL_ENV)
    };
    let mut env: Vec<_> = configured
        .iter()
        .filter(|(key, _)| !isolated || !profile_key(key))
        .cloned()
        .collect();
    env.extend(launch);
    let mut removed = Vec::new();
    if isolated && process != PaneProcess::TmuxClient {
        env.retain(|(key, _)| !profile_key(key));
        removed.push(crate::control::CONTROL_ENV.to_string());
    }
    let not_the_id = |(key, _): &(String, String)| !key.eq_ignore_ascii_case(MUXEL_INSTANCE_ID_ENV);
    match process {
        PaneProcess::Program => {
            env.retain(not_the_id);
            env.push((MUXEL_INSTANCE_ID_ENV.to_string(), instance_id.to_string()));
            (env, removed)
        }
        PaneProcess::TmuxClient | PaneProcess::SshClient => {
            env.retain(not_the_id);
            removed.push(MUXEL_INSTANCE_ID_ENV.to_string());
            (env, removed)
        }
    }
}

/// The tmux arguments for a local pane kept in tmux session `session` on the
/// server `socket` (see `integrations::local_tmux_socket`), running `program`
/// (with none, the user's shell).
///
/// The program starts from the tmux server's environment, not this client's, so
/// the pane's id and isolated profile are handed over with `new-session -e`
/// (`session_env`: tmux 3.2 and later). Older tmux cannot launch isolated panes.
/// No preset environment or secrets go into argv. First, on every tmux, the id
/// is cleared from the server: one started
/// from inside another pane (by a `tmux` typed in a shell pane, or by a muxel run
/// there) has that pane's id, which a program started without `-e` would take as
/// its own.
pub(crate) struct LocalTmuxLaunch {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub(crate) fn tmux_pane_args(
    socket: muxel_core::tmux::Socket,
    session: &str,
    program: Option<&str>,
    args: &[String],
    instance_id: Uuid,
    session_env: bool,
    user_data_root: Option<&Path>,
) -> Result<LocalTmuxLaunch, muxel_core::isolated_root::TmuxEnvironmentError> {
    let env = muxel_core::isolated_root::local_tmux_environment(
        user_data_root,
        instance_id,
        session_env,
    )?;
    // No `-c`: the client is spawned with `cwd` already, which is what tmux uses
    // for a new session anyway. Passing it would put the project's path in this
    // client's argv for no gain.
    let socket = user_data_root
        .filter(|root| !root.as_os_str().is_empty())
        .map(muxel_core::tmux::Socket::isolated)
        .unwrap_or(socket);
    let args = socket.argv(muxel_core::tmux::launch_session_args_with_env(
        session,
        None,
        program,
        args,
        &env,
        &[MUXEL_INSTANCE_ID_ENV],
    ));
    // The same profile values on the client cover user-defined
    // update-environment options during attach. pane_env strips the pane id.
    Ok(LocalTmuxLaunch { args, env })
}

/// What the hook adds to the record it writes: a nonce naming this write, and
/// when it ran. A record with a nonce muxel hasn't seen means a SessionStart ran
/// since the last one, even when it names the conversation already bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeHookStamp {
    pub nonce: Uuid,
    pub recorded_at: SystemTime,
}

/// A pane's binding record, validated, from one read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeBinding {
    pub session_id: String,
    /// `None` for a record written by an older muxel's hook.
    pub stamp: Option<ClaudeHookStamp>,
}

/// Whether a pane's binding record reports a switch, given the nonce of the
/// last record this launch read (`seen`) and the bound conversation. A stamped
/// record is known by its nonce: one this launch hasn't seen is a switch even
/// when it names the bound conversation, as after `/resume B` and `/resume A`
/// between two reads. The launch's first stamped record, and a record from an
/// older muxel's hook, switch only when they name another conversation if the
/// command resumed that conversation. A fresh `--session-id` launch has no
/// startup hook, so its first stamped record is a user switch even for the
/// same UUID.
pub(crate) fn claude_record_switches(
    binding: &ClaudeBinding,
    seen: Option<Uuid>,
    bound: Option<&str>,
    launch_resumed: bool,
) -> bool {
    match (binding.stamp, seen) {
        (Some(stamp), Some(seen)) => stamp.nonce != seen,
        (Some(_), None) if !launch_resumed => true,
        _ => bound != Some(binding.session_id.as_str()),
    }
}

pub(crate) fn hook_instance_from_args(
    args: impl IntoIterator<Item = OsString>,
    env_instance_id: Option<OsString>,
) -> std::result::Result<Option<Uuid>, String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == CLAUDE_HOOK_FLAG {
            // Old settings files passed the UUID as argv. Keep accepting that form so
            // hooks from an already-running pane survive a Muxel upgrade. New settings
            // route the pane through inherited environment instead.
            let raw = args.next().or(env_instance_id).ok_or_else(|| {
                format!(
                    "{CLAUDE_HOOK_FLAG} requires an instance UUID argument or {MUXEL_INSTANCE_ID_ENV}"
                )
            })?;
            let raw = raw
                .to_str()
                .ok_or_else(|| format!("{CLAUDE_HOOK_FLAG} requires a UTF-8 instance UUID"))?;
            let id = Uuid::parse_str(raw)
                .map_err(|_| format!("{CLAUDE_HOOK_FLAG} requires a valid instance UUID"))?;
            return Ok(Some(id));
        }
    }
    Ok(None)
}

pub(crate) fn run_claude_session_hook(instance_id: Uuid) -> Result<()> {
    let data_dir = muxel_store::data_dir().context("could not determine muxel data directory")?;
    write_claude_binding_from_reader(&data_dir, instance_id, std::io::stdin().lock())
}

pub(crate) fn claude_hook_settings() -> Option<String> {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            log::warn!("could not resolve Muxel executable for Claude session hook: {error}");
            return None;
        }
    };
    let Some(data_dir) = muxel_store::data_dir() else {
        log::warn!("could not resolve Muxel data directory for Claude session hook");
        return None;
    };
    let path = match write_claude_hook_settings_file(&data_dir, &executable) {
        Ok(path) => path,
        Err(error) => {
            log::warn!("could not write Claude session hook settings: {error:#}");
            return None;
        }
    };
    let argument = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    if !windows_batch_arg_is_safe(&argument) {
        log::warn!(
            "Claude hook settings path contains cmd.exe metacharacters; session rebinding is disabled for this pane"
        );
        return None;
    }
    Some(argument)
}

fn claude_hook_settings_for(executable: &Path) -> String {
    json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "resume|clear|fork",
                "hooks": [{
                    "type": "command",
                    "command": executable.to_string_lossy(),
                    "args": [CLAUDE_HOOK_FLAG],
                    "timeout": 5
                }]
            }]
        }
    })
    .to_string()
}

fn executable_identity(executable: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    executable.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn claude_hook_settings_path(data_dir: &Path, executable: &Path) -> PathBuf {
    data_dir
        .join(CLAUDE_SETTINGS_DIR)
        .join(format!("{}.json", executable_identity(executable)))
}

fn write_claude_hook_settings_file(data_dir: &Path, executable: &Path) -> Result<PathBuf> {
    let path = claude_hook_settings_path(data_dir, executable);
    let parent = path
        .parent()
        .context("Claude hook settings path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "creating Claude hook settings directory {}",
            parent.display()
        )
    })?;
    let desired = claude_hook_settings_for(executable);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(desired.as_str()) {
        return Ok(path);
    }

    let temp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    std::fs::write(&temp, &desired)
        .with_context(|| format!("writing Claude hook settings {}", temp.display()))?;
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    if let Err(error) = std::fs::rename(&temp, &path) {
        // Two pane launches can race. Identical content at the target is success;
        // only remove the uniquely-owned temporary file.
        if std::fs::read_to_string(&path).ok().as_deref() == Some(desired.as_str()) {
            let _ = std::fs::remove_file(&temp);
        } else {
            let _ = std::fs::remove_file(&temp);
            return Err(error)
                .with_context(|| format!("installing Claude hook settings {}", path.display()));
        }
    }
    Ok(path)
}

#[cfg(any(windows, test))]
fn windows_batch_arg_is_safe(value: &str) -> bool {
    !value
        .chars()
        .any(|ch| matches!(ch, '"' | '%' | '&' | '|' | '<' | '>' | '^' | '\r' | '\n'))
}

pub(crate) fn claude_binding_path(data_dir: &Path, instance_id: Uuid) -> PathBuf {
    data_dir
        .join(CLAUDE_BINDING_DIR)
        .join(format!("{instance_id}.json"))
}

fn write_claude_binding_from_reader(
    data_dir: &Path,
    instance_id: Uuid,
    reader: impl std::io::Read,
) -> Result<()> {
    let mut input = String::new();
    reader
        .take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_string(&mut input)
        .context("reading Claude SessionStart input")?;
    if input.len() as u64 > MAX_HOOK_INPUT_BYTES {
        bail!("Claude SessionStart input exceeds {MAX_HOOK_INPUT_BYTES} bytes");
    }
    let mut event: Value =
        serde_json::from_str(&input).context("parsing Claude SessionStart input")?;
    validate_claude_event(&event)?;
    stamp_claude_event(&mut event, Uuid::new_v4(), SystemTime::now());

    let path = claude_binding_path(data_dir, instance_id);
    let parent = path.parent().context("Claude binding path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating Claude binding directory {}", parent.display()))?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temp, event.to_string())
        .with_context(|| format!("writing Claude binding {}", temp.display()))?;
    // muxel may claim the previous record (rename it away) at any moment.
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error)
                .with_context(|| format!("replacing Claude binding {}", path.display()));
        }
        _ => {}
    }
    std::fs::rename(&temp, &path)
        .with_context(|| format!("installing Claude binding {}", path.display()))?;
    Ok(())
}

fn validate_claude_event(event: &Value) -> Result<()> {
    if event.get("hook_event_name").and_then(Value::as_str) != Some("SessionStart") {
        bail!("expected a Claude SessionStart event");
    }
    let source = event
        .get("source")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no source")?;
    if !matches!(source, "resume" | "clear" | "fork") {
        bail!("unexpected Claude SessionStart source {source:?}");
    }
    let session_id = event
        .get("session_id")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no session_id")?;
    Uuid::parse_str(session_id).context("Claude SessionStart session_id is not a UUID")?;
    let transcript = event
        .get("transcript_path")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no transcript_path")?;
    let expected_name = format!("{session_id}.jsonl");
    if Path::new(transcript)
        .file_name()
        .and_then(|name| name.to_str())
        != Some(expected_name.as_str())
    {
        bail!("Claude transcript filename does not match session_id");
    }
    let cwd = event
        .get("cwd")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no cwd")?;
    if !Path::new(cwd).is_absolute() || !Path::new(transcript).is_absolute() {
        bail!("Claude SessionStart paths must be absolute");
    }
    Ok(())
}

fn stamp_claude_event(event: &mut Value, nonce: Uuid, recorded_at: SystemTime) {
    let micros = recorded_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros() as u64);
    if let Some(event) = event.as_object_mut() {
        event.insert(
            HOOK_STAMP_FIELD.to_string(),
            json!({ "nonce": nonce.to_string(), "recorded_at_us": micros }),
        );
    }
}

fn claude_hook_stamp(event: &Value) -> Option<ClaudeHookStamp> {
    let stamp = event.get(HOOK_STAMP_FIELD)?;
    let nonce = Uuid::parse_str(stamp.get("nonce")?.as_str()?).ok()?;
    let micros = stamp.get("recorded_at_us")?.as_u64()?;
    Some(ClaudeHookStamp {
        nonce,
        recorded_at: SystemTime::UNIX_EPOCH + Duration::from_micros(micros),
    })
}

/// The pane's binding record, if one is present and valid.
#[cfg(test)]
fn read_claude_binding(data_dir: &Path, instance_id: Uuid, home: &Path) -> Option<ClaudeBinding> {
    claude_binding_from_record(
        &std::fs::read(claude_binding_path(data_dir, instance_id)).ok()?,
        home,
    )
}

/// Whether a binding record isn't a Claude `SessionStart` event the hook would
/// write, so no later read can validate it. A well-formed record that fails
/// [`claude_binding_from_record`] may validate later (`/clear` before Claude
/// creates the project directory), or never: Claude's store lives elsewhere
/// (`CLAUDE_CONFIG_DIR`), or a resumed transcript was deleted. Such a record is
/// read again on each poll until a newer one supersedes it.
pub(crate) fn claude_record_is_malformed(record: &[u8]) -> bool {
    serde_json::from_slice::<Value>(record)
        .ok()
        .is_none_or(|event| validate_claude_event(&event).is_err())
}

/// Parse and validate one read of a binding record. The session id, its
/// transcript and the hook's stamp all come from the same bytes.
pub(crate) fn claude_binding_from_record(record: &[u8], home: &Path) -> Option<ClaudeBinding> {
    let event: Value = serde_json::from_slice(record).ok()?;
    validate_claude_event(&event).ok()?;
    let session_id = event.get("session_id")?.as_str()?;
    let source = event.get("source")?.as_str()?;
    let transcript = Path::new(event.get("transcript_path")?.as_str()?);
    let projects = home.join(".claude").join("projects").canonicalize().ok()?;
    let expected_name = format!("{session_id}.jsonl");
    if transcript.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return None;
    }

    // `/clear` and `/fork` may announce the new UUID before Claude writes its
    // first transcript record. Validate the existing project directory in that
    // case; resumed sessions must already have a real transcript.
    let project = transcript.parent()?.canonicalize().ok()?;
    if project.parent()? != projects {
        return None;
    }
    match transcript.canonicalize() {
        Ok(resolved) if resolved.is_file() && resolved.parent() == Some(project.as_path()) => {}
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && matches!(source, "clear" | "fork") => {}
        _ => return None,
    }
    Some(ClaudeBinding {
        session_id: session_id.to_string(),
        stamp: claude_hook_stamp(&event),
    })
}

/// A binding record muxel has claimed: its bytes, read once, and its write
/// time (an older muxel's hook didn't stamp its record).
/// Immutable worker input. Reading it does not claim or acknowledge the hook.
pub(crate) struct ClaudeBindingSnapshot {
    pub bytes: Vec<u8>,
    pub modified: Option<SystemTime>,
}

/// Validate a pending claim before a newer shared hook, without changing either.
/// Read bytes and modification time from the same open file.
pub(crate) fn read_pending_claude_binding(
    data_dir: &Path,
    instance_id: Uuid,
) -> Option<ClaudeBindingSnapshot> {
    let mut file = match std::fs::File::open(claude_claimed_binding_path(data_dir, instance_id)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::File::open(claude_binding_path(data_dir, instance_id)).ok()?
        }
        Err(_) => return None,
    };
    let modified = file
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(ClaudeBindingSnapshot { bytes, modified })
}

pub(crate) struct ClaimedClaudeRecord {
    path: PathBuf,
    /// The shared path, where the hook writes the pane's next record.
    shared: PathBuf,
    pub bytes: Vec<u8>,
    pub modified: Option<SystemTime>,
}

impl ClaimedClaudeRecord {
    /// Whether the hook has written a newer record since this one was claimed.
    fn superseded(&self) -> Result<bool> {
        self.shared
            .try_exists()
            .context("check for a newer Claude conversation binding")
    }
}

/// Where a pane's binding record moves while muxel handles it. The hook never
/// writes this name, so a record it writes meanwhile lands at the shared path.
fn claude_claimed_binding_path(data_dir: &Path, instance_id: Uuid) -> PathBuf {
    data_dir
        .join(CLAUDE_BINDING_DIR)
        .join(format!("{instance_id}.claimed.json"))
}

/// Claim the pane's next binding record: one claimed before whose handling
/// failed, else the shared record, moved to the claimed name by an atomic
/// rename. Renaming keeps its write time.
pub(crate) fn claim_claude_binding(
    data_dir: &Path,
    instance_id: Uuid,
) -> Option<ClaimedClaudeRecord> {
    try_claim_claude_binding(data_dir, instance_id)
        .ok()
        .flatten()
}

fn try_claim_claude_binding(
    data_dir: &Path,
    instance_id: Uuid,
) -> Result<Option<ClaimedClaudeRecord>> {
    let path = claude_claimed_binding_path(data_dir, instance_id);
    let shared = claude_binding_path(data_dir, instance_id);
    if !path.try_exists()? {
        match std::fs::rename(&shared, &path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("claim Claude conversation binding"),
        }
    }
    let bytes = std::fs::read(&path).context("read claimed Claude conversation binding")?;
    let modified = std::fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok();
    Ok(Some(ClaimedClaudeRecord {
        path,
        shared,
        bytes,
        modified,
    }))
}

/// What prelaunch recovery's handler did with one claimed record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveredClaudeRecord {
    /// Adopted and saved, or nothing to adopt: it names the saved
    /// conversation. Acknowledge it.
    Handled,
    /// It isn't adopted now (see [`defer_claude_binding`]). Recovery keeps it
    /// only while the process that wrote it survives the launch.
    NotYet,
}

/// Drain pending records before choosing launch arguments, through the newest.
/// A failed handler leaves its claim intact and stops the launch. A record that
/// can't be adopted yet names a conversation of the process that wrote it. It
/// stays claimed for a later retry, and the launch uses the saved binding, only
/// when `writer_survives` (a reattach to that process's live tmux session),
/// unless the hook has written a newer record, which supersedes it. Any other
/// launch starts a new process, which is in none of the old one's
/// conversations, so the record goes, as one that can never validate does.
///
/// A handled record's binding is already saved, so failing to acknowledge it is
/// logged and the launch goes ahead; the claim stays for a later retry, where it
/// names the saved conversation. Only a newer record waiting behind it stops
/// the launch: it can't be claimed past the stuck one, and launching would
/// resume the conversation it replaced. So do claim and read failures, which
/// leave a pending record unread.
pub(crate) fn recover_claude_bindings_before_resume(
    data_dir: &Path,
    instance_id: Uuid,
    writer_survives: impl FnOnce() -> bool,
    mut adopt: impl FnMut(&ClaimedClaudeRecord) -> Result<RecoveredClaudeRecord>,
) -> Result<()> {
    // Asked only for a record that can't be adopted yet, and at most once: an
    // answer of yes keeps that record and ends recovery.
    let mut writer_survives = Some(writer_survives);
    while let Some(record) = try_claim_claude_binding(data_dir, instance_id)? {
        if adopt(&record)? == RecoveredClaudeRecord::NotYet
            && !claude_record_is_malformed(&record.bytes)
            && !record.superseded()?
            && writer_survives.take().is_some_and(|survives| survives())
        {
            return Ok(());
        }
        match std::fs::remove_file(&record.path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                if record.superseded()? {
                    return Err(error).context("acknowledge recovered Claude conversation");
                }
                log::warn!(
                    "could not acknowledge a recovered Claude conversation; launching with the saved binding: {error}"
                );
                return Ok(());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Delete a claimed record once it has been handled. Only the claimed file
/// goes; a record the hook has written since waits for the next claim.
pub(crate) fn acknowledge_claude_binding(record: ClaimedClaudeRecord) {
    let _ = std::fs::remove_file(record.path);
}

/// Keep a claimed record muxel can't adopt yet: its transcript doesn't
/// validate yet (`/clear` can run before Claude creates the project
/// directory), or another pane owns its conversation. The next claim retries
/// it, as a later read retried an unclaimed record, until the hook writes a
/// newer one: that supersedes it, so a record that never validates can't hold
/// back the next. A record that can never validate goes now.
pub(crate) fn defer_claude_binding(record: ClaimedClaudeRecord) {
    if claude_record_is_malformed(&record.bytes) || record.superseded().unwrap_or(false) {
        acknowledge_claude_binding(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxel_core::tmux::Socket;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxel-claude-binding-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Run the hook as Claude does on `/resume <session_id>` in `cwd`.
    fn resume_hook(data: &Path, home: &Path, cwd: &Path, instance_id: Uuid, session_id: &str) {
        let transcript = muxel_core::claude_session_path(home, cwd, session_id);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let event = json!({
            "session_id": session_id,
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        write_claude_binding_from_reader(
            data,
            instance_id,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();
    }

    fn claimed_session(record: &ClaimedClaudeRecord, home: &Path) -> String {
        claude_binding_from_record(&record.bytes, home)
            .unwrap()
            .session_id
    }

    struct PendingResume {
        root: PathBuf,
        data: PathBuf,
        home: PathBuf,
        workspace_path: PathBuf,
        instance_id: Uuid,
        old_id: String,
        new_id: String,
    }

    impl PendingResume {
        fn new() -> Self {
            let root = temp_dir();
            let data = root.join("data");
            let home = root.join("home");
            let cwd = root.join("project");
            let mut instance = muxel_core::Instance::from_preset(
                Uuid::new_v4(),
                &muxel_core::AgentPreset::claude(),
            );
            let instance_id = instance.id;
            let old_id = Uuid::new_v4().to_string();
            let new_id = Uuid::new_v4().to_string();
            instance.session_id = Some(old_id.clone());
            instance.session_started = true;
            let mut workspace = muxel_core::Workspace::default();
            workspace.instances.push(instance);
            let workspace_path = root.join("workspace.json");
            muxel_store::save_workspace_to(&workspace_path, &workspace).unwrap();
            resume_hook(&data, &home, &cwd, instance_id, &old_id);
            drop(claim_claude_binding(&data, instance_id).unwrap());
            resume_hook(&data, &home, &cwd, instance_id, &new_id);
            Self {
                root,
                data,
                home,
                workspace_path,
                instance_id,
                old_id,
                new_id,
            }
        }

        fn workspace(&self) -> muxel_core::Workspace {
            muxel_store::load_workspace_from(&self.workspace_path).unwrap()
        }

        /// The production handler's outcomes: a record that doesn't validate
        /// yet isn't adopted yet; any other is saved, then handled.
        fn persist_record(
            &self,
            workspace: &mut muxel_core::Workspace,
            record: &ClaimedClaudeRecord,
        ) -> Result<RecoveredClaudeRecord> {
            let Some(binding) = claude_binding_from_record(&record.bytes, &self.home) else {
                return Ok(RecoveredClaudeRecord::NotYet);
            };
            let instance = workspace.instance_mut(self.instance_id).unwrap();
            instance.session_id = Some(binding.session_id);
            instance.session_started = true;
            muxel_store::save_workspace_to(&self.workspace_path, workspace)?;
            Ok(RecoveredClaudeRecord::Handled)
        }

        fn saved_id(&self) -> Option<String> {
            self.workspace()
                .instance(self.instance_id)
                .unwrap()
                .session_id
                .clone()
        }

        /// Run the hook as Claude does on `/clear`, which can announce the new
        /// conversation before Claude creates its project directory.
        fn clear_hook(&self, session_id: &str, cwd: &Path) {
            let transcript = muxel_core::claude_session_path(&self.home, cwd, session_id);
            let event = json!({
                "session_id": session_id,
                "transcript_path": transcript,
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": "clear"
            });
            write_claude_binding_from_reader(
                &self.data,
                self.instance_id,
                std::io::Cursor::new(event.to_string()),
            )
            .unwrap();
        }

        /// Drain the old record, leaving nothing pending.
        fn drain(&self) {
            let mut workspace = self.workspace();
            recover_claude_bindings_before_resume(
                &self.data,
                self.instance_id,
                || false,
                |record| self.persist_record(&mut workspace, record),
            )
            .unwrap();
            assert!(claim_claude_binding(&self.data, self.instance_id).is_none());
        }

        fn resume_args(&self, workspace: &muxel_core::Workspace) -> Vec<String> {
            muxel_core::session_resume_args(
                &muxel_core::AgentPreset::claude(),
                workspace.instance(self.instance_id).unwrap(),
            )
            .unwrap()
        }
    }

    impl Drop for PendingResume {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn prelaunch_recovery_selects_newer_binding_after_a_claimed_record() {
        let pending = PendingResume::new();
        let mut workspace = pending.workspace();
        let adoptable = || unreachable!("asked whether a record it adopts survives");
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            adoptable,
            |record| pending.persist_record(&mut workspace, record),
        )
        .unwrap();
        assert_eq!(
            pending.resume_args(&workspace),
            vec!["--resume".to_string(), pending.new_id.clone()]
        );
        assert_eq!(
            pending.resume_args(&pending.workspace()),
            pending.resume_args(&workspace)
        );
        assert!(claim_claude_binding(&pending.data, pending.instance_id).is_none());
    }

    #[test]
    fn prelaunch_recovery_stops_on_a_partial_persistence_failure() {
        let pending = PendingResume::new();
        let mut workspace = pending.workspace();
        let mut handled = Vec::new();
        let result = recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| {
                let session = claimed_session(record, &pending.home);
                handled.push(session.clone());
                if session == pending.new_id {
                    bail!("simulated workspace save failure");
                }
                pending.persist_record(&mut workspace, record)
            },
        );
        assert!(
            result.is_err(),
            "failed recovery must prevent launch argument selection"
        );
        assert_eq!(
            handled,
            vec![pending.old_id.clone(), pending.new_id.clone()]
        );
        assert_eq!(
            pending
                .workspace()
                .instance(pending.instance_id)
                .unwrap()
                .session_id
                .as_deref(),
            Some(pending.old_id.as_str())
        );
        let retry = claim_claude_binding(&pending.data, pending.instance_id).unwrap();
        assert_eq!(claimed_session(&retry, &pending.home), pending.new_id);
        drop(retry);
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| pending.persist_record(&mut workspace, record),
        )
        .unwrap();
        assert_eq!(
            pending.resume_args(&workspace),
            vec!["--resume".to_string(), pending.new_id.clone()]
        );
    }

    /// Make a claimed record impossible to acknowledge: a directory takes its
    /// place, which `remove_file` refuses on every platform.
    fn block_acknowledgement(record: &ClaimedClaudeRecord) -> Result<()> {
        std::fs::remove_file(&record.path)?;
        std::fs::create_dir(&record.path)?;
        Ok(())
    }

    #[test]
    fn prelaunch_recovery_stops_if_a_newer_record_waits_behind_a_failed_acknowledgement() {
        let pending = PendingResume::new();
        let mut workspace = pending.workspace();
        let mut handled = 0;
        let result = recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| {
                handled += 1;
                let outcome = pending.persist_record(&mut workspace, record)?;
                block_acknowledgement(record)?;
                Ok(outcome)
            },
        );
        // The newer record can't be claimed past the stuck one, and the saved
        // binding is the conversation it replaced.
        assert!(result.unwrap_err().to_string().contains("acknowledge"));
        assert_eq!(handled, 1);
        assert_eq!(pending.saved_id(), Some(pending.old_id.clone()));
        assert!(claude_binding_path(&pending.data, pending.instance_id).exists());
    }

    #[test]
    fn prelaunch_recovery_launches_after_failing_to_acknowledge_a_saved_binding() {
        let pending = PendingResume::new();
        pending.drain();
        let cwd = pending.root.join("project");
        let newest = Uuid::new_v4().to_string();
        resume_hook(
            &pending.data,
            &pending.home,
            &cwd,
            pending.instance_id,
            &newest,
        );
        let mut workspace = pending.workspace();
        let mut handled = 0;
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| {
                handled += 1;
                let outcome = pending.persist_record(&mut workspace, record)?;
                block_acknowledgement(record)?;
                Ok(outcome)
            },
        )
        .expect("a saved binding whose record can't be removed still launches");
        assert_eq!(handled, 1, "the stuck claim isn't retried in a loop");
        assert_eq!(
            pending.resume_args(&pending.workspace()),
            vec!["--resume".to_string(), newest]
        );
    }

    #[test]
    fn prelaunch_recovery_keeps_an_unadoptable_record_only_for_its_live_writer() {
        let pending = PendingResume::new();
        pending.drain();
        // `/clear` before Claude has created the cwd's project directory.
        let cwd = pending.root.join("fresh-project");
        let cleared = Uuid::new_v4().to_string();
        pending.clear_hook(&cleared, &cwd);
        let recover = |writer_survives: bool| {
            let mut workspace = pending.workspace();
            recover_claude_bindings_before_resume(
                &pending.data,
                pending.instance_id,
                || writer_survives,
                |record| pending.persist_record(&mut workspace, record),
            )
        };
        // Reattaching the live tmux session that wrote it keeps it for a retry.
        recover(true).expect("launch goes ahead with the saved binding");
        assert_eq!(pending.saved_id(), Some(pending.new_id.clone()));
        let kept = claim_claude_binding(&pending.data, pending.instance_id)
            .expect("the record is kept for a retry");
        assert_eq!(claude_binding_from_record(&kept.bytes, &pending.home), None);
        drop(kept);
        // Once the directory exists, that process's record validates.
        let transcript = muxel_core::claude_session_path(&pending.home, &cwd, &cleared);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        recover(true).unwrap();
        assert_eq!(pending.saved_id(), Some(cleared));
        assert!(claim_claude_binding(&pending.data, pending.instance_id).is_none());
    }

    #[test]
    fn a_fresh_process_never_inherits_its_predecessors_unadoptable_record() {
        let pending = PendingResume::new();
        pending.drain();
        // A direct pane in a new directory: `/clear` names C before Claude
        // creates the project directory, then the pane restarts untouched.
        let cwd = pending.root.join("fresh-project");
        let cleared = Uuid::new_v4().to_string();
        pending.clear_hook(&cleared, &cwd);
        let mut workspace = pending.workspace();
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| pending.persist_record(&mut workspace, record),
        )
        .unwrap();
        assert_eq!(pending.saved_id(), Some(pending.new_id.clone()));
        // The new process writes its own transcript in that directory, which
        // would make C's record validate. Nothing is left to adopt it.
        let transcript = muxel_core::claude_session_path(&pending.home, &cwd, &cleared);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        assert!(read_pending_claude_binding(&pending.data, pending.instance_id).is_none());

        // `/resume X` while a sibling owns X: not adoptable yet, and gone once
        // the pane relaunches, so a later release of X never rebinds it.
        let owned = Uuid::new_v4().to_string();
        resume_hook(
            &pending.data,
            &pending.home,
            &pending.root.join("project"),
            pending.instance_id,
            &owned,
        );
        let sibling_owns = |_: &ClaimedClaudeRecord| Ok(RecoveredClaudeRecord::NotYet);
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || true,
            sibling_owns,
        )
        .unwrap();
        assert!(
            claim_claude_binding(&pending.data, pending.instance_id).is_some(),
            "a live writer keeps it"
        );
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            sibling_owns,
        )
        .unwrap();
        assert!(read_pending_claude_binding(&pending.data, pending.instance_id).is_none());
        assert_eq!(pending.saved_id(), Some(pending.new_id.clone()));
    }

    #[test]
    fn a_kept_record_gives_way_to_a_newer_one_and_a_malformed_one_goes() {
        let pending = PendingResume::new();
        pending.drain();
        let cwd = pending.root.join("fresh-project");
        pending.clear_hook(&Uuid::new_v4().to_string(), &cwd);
        // The recurring reader claims it and can't adopt it yet.
        defer_claude_binding(claim_claude_binding(&pending.data, pending.instance_id).unwrap());
        assert!(claim_claude_binding(&pending.data, pending.instance_id).is_some());
        // `/resume` before the cleared conversation ever validated: the newer
        // record supersedes the kept one.
        let resumed = Uuid::new_v4().to_string();
        resume_hook(
            &pending.data,
            &pending.home,
            &pending.root.join("project"),
            pending.instance_id,
            &resumed,
        );
        defer_claude_binding(claim_claude_binding(&pending.data, pending.instance_id).unwrap());
        let next = claim_claude_binding(&pending.data, pending.instance_id).unwrap();
        assert_eq!(claimed_session(&next, &pending.home), resumed);
        acknowledge_claude_binding(next);

        // Prelaunch recovery reaches the newer record the same way.
        pending.clear_hook(&Uuid::new_v4().to_string(), &cwd);
        drop(claim_claude_binding(&pending.data, pending.instance_id).unwrap());
        let newest = Uuid::new_v4().to_string();
        resume_hook(
            &pending.data,
            &pending.home,
            &pending.root.join("project"),
            pending.instance_id,
            &newest,
        );
        let mut workspace = pending.workspace();
        recover_claude_bindings_before_resume(
            &pending.data,
            pending.instance_id,
            || false,
            |record| pending.persist_record(&mut workspace, record),
        )
        .unwrap();
        assert_eq!(pending.saved_id(), Some(newest));
        assert!(claim_claude_binding(&pending.data, pending.instance_id).is_none());

        // A record that isn't a SessionStart event never will be.
        let shared = claude_binding_path(&pending.data, pending.instance_id);
        std::fs::write(&shared, b"not json").unwrap();
        defer_claude_binding(claim_claude_binding(&pending.data, pending.instance_id).unwrap());
        assert!(claim_claude_binding(&pending.data, pending.instance_id).is_none());
        std::fs::write(&shared, b"not json").unwrap();
        pending.drain();
    }

    #[test]
    fn hook_settings_use_exec_form_and_resume_sources() {
        let settings = claude_hook_settings_for(Path::new("C:/Program Files/muxel.exe"));
        let value: Value = serde_json::from_str(&settings).unwrap();
        let handler = &value["hooks"]["SessionStart"][0];
        assert_eq!(handler["matcher"], "resume|clear|fork");
        assert_eq!(handler["hooks"][0]["command"], "C:/Program Files/muxel.exe");
        assert_eq!(handler["hooks"][0]["args"][0], CLAUDE_HOOK_FLAG);
        assert_eq!(handler["hooks"][0]["args"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn hook_settings_are_passed_by_batch_safe_file() {
        let root = temp_dir();
        let executable = Path::new("C:/Program Files/muxel.exe");
        let path = write_claude_hook_settings_file(&root, executable).unwrap();
        let same_path = write_claude_hook_settings_file(&root, executable).unwrap();
        let argument = path.to_string_lossy();
        assert!(windows_batch_arg_is_safe(&argument));
        assert!(!argument.contains("SessionStart"));
        assert_eq!(path, same_path);
        assert_ne!(
            path,
            claude_hook_settings_path(&root, Path::new("C:/Other/muxel.exe"))
        );

        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let hook = &value["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(hook["command"], "C:/Program Files/muxel.exe");
        assert_eq!(hook["args"][0], CLAUDE_HOOK_FLAG);
        assert_eq!(hook["args"].as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn valid_resume_event_accepts_a_transcript_from_another_claude_project() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4().to_string();
        let transcript_cwd = root.join("moved-worktree");
        let transcript = muxel_core::claude_session_path(&home, &transcript_cwd, &session_id);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let event = json!({
            "session_id": session_id,
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        write_claude_binding_from_reader(
            &data,
            instance_id,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();

        assert_eq!(
            read_claude_binding(&data, instance_id, &home)
                .map(|binding| binding.session_id)
                .as_deref(),
            Some(session_id.as_str())
        );

        let outside = root.join(format!("{session_id}.jsonl"));
        std::fs::write(&outside, "{}\n").unwrap();
        let outside_event = json!({
            "session_id": session_id,
            "transcript_path": outside,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        write_claude_binding_from_reader(
            &data,
            instance_id,
            std::io::Cursor::new(outside_event.to_string()),
        )
        .unwrap();
        assert_eq!(
            read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
            None
        );

        write_claude_binding_from_reader(
            &data,
            instance_id,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();
        std::fs::remove_file(&transcript).unwrap();
        assert_eq!(
            read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
            None
        );

        for source in ["clear", "fork"] {
            let pending_event = json!({
                "session_id": session_id,
                "transcript_path": transcript,
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": source
            });
            write_claude_binding_from_reader(
                &data,
                instance_id,
                std::io::Cursor::new(pending_event.to_string()),
            )
            .unwrap();
            assert_eq!(
                read_claude_binding(&data, instance_id, &home)
                    .map(|binding| binding.session_id)
                    .as_deref(),
                Some(session_id.as_str()),
                "{source} should bind before Claude writes the first transcript record"
            );
        }
        let record = claim_claude_binding(&data, instance_id).unwrap();
        acknowledge_claude_binding(record);
        assert!(!claude_binding_path(&data, instance_id).exists());
        assert!(claim_claude_binding(&data, instance_id).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_launch_first_resume_of_its_pinned_id_is_a_switch() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();

        // A fresh `--session-id A` writes no startup SessionStart record.
        resume_hook(&data, &home, &cwd, instance_id, &a);
        let first = read_claude_binding(&data, instance_id, &home).unwrap();
        assert!(first.stamp.is_some());
        assert!(claude_record_switches(
            &first,
            None,
            Some(a.as_str()),
            false
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_launch_a_to_b_to_a_before_first_poll_is_a_switch() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();

        // Both user resumes replace a fresh `--session-id A` between polls.
        resume_hook(&data, &home, &cwd, instance_id, &b);
        resume_hook(&data, &home, &cwd, instance_id, &a);
        let last = read_claude_binding(&data, instance_id, &home).unwrap();
        assert_eq!(last.session_id, a);
        assert!(claude_record_switches(&last, None, Some(a.as_str()), false));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_to_b_to_a_between_polls_reads_as_a_switch() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();
        let resume = |session_id: &str| resume_hook(&data, &home, &cwd, instance_id, session_id);

        // `claude --resume A` runs the hook as it launches, naming A.
        let before = SystemTime::now();
        resume(&a);
        let first = read_claude_binding(&data, instance_id, &home).unwrap();
        let stamp = first.stamp.expect("the hook stamps its record");
        assert!(before <= stamp.recorded_at && stamp.recorded_at <= SystemTime::now());
        assert!(!claude_record_switches(
            &first,
            None,
            Some(a.as_str()),
            true
        ));

        // `/resume B` and then `/resume A`, both before muxel's next read.
        resume(&b);
        resume(&a);
        let last = read_claude_binding(&data, instance_id, &home).unwrap();
        assert_eq!(last.session_id, a);
        assert!(
            claude_record_switches(&last, Some(stamp.nonce), Some(a.as_str()), true),
            "a record muxel hasn't seen is a switch, whatever it names"
        );
        // The same record read again, because removing it failed, is not.
        let nonce = last.stamp.map(|stamp| stamp.nonce);
        assert!(!claude_record_switches(
            &last,
            nonce,
            Some(a.as_str()),
            true
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_older_hooks_record_switches_only_to_another_conversation() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4().to_string();
        let transcript = muxel_core::claude_session_path(&home, &cwd, &session_id);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        // An older muxel's hook wrote Claude's event as it came.
        let path = claude_binding_path(&data, instance_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let event = json!({
            "session_id": session_id,
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        std::fs::write(&path, event.to_string()).unwrap();
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();

        let record = claim_claude_binding(&data, instance_id).unwrap();
        // Its write time stands in for the stamp; claiming it keeps that.
        assert_eq!(record.modified, Some(written));
        let binding = claude_binding_from_record(&record.bytes, &home).unwrap();
        assert_eq!(binding.stamp, None);
        let seen = Some(Uuid::new_v4());
        assert!(!claude_record_switches(
            &binding,
            seen,
            Some(session_id.as_str()),
            true
        ));
        let other = Uuid::new_v4().to_string();
        assert!(claude_record_switches(
            &binding,
            seen,
            Some(other.as_str()),
            true
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_record_written_while_muxel_handles_the_last_survives_its_acknowledgement() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();

        resume_hook(&data, &home, &cwd, instance_id, &a);
        let claimed = claim_claude_binding(&data, instance_id).unwrap();
        assert_eq!(claimed_session(&claimed, &home), a);
        // The hook runs again between muxel's read and its acknowledgement.
        resume_hook(&data, &home, &cwd, instance_id, &b);
        acknowledge_claude_binding(claimed);

        let next = claim_claude_binding(&data, instance_id).expect("B wasn't deleted unread");
        assert_eq!(claimed_session(&next, &home), b);
        acknowledge_claude_binding(next);
        assert!(claim_claude_binding(&data, instance_id).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_record_whose_handling_failed_is_claimed_again_before_a_newer_one() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();

        resume_hook(&data, &home, &cwd, instance_id, &a);
        let claimed = claim_claude_binding(&data, instance_id).unwrap();
        // Saving the binding failed, so the record isn't acknowledged; the
        // hook runs again before the next tick.
        drop(claimed);
        resume_hook(&data, &home, &cwd, instance_id, &b);

        let retry = claim_claude_binding(&data, instance_id).unwrap();
        assert_eq!(claimed_session(&retry, &home), a);
        acknowledge_claude_binding(retry);
        let next = claim_claude_binding(&data, instance_id).unwrap();
        assert_eq!(claimed_session(&next, &home), b);
        acknowledge_claude_binding(next);
        assert!(claim_claude_binding(&data, instance_id).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_non_resume_events_and_mismatched_transcript_names() {
        let root = temp_dir();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        for event in [
            json!({
                "session_id": session_id,
                "transcript_path": root.join(format!("{session_id}.jsonl")),
                "cwd": root,
                "hook_event_name": "SessionStart",
                "source": "startup"
            }),
            json!({
                "session_id": session_id,
                "transcript_path": root.join("other.jsonl"),
                "cwd": root,
                "hook_event_name": "SessionStart",
                "source": "resume"
            }),
        ] {
            assert!(
                write_claude_binding_from_reader(
                    &root,
                    instance_id,
                    std::io::Cursor::new(event.to_string()),
                )
                .is_err()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_transcript_paths_that_escape_or_nest_below_the_project_store() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let projects = home.join(".claude/projects");
        std::fs::create_dir_all(&projects).unwrap();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4().to_string();
        let cwd = root.join("project");

        let outside = home.join(".claude").join(format!("{session_id}.jsonl"));
        std::fs::write(&outside, "{}\n").unwrap();
        let traversal = projects.join("..").join(format!("{session_id}.jsonl"));

        let nested = projects
            .join("project")
            .join("nested")
            .join(format!("{session_id}.jsonl"));
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "{}\n").unwrap();

        for transcript in [traversal, nested] {
            let event = json!({
                "session_id": session_id,
                "transcript_path": transcript,
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": "resume"
            });
            write_claude_binding_from_reader(
                &data,
                instance_id,
                std::io::Cursor::new(event.to_string()),
            )
            .unwrap();
            assert_eq!(
                read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
                None
            );
        }

        #[cfg(unix)]
        {
            let escaped = root.join("escaped-project");
            std::fs::create_dir_all(&escaped).unwrap();
            std::fs::write(escaped.join(format!("{session_id}.jsonl")), "{}\n").unwrap();
            let linked = projects.join("linked-project");
            std::os::unix::fs::symlink(&escaped, &linked).unwrap();
            let event = json!({
                "session_id": session_id,
                "transcript_path": linked.join(format!("{session_id}.jsonl")),
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": "resume"
            });
            write_claude_binding_from_reader(
                &data,
                instance_id,
                std::io::Cursor::new(event.to_string()),
            )
            .unwrap();
            assert_eq!(
                read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
                None
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pane_muxel_starts_itself_is_told_its_own_instance_id_and_no_other_carries_one() {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        let id = Uuid::new_v4();
        let configured = vec![
            pair("FOO", "bar"),
            pair("muxel_instance_id", "stale"),
            pair(MUXEL_INSTANCE_ID_ENV, "also-stale"),
        ];

        let (local, removed) = pane_env(&configured, Vec::new(), id, PaneProcess::Program, false);
        assert_eq!(
            local,
            vec![
                pair("FOO", "bar"),
                pair(MUXEL_INSTANCE_ID_ENV, &id.to_string())
            ]
        );
        assert!(removed.is_empty());
        // It is what the Claude hook reads back.
        assert_eq!(
            hook_instance_from_args(
                [OsString::from(CLAUDE_HOOK_FLAG)],
                Some(local[1].1.clone().into())
            ),
            Ok(Some(id))
        );

        // A tmux client has no id, set or inherited (tmux hands the id over).
        let (client, removed) =
            pane_env(&configured, Vec::new(), id, PaneProcess::TmuxClient, false);
        assert_eq!(client, vec![pair("FOO", "bar")]);
        assert_eq!(removed, [MUXEL_INSTANCE_ID_ENV]);

        // Nor has an ssh client, which an ssh `SendEnv` could forward it from. What
        // the launch needs, such as ssh's password, is kept.
        let launch = vec![pair("SSHPASS", "pw")];
        let (remote, removed) = pane_env(&configured, launch, id, PaneProcess::SshClient, false);
        assert_eq!(remote, vec![pair("FOO", "bar"), pair("SSHPASS", "pw")]);
        assert_eq!(removed, [MUXEL_INSTANCE_ID_ENV]);
    }

    #[test]
    fn a_tmux_panes_program_gets_its_own_id_or_none_never_the_servers() {
        let id = Uuid::new_v4();
        let own = format!("{MUXEL_INSTANCE_ID_ENV}={id}");
        let clear = ["set-environment", "-g", "-u", MUXEL_INSTANCE_ID_ENV, ";"];
        // After `-u set -g mouse on ;`, before the session is created.
        let args = tmux_pane_args(Socket::Default, "s", Some("claude"), &[], id, true, None)
            .unwrap()
            .args;
        assert_eq!(args[6..11], clear);
        assert_eq!(
            args[11..],
            ["new-session", "-A", "-s", "s", "-e", &own, "--", "claude"]
        );
        // Older tmux has no `-e`: the program starts without an id rather than
        // with whichever pane's the server had.
        let args = tmux_pane_args(Socket::Default, "s", None, &[], id, false, None)
            .unwrap()
            .args;
        assert_eq!(args[6..11], clear);
        assert_eq!(args[11..], ["new-session", "-A", "-s", "s"]);
    }

    /// muxel's own server is named ahead of everything else the client is told; a
    /// pane from before it had one reattaches on the default server with exactly
    /// the argv it always had.
    #[test]
    fn a_tmux_pane_names_the_server_its_session_is_on() {
        let id = Uuid::new_v4();
        let private = tmux_pane_args(Socket::Private, "s", Some("claude"), &[], id, true, None)
            .unwrap()
            .args;
        let default = tmux_pane_args(Socket::Default, "s", Some("claude"), &[], id, true, None)
            .unwrap()
            .args;
        assert_eq!(private[..2], ["-L", "mxl"]);
        assert_eq!(private[2..], default[..]);
        assert_eq!(default[0], "-u");
    }

    #[test]
    fn isolated_tmux_sets_profile_only_on_its_session_and_client() {
        let id = Uuid::new_v4();
        let root = Path::new("/isolated root/quote'and;equals=dir");
        let argv = vec!["two words".into(), "".into(), "--flag".into()];
        let launch = tmux_pane_args(
            Socket::Default,
            "s",
            Some("agent"),
            &argv,
            id,
            true,
            Some(root),
        )
        .unwrap();
        assert_eq!(&launch.args[..2], Socket::isolated(root).args());
        let new = launch
            .args
            .iter()
            .position(|arg| arg == "new-session")
            .unwrap();
        assert_eq!(&launch.args[new..new + 4], ["new-session", "-A", "-s", "s"]);
        for (i, (key, value)) in launch.env.iter().enumerate() {
            assert_eq!(launch.args[new + 4 + i * 2], "-e");
            assert_eq!(launch.args[new + 5 + i * 2], format!("{key}={value}"));
        }
        assert_eq!(launch.env.len(), 3);
        assert_eq!(
            &launch.args[new + 10..],
            ["--", "agent", "two words", "", "--flag"]
        );
        // No profile values are installed globally on this profile's server.
        assert_eq!(
            &launch.args[2..new],
            [
                "-u",
                "set",
                "-g",
                "mouse",
                "on",
                ";",
                "set-environment",
                "-g",
                "-u",
                MUXEL_INSTANCE_ID_ENV,
                ";"
            ]
        );
        let configured = vec![
            ("MUXEL_CONTROL".into(), "foreign.json".into()),
            ("muxel_control".into(), "also-foreign.json".into()),
            ("muxel_user_data_dir".into(), "".into()),
        ];
        let (client, removed) =
            pane_env(&configured, launch.env, id, PaneProcess::TmuxClient, true);
        assert_eq!(removed, [MUXEL_INSTANCE_ID_ENV]);
        assert!(!client.iter().any(|(key, _)| key == MUXEL_INSTANCE_ID_ENV));
        for key in [muxel_store::USER_DATA_DIR_ENV, crate::control::CONTROL_ENV] {
            assert_eq!(
                client
                    .iter()
                    .filter(|(name, _)| name.eq_ignore_ascii_case(key))
                    .count(),
                1
            );
        }
        assert_eq!(
            client.last().unwrap(),
            &(
                "MUXEL_CONTROL".into(),
                root.join("data")
                    .join(muxel_core::control::ENDPOINT_FILE)
                    .to_str()
                    .unwrap()
                    .into()
            )
        );
        assert!(tmux_pane_args(Socket::Default, "s", None, &[], id, false, Some(root)).is_err());
    }

    #[test]
    fn isolated_tmux_client_keeps_the_root_while_argv_escapes_its_separator() {
        let root = Path::new("/isolated;");
        let launch = tmux_pane_args(
            Socket::Default,
            "s",
            None,
            &[],
            Uuid::new_v4(),
            true,
            Some(root),
        )
        .unwrap();
        assert!(
            launch
                .args
                .contains(&r"MUXEL_USER_DATA_DIR=/isolated\;".to_string())
        );
        assert!(
            launch
                .env
                .contains(&("MUXEL_USER_DATA_DIR".into(), "/isolated;".into()))
        );
        let endpoint = root.join("data").join(muxel_core::control::ENDPOINT_FILE);
        assert!(
            launch
                .args
                .contains(&format!("MUXEL_CONTROL={}", endpoint.display()))
        );
    }

    #[test]
    fn profile_overrides_remain_configurable_without_isolation() {
        let id = Uuid::new_v4();
        let configured = vec![
            ("muxel_user_data_dir".into(), "configured-root".into()),
            ("MUXEL_CONTROL".into(), "configured-control".into()),
            ("FOO".into(), "bar".into()),
        ];
        for process in [
            PaneProcess::Program,
            PaneProcess::TmuxClient,
            PaneProcess::SshClient,
        ] {
            let (env, removed) = pane_env(&configured, vec![], id, process, false);
            assert_eq!(&env[..configured.len()], &configured);
            assert!(!removed.iter().any(|key| key == crate::control::CONTROL_ENV));
        }
        for process in [PaneProcess::Program, PaneProcess::SshClient] {
            let (env, removed) = pane_env(
                &configured,
                vec![("muxel_control".into(), "untrusted-launch".into())],
                id,
                process,
                true,
            );
            assert!(!env.iter().any(|(key, _)| {
                key.eq_ignore_ascii_case(muxel_store::USER_DATA_DIR_ENV)
                    || key.eq_ignore_ascii_case(crate::control::CONTROL_ENV)
            }));
            assert!(env.contains(&("FOO".into(), "bar".into())));
            assert!(removed.contains(&crate::control::CONTROL_ENV.to_string()));
            assert_eq!(
                removed.contains(&MUXEL_INSTANCE_ID_ENV.to_string()),
                process == PaneProcess::SshClient
            );
        }
    }

    #[test]
    fn isolated_direct_preset_overrides_cannot_escape_in_spawned_pty_children() {
        let root = temp_dir();
        for value in ["", "foreign-profile"] {
            for lowercase in [false, true] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "session_binding::tests::isolated_direct_child_probe",
                        "--nocapture",
                    ])
                    .env("MUXEL_B3_PROBE_STAGE", "launch")
                    .env("MUXEL_B3_PROBE_ROOT", &root)
                    .env("MUXEL_B3_PRESET_VALUE", value)
                    .env("MUXEL_B3_LOWERCASE", lowercase.to_string())
                    .env(muxel_store::USER_DATA_DIR_ENV, &root)
                    .env(crate::control::CONTROL_ENV, "inherited-foreign.json")
                    .env(MUXEL_INSTANCE_ID_ENV, "inherited-wrong-id")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "value={value:?}, lowercase={lowercase}: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn isolated_direct_child_probe() {
        let Ok(stage) = std::env::var("MUXEL_B3_PROBE_STAGE") else {
            return;
        };
        let root = std::env::var_os("MUXEL_B3_PROBE_ROOT").unwrap();
        if stage == "observe" {
            assert_eq!(
                std::env::var_os(muxel_store::USER_DATA_DIR_ENV),
                Some(root.clone())
            );
            assert_eq!(
                muxel_store::data_dir(),
                Some(PathBuf::from(root).join("data"))
            );
            assert!(std::env::var_os(crate::control::CONTROL_ENV).is_none());
            assert_eq!(
                std::env::var(MUXEL_INSTANCE_ID_ENV).unwrap(),
                std::env::var("MUXEL_B3_EXPECTED_ID").unwrap()
            );
            println!("B3_ISOLATED_CHILD_OK");
            return;
        }
        let lowercase = std::env::var("MUXEL_B3_LOWERCASE").unwrap() == "true";
        let value = std::env::var("MUXEL_B3_PRESET_VALUE").unwrap();
        let keys = [muxel_store::USER_DATA_DIR_ENV, crate::control::CONTROL_ENV];
        let configured: Vec<_> = keys
            .into_iter()
            .map(|key| {
                (
                    if lowercase {
                        key.to_ascii_lowercase()
                    } else {
                        key.to_string()
                    },
                    value.clone(),
                )
            })
            .collect();
        let id = Uuid::new_v4();
        let (env, env_remove) = pane_env(&configured, Vec::new(), id, PaneProcess::Program, true);
        let mut spec = muxel_terminal::CommandSpec::program(
            std::env::current_exe().unwrap().to_str().unwrap(),
            vec![
                "--exact".into(),
                "session_binding::tests::isolated_direct_child_probe".into(),
                "--nocapture".into(),
            ],
        );
        spec.env = env;
        spec.env.extend([
            ("MUXEL_B3_PROBE_STAGE".into(), "observe".into()),
            ("MUXEL_B3_EXPECTED_ID".into(), id.to_string()),
        ]);
        spec.env_remove = env_remove;
        let (_session, rx) = muxel_terminal::TerminalSession::spawn(spec, 100, 30).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut output = Vec::new();
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "child timed out: {}",
                String::from_utf8_lossy(&output)
            );
            match rx.try_recv() {
                Ok(muxel_terminal::PtyChunk::Output(bytes)) => output.extend(bytes),
                Ok(muxel_terminal::PtyChunk::Exit {
                    code, read_error, ..
                }) => {
                    assert_eq!(code, Some(0), "{}", String::from_utf8_lossy(&output));
                    assert!(read_error.is_none());
                    assert!(String::from_utf8_lossy(&output).contains("B3_ISOLATED_CHILD_OK"));
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }

    #[test]
    fn hook_mode_requires_one_valid_instance_uuid() {
        let id = Uuid::new_v4();
        assert_eq!(
            hook_instance_from_args(
                [OsString::from(CLAUDE_HOOK_FLAG), id.to_string().into()],
                None
            ),
            Ok(Some(id))
        );
        assert_eq!(
            hook_instance_from_args(
                [OsString::from(CLAUDE_HOOK_FLAG)],
                Some(id.to_string().into())
            ),
            Ok(Some(id))
        );
        assert!(hook_instance_from_args([OsString::from(CLAUDE_HOOK_FLAG)], None).is_err());
        assert_eq!(
            hook_instance_from_args([OsString::from("--other")], Some(id.to_string().into())),
            Ok(None)
        );
    }
}
