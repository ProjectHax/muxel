//! Pure construction of `ssh` command arguments for remote development. No I/O —
//! the muxel binary runs the actual `ssh` process; this module only builds argv
//! and the remote command string. Mirrors `tmux.rs`, and is unit-tested on its
//! own.
//!
//! A remote pane runs `ssh [opts] [user@]host -t -- '<remote command>'`, where
//! the remote command is `cd <dir> && exec <program>` (or a `tmux new-session
//! -A` for a persistent, reconnectable session). All hosts use a shared
//! ControlMaster socket so repeated connections (the pane plus every git call)
//! reuse one authenticated connection.

use crate::{RemoteHost, SshAuth};

/// POSIX single-quote `s` for safe embedding in a remote shell command. Tokens
/// made only of safe characters are left bare (readability + test clarity).
pub fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+/:=@,%".contains(c));
    if safe {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// `[user@]hostname` for a host (the ssh destination).
pub fn target(host: &RemoteHost) -> String {
    if host.user.is_empty() {
        host.hostname.clone()
    } else {
        format!("{}@{}", host.user, host.hostname)
    }
}

/// Base connection **options** for a host (port, identity, jump, agent
/// forwarding, host-key policy, keepalive, extra `-o`s) — everything *except*
/// ControlMaster multiplexing, the target, and the command. Used directly by the
/// connection test (which must NOT reuse a shared master) and via
/// [`connection_args`] (which adds multiplexing) for panes and git.
pub fn base_args(host: &RemoteHost) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(port) = host.port {
        v.push("-p".into());
        v.push(port.to_string());
    }
    if host.auth == SshAuth::Key
        && let Some(id) = &host.identity_file
    {
        v.push("-i".into());
        v.push(id.display().to_string());
    }
    if let Some(jump) = host.jump_host.as_ref().filter(|j| !j.is_empty()) {
        v.push("-J".into());
        v.push(jump.clone());
    }
    if host.forward_agent {
        v.push("-A".into());
    }
    let strict = if host.strict_host_key.is_empty() {
        "accept-new"
    } else {
        host.strict_host_key.as_str()
    };
    v.push("-o".into());
    v.push(format!("StrictHostKeyChecking={strict}"));
    if let Some(secs) = host.keepalive_secs {
        v.push("-o".into());
        v.push(format!("ServerAliveInterval={secs}"));
    }
    for opt in host.extra_options.iter().filter(|o| !o.is_empty()) {
        v.push("-o".into());
        v.push(opt.clone());
    }
    v
}

/// Base options **plus** ControlMaster multiplexing (one shared master per host
/// → cheap repeated git calls, one auth, recoverable drops). Shared by remote
/// panes and remote git. Windows OpenSSH doesn't support multiplexing, so it's
/// omitted there. Callers append the [`target`] themselves.
pub fn connection_args(host: &RemoteHost, control_path: &str) -> Vec<String> {
    let mut v = base_args(host);
    if cfg!(not(target_os = "windows")) {
        v.push("-o".into());
        v.push("ControlMaster=auto".into());
        v.push("-o".into());
        v.push(format!("ControlPath={control_path}"));
        v.push("-o".into());
        v.push("ControlPersist=60".into());
    }
    v
}

/// Parameters for a remote interactive pane command.
pub struct SshSpec<'a> {
    pub host: &'a RemoteHost,
    pub control_path: &'a str,
    /// Working directory on the remote host (None = the login default).
    pub remote_cwd: Option<&'a str>,
    /// Program to run remotely (None = the remote login shell).
    pub program: Option<&'a str>,
    pub args: &'a [String],
    /// Run inside a persistent tmux session on the remote host.
    pub use_tmux: bool,
    pub tmux_session: Option<&'a str>,
}

/// The remote command string a pane runs (the single argument after `--`).
fn remote_command(spec: &SshSpec) -> String {
    if spec.use_tmux {
        // `tmux new-session -A` attaches if the session exists, so a reconnect
        // resumes the running agent. Reuse the local tmux arg builder, run remote.
        let session = spec.tmux_session.unwrap_or("muxel");
        let targs =
            crate::tmux::launch_session_args(session, spec.remote_cwd, spec.program, spec.args);
        let mut cmd = String::from("exec tmux");
        for a in &targs {
            cmd.push(' ');
            cmd.push_str(&sh_quote(a));
        }
        cmd
    } else {
        let mut cmd = String::new();
        if let Some(cwd) = spec.remote_cwd {
            cmd.push_str("cd ");
            cmd.push_str(&sh_quote(cwd));
            cmd.push_str(" && ");
        }
        cmd.push_str("exec ");
        match spec.program {
            Some(p) => {
                cmd.push_str(&sh_quote(p));
                for a in spec.args {
                    cmd.push(' ');
                    cmd.push_str(&sh_quote(a));
                }
            }
            // The remote login shell (expanded by the remote shell — left unquoted).
            None => cmd.push_str("${SHELL:-/bin/sh} -l"),
        }
        cmd
    }
}

