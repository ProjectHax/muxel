//! Running a tmux install: probing this computer for a plan, elevating only the
//! install itself, and streaming its output. The decisions — which package
//! manager, which commands, what an exit status means — are
//! `muxel_core::tmux_install`.
//!
//! Elevation, when the plan needs root: already root, or passwordless sudo, runs
//! it with no prompt; otherwise `pkexec`, which shows the desktop's own password
//! dialog. With no polkit agent to show one (or no pkexec), the app asks for the
//! sudo password and calls back with it. Homebrew runs as the user — it refuses
//! root.

use crate::i18n::{t, tf};
use muxel_core::tmux_install::{self as core, Install, PkexecExit, Plan, SudoRefusal};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// What muxel found about this computer.
#[derive(Clone, Debug)]
pub struct Probe {
    pub plan: Plan,
    /// The OS as it names itself (`Ubuntu 24.04.1 LTS`), when known.
    pub system: Option<String>,
}

/// Probe for an install plan. `None` where muxel doesn't run tmux (Windows).
#[cfg(target_os = "linux")]
pub fn probe() -> Option<Probe> {
    let release = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .map(|text| core::parse_os_release(&text))
        .unwrap_or_default();
    let ostree = Path::new("/run/ostree-booted").exists();
    let plan = core::linux_plan(&release, ostree, |p| {
        system_program(p).map(|p| p.to_string_lossy().into_owned())
    });
    Some(Probe {
        plan,
        system: Some(release.name).filter(|n| !n.is_empty()),
    })
}

