//! `muxel --user-data-dir DIR`: keep a run's settings and state in `DIR` instead
//! of the platform's config and data directories.
//!
//! The name is the one Zed, VS Code and Chromium use for the same thing. It is the
//! only way to isolate a run on Windows, where the platform directories come from
//! the OS and ignore `XDG_*` and `HOME`.
//!
//! The flag is turned into [`muxel_store::USER_DATA_DIR_ENV`] at startup, which is
//! what `muxel_store` reads. What muxel starts inherits it: its panes, the Claude
//! session hook, and `muxel ctl` typed in a pane, which therefore reaches the muxel
//! it runs in. Setting the variable directly does the same as the flag. On Windows
//! and Linux the updater relaunches the new binary directly, so it inherits the
//! variable too. macOS relaunches through `open`, whose documented environment
//! inheritance has not been exercised here; updater relaunch isolation remains
//! unverified, not a demonstrated escape.
//!
//! Keychain entries get a service named from the directory (see `secrets`). Not
//! moved: files muxel keeps in a project, and the opt-in profiler logs, which go
//! where `MUXEL_PROFILE_*_LOG` or `XDG_DATA_HOME` say.
//!
//! The muxel that `muxel ctl` reaches is named by the first of these that is set:
//!
//! 1. `--user-data-dir DIR` (or `--user-data-dir=DIR`) on this command line;
//! 2. an inherited `MUXEL_USER_DATA_DIR`;
//! 3. `MUXEL_CONTROL`, only without an isolated root;
//! 4. the platform's data directory.
//!
//! With an isolated root, `ctl` accepts `MUXEL_CONTROL` only when it names that
//! root's own endpoint; a foreign endpoint is an `isolated_endpoint_conflict`.
//! A GUI run under any isolated root clears `MUXEL_CONTROL` before starting panes.

use std::ffi::OsString;
use std::path::PathBuf;

const FLAG: &str = "--user-data-dir";

pub(crate) fn is_isolated() -> bool {
    std::env::var_os(muxel_store::USER_DATA_DIR_ENV).is_some_and(|root| !root.is_empty())
}

/// Apply `--user-data-dir` (or an inherited [`muxel_store::USER_DATA_DIR_ENV`]) to
/// this process and everything it starts, and return the command line without the
/// flag. Must run first in `main`, while the process is still single-threaded.
pub fn configure(args: Vec<OsString>) -> Result<Vec<OsString>, String> {
    let (flag, rest) = take_flag(args)?;
    let inherited = std::env::var_os(muxel_store::USER_DATA_DIR_ENV);
    let dir = resolve(flag, inherited)?;
    let isolated = dir.is_some();
    if let Some(dir) = dir {
        // SAFETY: called first in `main`, before any thread is spawned.
        unsafe { std::env::set_var(muxel_store::USER_DATA_DIR_ENV, dir) };
    }
    if isolated && rest.first().is_none_or(|arg| arg != "ctl") {
        // GUI children must never inherit a foreign endpoint. ctl retains it so
        // endpoint selection can report a conflict instead of silently overriding it.
        // SAFETY: as above.
        unsafe { std::env::remove_var(crate::control::CONTROL_ENV) };
    }
    Ok(rest)
}

/// Split `--user-data-dir DIR` or `--user-data-dir=DIR` out of `args`. It may
/// appear anywhere before a `ctl` subcommand; everything from `ctl` on belongs to
/// `ctl` and is left alone.
fn take_flag(args: Vec<OsString>) -> Result<(Option<PathBuf>, Vec<OsString>), String> {
    let mut dir: Option<OsString> = None;
    let mut rest = Vec::with_capacity(args.len());
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "ctl" {
            rest.push(arg);
            rest.extend(args);
            break;
        }
        let value = if arg == FLAG {
            args.next()
                .ok_or_else(|| format!("invalid_user_data_dir: {FLAG} needs a directory"))?
        } else if arg.as_encoded_bytes().starts_with(b"--user-data-dir=") {
            let text = arg.to_str().ok_or_else(|| {
                format!("invalid_user_data_dir: {FLAG}=DIR needs a Unicode path; use `{FLAG} DIR`")
            })?;
            OsString::from(&text[FLAG.len() + 1..])
        } else if muxel_core::startup_flags::names_flag(arg.as_os_str(), FLAG) {
            return Err(format!("invalid_user_data_dir: unsupported {FLAG} form"));
        } else {
            rest.push(arg);
            continue;
        };
        if value.is_empty() || value.as_encoded_bytes().starts_with(b"--") {
            return Err(format!("invalid_user_data_dir: {FLAG} needs a directory"));
        }
        if dir.is_some() {
            return Err(format!(
                "invalid_user_data_dir: {FLAG} was given more than once"
            ));
        }
        dir = Some(value);
    }
    Ok((dir.map(PathBuf::from), rest))
}

