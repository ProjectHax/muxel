//! Linux: each local pane's processes run in a systemd scope of their own.
//!
//! Without one, a pane's processes live in whatever cgroup started them: the
//! tmux server's `tmux-spawn-….scope`, or muxel's own group, which for a muxel
//! started from GNOME can be GNOME Shell's service. When the kernel's
//! out-of-memory killer kills a runaway there (a test binary at 52 GB, say),
//! systemd's default `OOMPolicy=stop` then stops the rest of that unit: the
//! agent that ran it, or the whole desktop session.
//!
//! So muxel starts each local pane's program through
//! `systemd-run --user --scope`, which puts the program in a new transient
//! scope and then execs it, keeping its PID, environment, terminal and exit
//! status. The scope has:
//!
//! - `OOMPolicy=continue`: a process killed for memory takes nothing else in
//!   the pane with it.
//! - `MemoryMax` from Settings → Behavior's pane memory limit (16 GB by
//!   default), for the pane's processes together, so a runaway is killed
//!   inside its pane before the computer runs out. Swap is capped at a quarter
//!   of the limit, so a runaway can't push the rest of the machine into swap
//!   first. Off sets no limit and keeps `OOMPolicy=continue`.
//!
//! The scope is named after the pane ([`unit_name`]), so muxel can find it again
//! and read its `memory.events` to report a kill ([`oom_counts`]).
//!
//! This module only builds argv and parses files; whether `systemd-run` works
//! here is checked by the app before it uses any of this.

use uuid::Uuid;

/// Every pane scope's name starts with this.
pub const UNIT_PREFIX: &str = "muxel-pane-";

/// The start of every scope name for `instance`'s launches.
pub fn instance_prefix(instance: Uuid) -> String {
    format!("{UNIT_PREFIX}{}-", instance.simple())
}

/// The scope for one launch of `instance`. `launch` keeps a relaunch from
/// colliding with a scope its previous run left behind (a program it started
/// that outlived it).
pub fn unit_name(instance: Uuid, launch: Uuid) -> String {
    let launch = launch.simple().to_string();
    format!("{}{}.scope", instance_prefix(instance), &launch[..8])
}

/// The instance a scope directory named `name` belongs to, if it is a pane
/// scope.
pub fn instance_of(name: &str) -> Option<Uuid> {
    let rest = name.strip_prefix(UNIT_PREFIX)?.strip_suffix(".scope")?;
    let (instance, launch) = rest.split_once('-')?;
    (launch.len() == 8 && launch.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| Uuid::parse_str(instance).ok())
        .flatten()
}

/// The argv that comes before a pane's program: `systemd-run … --`.
pub fn scope_argv(unit: &str, memory_limit: Option<u64>) -> Vec<String> {
    let mut argv: Vec<String> = [
        "systemd-run",
        "--user",
        "--scope",
        // Without it, systemd-run prints "Running as unit …" into the pane.
        "--quiet",
        // Unload the scope even when its last process failed, rather than
        // keeping a failed unit around.
        "--collect",
    ]
    .map(String::from)
    .into();
    argv.push(format!("--unit={unit}"));
    argv.push("--description=muxel pane".into());
    argv.extend(["-p".into(), "OOMPolicy=continue".into()]);
    if let Some(limit) = memory_limit.filter(|limit| *limit > 0) {
        argv.extend(["-p".into(), format!("MemoryMax={limit}")]);
        argv.extend(["-p".into(), format!("MemorySwapMax={}", limit / 4)]);
    }
    argv.push("--".into());
    argv
}

/// The program and arguments a local tmux session should start, run inside
/// `scope` (from [`scope_argv`]). Keeps what tmux would otherwise have run:
/// - a program with arguments runs as is;
/// - a program without arguments is a command line for the user's shell
///   (`$SHELL -c PROGRAM`), as muxel's tmux launch runs it;
/// - no program means the user's shell, as a login shell like tmux starts it.
pub fn wrap_tmux_program(
    scope: &[String],
    program: Option<&str>,
    args: &[String],
    shell: &str,
) -> (String, Vec<String>) {
    let mut argv: Vec<String> = scope.to_vec();
    match program {
        Some(program) if args.is_empty() => argv.extend([
            "sh".into(),
            "-c".into(),
            "exec \"${SHELL:-/bin/sh}\" -c \"$1\"".into(),
            "muxel-agent".into(),
            program.into(),
        ]),
        Some(program) => {
            argv.push(program.into());
            argv.extend(args.iter().cloned());
        }
        None => argv.extend([shell.into(), "-l".into()]),
    }
    let program = argv.remove(0);
    (program, argv)
}

/// A POSIX shell command for a remote host that runs `command` (one simple
/// command, as shell words) in a scope of its own with `OOMPolicy=continue`,
/// so a process the kernel kills there for memory takes nothing else in the
/// pane with it. The remote tmux server's `tmux-spawn-….scope` otherwise holds
/// the agent and everything it started, and systemd stops all of it.
///
/// Where `systemd-run --user` doesn't work (macOS, a host without a user
/// manager, a tmux server started without one's environment), `command` runs
/// as before; a trial run of `true` decides, so a failing `systemd-run` never
/// costs the pane its program. No memory limit is set: the pane memory limit
/// setting is about this computer, and the remote host's size is unknown.
pub fn remote_exec(command: &str) -> String {
    const SCOPE: &str = "systemd-run --user --scope --quiet --collect -p OOMPolicy=continue";
    format!(
        "if command -v systemd-run >/dev/null 2>&1 && {SCOPE} true </dev/null >/dev/null 2>&1; \
         then exec {SCOPE} '--description=muxel pane' -- {command}; fi; exec {command}"
    )
}

