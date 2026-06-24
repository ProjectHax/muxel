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

    #[test]
    fn kill_uses_exact_match_target() {
        assert_eq!(kill_session_args("s"), vec!["kill-session", "-t", "=s"]);
    }
}
