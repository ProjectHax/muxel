//! Pure helpers for driving tmux. Building command arguments and names only —
//! the binary runs the actual `tmux` process.

use uuid::Uuid;

/// A stable tmux session name for an instance, e.g. `muxel_myproj_1a2b3c4d`.
pub fn session_name(project: &str, instance: Uuid) -> String {
    let slug: String = project
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let slug = slug.trim_matches('_');
    let id = instance.simple().to_string();
    format!(
        "muxel_{}_{}",
        if slug.is_empty() { "p" } else { slug },
        &id[..8]
    )
}

/// Arguments for `tmux …` that start the server *before* any session exists, from a
/// command line that names no project. Run this once per host before creating the
/// first session — locally, and on every remote host muxel or the iOS app touches.
///
/// tmux forks its server from whichever client first needs one, and the server keeps
/// that client's command line (only its `comm` becomes `tmux: server`). One server
/// hosts every session on that host. So if the first client is a pane's
/// `tmux new-session -A -s muxel_<project>_… -c <project root>`, the shared server's
/// argv carries a project name — and an agent running `pkill -f <project>` to clear
/// its own dev server matches the server, SIGKILLs it, and takes down every muxel
/// session and every agent inside them.
///
/// `exit-empty off` is required, not incidental: by default a server holding no
/// sessions exits immediately, so `start-server` alone would evaporate before the
/// first `new-session` and the server would be re-forked with the project name back
/// in its argv. The desktop restores `exit-empty on` when it quits.
///
/// Ported to Swift as `TmuxCommands.startServer()` — keep both in step.
pub fn start_server_args() -> Vec<String> {
    ["start-server", ";", "set", "-s", "exit-empty", "off"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Arguments for `tmux …` to hand `exit-empty` back, so the server exits with its
/// last session once muxel is gone. The inverse of [`start_server_args`].
pub fn restore_exit_empty_args() -> Vec<String> {
    ["set", "-s", "exit-empty", "on"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Arguments for `tmux …` to create-or-attach (`-A`) a session named `session`,
/// starting in `cwd` and running `program` + `args`. With no `program`, tmux
/// runs the user's default shell.
pub fn new_session_args(
    session: &str,
    cwd: Option<&str>,
    program: Option<&str>,
    args: &[String],
) -> Vec<String> {
    let mut v = vec![
        "new-session".to_string(),
        "-A".to_string(),
        "-s".to_string(),
        session.to_string(),
    ];
    if let Some(cwd) = cwd {
        v.push("-c".to_string());
        v.push(cwd.to_string());
    }
    if let Some(program) = program {
        v.push("--".to_string());
        v.push(program.to_string());
        v.extend(args.iter().cloned());
    }
    v
}

/// Arguments for a single `tmux …` invocation that turns on mouse mode and then
/// create-or-attaches the session. tmux runs the `;`-separated commands in order:
/// `set -g mouse on` first, then `new-session -A …`. Enabling mouse mode is what lets
/// the terminal's scroll-wheel forwarding reach tmux's own copy-mode scrollback
/// (without it, tmux never sets the emulator's mouse flag and the wheel only scrolls
/// the local, tmux-painted screen). `-g` (global) so it applies before the session
/// exists; it's idempotent, so re-running on every launch is harmless.
pub fn launch_session_args(
    session: &str,
    cwd: Option<&str>,
    program: Option<&str>,
    args: &[String],
) -> Vec<String> {
    let mut v = vec![
        "set".to_string(),
        "-g".to_string(),
        "mouse".to_string(),
        "on".to_string(),
        ";".to_string(),
    ];
    v.extend(new_session_args(session, cwd, program, args));
    v
}

/// Arguments for `tmux …` to kill a session (exact-match `=` target).
pub fn kill_session_args(session: &str) -> Vec<String> {
    vec![
        "kill-session".to_string(),
        "-t".to_string(),
        format!("={session}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_is_sanitized_and_stable() {
        let id = Uuid::nil();
        let name = session_name("My Project!", id);
        assert!(name.starts_with("muxel_My_Project_"));
        assert_eq!(
            name,
            session_name("My Project!", id),
            "stable for same inputs"
        );
    }

    #[test]
    fn new_session_wraps_program_after_separator() {
        let args = new_session_args(
            "muxel_p_123",
            Some("/work"),
            Some("claude"),
            &["--flag".to_string(), "x".to_string()],
        );
        assert_eq!(
            args,
            vec![
                "new-session",
                "-A",
                "-s",
                "muxel_p_123",
                "-c",
                "/work",
                "--",
                "claude",
                "--flag",
                "x"
            ]
        );
    }

    #[test]
    fn new_session_without_program_runs_default_shell() {
        let args = new_session_args("s", Some("/work"), None, &[]);
        assert_eq!(args, vec!["new-session", "-A", "-s", "s", "-c", "/work"]);
        assert!(!args.contains(&"--".to_string()));
    }

    /// Local panes pass `cwd: None` and are spawned with that cwd already set, so the
    /// project's path never enters the tmux client's argv — an agent's routine
    /// `pkill -f <project>` has one less thing in muxel's process table to match.
    #[test]
    fn new_session_without_cwd_omits_the_project_path() {
        let args = new_session_args("muxel_p_123", None, Some("claude"), &[]);
        assert_eq!(
            args,
            vec!["new-session", "-A", "-s", "muxel_p_123", "--", "claude"]
        );
        assert!(!args.contains(&"-c".to_string()));
    }

    /// Mirrored by `TmuxCommands.startServer()` in the iOS port — both must produce
    /// this exact argv, and it must name no project (that's the whole point).
    #[test]
    fn start_server_args_name_no_project_and_keep_the_server_alive() {
        assert_eq!(
            start_server_args(),
            vec!["start-server", ";", "set", "-s", "exit-empty", "off"]
        );
        assert_eq!(
            restore_exit_empty_args(),
            vec!["set", "-s", "exit-empty", "on"]
        );
    }

    #[test]
    fn kill_uses_exact_match_target() {
        assert_eq!(kill_session_args("s"), vec!["kill-session", "-t", "=s"]);
    }

    #[test]
    fn launch_enables_mouse_then_creates_session() {
        let args = launch_session_args("muxel_p_123", Some("/work"), None, &[]);
        assert_eq!(
            args,
            vec![
                "set",
                "-g",
                "mouse",
                "on",
                ";",
                "new-session",
                "-A",
                "-s",
                "muxel_p_123",
                "-c",
                "/work"
            ]
        );
    }
}
