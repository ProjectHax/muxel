//! Shell-owned marks through a real Git for Windows launcher and shell.
use super::{CommandSpec, PtyChunk, PtyReceiver, ShellCloseCheck, TerminalSession};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Shell {
    session: Arc<TerminalSession>,
    rx: PtyReceiver,
}

impl Shell {
    fn new(program: &Path) -> Self {
        let mut spec = CommandSpec::program(
            program.to_string_lossy(),
            vec!["--noprofile".into(), "--norc".into(), "-i".into()],
        );
        spec.env.push(("HISTFILE".into(), "/dev/null".into()));
        spec.env_remove.extend(
            ["BASH_ENV", "ENV", "PROMPT_COMMAND"]
                .into_iter()
                .map(str::to_owned),
        );
        let (session, rx) = TerminalSession::spawn(spec, 120, 24).unwrap();
        let shell = Self { session, rx };
        shell
            .session
            .write_input(b"PS1='\\[\\e]133;A\\a\\]fixture> \\[\\e]133;B\\a\\]'\r");
        shell.prompt_after(0);
        shell
    }

    fn prompt_after(&self, before: u64) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            while let Ok(chunk) = self.rx.try_recv() {
                match chunk {
                    PtyChunk::Output(bytes) => self.session.process_output(&bytes),
                    PtyChunk::Exit { .. } => panic!("fixture shell exited"),
                }
            }
            if self.session.shell_at_prompt()
                && self.session.shell_close_reading().marks >= before + 2
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no marked prompt: {}",
                self.session.visible_text()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.session.kill();
    }
}

struct Hold(PathBuf);
impl Drop for Hold {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn quote(path: &str) -> String {
    format!("'{}'", path.replace('\\', "/").replace('\'', "'\\''"))
}

fn git_shell(launcher: bool) {
    let Some(wrapper) = muxel_core::resolved_git_bash_program() else {
        eprintln!("Git Bash resolver found no installed launcher; fixture skipped");
        return;
    };
    let Some(root) = wrapper.parent().and_then(Path::parent) else {
        eprintln!("Selected Git Bash has no bin directory; fixture skipped");
        return;
    };
    let real = root.join("usr/bin/bash.exe");
    if !wrapper.is_absolute()
        || !wrapper
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("bash.exe"))
        || !wrapper
            .parent()
            .and_then(Path::file_name)
            .and_then(|dir| dir.to_str())
            .is_some_and(|dir| dir.eq_ignore_ascii_case("bin"))
        || !real.is_file()
    {
        eprintln!(
            "Selected Git Bash is not the bin/bash.exe and usr/bin/bash.exe pair; fixture skipped"
        );
        return;
    }
    let program = if launcher { wrapper } else { real };
    let shell = Shell::new(&program);
    assert!(
        shell.session.shell_close_reading().idle,
        "launcher child is the shell, not background work"
    );
    let unavailable = ShellCloseCheck::default();
    assert!(unavailable.processes.set(None).is_ok());
    assert!(
        !shell.session.shell_close_reading_in(&unavailable).idle,
        "an unavailable process snapshot cannot certify an idle prompt"
    );
    let hold = Hold(std::env::temp_dir().join(format!("muxel-bash-{}.hold", uuid::Uuid::new_v4())));
    std::fs::write(&hold.0, b"").unwrap();
    let wait = format!(
        "while test -e {}; do :; done",
        quote(&hold.0.to_string_lossy())
    );
    let before = shell.session.shell_close_reading().marks;
    // Keep a direct MSYS fork alive. Fork/exec of another bash can orphan its
    // native child when the intermediate process exits, outside this walker.
    shell
        .session
        .write_input(format!("({wait}) </dev/null >/dev/null 2>&1 &\r").as_bytes());
    shell.prompt_after(before);
    assert!(
        !shell.session.shell_close_reading().idle,
        "a same-image background bash is work"
    );
    std::fs::remove_file(&hold.0).unwrap();
    let before = shell.session.shell_close_reading().marks;
    shell.session.write_input(b"wait\r");
    shell.prompt_after(before);
    assert!(shell.session.shell_close_reading().idle);
}

#[test]
fn git_bash_launcher_uses_its_shell_delegate() {
    git_shell(true);
}

#[test]
fn real_git_bash_keeps_same_image_background_work_busy() {
    git_shell(false);
}