/// The directory to use, made absolute so it still names the same place from a
/// pane that starts in another directory. The flag wins over an inherited value.
fn resolve(flag: Option<PathBuf>, inherited: Option<OsString>) -> Result<Option<PathBuf>, String> {
    let Some(dir) = flag.or_else(|| inherited.filter(|d| !d.is_empty()).map(PathBuf::from)) else {
        return Ok(None);
    };
    std::path::absolute(&dir)
        .map(Some)
        .map_err(|e| format!("{FLAG} {}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::{configure, resolve, take_flag};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn takes_the_flag_out_wherever_it_is() {
        let dir = std::env::temp_dir().join("muxel-user-data");
        let dir_str = dir.to_str().unwrap();
        for (line, rest) in [
            (vec!["--user-data-dir", dir_str], vec![]),
            (vec!["--other", "--user-data-dir", dir_str], vec!["--other"]),
            (vec!["--user-data-dir", dir_str, "--other"], vec!["--other"]),
        ] {
            assert_eq!(take_flag(args(&line)), Ok((Some(dir.clone()), args(&rest))));
        }
        let joined = format!("--user-data-dir={dir_str}");
        assert_eq!(take_flag(args(&[&joined])), Ok((Some(dir), vec![])));
    }

    #[test]
    fn leaves_ctl_and_its_arguments_alone() {
        let line = args(&[
            "--user-data-dir",
            "x",
            "ctl",
            "send",
            "a",
            "--user-data-dir",
        ]);
        assert_eq!(
            take_flag(line),
            Ok((
                Some(PathBuf::from("x")),
                args(&["ctl", "send", "a", "--user-data-dir"])
            ))
        );
        let line = args(&["ctl", "status"]);
        assert_eq!(take_flag(line.clone()), Ok((None, line)));
    }

    #[test]
    fn refuses_a_missing_empty_or_repeated_directory() {
        assert!(take_flag(args(&["--user-data-dir"])).is_err());
        assert!(take_flag(args(&["--user-data-dir", ""])).is_err());
        assert!(take_flag(args(&["--user-data-dir="])).is_err());
        assert!(take_flag(args(&["--user-data-dir", "a", "--user-data-dir", "b"])).is_err());
    }

    #[test]
    fn refuses_unconsumed_user_data_dir_forms() {
        for arg in ["--user-data-dir:root", "--user-data-directory=root"] {
            assert!(take_flag(args(&[arg])).is_err(), "{arg}");
        }
    }

    #[test]
    fn refuses_case_and_dash_user_data_dir_variants() {
        for flag in [
            "--USER-DATA-DIR",
            "-user-data-dir",
            "---user-data-dir",
            "--User-Data-Dir=root",
        ] {
            assert!(take_flag(args(&[flag, "root"])).is_err(), "{flag}");
        }
        let payload = args(&["ctl", "send", "pane", "--USER-DATA-DIR"]);
        assert_eq!(take_flag(payload.clone()), Ok((None, payload)));
        let ordinary = args(&["user-data-dir", "root"]);
        assert_eq!(take_flag(ordinary.clone()), Ok((None, ordinary)));
    }

    #[test]
    fn refuses_another_flag_as_the_directory() {
        for value in ["--version", "--build-info-json", "--user-data-dir=root"] {
            assert!(take_flag(args(&["--user-data-dir", value])).is_err());
        }
    }

    #[test]
    fn isolated_gui_does_not_inherit_foreign_control() {
        const CHILD: &str = "MUXEL_ISOLATION_ENV_TEST";
        if let Ok(kind) = std::env::var(CHILD) {
            let line = if kind == "explicit" {
                vec![
                    "--user-data-dir".into(),
                    std::env::temp_dir().into_os_string(),
                ]
            } else {
                vec![]
            };
            assert_eq!(configure(line), Ok(vec![]));
            assert!(std::env::var_os(crate::control::CONTROL_ENV).is_none());
            return;
        }
        for kind in ["explicit", "inherited"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "user_data_dir::tests::isolated_gui_does_not_inherit_foreign_control",
                ])
                .env(CHILD, kind)
                .env(muxel_store::USER_DATA_DIR_ENV, std::env::temp_dir())
                .env(crate::control::CONTROL_ENV, "foreign-control.json")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{kind}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }

    #[test]
    fn no_flag_changes_nothing() {
        assert_eq!(take_flag(args(&[])), Ok((None, vec![])));
        assert_eq!(resolve(None, None), Ok(None));
        assert_eq!(resolve(None, Some("".into())), Ok(None));
    }

    #[test]
    fn resolves_to_an_absolute_path_with_the_flag_first() {
        let flag = std::env::temp_dir().join("from-flag");
        let inherited = std::env::temp_dir().join("from-env");
        assert_eq!(
            resolve(Some(flag.clone()), Some(inherited.clone().into())),
            Ok(Some(flag))
        );
        assert_eq!(
            resolve(None, Some(inherited.clone().into())),
            Ok(Some(inherited))
        );
        let relative = resolve(Some(PathBuf::from("sandbox")), None)
            .unwrap()
            .unwrap();
        assert!(relative.is_absolute());
        assert!(relative.ends_with("sandbox"));
    }
}