/// What a scope's cgroup `memory.events` says about running out of memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OomCounts {
    /// Times the scope reached its own `MemoryMax` (`oom`).
    pub limit_reached: u64,
    /// Processes in it the kernel killed for memory, for its limit or because
    /// the whole computer ran out (`oom_kill`).
    pub killed: u64,
}

impl std::ops::AddAssign for OomCounts {
    fn add_assign(&mut self, other: Self) {
        self.limit_reached += other.limit_reached;
        self.killed += other.killed;
    }
}

/// [`OomCounts`] from a cgroup's `memory.events`; missing lines count as 0.
pub fn oom_counts(memory_events: &str) -> OomCounts {
    let field = |name: &str| {
        memory_events
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or(0)
    };
    OomCounts {
        limit_reached: field("oom"),
        killed: field("oom_kill"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OomCounts, instance_of, oom_counts, remote_exec, scope_argv, unit_name, wrap_tmux_program,
    };
    use uuid::Uuid;

    #[test]
    fn a_scope_is_named_after_its_pane_and_found_again() {
        let (instance, launch) = (Uuid::new_v4(), Uuid::new_v4());
        let unit = unit_name(instance, launch);
        assert!(unit.starts_with("muxel-pane-") && unit.ends_with(".scope"));
        assert_eq!(instance_of(&unit), Some(instance));
        assert_ne!(unit, unit_name(instance, Uuid::new_v4()));
        for other in [
            "tmux-spawn-9336112d-35d5-45ef-9eb2-6d35b4741fef.scope",
            "muxel-pane-nope.scope",
            &unit.replace(".scope", ".service"),
        ] {
            assert_eq!(instance_of(other), None, "{other}");
        }
    }

    #[test]
    fn the_scope_keeps_the_pane_alive_and_limits_it_when_asked() {
        let argv = scope_argv("muxel-pane-x.scope", Some(16 << 30));
        assert_eq!(
            &argv[..5],
            ["systemd-run", "--user", "--scope", "--quiet", "--collect"]
        );
        assert!(argv.contains(&"--unit=muxel-pane-x.scope".to_string()));
        assert!(argv.contains(&"OOMPolicy=continue".to_string()));
        assert!(argv.contains(&format!("MemoryMax={}", 16u64 << 30)));
        assert!(argv.contains(&format!("MemorySwapMax={}", 4u64 << 30)));
        assert_eq!(argv.last().map(String::as_str), Some("--"));

        // Off: still its own scope that survives a kill, with no limit.
        let off = scope_argv("u.scope", None);
        assert!(off.contains(&"OOMPolicy=continue".to_string()));
        assert!(!off.iter().any(|a| a.starts_with("Memory")));
        assert_eq!(scope_argv("u.scope", Some(0)), off);
    }

    #[test]
    fn tmux_programs_run_as_they_would_have_inside_the_scope() {
        let scope = scope_argv("u.scope", None);
        let n = scope.len();
        let args = vec!["--resume".to_string(), "abc".to_string()];
        let (program, wrapped) = wrap_tmux_program(&scope, Some("claude"), &args, "/bin/zsh");
        assert_eq!(program, "systemd-run");
        assert_eq!(&wrapped[n - 1..], ["claude", "--resume", "abc"]);

        // A bare program is a shell command line, as before.
        let (_, wrapped) = wrap_tmux_program(&scope, Some("npm run dev"), &[], "/bin/zsh");
        assert_eq!(
            &wrapped[n - 1..],
            [
                "sh",
                "-c",
                "exec \"${SHELL:-/bin/sh}\" -c \"$1\"",
                "muxel-agent",
                "npm run dev"
            ]
        );

        // No program: the user's login shell.
        let (_, wrapped) = wrap_tmux_program(&scope, None, &[], "/bin/zsh");
        assert_eq!(&wrapped[n - 1..], ["/bin/zsh", "-l"]);
    }

    #[test]
    fn a_remote_program_runs_in_a_scope_when_the_host_can_make_one() {
        let cmd = remote_exec("\"${SHELL:-/bin/sh}\" -ilc 'exec claude'");
        assert!(cmd.starts_with("if command -v systemd-run >/dev/null 2>&1 && "));
        assert!(cmd.contains(
            "then exec systemd-run --user --scope --quiet --collect -p OOMPolicy=continue \
             '--description=muxel pane' -- \"${SHELL:-/bin/sh}\" -ilc 'exec claude'; fi; "
        ));
        // Otherwise exactly as before.
        assert!(cmd.ends_with("; exec \"${SHELL:-/bin/sh}\" -ilc 'exec claude'"));
        assert!(!cmd.contains("MemoryMax"));
    }

    /// Without `systemd-run` on PATH, the program still starts. Unix only: the
    /// command is for a remote Unix host, and Windows has no `/bin/sh`. Absolute
    /// paths, because PATH is emptied for the child (and also used to find it).
    #[cfg(unix)]
    #[test]
    fn a_remote_program_starts_where_the_host_cannot_make_a_scope() {
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &remote_exec("/bin/sh -c 'echo ran'")])
            .env("PATH", "/nonexistent")
            .output()
            .expect("run /bin/sh");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
        assert!(out.status.success());
    }

    #[test]
    fn kills_are_read_from_memory_events() {
        let events = "low 0\nhigh 0\nmax 12\noom 1\noom_kill 2\noom_group_kill 0\n";
        assert_eq!(
            oom_counts(events),
            OomCounts {
                limit_reached: 1,
                killed: 2
            }
        );
        assert_eq!(oom_counts("low 0\n"), OomCounts::default());
    }
}
