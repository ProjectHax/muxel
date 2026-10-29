//! Pure construction of `ssh` command arguments for remote development. No I/O —
//! the muxel binary runs the actual `ssh` process; this module only builds argv
//! and the remote command string. Mirrors `tmux.rs`, and is unit-tested on its
//! own.
//!
//! A remote pane runs `ssh [opts] [user@]host -t -- '<remote command>'`, where
//! the remote command is `mkdir -p <dir> && cd <dir> && exec <program>` (or a
//! `tmux new-session -A` for a persistent, reconnectable session). Connections to a host ride
//! ControlMaster sockets, so repeated connections reuse an authenticated one: the
//! panes share a few masters, and muxel's own commands (git, layout sync, tmux
//! queries) share another. See [`Master`].

use crate::tmux::Socket;
use crate::winshell::{self, WindowsShell};
use crate::{RemoteHost, RemoteOs, SshAuth};
use uuid::Uuid;

/// POSIX single-quote `s` for safe embedding in a remote shell command. Tokens
/// made only of safe characters are left bare (readability + test clarity).
///
/// A leading `=` is never bare: zsh — the login shell ssh runs the command in on
/// a macOS host — expands `=word` to the path of command `word` and fails when
/// there is none, which is every tmux exact-match target (`=muxel_…`).
pub fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && !s.starts_with('=')
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