/// The full argv after `ssh` for a remote pane: `-t`, connection opts, the
/// target, `--`, and the single remote command string.
pub fn ssh_args(spec: &SshSpec) -> Vec<String> {
    // `-t` forces a remote PTY so the shell/agent is fully interactive.
    let mut v = vec!["-t".to_string()];
    v.extend(connection_args(spec.host, spec.control_path));
    v.push(target(spec.host));
    v.push("--".into());
    v.push(remote_command(spec));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RemoteHost;
    use std::path::PathBuf;

    fn host() -> RemoteHost {
        RemoteHost::new("dev", "example.com")
    }

    #[test]
    fn sh_quote_leaves_safe_tokens_bare() {
        assert_eq!(sh_quote("claude"), "claude");
        assert_eq!(sh_quote("/home/me/proj"), "/home/me/proj");
        assert_eq!(sh_quote("--model"), "--model");
    }

    #[test]
    fn sh_quote_wraps_and_escapes() {
        assert_eq!(sh_quote("/my work"), "'/my work'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn connection_args_default_host() {
        let h = host();
        // ControlMaster multiplexing is added on non-Windows only (Windows ssh has
        // no ControlMaster), so the expected args differ by platform.
        let mut want = vec!["-o", "StrictHostKeyChecking=accept-new"];
        if cfg!(not(target_os = "windows")) {
            want.extend([
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=/tmp/s.sock",
                "-o",
                "ControlPersist=60",
            ]);
        }
        assert_eq!(connection_args(&h, "/tmp/s.sock"), want);
        assert_eq!(target(&h), "example.com");
    }

    #[test]
    fn connection_args_full_options() {
        let mut h = host();
        h.user = "ryan".into();
        h.port = Some(2222);
        h.auth = SshAuth::Key;
        h.identity_file = Some(PathBuf::from("/home/dev/.ssh/id_ed25519"));
        h.jump_host = Some("bastion".into());
        h.forward_agent = true;
        h.keepalive_secs = Some(30);
        h.extra_options = vec!["Compression=yes".into()];
        let a = connection_args(&h, "/sock");
        assert_eq!(&a[0..2], &["-p", "2222"]);
        assert!(
            a.windows(2)
                .any(|w| w == ["-i", "/home/dev/.ssh/id_ed25519"])
        );
        assert!(a.windows(2).any(|w| w == ["-J", "bastion"]));
        assert!(a.contains(&"-A".to_string()));
        assert!(a.contains(&"ServerAliveInterval=30".to_string()));
        assert!(a.contains(&"Compression=yes".to_string()));
        assert_eq!(target(&h), "ryan@example.com");
    }

    #[test]
    fn ssh_args_includes_target_before_separator() {
        let h = host();
        let spec = SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: None,
            program: Some("bash"),
            args: &[],
            use_tmux: false,
            tmux_session: None,
        };
        let v = ssh_args(&spec);
        // …target, "--", command (last three).
        assert_eq!(&v[v.len() - 3..v.len() - 1], &["example.com", "--"]);
    }

    #[test]
    fn strict_host_key_override() {
        let mut h = host();
        h.strict_host_key = "yes".into();
        assert!(connection_args(&h, "/s").contains(&"StrictHostKeyChecking=yes".to_string()));
    }

    #[test]
    fn ssh_args_non_tmux_program_and_cwd() {
        let h = host();
        let args = ["--model".to_string(), "opus".to_string()];
        let spec = SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: Some("/srv/app"),
            program: Some("claude"),
            args: &args,
            use_tmux: false,
            tmux_session: None,
        };
        let v = ssh_args(&spec);
        assert_eq!(v[0], "-t");
        assert_eq!(v[v.len() - 2], "--");
        assert_eq!(v.last().unwrap(), "cd /srv/app && exec claude --model opus");
    }

    #[test]
    fn ssh_args_non_tmux_login_shell_when_no_program() {
        let h = host();
        let spec = SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: Some("/srv/app"),
            program: None,
            args: &[],
            use_tmux: false,
            tmux_session: None,
        };
        assert_eq!(
            ssh_args(&spec).last().unwrap(),
            "cd /srv/app && exec ${SHELL:-/bin/sh} -l"
        );
    }

    #[test]
    fn ssh_args_tmux_wraps_in_new_session() {
        let h = host();
        let spec = SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: Some("/srv/app"),
            program: Some("claude"),
            args: &[],
            use_tmux: true,
            tmux_session: Some("muxel-abc123"),
        };
        assert_eq!(
            ssh_args(&spec).last().unwrap(),
            "exec tmux set -g mouse on ';' new-session -A -s muxel-abc123 -c /srv/app -- claude"
        );
    }

    #[test]
    fn ssh_args_quotes_paths_with_spaces() {
        let h = host();
        let spec = SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: Some("/srv/my app"),
            program: Some("bash"),
            args: &[],
            use_tmux: false,
            tmux_session: None,
        };
        assert_eq!(
            ssh_args(&spec).last().unwrap(),
            "cd '/srv/my app' && exec bash"
        );
    }
}
