//! Exact provider session bindings that must survive an in-process `/resume`.

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::{Hash as _, Hasher as _};
use std::io::Read as _;
use std::path::{Path, PathBuf};
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
pub(crate) fn run_muse_hook_from_args(args: &[String]) -> bool {
    if args.first().map(String::as_str) != Some(MUSE_HOOK_FLAG) {
        return false;
    }
    let args = args.to_vec();
    wait_for_muse_hook(MUSE_HOOK_WORK_TIMEOUT, move || {
        // These observation/context hooks must not turn provider I/O errors
        // into a failed hook or add unbounded stderr writes to the exit path.
        let _ = run_muse_hook(&args);
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

fn write_muse_binding_from_reader(
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

pub(crate) fn read_muse_binding(
    data_dir: &Path,
    instance_id: Uuid,
    current: Option<&str>,
) -> Option<muxel_core::muse::MuseMetadata> {
    let pane = muse_pane_dir(data_dir, instance_id);
    let launch: MuseLaunch =
        serde_json::from_slice(&std::fs::read(pane.join("launch.json")).ok()?).ok()?;
    let binding: MuseBinding = serde_json::from_slice(
        &std::fs::read(pane.join(format!("{}.json", launch.launch_id))).ok()?,
    )
    .ok()?;
    (launch.launch_id == binding.launch_id
        && !launch.launch_id.is_nil()
        && Uuid::parse_str(&binding.metadata.session_id).is_ok()
        && (current == Some(binding.metadata.session_id.as_str())
            || muse_session_exists(&launch.session_root, &binding.metadata.session_id).ok()?))
    .then_some(binding.metadata)
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
            let binding = read_muse_binding(&root, pane, None).unwrap();
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
pub(crate) fn pane_env(
    configured: &[(String, String)],
    launch: Vec<(String, String)>,
    instance_id: Uuid,
    process: PaneProcess,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut env = configured.to_vec();
    env.extend(launch);
    let not_the_id = |(key, _): &(String, String)| !key.eq_ignore_ascii_case(MUXEL_INSTANCE_ID_ENV);
    match process {
        PaneProcess::Program => {
            env.retain(not_the_id);
            env.push((MUXEL_INSTANCE_ID_ENV.to_string(), instance_id.to_string()));
            (env, Vec::new())
        }
        PaneProcess::TmuxClient | PaneProcess::SshClient => {
            env.retain(not_the_id);
            (env, vec![MUXEL_INSTANCE_ID_ENV.to_string()])
        }
    }
}

/// The tmux arguments for a local pane kept in tmux session `session`, running
/// `program` (with none, the user's shell).
///
/// The program starts from the tmux server's environment, not this client's, so
/// the pane's id is handed over with `new-session -e` where the local tmux takes
/// it (`session_env`: tmux 3.2 and later). Only the id: a command line is visible
/// to other processes, and the rest of a pane's environment can hold secrets and
/// paths. First, on every tmux, the id is cleared from the server: one started
/// from inside another pane (by a `tmux` typed in a shell pane, or by a muxel run
/// there) has that pane's id, which a program started without `-e` would take as
/// its own.
pub(crate) fn tmux_pane_args(
    session: &str,
    program: Option<&str>,
    args: &[String],
    instance_id: Uuid,
    session_env: bool,
) -> Vec<String> {
    let own = [(MUXEL_INSTANCE_ID_ENV.to_string(), instance_id.to_string())];
    let env: &[(String, String)] = if session_env { &own } else { &[] };
    // No `-c`: the client is spawned with `cwd` already, which is what tmux uses
    // for a new session anyway. Passing it would put the project's path in this
    // client's argv for no gain.
    muxel_core::tmux::launch_session_args_with_env(
        session,
        None,
        program,
        args,
        env,
        &[MUXEL_INSTANCE_ID_ENV],
    )
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
    let event: Value = serde_json::from_str(&input).context("parsing Claude SessionStart input")?;
    validate_claude_event(&event)?;

    let path = claude_binding_path(data_dir, instance_id);
    let parent = path.parent().context("Claude binding path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating Claude binding directory {}", parent.display()))?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temp, input.as_bytes())
        .with_context(|| format!("writing Claude binding {}", temp.display()))?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("replacing Claude binding {}", path.display()))?;
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

pub(crate) fn claude_session_id_from_binding(
    data_dir: &Path,
    instance_id: Uuid,
    home: &Path,
) -> Option<String> {
    let event: Value =
        serde_json::from_slice(&std::fs::read(claude_binding_path(data_dir, instance_id)).ok()?)
            .ok()?;
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
    Some(session_id.to_string())
}

pub(crate) fn clear_claude_binding(data_dir: &Path, instance_id: Uuid) {
    let _ = std::fs::remove_file(claude_binding_path(data_dir, instance_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxel-claude-binding-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
            claude_session_id_from_binding(&data, instance_id, &home).as_deref(),
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
            claude_session_id_from_binding(&data, instance_id, &home),
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
            claude_session_id_from_binding(&data, instance_id, &home),
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
                claude_session_id_from_binding(&data, instance_id, &home).as_deref(),
                Some(session_id.as_str()),
                "{source} should bind before Claude writes the first transcript record"
            );
        }
        clear_claude_binding(&data, instance_id);
        assert!(!claude_binding_path(&data, instance_id).exists());
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
                claude_session_id_from_binding(&data, instance_id, &home),
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
                claude_session_id_from_binding(&data, instance_id, &home),
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

        let (local, removed) = pane_env(&configured, Vec::new(), id, PaneProcess::Program);
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
        let (client, removed) = pane_env(&configured, Vec::new(), id, PaneProcess::TmuxClient);
        assert_eq!(client, vec![pair("FOO", "bar")]);
        assert_eq!(removed, [MUXEL_INSTANCE_ID_ENV]);

        // Nor has an ssh client, which an ssh `SendEnv` could forward it from. What
        // the launch needs, such as ssh's password, is kept.
        let launch = vec![pair("SSHPASS", "pw")];
        let (remote, removed) = pane_env(&configured, launch, id, PaneProcess::SshClient);
        assert_eq!(remote, vec![pair("FOO", "bar"), pair("SSHPASS", "pw")]);
        assert_eq!(removed, [MUXEL_INSTANCE_ID_ENV]);
    }

    #[test]
    fn a_tmux_panes_program_gets_its_own_id_or_none_never_the_servers() {
        let id = Uuid::new_v4();
        let own = format!("{MUXEL_INSTANCE_ID_ENV}={id}");
        let clear = ["set-environment", "-g", "-u", MUXEL_INSTANCE_ID_ENV, ";"];
        // After `-u set -g mouse on ;`, before the session is created.
        let args = tmux_pane_args("s", Some("claude"), &[], id, true);
        assert_eq!(args[6..11], clear);
        assert_eq!(
            args[11..],
            ["new-session", "-A", "-s", "s", "-e", &own, "--", "claude"]
        );
        // Older tmux has no `-e`: the program starts without an id rather than
        // with whichever pane's the server had.
        let args = tmux_pane_args("s", None, &[], id, false);
        assert_eq!(args[6..11], clear);
        assert_eq!(args[11..], ["new-session", "-A", "-s", "s"]);
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