#[cfg(target_os = "macos")]
pub fn probe() -> Option<Probe> {
    let brew = on_path("brew")
        .or_else(|| {
            core::BREW_PATHS
                .iter()
                .map(PathBuf::from)
                .find(|p| executable(p))
        })
        .map(|p| p.to_string_lossy().into_owned());
    Some(Probe {
        plan: core::macos_plan(brew),
        system: None,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn probe() -> Option<Probe> {
    None
}

/// Progress from [`run`]: output lines as they come, then how it ended.
#[derive(Clone, Debug)]
pub enum Event {
    Line(String),
    Done(Outcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// tmux is installed and on `PATH`; its `tmux -V`.
    Installed(String),
    /// The user closed the system's authentication dialog.
    Dismissed,
    /// Root is needed and only sudo with a password can give it. `wrong` when a
    /// password was just tried and refused.
    NeedPassword { wrong: bool },
    /// It didn't work, in words for the dialog.
    Failed(String),
}

/// Install tmux, reporting to `tx`; ends with [`Event::Done`]. Blocking — call on
/// a thread of its own: it waits for the package manager (and for the user at an
/// authentication dialog). `password` is a sudo password the user gave after an
/// [`Outcome::NeedPassword`].
pub fn run(install: &Install, password: Option<String>, tx: async_channel::Sender<Event>) {
    let outcome = match install_as_needed(install, password, &tx) {
        Outcome::Installed(_) => verify(install),
        other => other,
    };
    let _ = tx.send_blocking(Event::Done(outcome));
}

fn install_as_needed(
    install: &Install,
    password: Option<String>,
    tx: &async_channel::Sender<Event>,
) -> Outcome {
    let script = install.script();
    let sh = |elevate: Option<(&Path, &[&str])>| shell(&script, elevate);
    let finished = |status: std::io::Result<ExitStatus>| match status {
        Ok(s) if s.success() => Outcome::Installed(String::new()),
        Ok(s) => Outcome::Failed(exit_message(install, s.code())),
        Err(e) => Outcome::Failed(e.to_string()),
    };

    if !install.needs_root() || is_root() {
        return finished(stream(sh(None), None, tx));
    }
    let sudo = system_program("sudo");
    if let Some(password) = password {
        let Some(sudo) = sudo else {
            return Outcome::Failed(no_elevation(install));
        };
        return match check_sudo_password(&sudo, &password) {
            Ok(()) => {
                let cmd = sh(Some((sudo.as_path(), &["-S", "-k", "-p", ""][..])));
                finished(stream(cmd, Some(format!("{password}\n")), tx))
            }
            Err(SudoRefusal::WrongPassword) => Outcome::NeedPassword { wrong: true },
            Err(SudoRefusal::NotAllowed) => Outcome::Failed(
                t("Your account isn’t allowed to use sudo, so it can’t install software.")
                    .to_string(),
            ),
            Err(SudoRefusal::Other(message)) => Outcome::Failed(message),
        };
    }
    // Passwordless sudo (NOPASSWD, or a still-valid timestamp) needs no prompt
    // at all.
    let passwordless = sudo.as_deref().filter(|sudo| {
        Command::new(sudo)
            .args(["-n", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    });
    if let Some(sudo) = passwordless {
        return finished(stream(sh(Some((sudo, &["-n"][..]))), None, tx));
    }
    if let Some(pkexec) = system_program("pkexec") {
        match stream(sh(Some((pkexec.as_path(), &[][..]))), None, tx) {
            Ok(status) => match core::pkexec_exit(status.code()) {
                PkexecExit::Installed => return Outcome::Installed(String::new()),
                PkexecExit::Dismissed => return Outcome::Dismissed,
                PkexecExit::Failed(code) => return Outcome::Failed(exit_message(install, code)),
                // No agent to ask, or it said no — sudo may still work.
                PkexecExit::Unauthorized => {}
            },
            Err(e) => log::warn!("tmux install: pkexec failed to start: {e}"),
        }
    }
    if sudo.is_none() {
        return Outcome::Failed(no_elevation(install));
    }
    Outcome::NeedPassword { wrong: false }
}

/// `/bin/sh -c <script>`, behind an elevation helper and its flags when given.
fn shell(script: &str, elevate: Option<(&Path, &[&str])>) -> Command {
    let mut cmd = match elevate {
        Some((helper, flags)) => {
            let mut cmd = Command::new(helper);
            cmd.args(flags).arg("/bin/sh");
            cmd
        }
        None => Command::new("/bin/sh"),
    };
    cmd.arg("-c").arg(script);
    cmd
}

/// Try `password` with `sudo -v` alone, so a typo reads as a wrong password
/// rather than a failed install.
fn check_sudo_password(sudo: &Path, password: &str) -> Result<(), SudoRefusal> {
    let mut child = Command::new(sudo)
        .args(["-S", "-k", "-p", "", "-v"])
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SudoRefusal::Other(e.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "{password}");
    }
    let out = child
        .wait_with_output()
        .map_err(|e| SudoRefusal::Other(e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(core::sudo_refusal(&String::from_utf8_lossy(&out.stderr)))
    }
}

/// The package manager said it worked; make sure muxel can now run tmux.
fn verify(install: &Install) -> Outcome {
    match Command::new("tmux").arg("-V").stdin(Stdio::null()).output() {
        Ok(out) if out.status.success() => {
            Outcome::Installed(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        _ => Outcome::Failed(tf(
            "{manager} finished, but muxel still can’t find tmux on its PATH.",
            &[("manager", install.manager.label())],
        )),
    }
}

/// Run `cmd`, sending each visible line of its output to `tx`, and wait for it.
/// `stdin` is written and closed first (sudo's password); otherwise stdin is
/// empty, so nothing can stop to ask a question no one will see.
fn stream(
    mut cmd: Command,
    stdin: Option<String>,
    tx: &async_channel::Sender<Event>,
) -> std::io::Result<ExitStatus> {
    // An AppImage's library path must not reach a package manager run as root.
    cmd.env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(input.as_bytes());
    }
    let (eof_tx, eof_rx) = mpsc::channel();
    let readers: Vec<Box<dyn Read + Send>> = [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .collect();
    let count = readers.len();
    for pipe in readers {
        let tx = tx.clone();
        let eof_tx = eof_tx.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(pipe);
            let mut buf = Vec::new();
            while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                let text = String::from_utf8_lossy(&buf);
                if let Some(line) = core::visible_line(&text)
                    && tx.send_blocking(Event::Line(line.to_string())).is_err()
                {
                    break;
                }
                buf.clear();
            }
            let _ = eof_tx.send(());
        });
    }
    let status = child.wait()?;
    // Let the readers drain what's left — but not forever: a service the
    // package started can inherit the pipe and hold it open.
    for _ in 0..count {
        if eof_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            break;
        }
    }
    Ok(status)
}

fn exit_message(install: &Install, code: Option<i32>) -> String {
    match code {
        Some(code) => tf(
            "{manager} exited with status {code}.",
            &[
                ("manager", install.manager.label()),
                ("code", &code.to_string()),
            ],
        ),
        None => tf(
            "{manager} was stopped before it finished.",
            &[("manager", install.manager.label())],
        ),
    }
}

fn no_elevation(install: &Install) -> String {
    tf(
        "muxel can’t get administrator rights here (no pkexec or sudo). Run this as root:\n{commands}",
        &[("commands", &install.display())],
    )
}

/// The commands for the user to paste into a terminal themselves.
pub fn terminal_commands(install: &Install) -> String {
    let prefix = if install.needs_root() { "sudo " } else { "" };
    install
        .display()
        .lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join(" && ")
}

#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn is_root() -> bool {
    false
}

/// A system program by absolute path, looked up in the standard system dirs
/// rather than `PATH` (see [`core::SYSTEM_BIN_DIRS`]).
fn system_program(name: &str) -> Option<PathBuf> {
    core::SYSTEM_BIN_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|p| executable(p))
}

#[cfg(target_os = "macos")]
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| executable(p))
}

fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Event, Outcome, run, stream, terminal_commands};
    use muxel_core::tmux_install::{Install, Manager};
    use std::process::Command;

    fn drain(rx: &async_channel::Receiver<Event>) -> Vec<String> {
        let mut lines = Vec::new();
        while let Ok(Event::Line(line)) = rx.try_recv() {
            lines.push(line);
        }
        lines
    }

    #[test]
    fn streams_both_pipes_and_reports_the_exit_status() {
        let (tx, rx) = async_channel::unbounded();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("echo out; echo err >&2; printf ' 10%%\\r100%%\\n'; exit 3");
        let status = stream(cmd, None, &tx).expect("spawn sh");
        assert_eq!(status.code(), Some(3));
        let mut lines = drain(&rx);
        lines.sort();
        assert_eq!(lines, vec!["100%", "err", "out"]);
    }

    #[test]
    fn feeds_stdin_then_closes_it() {
        let (tx, rx) = async_channel::unbounded();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("read pw; echo got:$pw; cat");
        let status = stream(cmd, Some("secret\n".into()), &tx).expect("spawn sh");
        assert!(status.success(), "cat must see EOF, not hang");
        assert_eq!(drain(&rx), vec!["got:secret"]);
    }

    #[test]
    fn terminal_commands_add_sudo_only_when_root_is_needed() {
        let apt = Install {
            manager: Manager::Apt,
            program: "/usr/bin/apt-get".into(),
        };
        assert_eq!(
            terminal_commands(&apt),
            "sudo apt-get update && sudo apt-get install -y -o DPkg::Lock::Timeout=120 tmux"
        );
        let brew = Install {
            manager: Manager::Brew,
            program: "/opt/homebrew/bin/brew".into(),
        };
        assert_eq!(terminal_commands(&brew), "brew install tmux");
    }

    #[test]
    fn a_failing_manager_ends_in_a_failure_that_names_it() {
        // Homebrew runs unelevated, so a stand-in exercises the whole run.
        let install = Install {
            manager: Manager::Brew,
            program: "/bin/false".into(),
        };
        let (tx, rx) = async_channel::unbounded();
        run(&install, None, tx);
        let mut last = None;
        while let Ok(event) = rx.try_recv() {
            last = Some(event);
        }
        let Some(Event::Done(outcome)) = last else {
            panic!("run must end with Done, got {last:?}");
        };
        assert_eq!(
            outcome,
            Outcome::Failed("Homebrew exited with status 1.".into())
        );
    }
}
