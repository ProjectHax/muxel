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

/// TCP-connect timeout applied to every ssh invocation, so a dead/slow host
/// fails promptly (the OS default can hang for a minute or more) instead of
/// wedging a pane or the connection test. Overridable via `extra_options`.
const CONNECT_TIMEOUT_SECS: u32 = 15;

/// Base connection **options** for a host (port, identity, jump, agent
/// forwarding, host-key policy, connect timeout, keepalive, compression, extra
/// `-o`s) — everything *except* ControlMaster multiplexing, the target, and the
/// command. Used directly by the connection test (which must NOT reuse a shared
/// master) and via [`connection_args`] (which adds multiplexing) for panes and
/// git.
pub fn base_args(host: &RemoteHost) -> Vec<String> {
    let mut v = Vec::new();
    // Whether the user already set option `key` in `extra_options` — a hand-
    // written `-o` should win, so we skip emitting our default for that key.
    let user_set = |key: &str| {
        host.extra_options.iter().any(|o| {
            o.split('=')
                .next()
                .map(|k| k.trim().eq_ignore_ascii_case(key))
                .unwrap_or(false)
        })
    };
    if let Some(port) = host.port {
        v.push("-p".into());
        v.push(port.to_string());
    }
    if host.auth == SshAuth::Key
        && let Some(id) = &host.identity_file
    {
        v.push("-i".into());
        v.push(id.display().to_string());
        // With an explicit key, don't let ssh offer every agent key first — that
        // can trip the server's MaxAuthTries ("Too many authentication
        // failures") before the right key is reached.
        if !user_set("IdentitiesOnly") {
            v.push("-o".into());
            v.push("IdentitiesOnly=yes".into());
        }
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
    if !user_set("ConnectTimeout") {
        v.push("-o".into());
        v.push(format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"));
    }
    if let Some(secs) = host.keepalive_secs {
        v.push("-o".into());
        v.push(format!("ServerAliveInterval={secs}"));
    }
    if host.compression && !user_set("Compression") {
        v.push("-o".into());
        v.push("Compression=yes".into());
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

/// A "REMOTE HOST IDENTIFICATION HAS CHANGED" refusal parsed from ssh stderr —
/// the input to the changed-key trust dialog. All fields are best-effort: the
/// gate is strict (see [`HostKeyChange::parse`]) but individual lines may be
/// missing or reordered across OpenSSH builds (Debian patches insert extra
/// lines), so each detail is optional.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyChange {
    /// Host exactly as ssh matched it in known_hosts — `example.com`,
    /// `[example.com]:2222` for a non-default port, or a config alias. This is
    /// definitionally the right token to pass to `ssh-keygen -R`.
    pub host: Option<String>,
    /// The newly presented fingerprint (`SHA256:…`, trailing `.` stripped).
    pub presented_fingerprint: Option<String>,
    /// Key type on the "fingerprint for the … key" line (the *presented* key).
    pub presented_key_type: Option<String>,
    /// Key type on the "Offending … key in" line (the *stored* key — the two
    /// can differ when a server rotated key algorithms).
    pub offending_key_type: Option<String>,
    /// known_hosts file holding the stale entry, and its 1-based line.
    pub known_hosts_file: Option<String>,
    pub known_hosts_line: Option<u64>,
}

impl HostKeyChange {
    /// Parse ssh stderr; `Some` only when the text is definitely a changed-key
    /// refusal — the WARNING banner or the "…has changed and you have requested
    /// strict checking." line. The terse `Host key verification failed.` alone
    /// is ambiguous (it also appears for unknown-host + `StrictHostKeyChecking=
    /// yes` and a declined `ask` prompt) and stays `None`. Line-based and
    /// order-tolerant; prefix noise (e.g. a `git …:` wrapper on the first line)
    /// is fine because every marker is matched by substring.
    pub fn parse(stderr: &str) -> Option<HostKeyChange> {
        const BANNER: &str = "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!";
        const STRICT: &str = "has changed and you have requested strict checking.";
        const FP_PREFIX: &str = "The fingerprint for the ";
        const FP_SUFFIX: &str = " key sent by the remote host is";
        const OFFENDING: &str = "Offending ";
        const OFFENDING_IN: &str = " key in ";
        const HOST_PREFIX: &str = "Host key for ";

        if !stderr
            .lines()
            .any(|l| l.contains(BANNER) || l.trim_end().ends_with(STRICT))
        {
            return None;
        }

        let mut change = HostKeyChange {
            host: None,
            presented_fingerprint: None,
            presented_key_type: None,
            offending_key_type: None,
            known_hosts_file: None,
            known_hosts_line: None,
        };

        let mut lines = stderr.lines().peekable();
        while let Some(line) = lines.next() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix(FP_PREFIX)
                && let Some(key_type) = rest.strip_suffix(FP_SUFFIX)
            {
                change.presented_key_type = Some(key_type.to_string());
                // The fingerprint is on the next non-empty line, printed with a
                // trailing period.
                for next in lines.by_ref() {
                    let fp = next.trim().trim_end_matches('.');
                    if !fp.is_empty() {
                        change.presented_fingerprint = Some(fp.to_string());
                        break;
                    }
                }
            } else if let Some(rest) = trimmed.strip_prefix(OFFENDING)
                && let Some((key_type, location)) = rest.split_once(OFFENDING_IN)
            {
                // "Offending key for IP in …" has no " key in " and is skipped
                // naturally (only relevant under CheckHostIP=yes).
                change.offending_key_type = Some(key_type.to_string());
                // rsplit keeps Windows drive letters (`C:\…`) in the path.
                if let Some((file, line_no)) = location.rsplit_once(':') {
                    change.known_hosts_file = Some(file.to_string());
                    change.known_hosts_line = line_no.trim().parse().ok();
                } else {
                    change.known_hosts_file = Some(location.to_string());
                }
            } else if let Some(rest) = trimmed.strip_prefix(HOST_PREFIX)
                && let Some(host) = rest.strip_suffix(STRICT)
            {
                change.host = Some(host.trim().to_string());
            }
        }
        Some(change)
    }
}

/// The known_hosts token for a host + port: the bare hostname for the default
/// port (or none), OpenSSH's bracketed `[host]:port` form otherwise.
pub fn known_hosts_name(hostname: &str, port: Option<u16>) -> String {
    match port {
        Some(p) if p != 22 => format!("[{hostname}]:{p}"),
        _ => hostname.to_string(),
    }
}

/// argv after `ssh-keygen` to delete a host's stale entry: `[-f <file>,] -R
/// <entry>`. ssh-keygen handles hashed entries (`HashKnownHosts=yes`) and the
/// `[host]:port` form itself, and backs the file up to `known_hosts.old` —
/// which is exactly why muxel delegates instead of editing the file.
pub fn keygen_remove_args(entry: &str, file: Option<&str>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(f) = file {
        v.push("-f".into());
        v.push(f.into());
    }
    v.push("-R".into());
    v.push(entry.into());
    v
}

/// argv after `ssh-keygen` to look up a host's stored keys:
/// `-l [-f <file>,] -F <entry>` (fingerprints, not full keys).
pub fn keygen_find_args(entry: &str, file: Option<&str>) -> Vec<String> {
    let mut v = vec!["-l".to_string()];
    if let Some(f) = file {
        v.push("-f".into());
        v.push(f.into());
    }
    v.push("-F".into());
    v.push(entry.into());
    v
}

/// Parse `ssh-keygen -l -F` stdout into `(key_type, fingerprint)` pairs. The
/// output interleaves `# Host <h> found: line <n>` comments with
/// `<host-or-hash> <TYPE> SHA256:<fp>` lines; comments are skipped and the
/// host/hash column ignored.
pub fn parse_keygen_lookup(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| {
            let mut fields = l.split_whitespace();
            let _host = fields.next()?;
            let key_type = fields.next()?;
            let fingerprint = fields.next()?;
            Some((key_type.to_string(), fingerprint.to_string()))
        })
        .collect()
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
        let mut want = vec![
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "ConnectTimeout=15",
        ];
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
    fn key_auth_with_identity_adds_identities_only() {
        let mut h = host();
        h.auth = SshAuth::Key;
        h.identity_file = Some(PathBuf::from("/home/dev/.ssh/id_ed25519"));
        let a = base_args(&h);
        assert!(
            a.windows(2)
                .any(|w| w == ["-i", "/home/dev/.ssh/id_ed25519"])
        );
        assert!(a.contains(&"IdentitiesOnly=yes".to_string()));
        // Agent auth (no explicit key) must NOT force IdentitiesOnly.
        assert!(!base_args(&host()).contains(&"IdentitiesOnly=yes".to_string()));
    }

    #[test]
    fn compression_is_opt_in() {
        assert!(!base_args(&host()).contains(&"Compression=yes".to_string()));
        let mut h = host();
        h.compression = true;
        assert!(base_args(&h).contains(&"Compression=yes".to_string()));
    }

    #[test]
    fn extra_options_override_the_builtins() {
        let mut h = host();
        h.compression = true;
        h.extra_options = vec![
            "ConnectTimeout=60".into(),
            "Compression=no".into(),
            "IdentitiesOnly=no".into(),
        ];
        h.auth = SshAuth::Key;
        h.identity_file = Some(PathBuf::from("/k"));
        let a = base_args(&h);
        // A user-set option wins → we don't also emit our default for that key.
        assert!(!a.contains(&"ConnectTimeout=15".to_string()));
        assert!(a.contains(&"ConnectTimeout=60".to_string()));
        assert_eq!(
            a.iter()
                .filter(|o| o.starts_with("ConnectTimeout="))
                .count(),
            1
        );
        assert!(!a.contains(&"Compression=yes".to_string()));
        assert!(!a.contains(&"IdentitiesOnly=yes".to_string()));
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

    // ---- HostKeyChange parsing (fixtures composed from OpenSSH's literal
    // error() format strings, extracted from the client binary) ----

    /// Full upstream/macOS-style block: presented key type differs from the
    /// stored (offending) one — a server that rotated ECDSA → ED25519.
    const CHANGED_FULL: &str = "\
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @
@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@
IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!
Someone could be eavesdropping on you right now (man-in-the-middle attack)!
It is also possible that a host key has just been changed.
The fingerprint for the ED25519 key sent by the remote host is
SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s.
Please contact your system administrator.
Add correct host key in /Users/ryan/.ssh/known_hosts to get rid of this message.
Offending ECDSA key in /Users/ryan/.ssh/known_hosts:42
Host key for example.com has changed and you have requested strict checking.
Host key verification failed.";

    #[test]
    fn host_key_change_parses_full_block() {
        let c = HostKeyChange::parse(CHANGED_FULL).expect("classified");
        assert_eq!(c.host.as_deref(), Some("example.com"));
        assert_eq!(
            c.presented_fingerprint.as_deref(),
            Some("SHA256:uNiVztksCsDhcc0u9e8BujQXVUpKZIDTMczCvj3tD2s"),
            "trailing period stripped"
        );
        assert_eq!(c.presented_key_type.as_deref(), Some("ED25519"));
        assert_eq!(c.offending_key_type.as_deref(), Some("ECDSA"));
        assert_eq!(
            c.known_hosts_file.as_deref(),
            Some("/Users/ryan/.ssh/known_hosts")
        );
        assert_eq!(c.known_hosts_line, Some(42));
    }

    /// Debian/Ubuntu builds patch in two extra lines after the Offending line.
    #[test]
    fn host_key_change_tolerates_debian_extra_lines() {
        let stderr = CHANGED_FULL.replace(
            "Host key for example.com has changed",
            "  remove with:\n  ssh-keygen -f \"/home/u/.ssh/known_hosts\" -R \"example.com\"\nHost key for example.com has changed",
        );
        let c = HostKeyChange::parse(&stderr).expect("classified");
        assert_eq!(c.host.as_deref(), Some("example.com"));
        assert_eq!(c.known_hosts_line, Some(42));
    }

    #[test]
    fn host_key_change_keeps_bracketed_port_form() {
        let stderr = CHANGED_FULL.replace("example.com", "[example.com]:2222");
        let c = HostKeyChange::parse(&stderr).expect("classified");
        assert_eq!(c.host.as_deref(), Some("[example.com]:2222"));
    }

    /// With CheckHostIP=yes a DNS-spoofing block precedes the main one; its
    /// "Offending key for IP in …" line must not clobber the host entry.
    #[test]
    fn host_key_change_ignores_ip_offending_line() {
        let stderr = format!(
            "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
             @       WARNING: POSSIBLE DNS SPOOFING DETECTED!          @\n\
             @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
             Offending key for IP in /Users/ryan/.ssh/known_hosts:7\n\
             {CHANGED_FULL}"
        );
        let c = HostKeyChange::parse(&stderr).expect("classified");
        assert_eq!(
            c.known_hosts_line,
            Some(42),
            "host line wins, IP line skipped"
        );
        assert_eq!(c.offending_key_type.as_deref(), Some("ECDSA"));
    }

    /// Remote-git wraps stderr behind a `git …:` prefix on the first line.
    #[test]
    fn host_key_change_parses_with_wrapped_first_line() {
        let stderr = format!("git status --porcelain=v1 -z: {CHANGED_FULL}");
        assert!(HostKeyChange::parse(&stderr).is_some());
    }

    #[test]
    fn host_key_change_keeps_windows_paths() {
        let stderr = CHANGED_FULL
            .replace(
                "/Users/ryan/.ssh/known_hosts",
                r"C:\Users\ryan\.ssh\known_hosts",
            )
            .replace(":42", ":7");
        let c = HostKeyChange::parse(&stderr).expect("classified");
        assert_eq!(
            c.known_hosts_file.as_deref(),
            Some(r"C:\Users\ryan\.ssh\known_hosts")
        );
        assert_eq!(c.known_hosts_line, Some(7));
    }

    /// The terse tail alone is ambiguous (unknown host + strict, declined
    /// `ask`) — must NOT classify.
    #[test]
    fn host_key_change_rejects_non_changed_key_errors() {
        assert_eq!(HostKeyChange::parse("Host key verification failed."), None);
        assert_eq!(
            HostKeyChange::parse(
                "No ED25519 host key is known for example.com and you have requested strict checking.\n\
                 Host key verification failed."
            ),
            None
        );
        assert_eq!(HostKeyChange::parse("Permission denied (publickey)."), None);
        assert_eq!(HostKeyChange::parse(""), None);
    }

    #[test]
    fn known_hosts_name_brackets_only_nonstandard_ports() {
        assert_eq!(known_hosts_name("example.com", None), "example.com");
        assert_eq!(known_hosts_name("example.com", Some(22)), "example.com");
        assert_eq!(
            known_hosts_name("example.com", Some(2222)),
            "[example.com]:2222"
        );
    }

    #[test]
    fn keygen_args_builders() {
        assert_eq!(
            keygen_remove_args("example.com", None),
            ["-R", "example.com"]
        );
        assert_eq!(
            keygen_remove_args("[example.com]:2222", Some("/home/u/.ssh/known_hosts")),
            ["-f", "/home/u/.ssh/known_hosts", "-R", "[example.com]:2222"]
        );
        assert_eq!(
            keygen_find_args("example.com", None),
            ["-l", "-F", "example.com"]
        );
        assert_eq!(
            keygen_find_args("h", Some("/kh")),
            ["-l", "-f", "/kh", "-F", "h"]
        );
    }

    #[test]
    fn parse_keygen_lookup_skips_comments_and_hashes() {
        // Verbatim shape of `ssh-keygen -l -F` output (note the trailing space
        // on the comment line), plus a hashed-host entry.
        let out = "# Host gitlab found: line 1 \n\
                   gitlab ED25519 SHA256:7DtyCzf8LVX8+TIRg3MId33qzZC44LPfwSk06ZtnKaU\n\
                   # Host example found: line 3\n\
                   |1|abc123hash= RSA SHA256:aaaabbbbcccc\n";
        assert_eq!(
            parse_keygen_lookup(out),
            vec![
                (
                    "ED25519".to_string(),
                    "SHA256:7DtyCzf8LVX8+TIRg3MId33qzZC44LPfwSk06ZtnKaU".to_string()
                ),
                ("RSA".to_string(), "SHA256:aaaabbbbcccc".to_string()),
            ]
        );
        assert!(parse_keygen_lookup("").is_empty());
    }
}
