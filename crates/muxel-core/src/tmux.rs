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

/// Every muxel session name starts with this.
const SESSION_PREFIX: &str = "muxel_";

/// A tmux session living on a host, as reported by [`list_sessions_args`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSession {
    pub name: String,
    /// The directory the session was started in. This — not the name — is what
    /// attributes a session to a project: the name's slug is the *project's* when
    /// the session came from a recorded name, but the *host's* when it was derived,
    /// so the name alone can't say which project a session belongs to.
    pub path: String,
    /// What is running in it now (`claude`, `zsh`, …) — enough to re-adopt the
    /// session as the right kind of pane.
    pub command: String,
}

/// `tmux …` args listing one line per pane: session, start dir, running command.
///
/// `list-panes -a` rather than `list-sessions`, because only a pane knows the
/// command running in it; [`parse_sessions`] keeps the first pane of each session.
pub fn list_sessions_args() -> Vec<String> {
    vec![
        "list-panes".to_string(),
        "-a".to_string(),
        "-F".to_string(),
        "#{session_name}|#{session_path}|#{pane_current_command}".to_string(),
    ]
}

/// Parse [`list_sessions_args`] output — one [`RemoteSession`] per session, the
/// first pane winning. Malformed lines are skipped rather than failing the lot: a
/// host is free to have sessions muxel knows nothing about.
pub fn parse_sessions(out: &str) -> Vec<RemoteSession> {
    let mut sessions: Vec<RemoteSession> = Vec::new();
    for line in out.lines() {
        let mut fields = line.splitn(3, '|');
        let (Some(name), Some(path), Some(command)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if name.is_empty() || sessions.iter().any(|s| s.name == name) {
            continue;
        }
        sessions.push(RemoteSession {
            name: name.to_string(),
            path: path.to_string(),
            command: command.to_string(),
        });
    }
    sessions
}

/// muxel's sessions under `project_root` that **no instance owns** — agents still
/// running from an earlier run, or started from the iOS app, that nothing in the
/// workspace points at any more.
///
/// They are otherwise unreachable: a session is only ever found by name, from an
/// instance, so once the instance is gone the running agent is invisible to muxel
/// while quietly holding the host's resources. `owned` is the resolved session name
/// ([`session_for`]) of every instance in the project.
///
/// Sessions outside the project's tree, and any session not started by muxel, are
/// left strictly alone — this adopts, it never adopts *someone else's* tmux.
pub fn orphan_sessions(
    sessions: &[RemoteSession],
    project_root: &str,
    owned: &[String],
) -> Vec<RemoteSession> {
    sessions
        .iter()
        .filter(|s| s.name.starts_with(SESSION_PREFIX))
        .filter(|s| in_tree(&s.path, project_root))
        .filter(|s| !owned.iter().any(|o| o == &s.name))
        .cloned()
        .collect()
}

/// Whether `path` is `root` or sits beneath it (a worktree, say).
fn in_tree(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    path == root || path.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

/// The tmux session an instance uses: the name **recorded on the instance** wins;
/// a canonical name is derived from `slug` + `instance` only when it has none.
///
/// One rule, for every site that launches, checks, or kills a session — because the
/// two halves disagreeing is how an instance ends up with *two* sessions on a host.
/// muxel records `muxel_<project>_<id>` on an instance when tmux is enabled, and the
/// iOS app launches an instance from exactly that recorded name (falling back to the
/// canonical one when it's empty — mirror that here). A desktop that recomputed
/// `muxel_<host>_<id>` instead would attach to neither the session iOS created nor
/// the one it left behind itself: it would mint a fresh duplicate on every launch,
/// and its teardown — killing the recomputed name — would never reap the session it
/// was actually running, so the host accumulates orphans.
///
/// Ported to Swift as the `instance.tmuxSession ?? TmuxSession.name(…)` resolution
/// in `TerminalPaneView` — keep both in step.
pub fn session_for(recorded: Option<&str>, slug: &str, instance: Uuid) -> String {
    match recorded.map(str::trim).filter(|s| !s.is_empty()) {
        Some(recorded) => recorded.to_string(),
        None => session_name(slug, instance),
    }
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
///
/// `-u` (a client flag, so it must lead) forces the client to write UTF-8. Without
/// it tmux decides from `LC_ALL`/`LC_CTYPE`/`LANG`, and when those say nothing —
/// which is exactly what a GUI app on macOS passes on, since launchd sets no
/// locale — it downgrades every non-ASCII cell to `_` on its way to the terminal.
/// Box-drawing and agent glyphs then arrive as garbage that no redraw can repair,
/// because the damage is done before muxel ever sees the bytes. `LANG` is also
/// defaulted for local children (see `muxel_core::locale`), but `-u` is what covers
/// a *remote* pane, whose locale belongs to the far host, and a user who has
/// deliberately set a non-UTF-8 locale.
pub fn launch_session_args(
    session: &str,
    cwd: Option<&str>,
    program: Option<&str>,
    args: &[String],
) -> Vec<String> {
    let mut v = vec![
        "-u".to_string(),
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

    /// The recorded name wins. muxel writes `muxel_<project>_<id>` onto an instance
    /// when tmux is enabled, and the iOS app launches from that; a desktop that
    /// recomputed `muxel_<host>_<id>` would attach to a *different* session and leave
    /// the real one orphaned on the host.
    #[test]
    fn a_recorded_session_wins_over_a_recomputed_one() {
        let id = Uuid::from_u128(0x1a2b3c4d_0000_0000_0000_000000000000);
        assert_eq!(
            session_for(Some("muxel_sro_client_1a2b3c4d"), "rhel", id),
            "muxel_sro_client_1a2b3c4d"
        );
    }

    /// With nothing recorded — the common remote case, where tmux comes from the
    /// host's default rather than the instance — both clients derive the same name.
    #[test]
    fn without_a_recorded_session_the_canonical_name_is_derived() {
        let id = Uuid::from_u128(0x1a2b3c4d_0000_0000_0000_000000000000);
        assert_eq!(session_for(None, "rhel", id), "muxel_rhel_1a2b3c4d");
        assert_eq!(session_for(Some(""), "rhel", id), "muxel_rhel_1a2b3c4d");
        assert_eq!(session_for(Some("   "), "rhel", id), "muxel_rhel_1a2b3c4d");
    }

    fn sessions() -> Vec<RemoteSession> {
        parse_sessions(concat!(
            "muxel_sro_client_90f9def0|/home/ryan/Projects/sro_client|claude\n",
            "muxel_sro_client_d0d464c4|/home/ryan/Projects/sro_client|claude\n",
            "muxel_rhel_ae15cabf|/home/ryan/Projects/Bot/Manager|claude\n",
            "muxel_codem_6b80c97f|/home/ryan/Projects/codem|claude\n",
            "work|/home/ryan|zsh\n",
        ))
    }

    #[test]
    fn parse_keeps_the_first_pane_of_each_session() {
        let s = parse_sessions("a|/p|claude\na|/p|node\nb|/q|zsh\ngarbage-line\n|/r|zsh\nc|/s\n");
        assert_eq!(s.len(), 2, "one entry per session, malformed lines dropped");
        assert_eq!(s[0].name, "a");
        assert_eq!(s[0].command, "claude", "the first pane wins");
        assert_eq!(s[1].path, "/q");
    }

    /// The whole point: sessions still running in this project that nothing owns.
    #[test]
    fn orphans_are_this_projects_unowned_muxel_sessions() {
        let owned = vec!["muxel_sro_client_d0d464c4".to_string()];
        let orphans = orphan_sessions(&sessions(), "/home/ryan/Projects/sro_client", &owned);
        assert_eq!(
            orphans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["muxel_sro_client_90f9def0"],
            "an owned session, another project's, and a non-muxel session are all left alone"
        );
        assert_eq!(orphans[0].command, "claude");
    }

    /// A session named for the *host* still belongs to whichever project it was
    /// started in — the path decides, not the slug.
    #[test]
    fn a_host_named_session_is_attributed_by_its_path() {
        let orphans = orphan_sessions(&sessions(), "/home/ryan/Projects/Bot/Manager", &[]);
        assert_eq!(
            orphans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["muxel_rhel_ae15cabf"]
        );
    }

    /// A worktree lives under the project root, and its session belongs to it. But a
    /// sibling directory that merely shares a prefix does not.
    #[test]
    fn the_project_tree_includes_worktrees_but_not_prefix_siblings() {
        let s = parse_sessions(concat!(
            "muxel_p_00000001|/home/ryan/Projects/app/.worktrees/feat|claude\n",
            "muxel_p_00000002|/home/ryan/Projects/app-other|claude\n",
        ));
        let orphans = orphan_sessions(&s, "/home/ryan/Projects/app", &[]);
        assert_eq!(
            orphans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["muxel_p_00000001"]
        );
    }

    /// muxel adopts only its own sessions — never the user's own tmux.
    #[test]
    fn a_users_own_session_is_never_adopted() {
        let s = parse_sessions("work|/home/ryan/Projects/app|vim\n");
        assert!(orphan_sessions(&s, "/home/ryan/Projects/app", &[]).is_empty());
    }

    #[test]
    fn launch_forces_utf8_then_enables_mouse_then_creates_session() {
        let args = launch_session_args("muxel_p_123", Some("/work"), None, &[]);
        assert_eq!(
            args,
            vec![
                "-u",
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

    #[test]
    fn utf8_flag_leads_because_tmux_client_options_precede_the_command() {
        // `tmux set -u …` would be parsed as an argument to `set` and fail; the
        // client flag has to come before the first command.
        let args = launch_session_args("s", None, Some("claude"), &[]);
        assert_eq!(args.first().map(String::as_str), Some("-u"));
    }
}