/// Default `ServerAliveInterval` (seconds) when a host doesn't set its own — the
/// gap between keepalive probes on an otherwise-silent connection.
const DEFAULT_KEEPALIVE_SECS: u32 = 20;
/// Default `ServerAliveCountMax`: missed probes before ssh declares the
/// connection dead and exits. With the interval above, a drop is detected in
/// roughly `SECS * COUNT` (~60s).
const DEFAULT_KEEPALIVE_COUNT: u32 = 3;

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
    // Keepalive: probe the server periodically so a dropped connection (Wi-Fi
    // blip, laptop sleep, host reboot) is *detected* — the ssh client exits
    // after `ServerAliveCountMax` missed probes instead of hanging forever on a
    // dead socket. This is what lets muxel notice a remote pane's relay died and
    // reconnect it; without it the pane freezes silently. `keepalive_secs` unset
    // takes the default; an explicit `Some(0)` disables it; a hand-written
    // `ServerAliveInterval` in `extra_options` wins.
    let keepalive = host.keepalive_secs.unwrap_or(DEFAULT_KEEPALIVE_SECS);
    if keepalive > 0 && !user_set("ServerAliveInterval") {
        v.push("-o".into());
        v.push(format!("ServerAliveInterval={keepalive}"));
        if !user_set("ServerAliveCountMax") {
            v.push("-o".into());
            v.push(format!("ServerAliveCountMax={DEFAULT_KEEPALIVE_COUNT}"));
        }
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

/// Base options **plus** ControlMaster multiplexing over `control_path` (cheap
/// repeated git calls, one auth, recoverable drops). Shared by remote panes and
/// remote git, each over its own master — see [`Master`]. Windows OpenSSH doesn't
/// support multiplexing, so it's omitted there. Callers append the [`target`]
/// themselves.
pub fn connection_args(host: &RemoteHost, control_path: &str) -> Vec<String> {
    let mut v = base_args(host);
    if cfg!(not(target_os = "windows")) {
        v.push("-o".into());
        v.push("ControlMaster=auto".into());
        v.push("-o".into());
        v.push(format!("ControlPath={}", quote_option_value(control_path)));
        v.push("-o".into());
        v.push("ControlPersist=60".into());
    }
    v
}

/// Quote an `-o` option value that contains whitespace.
///
/// ssh parses each `-o` argument as a line of `ssh_config` and splits the value on
/// whitespace, so an unquoted path with a space in it is read as trailing junk and
/// ssh refuses the whole command before connecting:
///
/// ```text
/// command-line line 0: keyword controlpath extra arguments at end of line
/// ```
///
/// Which is not hypothetical: muxel's control socket lives under the platform data
/// dir, and on macOS that is `~/Library/Application Support/…`. Double quotes are
/// how `ssh_config` carries a value with spaces. Values without whitespace are left
/// bare, so the common case stays readable in logs.
fn quote_option_value(value: &str) -> String {
    if value.contains(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

/// Most panes muxel puts on one ControlMaster.
///
/// Every connection multiplexed over a master is a *session* on the host's sshd,
/// and sshd caps those per connection (`MaxSessions`, 10 unless configured). Past
/// the cap the host refuses the next one: ssh prints `mux_client_request_session:
/// session request failed: Session open refused by peer` into the pane and falls
/// back to a connection of its own, a full handshake later. With every pane *and*
/// every background command sharing one master, a host running eight or nine
/// agents hit that whenever a git or layout poll overlapped a pane launching.
///
/// So panes are spread over as many masters as it takes to stay under the cap, with
/// room left for the `ssh … true` that warms a master before its panes connect.
pub const PANES_PER_MASTER: usize = 8;

/// Which of a host's ControlMasters a connection rides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Master {
    /// Pane master `n`, each carrying at most [`PANES_PER_MASTER`] panes. A pane
    /// keeps one for as long as it is connected.
    Panes(usize),
    /// muxel's own short commands: git, layout sync, tmux queries, kills. Off the
    /// pane masters, so a burst of polls can never take the session a pane needs.
    Commands,
}

/// File name of a host's ControlMaster socket: `<id8>p<n>` for pane master `n`,
/// `<id8>c` for commands.
///
/// No longer than the `<id8>.sock` a host's one master used to have, and that is
/// the point: a Unix socket path has to fit in 104 bytes on macOS, ssh appends 17
/// more while it binds and *exits* when the path won't fit, and muxel's sockets
/// already sit under `~/Library/Application Support/…` there. A longer name would
/// break hosts for anyone whose home directory path was already close.
pub fn control_socket_name(host_id: Uuid, master: Master) -> String {
    let id = host_id.simple().to_string();
    let id = &id[..8];
    match master {
        Master::Panes(n) => format!("{id}p{n}"),
        Master::Commands => format!("{id}c"),
    }
}

/// The pane master a new pane joins, given the masters the host's other panes ride:
/// the first with room, so a host only opens another master once every earlier one
/// is full.
pub fn pane_master(taken: &[usize]) -> usize {
    (0..)
        .find(|n| taken.iter().filter(|t| *t == n).count() < PANES_PER_MASTER)
        .unwrap_or_default()
}

/// The pane masters `pending` new panes will join, in order of first use — what a
/// connect warms before its panes launch together. Panes launched at once onto a
/// master that isn't up yet each open a connection of their own, racing to become
/// the master; all but one lose, print `ControlSocket … already exists, disabling
/// multiplexing` into their pane, and stay unshared.
pub fn pane_masters_for(taken: &[usize], pending: usize) -> Vec<usize> {
    let mut taken = taken.to_vec();
    let mut used = Vec::new();
    for _ in 0..pending {
        let n = pane_master(&taken);
        taken.push(n);
        if !used.contains(&n) {
            used.push(n);
        }
    }
    used
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

/// Dirs appended to a remote command's `PATH` so that `tmux` resolves.
///
/// sshd runs a command through a *non-interactive, non-login* shell, whose `PATH` is
/// sshd's bare default (`/usr/bin:/bin:/usr/sbin:/sbin`) — no profile, no rc file. A
/// Mac's tmux comes from Homebrew, which on Apple silicon lives in `/opt/homebrew/bin`
/// and is on none of them: every remote pane on such a host dies with
/// `command not found: tmux` before a session exists, shell and agent alike (the
/// remote command names `tmux` twice, so the failure even arrives twice). Linux hosts
/// never showed it — a distro tmux is `/usr/bin/tmux`, already on the bare PATH.
///
/// This is the far-side twin of the bug [`crate::gui_path`] fixes for a Finder launch,
/// so it gets the same fix: name the standard prefixes. The remote OS is unknown here,
/// so macOS and Linux ones share a list — a `PATH` entry that doesn't exist is
/// harmless.
///
/// **Appended, never prepended**: a host that resolves `tmux` today keeps resolving the
/// exact binary it resolves now, and these only add fallbacks behind it.
const REMOTE_TMUX_PATH_DIRS: &[&str] = &[
    "/opt/homebrew/bin",              // Homebrew, Apple silicon
    "/usr/local/bin",                 // Homebrew, Intel macOS — and the usual local prefix
    "/opt/local/bin",                 // MacPorts
    "/home/linuxbrew/.linuxbrew/bin", // Linuxbrew
    "/snap/bin",
    "$HOME/.local/bin",
    "$HOME/bin",
    "$HOME/.nix-profile/bin", // Nix
];

/// The `PATH` assignment that must lead **every** remote command line naming `tmux` —
/// panes (see [`ssh_args`]), session listing, and session teardown alike. See
/// [`REMOTE_TMUX_PATH_DIRS`] for why.
///
/// Only `tmux` itself is resolved this way. A *program* (the agent) still goes through
/// the user's shell — see [`login_shell_command`] — because it needs the user's whole
/// environment, not just a findable binary; tmux only needs to be found, and sourcing a
/// user's rc files to find it would put their startup output into command results muxel
/// parses.
///
/// `$PATH` and `$HOME` are expanded by the *remote* shell, so this is deliberately left
/// unquoted.
pub fn tmux_path_prelude() -> String {
    format!("export PATH=\"$PATH:{}\"", REMOTE_TMUX_PATH_DIRS.join(":"))
}

/// `tmux` on `socket`, as the words that start a remote command. Every word is
/// shell-quoted, including recorded socket names supplied by peers.
fn tmux_on(socket: &Socket) -> String {
    std::iter::once("tmux")
        .map(str::to_string)
        .chain(socket.args().iter().map(|arg| sh_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// How a remote command names tmux after [`select_tmux_socket`]: on whichever
/// server that chose.
const SELECTED_TMUX: &str = "tmux \"$@\"";

/// A remote shell fragment that points `"$@"` at the tmux server holding `session`
/// — muxel's own, unless the session is running only on the default server (see
/// [`Socket::locate`]) — so every tmux word after it can be [`SELECTED_TMUX`].
///
/// Positional parameters rather than a variable, because zsh (the login shell that
/// runs ssh's command on a macOS host) doesn't split an unquoted `$s` holding
/// `-L mxl` into two words, and rather than a function, because a pane has to
/// `exec` tmux itself. Must follow [`tmux_path_prelude`].
pub fn select_tmux_socket(session: &str) -> String {
    select_session_socket(&crate::tmux::SessionTarget::from(session))
}

pub fn select_session_socket(session: &crate::tmux::SessionTarget) -> String {
    let target = sh_quote(&format!("={}", session.name));
    let point_at = |socket: &Socket| {
        std::iter::once("set --")
            .map(str::to_string)
            .chain(socket.args().iter().map(|arg| sh_quote(arg)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    if let Some(socket) = &session.socket {
        return point_at(socket);
    }
    format!(
        "if ! {} has-session -t {target} 2>/dev/null \
         && {} has-session -t {target} 2>/dev/null; then {}; else {}; fi",
        tmux_on(&Socket::Private),
        tmux_on(&Socket::Default),
        point_at(&Socket::Default),
        point_at(&Socket::Private),
    )
}

/// A remote command line running `tmux <args>` on the server holding `session`.
pub fn session_tmux_command(session: &crate::tmux::SessionTarget, args: &[String]) -> String {
    let mut cmd = format!(
        "{}; {}; {SELECTED_TMUX}",
        tmux_path_prelude(),
        select_session_socket(session)
    );
    for a in args {
        cmd.push(' ');
        cmd.push_str(&sh_quote(a));
    }
    cmd
}

/// What closes each server's part of [`list_sessions_command`]'s output, followed by
/// that listing's exit status. No listed pane can look like it: each of their lines
/// starts with a session name, and tmux never puts a `:` in one.
const LISTING_END: &str = ":muxel-listed:";

/// A remote command line listing every pane on both of the host's tmux servers
/// ([`crate::tmux::list_sessions_args`]), each listing closed by its exit status so
/// [`parse_session_listings`] can tell a server that isn't running from one that
/// couldn't be asked. One round trip for both.
pub fn list_sessions_command() -> String {
    list_sessions_command_on(&Socket::BOTH)
}

pub fn list_sessions_command_on(sockets: &[Socket]) -> String {
    tmux_listing_command_on(sockets, crate::tmux::list_sessions_args())
}

/// Migration needs only names. Do not split a valid session name on the pane
/// inventory's `|` separator, or a live imported owner can appear absent.
pub fn list_session_names_command_on(sockets: &[Socket]) -> String {
    tmux_listing_command_on(
        sockets,
        vec![
            "list-sessions".into(),
            "-F".into(),
            "#{session_name}".into(),
        ],
    )
}

fn tmux_listing_command_on(sockets: &[Socket], args: Vec<String>) -> String {
    let args: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
    let mut cmd = tmux_path_prelude();
    for socket in sockets {
        cmd.push_str(&format!(
            "; {} {} 2>&1; echo \"{LISTING_END}$?\"",
            tmux_on(socket),
            args.join(" ")
        ));
    }
    cmd
}

/// The sessions in [`list_sessions_command`]'s output, muxel's own server's first.
///
/// `None` unless both servers answered. A server that isn't running is an answer —
/// nothing there — but one that couldn't be asked is not: read as "no sessions", it
/// would let a closed pane's unconfirmed kill be forgotten (see
/// [`crate::tmux::ClosedSession`]).
pub fn parse_session_listings(out: &str) -> Option<Vec<crate::tmux::RemoteSession>> {
    parse_session_listings_on(out, &Socket::BOTH)
}

pub fn parse_session_listings_on(
    out: &str,
    sockets: &[Socket],
) -> Option<Vec<crate::tmux::RemoteSession>> {
    let mut panes = Vec::new();
    let mut listing = String::new();
    let mut answered = 0;
    for line in out.lines() {
        let Some(status) = line.strip_prefix(LISTING_END) else {
            listing.push_str(line);
            listing.push('\n');
            continue;
        };
        let socket = sockets.get(answered)?;
        if status.trim() == "0" {
            panes.extend(crate::tmux::parse_sessions_on(&listing, socket.clone()));
        } else if !crate::tmux::no_server(&listing) {
            return None;
        }
        listing.clear();
        answered += 1;
    }
    (answered == sockets.len() && listing.trim().is_empty()).then_some(panes)
}

/// Both servers must give complete name-only answers before absence may authorize
/// moving a recorded default owner. Only name/socket are populated in these rows.
pub fn parse_session_listings_for_migration(out: &str) -> Option<Vec<crate::tmux::RemoteSession>> {
    parse_session_listings_for_migration_on(out, &Socket::BOTH)
}

/// The same checked reply contract when confirming one recorded named owner:
/// every server's [`checked_session_listings_on`] answer, and nothing after them.
pub fn parse_session_listings_for_migration_on(
    out: &str,
    sockets: &[Socket],
) -> Option<Vec<crate::tmux::RemoteSession>> {
    let (answers, complete) = checked_listings(out, sockets);
    if !complete {
        return None;
    }
    let answers: Option<Vec<_>> = answers.into_iter().collect();
    answers.map(|answers| answers.into_iter().flatten().collect())
}

/// [`list_session_names_command_on`]'s output, one answer per server queried, in
/// order: the sessions it listed, none for a server confirmed not running, or
/// `None` for any other reply (one cut off before that server answered
/// included). A server's answer stands whatever the others said.
///
/// A listing that exited 0 holds one exact session name per line. An empty line
/// is no name tmux lists, so it spoils that server's answer. Ported to Swift as
/// `TmuxCommands.parseConfirmedListings` — keep both in step.
pub fn checked_session_listings_on(
    out: &str,
    sockets: &[Socket],
) -> Vec<Option<Vec<crate::tmux::RemoteSession>>> {
    checked_listings(out, sockets).0
}

/// [`checked_session_listings_on`], and whether nothing but whitespace followed
/// the last server's answer.
fn checked_listings(
    out: &str,
    sockets: &[Socket],
) -> (Vec<Option<Vec<crate::tmux::RemoteSession>>>, bool) {
    let mut answers = Vec::with_capacity(sockets.len());
    let mut listing = String::new();
    let mut complete = true;
    for line in out.lines() {
        let Some(status) = line.strip_prefix(LISTING_END) else {
            listing.push_str(line);
            listing.push('\n');
            continue;
        };
        match sockets.get(answers.len()) {
            Some(socket) => answers.push(checked_listing(&listing, status, socket)),
            // More answers than servers asked.
            None => complete = false,
        }
        listing.clear();
    }
    complete &= answers.len() == sockets.len() && listing.trim().is_empty();
    answers.resize(sockets.len(), None);
    (answers, complete)
}

fn checked_listing(
    listing: &str,
    status: &str,
    socket: &Socket,
) -> Option<Vec<crate::tmux::RemoteSession>> {
    if status.trim() == "0" {
        return listing
            .lines()
            .map(|name| {
                (!name.is_empty()).then(|| crate::tmux::RemoteSession {
                    name: name.to_string(),
                    socket: socket.clone(),
                    path: String::new(),
                    command: String::new(),
                    title: String::new(),
                })
            })
            .collect();
    }
    // A malformed status is not a failed tmux command, even when its output
    // resembles an absent server.
    let failed = status.trim().parse::<u8>().is_ok_and(|code| code != 0);
    (failed && crate::tmux::confirmed_no_server(listing)).then(Vec::new)
}

/// Whether a pane recorded on the default server still has its session there,
/// by `checked` ([`checked_session_listings_on`] over `sockets`). It reattaches
/// there, whatever the other servers answered: only moving it off Default needs
/// every server's checked answer. Ported to Swift as
/// `TmuxCommands.defaultOwnerTargets`.
pub fn still_on_default(
    checked: &[Option<Vec<crate::tmux::RemoteSession>>],
    sockets: &[Socket],
    session: &crate::tmux::SessionTarget,
) -> bool {
    session.socket == Some(Socket::Default)
        && sockets.iter().zip(checked).any(|(socket, rows)| {
            *socket == Socket::Default && rows.iter().flatten().any(|row| row.name == session.name)
        })
}

/// Exit codes for [`kill_and_confirm_command`], chosen above the range a shell
/// hands out for a command that merely failed.
pub const TMUX_STILL_ALIVE: i32 = 91;
pub const TMUX_MISSING: i32 = 92;

/// A remote command line that kills tmux session `session` on both of the host's
/// servers, ignoring the answer — for quit-time cleanup, which can't wait for one.
/// The selected owner only, resolving legacy name-only bindings first. The `=`
/// target is exact-match, as in
/// [`crate::tmux::kill_session_args`].
pub fn kill_command(session: &crate::tmux::SessionTarget) -> String {
    session_tmux_command(session, &crate::tmux::kill_session_args(&session.name))
}

/// A remote command line that kills tmux session `session` and then **says whether
/// it worked**: exit `0` when the host no longer has the session,
/// [`TMUX_STILL_ALIVE`] when it does, [`TMUX_MISSING`] when there is no `tmux` to
/// ask.
///
/// The confirmation is the whole point. `kill-session` alone exits non-zero both
/// when the session was never there (fine) and when nothing ran at all (not fine),
/// so a caller that only looks at the kill cannot tell a finished teardown from a
/// silently dropped one — and a dropped one leaves the agent running on the host.
/// `has-session` is the question actually being asked — of the selected owner, as in
/// [`kill_command`].
///
/// `tmux` is separately checked with `command -v`, because an unfindable `tmux`
/// makes `has-session` fail too, which would otherwise read as "the session is
/// gone". The `=` target is exact-match, as in [`crate::tmux::kill_session_args`].
pub fn kill_and_confirm_command(session: &crate::tmux::SessionTarget) -> String {
    let target = sh_quote(&format!("={}", session.name));
    let mut steps = vec![
        tmux_path_prelude(),
        format!("command -v tmux >/dev/null 2>&1 || exit {TMUX_MISSING}"),
        select_session_socket(session),
    ];
    steps.push(format!(
        "{SELECTED_TMUX} kill-session -t {target} >/dev/null 2>&1"
    ));
    steps.push(format!(
        "{SELECTED_TMUX} has-session -t {target} >/dev/null 2>&1 && exit {TMUX_STILL_ALIVE}"
    ));
    steps.push("exit 0".to_string());
    steps.join("; ")
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
    /// A discovered or persisted owner. `None` resolves legacy name-only panes.
    pub tmux_socket: Option<Socket>,
    /// This machine's tag for its tmux client (see [`tmux_client_option`]). With
    /// one, a client joining a session that already has a client attaches without
    /// taking the window size and records its name, so
    /// [`claim_window_size_command`] can hand the size back once someone is
    /// actually at this machine. `None` attaches the plain way.
    pub tmux_client_tag: Option<&'a str>,
}

/// The session option a tagged client records its own tmux client name in, so this
/// machine can find its client among every other one attached to the session.
pub fn tmux_client_option(tag: &str) -> String {
    format!("@muxel_client_{tag}")
}

/// Whether the remote tmux supports client flags on `new-session` (tmux 3.2+,
/// whose usage then lists `[-f flags]`). An older tmux, or one that can't list a
/// single command, fails the match and attaches the plain way. Asked of the server
/// [`select_tmux_socket`] chose: `list-commands` starts a server where none runs.
const TMUX_HAS_CLIENT_FLAGS: &str =
    "tmux \"$@\" list-commands new-session 2>/dev/null | grep -q 'f flags'";

/// `exec tmux "$@" <targs> [-- <program>] [<tail>]` — on the server
/// [`select_tmux_socket`] chose — every word shell-quoted except the program, which
/// [`login_shell_command`] has already made a shell fragment.
fn exec_tmux(targs: &[String], program: Option<&str>, tail: &[String]) -> String {
    let mut cmd = format!("exec {SELECTED_TMUX}");
    for a in targs {
        cmd.push(' ');
        cmd.push_str(&sh_quote(a));
    }
    if let Some(program) = program {
        cmd.push_str(" -- ");
        cmd.push_str(program);
    }
    for a in tail {
        cmd.push(' ');
        cmd.push_str(&sh_quote(a));
    }
    cmd
}

/// A remote command line that gives this machine's tmux client for `session` the
/// window size again: it clears the `ignore-size` flag the client attached with,
/// then re-sets the client's own [`tmux_client_option`], because tmux recalculates
/// window sizes after any option change but not after a flag change. Run once the
/// user is at this machine. A no-op when the client never recorded itself (tmux
/// older than 3.2) or is no longer attached to the session, since its tty name may
/// have passed to someone else's client.
pub fn claim_window_size_command(session: &crate::tmux::SessionTarget, tag: &str) -> String {
    let pane = sh_quote(&format!("={}:", session.name));
    let target = sh_quote(&format!("={}", session.name));
    let option = sh_quote(&tmux_client_option(tag));
    format!(
        "{}; {}; c=$({SELECTED_TMUX} show-options -qv -t {pane} {option}) \
         && [ -n \"$c\" ] \
         && {SELECTED_TMUX} list-clients -t {target} -F '#{{client_name}}' \
         | grep -qxF -- \"$c\" \
         && {SELECTED_TMUX} refresh-client -t \"$c\" -f '!ignore-size' ';' \
         set-option -q -t {pane} {option} \"$c\"",
        tmux_path_prelude(),
        select_session_socket(session)
    )
}

/// A remote *program* — run through the user's login+interactive shell.
///
/// Handed to ssh or to tmux directly, a program is `exec`'d with the environment of
/// a non-interactive ssh command, whose `PATH` is sshd's bare default
/// (`/usr/local/bin:/usr/bin:/usr/local/sbin:/usr/sbin`). Agents don't live there —
/// they sit in a user prefix (`~/.local/bin`, an nvm/npm-global dir) that only the
/// user's shell profile adds. So `-- claude` dies with ENOENT the instant the pane
/// starts: tmux closes the window, the session dies with it, and the pane shows
/// `[exited]` — with a clean exit 0, which then reads as "the process finished".
///
/// A *shell* pane never hit this, which is what makes the bug look so odd: with no
/// program, tmux starts its own default shell as a login shell and the profile runs.
///
/// So give a program the same footing: `$SHELL -ilc 'exec <program> <args>'`, which
/// is the PATH the user gets on that host by hand. Login *and* interactive, because
/// `PATH` is as often set in `.zshrc`/`.bashrc` (interactive-only) as in a profile.
/// The inner `exec` keeps the agent as the pane's own process, so exit status and
/// signals are unchanged. `$SHELL` is deliberately unquoted — the *remote* shell
/// expands it.
///
/// Ported from Swift's `TmuxCommands.launchAgent`, which fixed this on iOS first —
/// keep the two in step.
fn login_shell_command(program: &str, args: &[String]) -> String {
    let mut inner = String::from("exec ");
    inner.push_str(&sh_quote(program));
    for a in args {
        inner.push(' ');
        inner.push_str(&sh_quote(a));
    }
    format!("\"${{SHELL:-/bin/sh}}\" -ilc {}", sh_quote(&inner))
}

/// The remote command a pane runs on a **Windows** host.
///
/// No tmux branch: Windows has none, so a pane here is exactly as durable as its
/// SSH connection. The caller has already decided that — see
/// [`RemoteHost::use_tmux`] — but this ignores `spec.use_tmux` outright rather
/// than trusting it, because a stale `true` would otherwise produce a pane that
/// dies on `tmux: command not found`.
///
/// Unlike muxel's own commands (see [`crate::winshell`]), this deliberately does
/// **not** pass `-NoProfile`: an interactive pane needs the user's real `PATH`,
/// or an agent installed by npm/winget is not on it. That is the same reason
/// [`login_shell_command`] uses `-ilc` on Unix.
///
/// A shell pane keeps the session open (`-NoExit` / `cmd /K`). A program pane
/// does not, so the pane ends when the agent ends — matching the Unix `exec`.
fn windows_remote_command(spec: &SshSpec) -> String {
    let shell = spec.host.windows_shell;
    if shell == WindowsShell::Cmd {
        return cmd_remote_command(spec);
    }

    // PowerShell. The whole script is base64'd below, so nothing here has to
    // survive the outer shell's parsing — see `crate::winshell`.
    let mut script = String::new();
    if let Some(cwd) = spec.remote_cwd {
        // A missing folder is created first, so the program starts in it rather
        // than in the home folder. `CreateDirectory` (no-op when it exists) and
        // -LiteralPath both take the path literally, where a project directory
        // containing `[` or `]` is a wildcard to -Path and would not be found.
        let cwd = winshell::ps_quote(cwd);
        script.push_str(&format!(
            "$null = [System.IO.Directory]::CreateDirectory({cwd}); \
             Set-Location -LiteralPath {cwd}"
        ));
    }
    if let Some(program) = spec.program {
        if !script.is_empty() {
            script.push_str("; ");
        }
        // `&` is the call operator: it runs the command a string names, where a
        // bare `'claude'` would just echo the word.
        script.push_str(&format!("& {}", winshell::ps_quote(program)));
        for a in spec.args {
            script.push(' ');
            script.push_str(&winshell::ps_quote(a));
        }
    }

    let mut argv = vec![shell.exe().to_string(), "-NoLogo".to_string()];
    if spec.program.is_none() {
        // No program: the interactive prompt *is* the pane, so keep it alive
        // after the Set-Location runs.
        argv.push("-NoExit".to_string());
    }
    if !script.is_empty() {
        argv.push("-EncodedCommand".to_string());
        argv.push(winshell::encoded_command(&script));
    }
    argv.join(" ")
}

/// The `cmd.exe` form of [`windows_remote_command`].
///
/// cmd has no scripting worth the trouble and no way to quote that survives
/// nesting, so muxel keeps this minimal and offers PowerShell as the default.
/// `cd /d` rather than a bare `cd`, which silently does nothing across drives.
/// Unlike every other pane, it does not create a missing folder.
fn cmd_remote_command(spec: &SshSpec) -> String {
    let mut inner = String::new();
    if let Some(cwd) = spec.remote_cwd {
        inner.push_str(&format!("cd /d {cwd}"));
    }
    if let Some(program) = spec.program {
        if !inner.is_empty() {
            inner.push_str(" && ");
        }
        inner.push_str(program);
        for a in spec.args {
            inner.push(' ');
            inner.push_str(a);
        }
    }
    if inner.is_empty() {
        return "cmd.exe".to_string();
    }
    // /K keeps an interactive session open; /C ends with the program, matching
    // the Unix `exec`.
    let flag = if spec.program.is_some() { "/C" } else { "/K" };
    format!("cmd.exe {flag} {inner}")
}

/// The remote command string a pane runs (the single argument after `--`).
fn remote_command(spec: &SshSpec) -> String {
    if spec.host.os == RemoteOs::Windows {
        return windows_remote_command(spec);
    }
    if spec.use_tmux {
        // `tmux new-session -A` attaches if the session exists, so a reconnect
        // resumes the running agent. Reuse the local tmux arg builder, run remote.
        // The program is appended separately, wrapped in a login shell — see
        // `login_shell_command` — so tmux is told to create the session with no
        // command of its own.
        //
        // The session's program starts from the remote tmux server's environment. A
        // server started from a muxel pane on that host has that pane's
        // `MUXEL_INSTANCE_ID`, which this pane's program would take as its own, so it
        // is cleared from the server before creating a session. A local control id
        // does not identify the remote program, so no id is passed to it.
        let session = spec.tmux_session.unwrap_or("muxel");
        let target = crate::tmux::SessionTarget::new(session, spec.tmux_socket.clone());
        let targs = crate::tmux::launch_session_args_with_env(
            session,
            spec.remote_cwd,
            None,
            &[],
            &[],
            &[crate::control::INSTANCE_ID_ENV],
        );
        let program = spec.program.map(|p| login_shell_command(p, spec.args));
        // Fork the remote tmux server off a project-less command line first — it's a
        // separate process, so the server never inherits the argv below. Without this,
        // a `pkill -f <project>` *on the remote host* matches the shared server and
        // kills every session on it. See `tmux::start_server_args`. This runs before
        // the `exec`, so it costs one short-lived process and nothing after.
        //
        // Every `tmux` word below is resolved against the augmented PATH — sshd's bare
        // default doesn't have Homebrew's on it. See `tmux_path_prelude`.
        //
        // Choose the owner before starting or attaching anything. A known socket
        // stays fixed; a legacy pane resolves its existing server once.
        let mut cmd = tmux_path_prelude();
        cmd.push_str("; ");
        cmd.push_str(&select_session_socket(&target));
        cmd.push_str("; ");
        // Keep owned servers alive between sessions, without changing the user's
        // default server's exit-empty policy when reattaching a legacy pane.
        cmd.push_str("if [ \"$#\" -gt 0 ]; then ");
        cmd.push_str(SELECTED_TMUX);
        for a in crate::tmux::start_server_args() {
            cmd.push(' ');
            cmd.push_str(&sh_quote(&a));
        }
        cmd.push_str("; fi; ");
        // A new session starts in its folder, so create a missing one; tmux would
        // otherwise start it in the home folder. After the server starts, whose
        // command line must not name the project. Best effort: a failure must not
        // keep a running session from being reattached.
        if let Some(cwd) = spec.remote_cwd {
            cmd.push_str(&format!("mkdir -p -- {}; ", sh_quote(cwd)));
        }
        // Attaching makes a client the window's latest, and under tmux's default
        // `window-size latest` the window takes its size — so a pane reconnecting
        // on an unattended laptop resized the session under whoever was using it
        // elsewhere. An `ignore-size` client sits out the size while any unflagged
        // client is attached. It records its own name after attaching — the
        // command after `;` runs as the attached client — so the app can clear the
        // flag once someone is here.
        //
        // Only flag the client when the session already has one: that is the only
        // time there is someone else's size to protect. tmux counts unflagged
        // clients across the whole *server*, not the session, so a flagged pane
        // alone in its session still sat out its own size whenever any other
        // client was attached to anything on the host — this machine's own other
        // panes included. Its window then stayed at the size it was created at
        // (80×24 for a new pane) until the user typed, so the agent wrapped its
        // lines at a width the pane didn't have and tmux cropped the rest.
        if let Some(tag) = spec.tmux_client_tag {
            let attached = format!(
                "{SELECTED_TMUX} list-clients -t {} 2>/dev/null | grep -q .",
                sh_quote(&format!("={session}"))
            );
            let mut flagged = targs.clone();
            flagged.extend(["-f".to_string(), "ignore-size".to_string()]);
            let record = [
                ";".to_string(),
                "set-option".to_string(),
                "-Fq".to_string(),
                "-t".to_string(),
                format!("={session}:"),
                tmux_client_option(tag),
                "#{client_name}".to_string(),
            ];
            cmd.push_str(&format!(
                "if {TMUX_HAS_CLIENT_FLAGS} && {attached}; then {}; fi; ",
                exec_tmux(&flagged, program.as_deref(), &record)
            ));
        }
        cmd.push_str(&exec_tmux(&targs, program.as_deref(), &[]));
        cmd
    } else {
        let mut cmd = String::new();
        if let Some(cwd) = spec.remote_cwd {
            // Create a missing folder rather than fail the `cd`.
            let cwd = sh_quote(cwd);
            cmd.push_str(&format!("mkdir -p -- {cwd} && cd {cwd} && "));
        }
        cmd.push_str("exec ");
        match spec.program {
            // Same PATH problem without tmux: ssh runs this via a *non-interactive*
            // shell, which reads no profile.
            Some(p) => cmd.push_str(&login_shell_command(p, spec.args)),
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
    use crate::winshell::WindowsShell;
    use crate::{RemoteHost, RemoteOs};
    use std::path::PathBuf;

    fn host() -> RemoteHost {
        RemoteHost::new("dev", "example.com")
    }

    fn win_host() -> RemoteHost {
        let mut h = RemoteHost::new("win", "win.example.com");
        h.os = RemoteOs::Windows;
        h
    }

    fn decode_encoded(cmd: &str) -> String {
        use base64::Engine as _;
        let b64 = cmd.rsplit(' ').next().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    /// The regression that matters most: a host flipped to Windows still carries
    /// whatever `use_tmux` it was saved with, and an `Instance` carries its own.
    /// Neither may put `tmux` on a Windows command line — the pane would die on
    /// `tmux: command not found`.
    #[test]
    fn windows_pane_never_uses_tmux_even_when_asked() {
        let h = win_host();
        for (use_tmux, session) in [(true, Some("muxel-proj-1")), (false, None)] {
            let cmd = remote_command(&SshSpec {
                host: &h,
                control_path: "/s",
                remote_cwd: Some("C:/src/app"),
                program: Some("claude"),
                args: &[],
                use_tmux,
                tmux_session: session,
                tmux_socket: None,
                tmux_client_tag: None,
            });
            assert!(
                !cmd.contains("tmux"),
                "tmux leaked into a Windows pane: {cmd}"
            );
        }
    }

    /// And the host-level gate agrees, whatever the instance wants.
    #[test]
    fn use_tmux_gate_is_false_for_windows_and_unchanged_for_unix() {
        let mut w = win_host();
        w.default_use_tmux = true;
        assert!(!w.use_tmux(true));
        assert!(!w.use_tmux(false));

        let mut u = host();
        u.default_use_tmux = false;
        assert!(u.use_tmux(true), "instance preference still counts on Unix");
        assert!(!u.use_tmux(false));
        u.default_use_tmux = true;
        assert!(u.use_tmux(false), "host default still counts on Unix");
    }

    #[test]
    fn windows_shell_pane_sets_cwd_and_stays_open() {
        let cmd = remote_command(&SshSpec {
            host: &win_host(),
            control_path: "/s",
            remote_cwd: Some(r"C:\src\my app"),
            program: None,
            args: &[],
            use_tmux: false,
            tmux_session: None,
            tmux_socket: None,
            tmux_client_tag: None,
        });
        assert!(cmd.starts_with("powershell.exe -NoLogo -NoExit -EncodedCommand "));
        let script = decode_encoded(&cmd);
        assert_eq!(
            script,
            "$null = [System.IO.Directory]::CreateDirectory('C:\\src\\my app'); \
             Set-Location -LiteralPath 'C:\\src\\my app'"
        );
    }

    /// A program pane must end with its program, the way the Unix `exec` does —
    /// so no `-NoExit`.
    #[test]
    fn windows_program_pane_ends_with_the_program() {
        let cmd = remote_command(&SshSpec {
            host: &win_host(),
            control_path: "/s",
            remote_cwd: Some("C:/src/app"),
            program: Some("claude"),
            args: &["--model".to_string(), "be terse".to_string()],
            use_tmux: false,
            tmux_session: None,
            tmux_socket: None,
            tmux_client_tag: None,
        });
        assert!(!cmd.contains("-NoExit"), "{cmd}");
        let script = decode_encoded(&cmd);
        assert_eq!(
            script,
            "$null = [System.IO.Directory]::CreateDirectory('C:/src/app'); \
             Set-Location -LiteralPath 'C:/src/app'; & 'claude' '--model' 'be terse'"
        );
    }

    /// An interactive pane must load the user's profile, or an agent installed by
    /// npm/winget is not on PATH — the Windows twin of `login_shell_command`'s
    /// `-ilc`. muxel's *own* commands are the opposite and pass `-NoProfile`.
    #[test]
    fn windows_pane_loads_the_user_profile() {
        let cmd = remote_command(&SshSpec {
            host: &win_host(),
            control_path: "/s",
            remote_cwd: None,
            program: Some("claude"),
            args: &[],
            use_tmux: false,
            tmux_session: None,
            tmux_socket: None,
            tmux_client_tag: None,
        });
        assert!(!cmd.contains("-NoProfile"), "{cmd}");
    }

    #[test]
    fn windows_cmd_shell_uses_cd_slash_d() {
        let mut h = win_host();
        h.windows_shell = WindowsShell::Cmd;
        let shell = remote_command(&SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: Some(r"D:\work"),
            program: None,
            args: &[],
            use_tmux: false,
            tmux_session: None,
            tmux_socket: None,
            tmux_client_tag: None,
        });
        // A bare `cd` does not change drive on Windows.
        assert_eq!(shell, r"cmd.exe /K cd /d D:\work");

        let prog = remote_command(&SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: Some(r"D:\work"),
            program: Some("claude"),
            args: &[],
            use_tmux: false,
            tmux_session: None,
            tmux_socket: None,
            tmux_client_tag: None,
        });
        assert!(prog.starts_with("cmd.exe /C "), "{prog}");
    }

    #[test]
    fn unix_pane_command_is_unchanged_by_the_windows_branch() {
        // The existing Unix shape, asserted verbatim: adding Windows must not
        // have moved anything on the side every current host uses.
        let cmd = remote_command(&SshSpec {
            host: &host(),
            control_path: "/s",
            remote_cwd: Some("/srv/app"),
            program: None,
            args: &[],
            use_tmux: false,
            tmux_session: None,
            tmux_socket: None,
            tmux_client_tag: None,
        });
        assert_eq!(
            cmd,
            "mkdir -p -- /srv/app && cd /srv/app && exec ${SHELL:-/bin/sh} -l"
        );
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
        // zsh would expand a bare `=word` (a tmux exact-match target).
        assert_eq!(sh_quote("=muxel_p_1a2b3c4d"), "'=muxel_p_1a2b3c4d'");
        assert_eq!(sh_quote("a=b"), "a=b");
    }

    /// The teardown command must answer "is it gone?", not "did a kill run?" —
    /// the difference between a closed remote agent and one still burning tokens.
    #[test]
    fn kill_and_confirm_asks_has_session_after_killing() {
        let cmd = kill_and_confirm_command(&crate::tmux::SessionTarget::new(
            "muxel_app_1a2b3c4d",
            Some(Socket::Private),
        ));
        assert!(
            cmd.starts_with(&tmux_path_prelude()),
            "tmux must be findable first: {cmd}"
        );
        let kill = cmd.find("kill-session").expect("kills the session");
        let has = cmd.find("has-session").expect("then confirms it is gone");
        assert!(kill < has, "confirmation comes after the kill: {cmd}");
        assert!(
            cmd.contains(&format!("exit {TMUX_STILL_ALIVE}")) && cmd.ends_with("exit 0"),
            "a surviving session is distinguishable from a clean teardown: {cmd}"
        );
        assert!(
            cmd.contains(&format!(
                "command -v tmux >/dev/null 2>&1 || exit {TMUX_MISSING}"
            )),
            "a missing tmux is its own answer, not a silent success: {cmd}"
        );
    }

    /// Exact-match, so killing `muxel_p_1` never takes down `muxel_p_12`, and a
    /// session name is never able to run anything of its own.
    #[test]
    fn kill_and_confirm_targets_one_session_exactly() {
        let cmd = kill_and_confirm_command(&crate::tmux::SessionTarget::new(
            "muxel_p_1",
            Some(Socket::Default),
        ));
        // The kill and the confirmation, on the selected server.
        assert_eq!(
            cmd.matches("-t '=muxel_p_1' ").count(),
            2,
            "both the kill and the confirmation name the same exact target: {cmd}"
        );
        // A session name is data, never a second command: it arrives single-quoted,
        // with its own quotes escaped, so the shell reads it as one word.
        let hostile = kill_and_confirm_command(&crate::tmux::SessionTarget::new(
            "x'; rm -rf ~; '",
            Some(Socket::Private),
        ));
        assert_eq!(
            hostile.matches(r"-t '=x'\''; rm -rf ~; '\''' ").count(),
            2,
            "hostile name is quoted whole: {hostile}"
        );
        let hostile = select_tmux_socket("x'; rm -rf ~; '");
        assert_eq!(
            hostile.matches(r"-t '=x'\''; rm -rf ~; '\''' ").count(),
            2,
            "hostile name is quoted whole: {hostile}"
        );
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
            "-o",
            "ServerAliveInterval=20",
            "-o",
            "ServerAliveCountMax=3",
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

    /// muxel's control socket lives under the platform data dir, which on macOS is
    /// `~/Library/Application Support/…`. Unquoted, ssh splits that on the space and
    /// dies with "keyword controlpath extra arguments at end of line" before it ever
    /// connects — every remote action (project scan, panes, git) fails.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn control_path_with_a_space_is_quoted() {
        let h = host();
        let path = "/Users/me/Library/Application Support/dev.muxel.muxel/ssh/ab12.sock";
        let args = connection_args(&h, path);
        let want =
            "ControlPath=\"/Users/me/Library/Application Support/dev.muxel.muxel/ssh/ab12.sock\""
                .to_string();
        assert!(
            args.contains(&want),
            "spacey control path must be quoted, got {args:?}"
        );
    }

    #[test]
    fn quoting_is_only_applied_where_it_is_needed() {
        assert_eq!(quote_option_value("/tmp/s.sock"), "/tmp/s.sock");
        assert_eq!(quote_option_value("/a b/s.sock"), "\"/a b/s.sock\"");
    }

    #[test]
    fn each_master_has_its_own_short_socket() {
        let id = Uuid::parse_str("ab12cd34-0000-4000-8000-000000000000").unwrap();
        assert_eq!(control_socket_name(id, Master::Panes(0)), "ab12cd34p0");
        assert_eq!(control_socket_name(id, Master::Panes(2)), "ab12cd34p2");
        assert_eq!(control_socket_name(id, Master::Commands), "ab12cd34c");
        // Never longer than the `ab12cd34.sock` every host's socket used to be: on
        // macOS the whole path is tight, and ssh won't start when it doesn't fit.
        for master in [Master::Panes(0), Master::Panes(99), Master::Commands] {
            assert!(control_socket_name(id, master).len() <= "ab12cd34.sock".len());
        }
    }

    /// sshd refuses sessions past `MaxSessions` (10 by default) on one connection.
    #[test]
    fn panes_fill_a_master_before_opening_another() {
        const { assert!(PANES_PER_MASTER < 10, "must stay under sshd's default cap") };
        assert_eq!(pane_master(&[]), 0);
        let full = vec![0; PANES_PER_MASTER];
        assert_eq!(pane_master(&full), 1);
        // A pane closed on a full master makes room there again, ahead of a later one.
        let mut gap = full.clone();
        gap.pop();
        gap.extend([1, 1]);
        assert_eq!(pane_master(&gap), 0);
        // Only how many ride each master matters, not the order they're listed in.
        let mut mixed = vec![1, 1];
        mixed.extend(vec![0; PANES_PER_MASTER]);
        assert_eq!(pane_master(&mixed), 1);
    }

    #[test]
    fn masters_to_warm_are_the_ones_pending_panes_join() {
        assert!(pane_masters_for(&[], 0).is_empty());
        assert_eq!(pane_masters_for(&[], 3), vec![0]);
        assert_eq!(pane_masters_for(&[], PANES_PER_MASTER + 1), vec![0, 1]);
        // Two panes into a host whose first master has one seat left.
        let taken = vec![0; PANES_PER_MASTER - 1];
        assert_eq!(pane_masters_for(&taken, 2), vec![0, 1]);
        let full = vec![0; PANES_PER_MASTER];
        assert_eq!(pane_masters_for(&full, 2), vec![1]);
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
        h.user = "me".into();
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
        assert!(a.contains(&"ServerAliveCountMax=3".to_string()));
        assert!(a.contains(&"Compression=yes".to_string()));
        assert_eq!(target(&h), "me@example.com");
    }

    #[test]
    fn keepalive_defaults_on_so_drops_are_detected() {
        // A host that never set keepalive still gets a sensible default — the fix
        // for a dropped connection hanging forever instead of exiting.
        let mut h = host();
        h.keepalive_secs = None;
        let a = base_args(&h);
        assert!(a.contains(&"ServerAliveInterval=20".to_string()));
        assert!(a.contains(&"ServerAliveCountMax=3".to_string()));
    }

    #[test]
    fn keepalive_can_be_disabled_and_overridden() {
        // Explicit 0 turns it off entirely.
        let mut h = host();
        h.keepalive_secs = Some(0);
        let a = base_args(&h);
        assert!(!a.iter().any(|o| o.starts_with("ServerAlive")));
        // A hand-written option in extra_options wins over the default.
        let mut h = host();
        h.keepalive_secs = None;
        h.extra_options = vec!["ServerAliveInterval=99".into()];
        let a = base_args(&h);
        assert!(!a.contains(&"ServerAliveInterval=20".to_string()));
        assert!(a.contains(&"ServerAliveInterval=99".to_string()));
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
            tmux_socket: None,
            tmux_client_tag: None,
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
            tmux_socket: None,
            tmux_client_tag: None,
        };
        let v = ssh_args(&spec);
        assert_eq!(v[0], "-t");
        assert_eq!(v[v.len() - 2], "--");
        assert_eq!(
            v.last().unwrap(),
            "mkdir -p -- /srv/app && cd /srv/app && \
             exec \"${SHELL:-/bin/sh}\" -ilc 'exec claude --model opus'"
        );
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
            tmux_socket: None,
            tmux_client_tag: None,
        };
        assert_eq!(
            ssh_args(&spec).last().unwrap(),
            "mkdir -p -- /srv/app && cd /srv/app && exec ${SHELL:-/bin/sh} -l"
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
            tmux_socket: None,
            tmux_client_tag: None,
        };
        // `-u` leads the attaching client: the remote host's login shell may hand
        // tmux no UTF-8 locale, and then it would mangle every non-ASCII cell. The
        // agent runs through a login shell so it resolves on the user's real PATH;
        // tmux itself resolves on the augmented PATH (`tmux_path_prelude`).
        assert_eq!(
            ssh_args(&spec).last().unwrap(),
            &format!(
                "{}; if ! tmux -L mxl has-session -t '=muxel-abc123' 2>/dev/null \
                 && tmux has-session -t '=muxel-abc123' 2>/dev/null; \
                 then set --; else set -- -L mxl; fi; \
                 if [ \"$#\" -gt 0 ]; then tmux \"$@\" start-server ';' set -s exit-empty off; fi; \
                 mkdir -p -- /srv/app; \
                 exec tmux \"$@\" -u set -g mouse on ';' set-environment -g -u MUXEL_INSTANCE_ID ';' \
                 new-session -A -s muxel-abc123 -c /srv/app \
                 -- \"${{SHELL:-/bin/sh}}\" -ilc 'exec claude'",
                tmux_path_prelude()
            )
        );
    }

    /// The migration on a host: a session still running on the default server from
    /// before muxel had its own is reattached where it is (`set --` leaves tmux on
    /// the default socket); every other session — a new one included — is on
    /// muxel's own (`set -- -L mxl`).
    #[test]
    fn a_remote_session_is_found_on_muxels_server_unless_it_runs_only_on_the_default_one() {
        assert_eq!(
            select_tmux_socket("muxel_app_1"),
            "if ! tmux -L mxl has-session -t '=muxel_app_1' 2>/dev/null \
             && tmux has-session -t '=muxel_app_1' 2>/dev/null; \
             then set --; else set -- -L mxl; fi"
        );
        assert_eq!(
            session_tmux_command(&"muxel_app_1".into(), &crate::tmux::kill_session_args("x")),
            format!(
                "{}; {}; tmux \"$@\" kill-session -t '=x'",
                tmux_path_prelude(),
                select_tmux_socket("muxel_app_1")
            )
        );
    }

    #[test]
    fn same_named_sessions_on_two_sockets_remain_distinct() {
        let sessions = parse_session_listings(
            "work|/private|claude|\n:muxel-listed:0\nwork|/default|zsh|\n:muxel-listed:0\n",
        )
        .unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].path, "/private");
        assert_eq!(sessions[1].path, "/default");
        assert_eq!(sessions[0].socket, Socket::Private);
        assert_eq!(sessions[1].socket, Socket::Default);
    }

    /// Both servers are listed in one round trip, muxel's own first, and only a
    /// server that isn't running counts as having no sessions.
    #[test]
    fn remote_sessions_are_listed_from_both_servers() {
        let cmd = list_sessions_command();
        let (prelude, rest) = cmd.split_once("; ").unwrap();
        assert_eq!(prelude, tmux_path_prelude());
        let format = "'#{session_name}|#{session_path}|#{pane_current_command}|#{pane_title}'";
        assert_eq!(
            rest,
            format!(
                "tmux -L mxl list-panes -a -F {format} 2>&1; echo \":muxel-listed:$?\"; \
                 tmux list-panes -a -F {format} 2>&1; echo \":muxel-listed:$?\""
            )
        );

        let names = |out: &str| {
            parse_session_listings(out).map(|s| s.into_iter().map(|s| s.name).collect::<Vec<_>>())
        };
        // A new session on muxel's server, a pane from before on the default one,
        // and the user's own tmux — all of them, muxel's own first.
        assert_eq!(
            names(
                "muxel_p_00000002|/w|claude|\n:muxel-listed:0\n\
                 muxel_p_00000001|/w|claude|\nwork|/home|zsh|\n:muxel-listed:0\n"
            ),
            Some(vec![
                "muxel_p_00000002".to_string(),
                "muxel_p_00000001".to_string(),
                "work".to_string()
            ])
        );
        // A server that isn't running has nothing on it — muxel's own on a host
        // that never started it, and the default one once the last pane from
        // before has ended.
        assert_eq!(
            names(
                "error connecting to /tmp/tmux-1000/mxl (No such file or directory)\n\
                 :muxel-listed:1\nno server running on /tmp/tmux-1000/default\n:muxel-listed:1\n"
            ),
            Some(vec![])
        );
        // Same-named sessions on independent servers remain distinct.
        assert_eq!(
            names("s|/a|claude|\n:muxel-listed:0\ns|/b|zsh|\n:muxel-listed:0\n"),
            Some(vec!["s".to_string(), "s".to_string()])
        );
        // A server that couldn't be asked is not "no sessions": a closed pane's
        // unconfirmed kill would be forgotten.
        assert_eq!(
            names(":muxel-listed:0\nsh: tmux: command not found\n:muxel-listed:127\n"),
            None
        );
        // Nor is an answer cut short, such as by a dropped connection.
        assert_eq!(
            names("muxel_p_00000002|/w|claude|\n:muxel-listed:0\n"),
            None
        );
        assert_eq!(names(""), None);
    }

    /// A known owner never probes or mutates a same-named session on another server.
    #[test]
    fn a_remote_session_is_killed_and_confirmed_only_on_its_owner() {
        for socket in Socket::BOTH {
            let target = crate::tmux::SessionTarget::new("work", Some(socket.clone()));
            let cmd = kill_and_confirm_command(&target);
            let selection = match socket {
                Socket::Private => "; set -- -L mxl;",
                Socket::Default => "; set --;",
                Socket::Isolated(_) => unreachable!("BOTH contains only real-profile sockets"),
                Socket::Named(_) => unreachable!("BOTH contains only the standard servers"),
            };
            assert!(cmd.contains(selection), "{cmd}");
            assert_eq!(cmd.matches("kill-session").count(), 1, "{cmd}");
            assert_eq!(cmd.matches("has-session").count(), 1, "{cmd}");
            assert!(cmd.contains("tmux \"$@\" kill-session -t '=work'"));
            assert!(cmd.contains("tmux \"$@\" has-session -t '=work'"));
            assert_eq!(kill_command(&target).matches("kill-session").count(), 1);
        }
    }

    #[test]
    fn legacy_teardown_resolves_once_before_a_single_kill() {
        let cmd = kill_and_confirm_command(&"work".into());
        assert!(cmd.find("has-session").unwrap() < cmd.find("kill-session").unwrap());
        assert_eq!(cmd.matches("kill-session").count(), 1);
        assert_eq!(cmd.matches("set --").count(), 2);
        assert!(cmd.contains("tmux \"$@\" has-session -t '=work'"));
    }

    #[test]
    fn named_remote_owners_are_quoted_and_never_discovered_again() {
        for name in ["   ", "mxl-profile", "owner '名; $(touch no)"] {
            let socket = Socket::Named(name.into());
            let target = crate::tmux::SessionTarget::new("work", Some(socket.clone()));
            let selection = format!("set -- -L {}", sh_quote(name));
            assert_eq!(select_session_socket(&target), selection);
            for command in [
                kill_command(&target),
                claim_window_size_command(&target, "ab12"),
            ] {
                assert!(command.contains(&format!("; {selection};")), "{command}");
                assert!(
                    !command.contains("has-session"),
                    "known owner must not rediscover: {command}"
                );
            }
            let listing = list_sessions_command_on(std::slice::from_ref(&socket));
            assert!(listing.contains(&format!("; tmux -L {} list-panes", sh_quote(name))));
            let parsed = parse_session_listings_on(
                "work|/p|zsh|\n:muxel-listed:0\n",
                std::slice::from_ref(&socket),
            )
            .unwrap();
            assert_eq!(parsed[0].socket, socket);
            let h = host();
            let command = ssh_args(&SshSpec {
                host: &h,
                control_path: "/s",
                remote_cwd: Some("/w"),
                program: Some("claude"),
                args: &[],
                use_tmux: true,
                tmux_session: Some("work"),
                tmux_socket: Some(socket),
                tmux_client_tag: None,
            })
            .pop()
            .unwrap();
            assert!(command.contains(&format!("; {selection};")), "{command}");
            assert!(!command.contains("has-session"));
        }
    }

    #[test]
    fn migration_listing_requires_complete_success_or_confirmed_absence() {
        let absent =
            "error connecting to /tmp/tmux-1000/mxl (Connection refused)\n:muxel-listed:1\n";
        let live = "work\n:muxel-listed:0\n";
        let parsed = parse_session_listings_for_migration(&format!("{absent}{live}")).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].socket, Socket::Default);
        assert_eq!(
            parse_session_listings_for_migration(&format!("{absent}{absent}")),
            Some(vec![])
        );
        for failed in [
            "error connecting to /tmp/tmux-1000/mxl (Permission denied)\n:muxel-listed:1\n",
            "error connecting to /tmp/tmux-1000/mxl (unknown error)\n:muxel-listed:1\n",
            "work|/p|claude|\nerror connecting to /tmp/tmux-1000/mxl (Connection refused)\n:muxel-listed:1\n",
            "work|/p|claude|\n", // missing completion marker
            "work|/p|claude|\n:muxel-listed:0\ntruncated",
            "\n:muxel-listed:0\n",
            "no server running on /tmp/tmux-1000/mxl\n:muxel-listed:not-a-status\n",
        ] {
            assert!(
                parse_session_listings_for_migration(&format!("{live}{failed}")).is_none(),
                "{failed}"
            );
        }
        assert!(parse_session_listings_for_migration(live).is_none());
        // The older discovery parser retains its existing error classification.
        let permission =
            "error connecting to /tmp/tmux-1000/mxl (Permission denied)\n:muxel-listed:1\n";
        assert!(
            parse_session_listings(&format!("work|/p|claude|\n:muxel-listed:0\n{permission}"))
                .is_some()
        );
    }

    /// Reattaching a session still running on Default needs only Default's answer;
    /// whatever `mxl` replied (an rc file's echo, an unfamiliar error, nothing at
    /// all), the pane attaches as it did before the migration existed.
    #[test]
    fn a_live_default_session_needs_only_defaults_answer() {
        let target = crate::tmux::SessionTarget::new("owned ", Some(Socket::Default));
        let live = "owned \n:muxel-listed:0\n";
        for own in [
            "Welcome to devbox\nowned \n:muxel-listed:0\n",
            "error connecting to /tmp/tmux-1000/mxl (Permission denied)\n:muxel-listed:1\n",
            "\n:muxel-listed:0\n",
            "something unfamiliar\n:muxel-listed:2\n",
            "garbage without a status\n:muxel-listed:\n",
        ] {
            let out = format!("{own}{live}");
            let checked = checked_session_listings_on(&out, &Socket::BOTH);
            assert!(still_on_default(&checked, &Socket::BOTH, &target), "{own}");
        }
        // Moving it would still need both servers' answers.
        let own = "error connecting to /tmp/tmux-1000/mxl (Permission denied)\n:muxel-listed:1\n";
        assert!(parse_session_listings_for_migration(&format!("{own}{live}")).is_none());
        // Default's answer must itself be complete and hold the exact name.
        for default in [
            "owned\n:muxel-listed:0\n",
            "owned \n",
            "owned \n:muxel-listed:1\n",
            "owned \n\n:muxel-listed:0\n",
            "no server running on /tmp/tmux-1000/default\n:muxel-listed:1\n",
        ] {
            let out = format!("{own}{default}");
            let checked = checked_session_listings_on(&out, &Socket::BOTH);
            assert!(
                !still_on_default(&checked, &Socket::BOTH, &target),
                "{default}"
            );
            assert!(
                parse_session_listings_for_migration(&out).is_none(),
                "{default}"
            );
        }
        // Only a pane recorded on Default reattaches by Default's answer alone.
        let checked = checked_session_listings_on(&format!("{own}{live}"), &Socket::BOTH);
        for socket in [None, Some(Socket::Private)] {
            let other = crate::tmux::SessionTarget::new("owned ", socket);
            assert!(!still_on_default(&checked, &Socket::BOTH, &other));
        }
    }

    #[test]
    fn checked_listings_answer_per_server() {
        let out = "\
            Welcome\n:muxel-listed:0\n\
            no server running on /tmp/tmux-1000/default\n:muxel-listed:1\n";
        let checked = checked_session_listings_on(out, &Socket::BOTH);
        assert_eq!(checked.len(), 2);
        assert_eq!(checked[0].as_ref().unwrap()[0].name, "Welcome");
        assert_eq!(checked[0].as_ref().unwrap()[0].socket, Socket::Private);
        assert_eq!(checked[1], Some(vec![]));
        // Cut off before Default answered, and an extra answer nobody asked for.
        let cut = checked_session_listings_on("a\n:muxel-listed:0\nb\n", &Socket::BOTH);
        assert!(cut[0].is_some() && cut[1].is_none());
        let extra = "a\n:muxel-listed:0\nb\n:muxel-listed:0\nc\n:muxel-listed:0\n";
        assert_eq!(
            checked_session_listings_on(extra, &Socket::BOTH)
                .iter()
                .filter(|a| a.is_some())
                .count(),
            2
        );
        assert!(parse_session_listings_for_migration(extra).is_none());
        // CRLF line ends, as the phone's port reads them too.
        let crlf =
            "owned\r\n:muxel-listed:0\r\nno server running on /tmp/default\r\n:muxel-listed:1\r\n";
        let rows = parse_session_listings_for_migration(crlf).unwrap();
        assert_eq!((rows.len(), rows[0].name.as_str()), (1, "owned"));
        assert_eq!(
            parse_session_listings_for_migration("a\n:muxel-listed:0\nb\n:muxel-listed:0\n \n")
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn strict_confirmation_keeps_a_named_owner_and_rejects_failed_replies() {
        let sockets = [Socket::Named("mxl-profile".into())];
        let rows = parse_session_listings_for_migration_on(
            "owned|with|pipes \n:muxel-listed:0\n",
            &sockets,
        )
        .unwrap();
        assert_eq!(rows[0].socket, sockets[0]);
        assert_eq!(rows[0].name, "owned|with|pipes ");
        let target = crate::tmux::SessionTarget::new(rows[0].name.clone(), Some(Socket::Default));
        let default_rows = parse_session_listings_for_migration(
            ":muxel-listed:0\nowned|with|pipes \n:muxel-listed:0\n",
        )
        .unwrap();
        let legacy = crate::tmux::ClosedSession {
            name: target.name.clone(),
            socket: None,
            instance: uuid::Uuid::nil(),
        };
        assert_eq!(
            legacy
                .resolve_owner(&default_rows, Socket::Private)
                .target(),
            target
        );
        assert_eq!(
            target
                .for_respawn::<()>(|socket| Ok(default_rows
                    .iter()
                    .any(|row| &row.socket == socket && row.name == target.name)))
                .unwrap()
                .unwrap(),
            target
        );
        assert_eq!(
            parse_session_listings_for_migration_on(":muxel-listed:0\n", &sockets),
            Some(vec![])
        );
        assert!(
            parse_session_listings_for_migration_on(
                "error connecting to /tmp/profile (Permission denied)\n:muxel-listed:1\n",
                &sockets
            )
            .is_none()
        );
        let command = list_session_names_command_on(&sockets);
        assert!(command.contains("-L mxl-profile"));
        assert!(command.contains("'#{session_name}'"));
        assert!(!command.contains("#{session_path}"));
        assert_eq!(command.matches(":muxel-listed:").count(), 1);
    }

    #[test]
    fn a_known_remote_owner_is_used_for_launch_and_window_claims() {
        let h = host();
        for socket in Socket::BOTH {
            let target = crate::tmux::SessionTarget::new("work", Some(socket));
            let spec = SshSpec {
                host: &h,
                control_path: "/s",
                remote_cwd: Some("/w"),
                program: Some("claude"),
                args: &[],
                use_tmux: true,
                tmux_session: Some(&target.name),
                tmux_socket: target.socket.clone(),
                tmux_client_tag: Some("ab12"),
            };
            for cmd in [
                ssh_args(&spec).pop().unwrap(),
                claim_window_size_command(&target, "ab12"),
            ] {
                assert!(
                    cmd.contains(&format!("; {};", select_session_socket(&target))),
                    "{cmd}"
                );
                assert!(
                    !cmd.contains("has-session"),
                    "known owner never probes: {cmd}"
                );
                assert!(
                    !cmd.contains("tmux -L mxl"),
                    "all commands use the selected argv: {cmd}"
                );
            }
        }
    }

    #[test]
    fn isolated_ssh_discovery_launch_claims_and_teardown_never_probe_global_servers() {
        let root = std::path::Path::new("/isolated-ssh");
        let socket = Socket::isolated(root);
        let label = String::from(socket.clone());
        let sockets = [socket.clone()];
        let listing = list_sessions_command_on(&sockets);
        assert_eq!(listing.matches(":muxel-listed:").count(), 1);
        let parsed =
            parse_session_listings_on("work|/w|claude|\n:muxel-listed:0\n", &sockets).unwrap();
        assert_eq!(parsed[0].socket, socket);
        assert!(
            parse_session_listings_on(":muxel-listed:0\n:muxel-listed:0\n", &sockets).is_none()
        );
        let h = host();
        for prior in [
            None,
            Some(Socket::Private),
            Some(Socket::Default),
            Some(Socket::Isolated(1)),
        ] {
            let target = crate::tmux::SessionTarget::new("work", prior).for_profile(Some(root));
            let spec = SshSpec {
                host: &h,
                control_path: "/s",
                remote_cwd: Some("/w"),
                program: Some("claude"),
                args: &[],
                use_tmux: true,
                tmux_session: Some(&target.name),
                tmux_socket: target.socket.clone(),
                tmux_client_tag: Some("ab12"),
            };
            let cleanup = crate::tmux::unset_option_if_args("work", "@claim", "owner").unwrap();
            for cmd in [
                listing.clone(),
                ssh_args(&spec).pop().unwrap(),
                session_tmux_command(&target, &crate::tmux::show_option_args("work", "@claim")),
                session_tmux_command(
                    &target,
                    &crate::tmux::set_option_args("work", "@claim", "owner"),
                ),
                session_tmux_command(&target, &cleanup),
                claim_window_size_command(&target, "ab12"),
                kill_command(&target),
                kill_and_confirm_command(&target),
            ] {
                assert!(cmd.contains(&format!("-L {label}")), "{cmd}");
                assert!(!cmd.contains("tmux -L mxl "), "global mxl: {cmd}");
                assert!(!cmd.contains("set --;"), "default selection: {cmd}");
                assert!(!cmd.contains("if ! tmux"), "legacy discovery: {cmd}");
                assert!(!cmd.contains("; tmux has-session"), "default probe: {cmd}");
            }
        }
    }

    /// ctl input to a remote tmux pane is one ssh command on the host's commands
    /// master: the pinned pane id and muxel's own buffer name are its only
    /// variable words, and the prompt itself travels on ssh's standard input.
    #[test]
    fn ctl_input_over_ssh_targets_the_pinned_pane_with_text_on_stdin() {
        use crate::control::Key;
        use crate::tmux::{self, SessionTarget};
        let target = SessionTarget::new("muxel_app_1a2b3c4d", Some(Socket::Private));
        let lead = format!("{}; set -- -L mxl; tmux \"$@\"", tmux_path_prelude());
        assert_eq!(
            session_tmux_command(&target, &tmux::control_pane_args(&target.name)),
            format!(
                "{lead} list-panes -s -t '=muxel_app_1a2b3c4d:' -F \
                 '#{{session_name}}:#{{pane_id}}:#{{?pane_start_command,1,0}}'"
            )
        );
        let buffer = tmux::control_buffer_name(uuid::Uuid::nil());
        assert_eq!(
            session_tmux_command(&target, &tmux::control_paste_args(&buffer, "%12", true)),
            format!(
                "{lead} load-buffer -b {buffer} - ';' paste-buffer -d -p -r -b {buffer} -t %12"
            )
        );
        assert_eq!(
            session_tmux_command(&target, &tmux::delete_buffer_args(&buffer)),
            format!("{lead} delete-buffer -b {buffer}")
        );
        assert_eq!(
            session_tmux_command(&target, &tmux::control_pane_exists_args("%12")),
            format!("{lead} display-message -p -t %12 '#{{pane_id}}'")
        );
        let keys = [Key::Ctrl('b'), Key::Char('c'), Key::Char('\''), Key::Enter];
        assert_eq!(
            session_tmux_command(&target, &tmux::control_keys_args("%12", &keys)),
            format!("{lead} send-keys -t %12 C-b 0x63 0x27 Enter")
        );
        // A recorded Default owner selects the default server the same way.
        let default = SessionTarget::new("muxel_app_1a2b3c4d", Some(Socket::Default));
        assert_eq!(
            session_tmux_command(&default, &tmux::control_keys_args("%3", &[Key::Enter])),
            format!(
                "{}; set --; tmux \"$@\" send-keys -t %3 Enter",
                tmux_path_prelude()
            )
        );
    }

    /// The bug this guards: sshd runs the remote command with its bare default PATH
    /// (`/usr/bin:/bin:/usr/sbin:/sbin`) and no profile, so a Mac's Homebrew tmux —
    /// `/opt/homebrew/bin/tmux` on Apple silicon — is not on it. Every `tmux` word in
    /// the command then dies with `command not found: tmux` and the pane closes,
    /// whether it was going to run a shell or an agent.
    #[test]
    fn remote_tmux_resolves_when_the_ssh_path_is_bare() {
        let h = host();
        for program in [None, Some("claude")] {
            let spec = SshSpec {
                host: &h,
                control_path: "/s",
                remote_cwd: Some("/srv/app"),
                program,
                args: &[],
                use_tmux: true,
                tmux_session: Some("s"),
                tmux_socket: None,
                tmux_client_tag: None,
            };
            let cmd = ssh_args(&spec).last().unwrap().clone();
            let (prelude, rest) = cmd.split_once("; ").expect("a PATH prelude leads");
            assert_eq!(prelude, tmux_path_prelude(), "for program {program:?}");
            // Every tmux invocation is behind it — nothing names tmux before the PATH
            // it will be looked up on is set.
            assert!(!prelude.contains("tmux "), "got: {prelude}");
            assert!(rest.starts_with(&select_tmux_socket("s")), "got: {rest}");
        }
        // The prefix that makes a Homebrew Mac work at all, and the `$HOME` dirs the
        // remote shell (not muxel) must expand.
        assert!(tmux_path_prelude().contains("/opt/homebrew/bin"));
        assert!(tmux_path_prelude().contains("$HOME/.local/bin"));
        // Appended, so a host that already resolves tmux keeps the binary it has.
        assert!(tmux_path_prelude().starts_with("export PATH=\"$PATH:"));
    }

    /// The bug this guards: tmux `execvp`s a program with the *tmux server's*
    /// environment, and on a remote host that server is forked from a
    /// non-interactive ssh command whose PATH is sshd's bare default. An agent in
    /// `~/.local/bin` (or an nvm dir) isn't on it, so a bare `-- claude` dies with
    /// ENOENT: the window closes, the session dies with it, and the pane shows
    /// `[exited]` and a clean exit 0. A shell pane never hits it — tmux runs its own
    /// default shell as a login shell.
    #[test]
    fn tmux_never_execs_an_agent_without_a_login_shell() {
        let h = host();
        let args = ["--model".to_string(), "opus".to_string()];
        let spec = SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: Some("/srv/app"),
            program: Some("claude"),
            args: &args,
            use_tmux: true,
            tmux_session: Some("s"),
            tmux_socket: None,
            tmux_client_tag: None,
        };
        let cmd = ssh_args(&spec).last().unwrap().clone();
        // The last ` -- `: the first is the `set -- -L mxl` choosing the server.
        let (_, after) = cmd
            .rsplit_once(" -- ")
            .expect("a command follows the separator");
        assert!(
            after.starts_with("\"${SHELL:-/bin/sh}\" -ilc "),
            "the agent must go through a login shell, got: {after}"
        );
        // …and the agent still ends up as the pane's own process.
        assert!(after.contains("'exec claude --model opus'"), "got: {after}");
    }

    /// With no program tmux starts its own login shell, so there is nothing to wrap.
    #[test]
    fn a_tmux_shell_pane_is_left_to_tmux() {
        let h = host();
        let spec = SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: Some("/srv/app"),
            program: None,
            args: &[],
            use_tmux: true,
            tmux_session: Some("s"),
            tmux_socket: None,
            tmux_client_tag: None,
        };
        let cmd = ssh_args(&spec).last().unwrap().clone();
        assert!(!cmd.contains("SHELL"), "got: {cmd}");
        assert!(
            cmd.ends_with("new-session -A -s s -c /srv/app"),
            "got: {cmd}"
        );
    }

    /// tmux starts a session whose folder is missing in the home folder instead,
    /// so the folder is created before either attach path can start one.
    #[test]
    fn a_missing_remote_folder_is_created_before_a_tmux_session_starts() {
        let h = host();
        let spec = |remote_cwd| SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd,
            program: Some("claude"),
            args: &[],
            use_tmux: true,
            tmux_session: Some("s"),
            tmux_socket: None,
            tmux_client_tag: Some("ab12"),
        };
        let cmd = ssh_args(&spec(Some("/srv/my app"))).last().unwrap().clone();
        let (before, after) = cmd
            .split_once("mkdir -p -- '/srv/my app'; ")
            .expect("creates the folder");
        assert!(!before.contains("exec "), "{cmd}");
        assert_eq!(after.matches("exec tmux ").count(), 2, "{cmd}");

        let cmd = ssh_args(&spec(None)).last().unwrap().clone();
        assert!(!cmd.contains("mkdir"), "{cmd}");
    }

    /// The server must be forked by its own short-lived `tmux` process, *before* the
    /// `exec`, so the long-lived server never inherits a command line naming the
    /// project. A `pkill -f <project>` on the remote host would otherwise match the
    /// shared server and kill every session on it.
    #[test]
    fn remote_tmux_starts_the_server_before_naming_the_project() {
        let h = host();
        let spec = SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: Some("/srv/webapp"),
            program: Some("claude"),
            args: &[],
            use_tmux: true,
            tmux_session: Some("muxel_webapp_abc123"),
            tmux_socket: Some(Socket::Private),
            tmux_client_tag: None,
        };
        let cmd = ssh_args(&spec).last().unwrap().clone();
        // Only this project-less tmux process may start the owned server.
        let server =
            "; if [ \"$#\" -gt 0 ]; then tmux \"$@\" start-server ';' set -s exit-empty off; fi; ";
        let (before, pane) = cmd.split_once(server).expect("server starts first");
        assert!(
            !before.contains("webapp"),
            "the server's command line must not name the project: {before}"
        );
        // …nor muxel: a `pkill -f muxel` on the host must not match it either.
        assert!(!format!("{before}{server}").contains("muxel"), "{cmd}");
        assert!(pane.contains("muxel_webapp_abc123"));
    }

    /// The bug this guards: attaching makes a client the window's latest, so a pane
    /// reconnecting on an unattended laptop resized the session under whoever was
    /// using it from another machine. A tagged client joining a session that already
    /// has a client attaches with `ignore-size` and records its name; a tmux too old
    /// for client flags attaches as before.
    ///
    /// The flag is conditional because tmux ignores a flagged client whenever an
    /// unflagged one is attached *anywhere* on the server: flagged unconditionally,
    /// a pane alone in its session stopped following its own size as soon as any
    /// other pane on the host was, and the agent drew at the wrong width.
    #[test]
    fn a_tagged_tmux_client_attaches_without_taking_the_window_size() {
        let h = host();
        let spec = |tag| SshSpec {
            host: &h,
            control_path: "/s",
            remote_cwd: Some("/srv/app"),
            program: Some("claude"),
            args: &[],
            use_tmux: true,
            tmux_session: Some("s"),
            tmux_socket: None,
            tmux_client_tag: tag,
        };
        let plain = ssh_args(&spec(None)).last().unwrap().clone();
        let tagged = ssh_args(&spec(Some("ab12"))).last().unwrap().clone();
        let program = "\"${SHELL:-/bin/sh}\" -ilc 'exec claude'";

        // The server still starts first, from a command line naming no session, and
        // the session's server is chosen before anything asks it a question.
        let (server, rest) = tagged
            .split_once("; if tmux \"$@\" list-commands")
            .expect("the flag probe follows");
        assert!(server.contains(&select_tmux_socket("s")));
        assert!(plain.starts_with(&format!("{server}; exec tmux \"$@\" ")));
        // tmux 3.2+ and someone already attached: attach flagged, then record this
        // client's name on the session.
        assert_eq!(
            rest,
            format!(
                " new-session 2>/dev/null | grep -q 'f flags' \
                 && tmux \"$@\" list-clients -t '=s' 2>/dev/null | grep -q .; then \
                 exec tmux \"$@\" -u set -g mouse on ';' set-environment -g -u MUXEL_INSTANCE_ID ';' \
                 new-session -A -s s -c /srv/app \
                 -f ignore-size -- {program} ';' set-option -Fq -t '=s:' \
                 @muxel_client_ab12 '#{{client_name}}'; fi; \
                 exec tmux \"$@\" -u set -g mouse on ';' set-environment -g -u MUXEL_INSTANCE_ID ';' \
                 new-session -A -s s -c /srv/app -- {program}"
            )
        );
        // Older tmux, or a session nobody else is in, falls through to exactly the
        // untagged attach.
        let (_, untagged_exec) = plain.split_once("; exec ").unwrap();
        assert!(tagged.ends_with(&format!("; exec {untagged_exec}")));
    }

    #[test]
    fn claiming_the_window_size_clears_only_this_machines_client() {
        let cmd = claim_window_size_command(&"muxel_app_1".into(), "ab12");
        let (prelude, rest) = cmd.split_once("; ").unwrap();
        assert_eq!(prelude, tmux_path_prelude());
        // On whichever server holds the session.
        let rest = rest
            .strip_prefix(&format!("{}; ", select_tmux_socket("muxel_app_1")))
            .expect("the session's server is chosen first");
        // Its own recorded client, and only while that client is still attached to
        // this session — a tty name outlives the client that held it.
        assert!(rest.starts_with(
            "c=$(tmux \"$@\" show-options -qv -t '=muxel_app_1:' @muxel_client_ab12) \
             && [ -n \"$c\" ]"
        ));
        assert!(rest.contains(
            "tmux \"$@\" list-clients -t '=muxel_app_1' -F '#{client_name}' \
             | grep -qxF -- \"$c\""
        ));
        // Clearing a flag recalculates nothing; re-setting an option does.
        assert!(rest.ends_with(
            "tmux \"$@\" refresh-client -t \"$c\" -f '!ignore-size' ';' \
             set-option -q -t '=muxel_app_1:' @muxel_client_ab12 \"$c\""
        ));
    }

    /// Both attach paths clear the server id before creating a session. Existing
    /// sessions retain their own environment when reattached.
    #[test]
    fn remote_tmux_commands_clear_the_server_instance_id_in_both_attach_paths() {
        use crate::control::INSTANCE_ID_ENV;
        let h = host();
        for (program, tag) in [
            (None, None),
            (Some("claude"), None),
            (None, Some("ab12")),
            (Some("claude"), Some("ab12")),
        ] {
            let spec = SshSpec {
                host: &h,
                control_path: "/s",
                remote_cwd: Some("/srv/app"),
                program,
                args: &[],
                use_tmux: true,
                tmux_session: Some("s"),
                tmux_socket: None,
                tmux_client_tag: tag,
            };
            let cmd = ssh_args(&spec).last().unwrap().clone();
            let clear = format!("set-environment -g -u {INSTANCE_ID_ENV} ';' new-session ");
            let clients: Vec<_> = cmd.split("exec tmux ").skip(1).collect();
            assert_eq!(clients.len(), if tag.is_some() { 2 } else { 1 });
            for client in clients {
                assert!(
                    client.contains(&clear),
                    "for {program:?}/{tag:?}, got: {client}"
                );
                assert!(
                    !client.contains(" -e "),
                    "for {program:?}/{tag:?}, got: {client}"
                );
            }
        }
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
            tmux_socket: None,
            tmux_client_tag: None,
        };
        assert_eq!(
            ssh_args(&spec).last().unwrap(),
            "mkdir -p -- '/srv/my app' && cd '/srv/my app' && \
             exec \"${SHELL:-/bin/sh}\" -ilc 'exec bash'"
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
Add correct host key in /Users/me/.ssh/known_hosts to get rid of this message.
Offending ECDSA key in /Users/me/.ssh/known_hosts:42
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
            Some("/Users/me/.ssh/known_hosts")
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
             Offending key for IP in /Users/me/.ssh/known_hosts:7\n\
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
                "/Users/me/.ssh/known_hosts",
                r"C:\Users\me\.ssh\known_hosts",
            )
            .replace(":42", ":7");
        let c = HostKeyChange::parse(&stderr).expect("classified");
        assert_eq!(
            c.known_hosts_file.as_deref(),
            Some(r"C:\Users\me\.ssh\known_hosts")
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
