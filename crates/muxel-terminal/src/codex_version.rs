//! `codex --version` for Windows Codex launches, probed once per executable.
//!
//! A launch picks its paste codec, and whether to add
//! `--config disable_paste_burst=true`, from the version before the program
//! starts, and launches run on the UI thread. So the probe runs on a thread of
//! its own and a launch waits for it at most [`LAUNCH_WAIT`]. A probe still
//! running then carries on, and its answer serves the next launch. An answer is
//! kept until the executable's size or modification time changes, as when Codex
//! updates. A failed probe isn't kept, so the next launch asks again.

use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};

/// The longest a launch waits for a probe.
pub(crate) const LAUNCH_WAIT: Duration = Duration::from_secs(2);
/// The longest a probe runs before it is abandoned.
const PROBE_LIMIT: Duration = Duration::from_secs(30);

/// One build of an executable, as its size and modification time tell it.
type Stamp = (u64, Option<SystemTime>);

#[derive(Default)]
struct Probe {
    /// Empty while the probe runs; then its output, or `None` if it failed.
    answer: Mutex<Option<Option<String>>>,
    answered: Condvar,
}

impl Probe {
    fn wait(&self, wait: Duration) -> Option<String> {
        let deadline = Instant::now() + wait;
        let mut answer = self.answer.lock();
        while answer.is_none() {
            if self.answered.wait_until(&mut answer, deadline).timed_out() {
                break;
            }
        }
        answer.clone().flatten()
    }

    fn answer(&self, output: Option<String>) {
        *self.answer.lock() = Some(output);
        self.answered.notify_all();
    }
}

type Probes = HashMap<PathBuf, (Stamp, Arc<Probe>)>;

static PROBES: LazyLock<Mutex<Probes>> = LazyLock::new(Default::default);

/// The `--version` output of the executable at `path`, waiting at most `wait`
/// for it. `None` when the probe failed or is still running.
pub(crate) fn version(path: &Path, wait: Duration) -> Option<String> {
    let stamp = stamp(path)?;
    let probe = {
        let mut probes = PROBES.lock();
        match probes.get(path) {
            Some((seen, probe)) if *seen == stamp => probe.clone(),
            _ => {
                let probe = Arc::new(Probe::default());
                if !start(path.to_path_buf(), probe.clone()) {
                    probes.remove(path);
                    return None;
                }
                probes.insert(path.to_path_buf(), (stamp, probe.clone()));
                probe
            }
        }
    };
    probe.wait(wait)
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

/// Run the probe on its own thread; `false` if the thread couldn't start.
fn start(path: PathBuf, probe: Arc<Probe>) -> bool {
    #[cfg(test)]
    note_started(&path);
    std::thread::Builder::new()
        .name("codex-version".into())
        .spawn(move || {
            let output = run(&path);
            if output.is_none() {
                // Before answering, so no launch can read the failure as kept.
                let mut probes = PROBES.lock();
                if probes
                    .get(&path)
                    .is_some_and(|(_, kept)| Arc::ptr_eq(kept, &probe))
                {
                    probes.remove(&path);
                }
            }
            probe.answer(output);
        })
        .is_ok()
}

fn run(path: &Path) -> Option<String> {
    use std::io::Read as _;
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = Command::new(path)
        .arg("--version")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_LIMIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = String::new();
    child
        .stdout
        .take()?
        .take(4096)
        .read_to_string(&mut output)
        .ok()?;
    Some(output)
}

#[cfg(test)]
static STARTED: LazyLock<Mutex<HashMap<PathBuf, usize>>> = LazyLock::new(Default::default);

#[cfg(test)]
fn note_started(path: &Path) {
    *STARTED.lock().entry(path.to_path_buf()).or_default() += 1;
}

#[cfg(test)]
fn probes_started(path: &Path) -> usize {
    STARTED.lock().get(path).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{LAUNCH_WAIT, probes_started, version};
    use crate::session::{CommandSpec, TerminalSession, literal_program_for_spawn};
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            // Best effort: a probe may still hold the executable open.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A stand-in Codex: a copy of `cmd.exe` named `codex.exe`. It answers
    /// `--version` with its banner and exits, and runs as a shell in a pane.
    fn fake_codex() -> (Scratch, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("muxel-codex-version-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let exe = dir.join("codex.exe");
        std::fs::copy(PathBuf::from(system).join("System32").join("cmd.exe"), &exe).unwrap();
        let exe = PathBuf::from(literal_program_for_spawn(exe.to_str().unwrap()).unwrap());
        (Scratch(dir), exe)
    }

    fn launch(exe: &std::path::Path) {
        let spec = CommandSpec::program(exe.to_str().unwrap(), vec![]);
        let (session, _output) = TerminalSession::spawn(spec, 80, 24).unwrap();
        session.kill();
    }

    #[test]
    fn launches_of_one_codex_executable_ask_its_version_once() {
        let (_dir, exe) = fake_codex();
        launch(&exe);
        launch(&exe);
        assert_eq!(
            probes_started(&exe),
            1,
            "the second launch reuses the answer"
        );
        // An update replaces the file; a new modification time stands for one.
        // The stand-in may still be running, so ask only to write attributes.
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        let file = std::fs::File::options()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .open(&exe)
            .unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        drop(file);
        launch(&exe);
        assert_eq!(
            probes_started(&exe),
            2,
            "a changed executable is asked again"
        );
    }

    #[test]
    fn a_probe_a_launch_stopped_waiting_for_still_answers_the_next() {
        let (_dir, exe) = fake_codex();
        let _ = version(&exe, Duration::ZERO);
        let answer = version(&exe, LAUNCH_WAIT * 5);
        assert!(answer.is_some_and(|out| !out.is_empty()));
        assert_eq!(
            probes_started(&exe),
            1,
            "the waiting launch joined the probe"
        );
    }
}
