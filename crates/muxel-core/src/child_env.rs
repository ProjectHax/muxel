//! Child process environment policy shared by PTYs and helper commands.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::process::Command;

/// These settings belong to the muxel process that enabled profiling. A child
/// muxel must opt in itself, with its own output files.
pub const PROFILER_VARS: &[&str] = &[
    "MUXEL_PROFILE",
    "MUXEL_PROFILE_TERMINAL",
    "MUXEL_PROFILE_UI",
    "MUXEL_PROFILE_LOG",
    "MUXEL_PROFILE_UI_LOG",
    "MUXEL_PROFILE_STDERR",
    "MUXEL_PROFILE_CHAINS",
    "MUXEL_PROFILE_KEEP_DAYS",
    "MUXEL_PROFILE_MAX_BYTES",
];

/// Profiler names are reserved as a family, including knobs introduced later.
pub fn is_profiler_var(key: &OsStr) -> bool {
    const PREFIX: &[u8] = b"MUXEL_PROFILE";
    key.as_encoded_bytes()
        .get(..PREFIX.len())
        .is_some_and(|prefix| {
            if cfg!(windows) {
                prefix.eq_ignore_ascii_case(PREFIX)
            } else {
                prefix == PREFIX
            }
        })
}

/// Keep known names for stale tmux servers, plus all matching names observed at
/// the launch boundary. Only matching keys are copied; pane context is untouched.
pub fn profiler_env_keys(names: impl IntoIterator<Item = OsString>) -> BTreeSet<OsString> {
    PROFILER_VARS
        .iter()
        .map(|key| OsString::from(*key))
        .chain(names.into_iter().filter(|key| is_profiler_var(key)))
        .collect()
}

/// Construct a child command without inheriting muxel's profiler settings.
/// Building the command performs no I/O and leaves pane/control context intact.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    remove_profiler_env(&mut command);
    command
}

/// Apply after custom overrides when a caller accepts an environment map.
pub fn remove_profiler_env(command: &mut Command) {
    let keys = profiler_env_keys(
        std::env::vars_os()
            .map(|(key, _)| key)
            .chain(command.get_envs().map(|(key, _)| key.to_owned())),
    );
    for key in keys {
        command.env_remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::{PROFILER_VARS, command};
    use std::ffi::OsStr;

    #[test]
    fn command_environment_drops_unknown_profiler_keys() {
        const PROBE: &str = "MUXEL_TEST_PROFILE_PREFIX_PROBE";
        if std::env::var_os(PROBE).as_deref() != Some(OsStr::new("child")) {
            // Seed only this isolated test process, never the suite's globals.
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "child_env::tests::command_environment_drops_unknown_profiler_keys",
                    "--nocapture",
                ])
                .env(PROBE, "child")
                .env("MUXEL_PROFILE_FUTURE", "inherited")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let mut child = command("unused");
        assert!(
            child
                .get_envs()
                .any(|(key, value)| key == "MUXEL_PROFILE_FUTURE" && value.is_none()),
            "inherited future profiler setting leaked"
        );
        child
            .env("MUXEL_PROFILE_FUTURE", "configured")
            .env("MUXEL_CONTROL", "own-control");
        super::remove_profiler_env(&mut child);
        assert!(
            !child
                .get_envs()
                .any(|(key, value)| key == "MUXEL_PROFILE_FUTURE" && value.is_some()),
            "unknown profiler setting leaked"
        );
        assert!(
            child
                .get_envs()
                .any(|(key, value)| key == "MUXEL_CONTROL"
                    && value == Some(OsStr::new("own-control")))
        );
    }

    #[test]
    fn profiler_key_population_keeps_pane_context_and_matches_os_case_rules() {
        use super::{is_profiler_var, profiler_env_keys};
        use std::ffi::OsString;
        for key in [
            "MUXEL_PROFILE",
            "MUXEL_PROFILE_FUTURE",
            "MUXEL_PROFILE_KEEP_DAYS",
            "MUXEL_PROFILE_MAX_BYTES",
        ] {
            assert!(is_profiler_var(OsStr::new(key)), "{key}");
        }
        assert_eq!(
            is_profiler_var(OsStr::new("muxel_profile_future")),
            cfg!(windows)
        );
        let keys = profiler_env_keys(
            [
                "MUXEL_PROFILE_FUTURE",
                "MUXEL_INSTANCE_ID",
                "MUXEL_USER_DATA_DIR",
                "MUXEL_CONTROL",
            ]
            .into_iter()
            .map(OsString::from),
        );
        assert!(keys.contains(OsStr::new("MUXEL_PROFILE_FUTURE")));
        for key in ["MUXEL_INSTANCE_ID", "MUXEL_USER_DATA_DIR", "MUXEL_CONTROL"] {
            assert!(!keys.contains(OsStr::new(key)), "{key}");
        }
    }

    #[test]
    fn helper_environment_removes_profiler_without_changing_pane_context() {
        let command = command("unused");
        let changes: Vec<_> = command.get_envs().collect();
        for key in PROFILER_VARS {
            assert!(changes.contains(&(OsStr::new(key), None)), "{key}");
        }
        for key in ["MUXEL_INSTANCE_ID", "MUXEL_USER_DATA_DIR", "MUXEL_CONTROL"] {
            assert!(!changes.iter().any(|(name, _)| *name == OsStr::new(key)));
        }
    }
}
