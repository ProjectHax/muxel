//! Child process environment policy shared by PTYs and helper commands.

use std::ffi::OsStr;
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
];

/// Construct a child command without inheriting muxel's profiler settings.
/// Building the command performs no I/O and leaves pane/control context intact.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    remove_profiler_env(&mut command);
    command
}

/// Apply after custom overrides when a caller accepts an environment map.
pub fn remove_profiler_env(command: &mut Command) {
    for key in PROFILER_VARS {
        command.env_remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::{PROFILER_VARS, command};
    use std::ffi::OsStr;

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
