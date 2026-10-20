//! Pure helpers for driving tmux. Building command arguments and names only —
//! the binary runs the actual `tmux` process.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

/// The name of muxel's own tmux socket: `tmux -L mxl`.
///
/// On the default socket muxel's sessions shared one server with the user's own
/// tmux and with every tool that runs a bare `tmux …`, and any of them could take
/// that server down with every agent in it. RHEL 10's tmux 3.3a aborts in
/// `capture-pane`, so an outside tool merely *reading* a muxel pane killed every
/// session muxel had. Only a command that names this socket reaches muxel's server.
///
/// Not `muxel`: the server keeps the command line of the client that started it
/// (see [`start_server_args`]), which names this socket, so a socket called `muxel`
/// would let a `pkill -f muxel` — an agent clearing away a dev build of muxel
/// itself — kill the server and every agent in it.
///
/// By hand: `tmux -L mxl ls`, `tmux -L mxl attach -t <session>`.
///
/// Ported to Swift as `TmuxCommands.socketName` — keep both in step.
pub const SOCKET_NAME: &str = "mxl";

/// Which tmux server a command talks to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Socket {
    /// muxel's own server ([`SOCKET_NAME`]). Every session muxel creates goes here.
    Private,
    /// The default server: where muxel kept its sessions before it had its own, and
    /// where the user's own tmux runs. muxel only uses a session here that is already
    /// running and is nowhere else — a pane from an earlier muxel, or a session the
    /// user imported.
    Default,
}

impl From<Socket> for String {
    fn from(socket: Socket) -> Self {
        match socket {
            Socket::Private => SOCKET_NAME.to_string(),
            Socket::Default => "default".to_string(),
        }
    }
}

impl TryFrom<String> for Socket {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        match name.as_str() {
            SOCKET_NAME => Ok(Self::Private),
            "default" => Ok(Self::Default),
            _ => Err("unrecognized tmux socket".to_string()),
        }
    }
}

/// A session name and the server that owns it. `None` is legacy metadata whose
/// location must be resolved once before operating on it; it never means both.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionTarget {
    pub name: String,
    pub socket: Option<Socket>,
}

impl SessionTarget {
    pub fn new(name: impl Into<String>, socket: Option<Socket>) -> Self {
        Self {
            name: name.into(),
            socket,
        }
    }
}

impl std::fmt::Display for SessionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.socket {
            Some(socket) => write!(f, "{} ({})", self.name, String::from(socket)),
            None => f.write_str(&self.name),
        }
    }
}

impl From<&str> for SessionTarget {
    fn from(name: &str) -> Self {
        Self::new(name, None)
    }
}

impl From<String> for SessionTarget {
    fn from(name: String) -> Self {
        Self::new(name, None)
    }
}

impl Socket {
    /// Both servers, muxel's own first: the order a session is looked for in.
    pub const BOTH: [Socket; 2] = [Socket::Private, Socket::Default];

    /// The client flags selecting this server. Like `-u`, they go before the first
    /// command of a `tmux …` argv.
    pub fn args(self) -> &'static [&'static str] {
        match self {
            Socket::Private => &["-L", SOCKET_NAME],
            Socket::Default => &[],
        }
    }

    /// The argv for `tmux …` running `args` on this server.
    pub fn argv(self, args: Vec<String>) -> Vec<String> {
        let mut v: Vec<String> = self.args().iter().map(|a| a.to_string()).collect();
        v.extend(args);
        v
    }

    /// The server holding `session`, given a way to ask a server whether it has it
    /// ([`has_session_args`]): muxel's own, unless the session is running on the
    /// default server and not on muxel's. A session on neither is about to be
    /// created, so it goes on muxel's own too.
    ///
    /// This is the whole migration. A pane muxel started before it had a socket of
    /// its own keeps its session where it is — a live session can't move between
    /// servers, and recreating it would kill the agent inside — until that session
    /// ends; the next one starts on muxel's server.
    pub fn locate(mut has_session: impl FnMut(Socket) -> bool) -> Socket {
        if !has_session(Socket::Private) && has_session(Socket::Default) {
            Socket::Default
        } else {
            Socket::Private
        }
    }

    /// A failed or timed-out probe cannot authorize probing or mutating another
    /// server. Callers retain the error and their original deadline.
    pub fn try_locate<E>(
        mut has_session: impl FnMut(Socket) -> Result<bool, E>,
    ) -> Result<Socket, E> {
        if !has_session(Socket::Private)? && has_session(Socket::Default)? {
            Ok(Socket::Default)
        } else {
            Ok(Socket::Private)
        }
    }
}

/// Arguments for `tmux …` that succeed only while session `session` exists
/// (exact-match `=` target, so `muxel_p_1` never matches `muxel_p_12`).
pub fn has_session_args(session: &str) -> Vec<String> {
    vec![
        "has-session".to_string(),
        "-t".to_string(),
        format!("={session}"),
    ]
}

/// Whether a failed tmux command's error says only that no server is running on
/// its socket — which answers "what sessions are there?" with "none" rather than
/// failing to answer. tmux says `no server running on …` when the socket is stale
/// and `error connecting to …` when it was never made, as muxel's own is on a host
/// that hasn't started it yet.
pub fn no_server(stderr: &str) -> bool {
    stderr.contains("no server running") || stderr.contains("error connecting to")
}

/// A tmux session living on a host, as reported by [`list_sessions_args`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteSession {
    pub name: String,
    pub socket: Socket,
    /// The directory the session was started in. This — not the name — is what
    /// attributes a session to a project: the name's slug is the *project's* when
    /// the session came from a recorded name, but the *host's* when it was derived,
    /// so the name alone can't say which project a session belongs to.
    pub path: String,
    /// What is running in it now (`claude`, `zsh`, …) — enough to re-adopt the
    /// session as the right kind of pane.
    pub command: String,
    /// The pane's terminal title. Agents publish what they are working on here
    /// (`✳ Review changes`), which is the only way to say what a session is *for*
    /// without reading its screen — and reading the screen means `capture-pane`,
    /// which muxel does not do. Empty when tmux reported no title.
    pub title: String,
}

impl RemoteSession {
    pub fn target(&self) -> SessionTarget {
        SessionTarget::new(self.name.clone(), Some(self.socket))
    }
}

/// Record the exact owner before spawning restored panes. Discovery order is
/// the legacy preference, while a known owner never migrates to another server.
pub fn bind_session_owners<'a>(
    slug: &str,
    instances: impl IntoIterator<Item = &'a mut crate::Instance>,
    sessions: &[RemoteSession],
    new_socket: Socket,
) {
    let mut by_name = HashMap::new();
    let mut by_suffix = HashMap::new();
    for session in sessions {
        for socket in [None, Some(session.socket)] {
            by_name
                .entry((socket, session.name.as_str()))
                .or_insert(session);
            if session.name.starts_with(SESSION_PREFIX)
                && let Some((_, suffix)) = session.name.rsplit_once('_')
            {
                by_suffix.entry((socket, suffix)).or_insert(session);
            }
        }
    }
    for instance in instances {
        let suffix = instance.id.simple().to_string();
        let found = instance
            .tmux_session
            .as_deref()
            .and_then(|name| by_name.get(&(instance.tmux_socket, name)))
            .or_else(|| by_suffix.get(&(instance.tmux_socket, &suffix[..8])))
            .copied();
        let name = found
            .map(|session| session.name.clone())
            .unwrap_or_else(|| session_for(instance.tmux_session.as_deref(), slug, instance.id));
        instance.tmux_session = Some(name);
        instance.tmux_socket = instance
            .tmux_socket
            .or_else(|| found.map(|session| session.socket))
            .or(Some(new_socket));
    }
}

/// `tmux …` args listing one line per pane: session, start dir, running command
/// and title.
///
/// `list-panes -a` rather than `list-sessions`, because only a pane knows the
/// command running in it; [`parse_sessions`] keeps the first pane of each session.
///
/// The title is last on the line on purpose: it is the one field whose contents
/// are the agent's to choose, so it is the one that can contain the separator.
pub fn list_sessions_args() -> Vec<String> {
    vec![
        "list-panes".to_string(),
        "-a".to_string(),
        "-F".to_string(),
        "#{session_name}|#{session_path}|#{pane_current_command}|#{pane_title}".to_string(),
    ]
}

/// Parse [`list_sessions_args`] output — one [`RemoteSession`] per session, the
/// first pane winning. Malformed lines are skipped rather than failing the lot: a
/// host is free to have sessions muxel knows nothing about.
pub fn parse_sessions(out: &str) -> Vec<RemoteSession> {
    parse_sessions_on(out, Socket::Private)
}

/// Parse one server's listing, retaining its identity even when another server
/// contains a session with the same name.
pub fn parse_sessions_on(out: &str, socket: Socket) -> Vec<RemoteSession> {
    let mut sessions: Vec<RemoteSession> = Vec::new();
    let mut seen = HashSet::new();
    for line in out.lines() {
        let mut fields = line.splitn(4, '|');
        let (Some(name), Some(path), Some(command)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if name.is_empty() || !seen.insert(name) {
            continue;
        }
        sessions.push(RemoteSession {
            name: name.to_string(),
            socket,
            path: path.to_string(),
            command: command.to_string(),
            // Absent from a line written before the title joined the format, and
            // from any host whose tmux reports no title. Not worth dropping a
            // session over: every other field is still good.
            title: fields.next().unwrap_or_default().to_string(),
        });
    }
    sessions
}

/// Resolve old name-only owners against discovery's preference order, while
/// keeping explicitly owned sessions on the exact server that was attached.
pub fn owned_session_keys<'a>(
    sessions: &[RemoteSession],
    owned: &'a [SessionTarget],
) -> HashSet<(&'a str, Socket)> {
    let mut preferred = HashMap::new();
    for session in sessions {
        preferred
            .entry(session.name.as_str())
            .or_insert(session.socket);
    }
    owned
        .iter()
        .filter_map(|target| {
            target
                .socket
                .or_else(|| preferred.get(target.name.as_str()).copied())
                .map(|socket| (target.name.as_str(), socket))
        })
        .collect()
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
///
/// A session in `closed` is never adopted either: the user closed that pane, and
/// its kill has not been confirmed. Adopting it would undo a deliberate close every
/// time a kill failed to land — the agent would come back, still running, as if
/// muxel had never been told to stop it.
pub fn orphan_sessions(
    sessions: &[RemoteSession],
    project_root: &str,
    owned: &[SessionTarget],
    closed: &[ClosedSession],
) -> Vec<RemoteSession> {
    let owned = owned_session_keys(sessions, owned);
    let closed: HashSet<_> = closed
        .iter()
        .filter_map(|c| live_closed_session(sessions, c))
        .map(|s| (s.name.as_str(), s.socket))
        .collect();
    sessions
        .iter()
        .filter(|s| s.name.starts_with(SESSION_PREFIX))
        .filter(|s| in_tree(&s.path, project_root))
        .filter(|s| !owned.contains(&(s.name.as_str(), s.socket)))
        .filter(|s| !closed.contains(&(s.name.as_str(), s.socket)))
        .cloned()
        .collect()
}

/// Whether `path` is `root` or sits beneath it (a worktree, say).
fn in_tree(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    path == root || path.strip_prefix(root).is_some_and(|r| r.starts_with('/'))
}

/// The `_<uuid8>` tail every muxel session name ends with — the one part of the
/// name that is the same whichever slug the peer that created it used.
fn instance_suffix(instance: Uuid) -> String {
    let id = instance.simple().to_string();
    format!("_{}", &id[..8])
}

/// A tmux session whose pane the user deliberately closed, kept until the host has
/// confirmed the session is actually gone.
///
/// Killing a remote session is one `ssh` round trip that can simply fail — a blip,
/// a control socket that went away with the pane's own ssh, a host that is briefly
/// unreachable. Unremembered, that failure is silent *and* self-reversing: the
/// agent keeps running on the host, and the next connect sees a muxel session no
/// instance owns and adopts it straight back into a pane
/// ([`orphan_sessions`]). Remembering the close is what lets muxel finish the
/// kill later and, until it has, refuse to resurrect the pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosedSession {
    /// The session name the closed pane was launched with.
    pub name: String,
    #[serde(default)]
    pub socket: Option<Socket>,
    /// The closed instance. The name is derived from a slug that can change under
    /// muxel's feet (a renamed host, a session a peer created under its own slug),
    /// so the instance's `_<uuid8>` suffix is the durable half of the match.
    pub instance: Uuid,
}

impl ClosedSession {
    pub fn target(&self) -> SessionTarget {
        SessionTarget::new(self.name.clone(), self.socket)
    }

    /// Bind a legacy close before its first mutation. Keep this result in both
    /// ledgers and across retries, even when the chosen session disappears.
    pub fn resolve_owner(&self, sessions: &[RemoteSession], new_socket: Socket) -> Self {
        let mut resolved = self.clone();
        if let Some(live) = live_closed_session(sessions, self) {
            resolved.name = live.name.clone();
            resolved.socket = Some(live.socket);
        } else {
            resolved.socket = resolved.socket.or(Some(new_socket));
        }
        resolved
    }

    /// Whether `session` is the one this close was meant to reap.
    pub fn matches(&self, session: &RemoteSession) -> bool {
        self.socket.is_none_or(|socket| socket == session.socket)
            && (session.name == self.name
                || (session.name.starts_with(SESSION_PREFIX)
                    && session.name.ends_with(&instance_suffix(self.instance))))
    }
}

/// The session a remembered close still has running on the host, if any.
///
/// `None` means the kill landed (or the session was never there) and the entry can
/// be forgotten. `Some` carries the session under **its own** name, which is what a
/// retry must target — a derived name can have drifted since the pane launched.
pub fn live_closed_session<'a>(
    sessions: &'a [RemoteSession],
    closed: &ClosedSession,
) -> Option<&'a RemoteSession> {
    sessions
        .iter()
        .find(|s| closed.matches(s) && s.name == closed.name)
        .or_else(|| sessions.iter().find(|s| closed.matches(s)))
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

/// The muxel session running `instance`, found by the `_<uuid8>` suffix every muxel
/// session name ends with, whichever slug the peer that created it used (its
/// project name, or its own name for this host). This is how a pane a peer created
/// without recording a session name gets attached to, rather than launched a
/// second time beside it. The iOS app resolves panes by the same suffix.
pub fn session_by_suffix(sessions: &[RemoteSession], instance: Uuid) -> Option<&RemoteSession> {
    let suffix = instance_suffix(instance);
    sessions
        .iter()
        .find(|s| s.name.starts_with(SESSION_PREFIX) && s.name.ends_with(&suffix))
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
    launch_session_args_with_env(session, cwd, program, args, &[], &[])
}

/// [`launch_session_args`], with the program started with `env` set
/// (`new-session -e NAME=VALUE`, which tmux 3.2 added: see
/// [`session_env_supported`]) and without the variables named in `unset`.
///
/// tmux starts a new session's program from its server's environment, not from
/// the environment of the client that asked for the session, so `-e` is the only
/// way to hand the program a variable of its own. The server's environment is
/// whatever the process that started it had, so `unset` is first removed from it
/// (`set-environment -g -u`, which every tmux has, and which every session on that
/// server shares). A session `-A` attaches to instead keeps the environment it
/// started with.
///
/// The values land in the client's command line, where any process can read
/// them, so pass nothing private and no project paths (see [`start_server_args`]).
pub fn launch_session_args_with_env(
    session: &str,
    cwd: Option<&str>,
    program: Option<&str>,
    args: &[String],
    env: &[(String, String)],
    unset: &[&str],
) -> Vec<String> {
    let mut v = vec![
        "-u".to_string(),
        "set".to_string(),
        "-g".to_string(),
        "mouse".to_string(),
        "on".to_string(),
        ";".to_string(),
    ];
    for name in unset {
        v.extend(["set-environment", "-g", "-u", name, ";"].map(String::from));
    }
    let mut new = new_session_args(session, cwd, program, args);
    // With `new-session`'s other options: after `-A -s NAME`, before `--`.
    let options = env
        .iter()
        .flat_map(|(name, value)| ["-e".to_string(), format!("{name}={value}")]);
    new.splice(4..4, options);
    v.extend(new);
    v
}

/// Whether the tmux that printed `version` (`tmux -V`: `tmux 3.4`, `tmux 3.2a`,
/// `tmux next-3.6`, `tmux master`) takes `new-session -e`, added in 3.2. Anything
/// it can't read is taken as no, which only costs the variables.
pub fn session_env_supported(version: &str) -> bool {
    let version = version.trim();
    let version = version.strip_prefix("tmux ").unwrap_or(version).trim();
    if version == "master" {
        return true;
    }
    let version = version.strip_prefix("next-").unwrap_or(version);
    let number = |part: Option<&str>| -> Option<u32> {
        let digits: String = part?.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    let mut parts = version.split('.');
    let Some(major) = number(parts.next()) else {
        return false;
    };
    (major, number(parts.next()).unwrap_or(0)) >= (3, 2)
}

/// Arguments for `tmux …` to read a session's user option (`@name`), printing its
/// value alone — or nothing when it is unset or the session is gone (`-q`).
/// Options take a pane target, so the exact-match session is `=name:`.
pub fn show_option_args(session: &str, option: &str) -> Vec<String> {
    vec![
        "show-options".to_string(),
        "-qv".to_string(),
        "-t".to_string(),
        format!("={session}:"),
        option.to_string(),
    ]
}

/// Arguments for `tmux …` to set a session's user option (`@name`).
pub fn set_option_args(session: &str, option: &str, value: &str) -> Vec<String> {
    vec![
        "set-option".to_string(),
        "-q".to_string(),
        "-t".to_string(),
        format!("={session}:"),
        option.to_string(),
        value.to_string(),
    ]
}

/// Arguments for `tmux …` to kill a session (exact-match `=` target).
pub fn kill_session_args(session: &str) -> Vec<String> {
    vec![
        "kill-session".to_string(),
        "-t".to_string(),
        format!("={session}"),
    ]
}

/// Atomically unset an option only while it matches this send's claim pattern.
/// tmux 2.6+ supports the format match; older versions leave the claim to expire.
pub fn unset_option_if_args(session: &str, option: &str, pattern: &str) -> Option<Vec<String>> {
    let option_ok = option.strip_prefix('@').is_some_and(|name| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    });
    if !option_ok
        || session.contains('\'')
        || !pattern
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "|*-_.".contains(c))
    {
        return None;
    }
    Some(vec![
        "if-shell".into(),
        "-F".into(),
        "-t".into(),
        format!("={session}:"),
        format!("#{{m:{pattern},#{{{option}}}}}"),
        format!("set-option -qu -t '={session}:' {option}"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_cleanup_is_conditional_and_refuses_unsafe_patterns() {
        let args = unset_option_if_args("pane", "@muxel-ctl", "v1|owner|*|*|*|nonce").unwrap();
        assert_eq!(
            args,
            [
                "if-shell",
                "-F",
                "-t",
                "=pane:",
                "#{m:v1|owner|*|*|*|nonce,#{@muxel-ctl}}",
                "set-option -qu -t '=pane:' @muxel-ctl"
            ]
        );
        assert!(unset_option_if_args("it's", "@muxel-ctl", "v1|*").is_none());
        assert!(unset_option_if_args("pane", "@muxel-ctl", "v1|a,b|*").is_none());
        assert!(unset_option_if_args("pane", "@bad option", "v1|*").is_none());
    }

    #[test]
    fn option_args_target_exactly_one_session() {
        assert_eq!(
            show_option_args("muxel_p_1a2b3c4d", "@muxel-ctl"),
            [
                "show-options",
                "-qv",
                "-t",
                "=muxel_p_1a2b3c4d:",
                "@muxel-ctl"
            ]
        );
        assert_eq!(
            set_option_args("muxel_p_1a2b3c4d", "@muxel-ctl", "v1|x"),
            [
                "set-option",
                "-q",
                "-t",
                "=muxel_p_1a2b3c4d:",
                "@muxel-ctl",
                "v1|x"
            ]
        );
    }

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
        // The server's command line names its socket, and must not name muxel
        // either: a `pkill -f muxel` would take every agent down with it.
        let server = Socket::Private.argv(start_server_args()).join(" ");
        assert_eq!(server, "-L mxl start-server ; set -s exit-empty off");
        assert!(!server.contains("muxel"), "{server}");
    }

    #[test]
    fn kill_uses_exact_match_target() {
        assert_eq!(kill_session_args("s"), vec!["kill-session", "-t", "=s"]);
        assert_eq!(has_session_args("s"), vec!["has-session", "-t", "=s"]);
    }

    /// Mirrored by `TmuxCommands.socketName` in the iOS port.
    #[test]
    fn muxels_own_server_is_named_and_the_default_one_is_bare_tmux() {
        assert_eq!(Socket::Private.args(), ["-L", "mxl"]);
        assert!(Socket::Default.args().is_empty());
        assert_eq!(
            Socket::Private.argv(kill_session_args("s")),
            ["-L", "mxl", "kill-session", "-t", "=s"]
        );
        assert_eq!(
            Socket::Default.argv(kill_session_args("s")),
            ["kill-session", "-t", "=s"]
        );
        // The socket is a client flag, so it leads — ahead of `-u` and the first
        // command alike.
        let launch = Socket::Private.argv(launch_session_args("s", None, None, &[]));
        assert_eq!(launch[..3], ["-L", "mxl", "-u"]);
    }

    /// The migration: a session still running on the default server from before
    /// muxel had its own is used where it is; everything else is muxel's own.
    #[test]
    fn a_session_is_found_on_muxels_server_unless_it_runs_only_on_the_default_one() {
        let on = |private: bool, default: bool| {
            Socket::locate(|s| match s {
                Socket::Private => private,
                Socket::Default => default,
            })
        };
        assert_eq!(on(true, false), Socket::Private);
        assert_eq!(
            on(false, true),
            Socket::Default,
            "a pane from before keeps running"
        );
        assert_eq!(
            on(false, false),
            Socket::Private,
            "a new session is muxel's own"
        );
        assert_eq!(on(true, true), Socket::Private, "muxel's own wins a tie");
    }

    /// Asking the default server is a subprocess muxel only spends when its own
    /// server doesn't have the session.
    #[test]
    fn the_default_server_is_asked_only_when_muxels_own_lacks_the_session() {
        let mut asked = Vec::new();
        Socket::locate(|s| {
            asked.push(s);
            true
        });
        assert_eq!(asked, [Socket::Private]);
    }

    #[test]
    fn only_a_missing_server_counts_as_no_sessions() {
        assert!(no_server("no server running on /tmp/tmux-1000/default\n"));
        assert!(no_server(
            "error connecting to /tmp/tmux-1000/mxl (No such file or directory)\n"
        ));
        assert!(!no_server("sh: tmux: command not found\n"));
        assert!(!no_server(""));
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

    #[test]
    fn ownership_survives_duplicate_names_and_workspace_roundtrip() {
        let mut found = parse_sessions_on("work|/p|claude|\n", Socket::Private);
        found.extend(parse_sessions_on("work|/q|zsh|\n", Socket::Default));
        let mut instance = crate::Instance::shell(Uuid::new_v4());
        instance.tmux_session = Some("work".into());
        instance.tmux_socket = Some(Socket::Default);
        bind_session_owners("p", [&mut instance], &found, Socket::Private);
        let saved = serde_json::to_string(&instance).unwrap();
        assert!(saved.contains("\"tmux_socket\":\"default\""));
        let restored: crate::Instance = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.tmux_target("p"), found[1].target());
        // Disappearance on the owned socket never migrates the pane to its namesake.
        bind_session_owners("p", [&mut instance], &found[..1], Socket::Private);
        assert_eq!(instance.tmux_target("p"), found[1].target());
        // A legacy instance binds once, using discovery's documented preference.
        instance.tmux_socket = None;
        bind_session_owners("p", [&mut instance], &found, Socket::Private);
        assert_eq!(instance.tmux_target("p"), found[0].target());
    }

    #[test]
    fn owned_and_closed_sessions_exclude_only_their_socket() {
        let name = "muxel_p_12345678";
        let mut found = parse_sessions_on(&format!("{name}|/p|claude|\n"), Socket::Private);
        found.extend(parse_sessions_on(
            &format!("{name}|/p|zsh|\n"),
            Socket::Default,
        ));
        let owned = [found[0].target()];
        assert_eq!(orphan_sessions(&found, "/p", &owned, &[]), found[1..]);
        let closed = ClosedSession {
            name: name.into(),
            socket: Some(Socket::Private),
            instance: Uuid::nil(),
        };
        assert_eq!(
            orphan_sessions(&found, "/p", &[], std::slice::from_ref(&closed)),
            found[1..]
        );
        assert!(live_closed_session(&found[1..], &closed).is_none());
        let legacy = ClosedSession {
            socket: None,
            ..closed
        };
        assert_eq!(orphan_sessions(&found, "/p", &[], &[legacy]), found[1..]);
    }

    #[test]
    fn imported_non_muxel_sessions_can_be_reaped_by_exact_name() {
        let found = parse_sessions_on("work|/p|claude|\n", Socket::Default);
        let closed = ClosedSession {
            name: "work".into(),
            socket: Some(Socket::Default),
            instance: Uuid::nil(),
        };
        assert_eq!(live_closed_session(&found, &closed), Some(&found[0]));
    }

    #[test]
    fn legacy_close_retains_its_owner_after_a_lost_reply_and_workspace_reload() {
        let mut sessions = parse_sessions_on("work|/p|claude|\n", Socket::Private);
        sessions.extend(parse_sessions_on("work|/p|zsh|\n", Socket::Default));
        let pending = ClosedSession {
            name: "work".into(),
            socket: None,
            instance: Uuid::new_v4(),
        };
        let resolved = pending.resolve_owner(&sessions, Socket::Private);
        let mut project = crate::Project::new("p", "/p");
        project.remember_closed_session(resolved.target(), pending.instance);
        let saved = serde_json::to_string(&project).unwrap();
        let restored: crate::Project = serde_json::from_str(&saved).unwrap();
        // The kill landed on mxl, but its reply was lost. Only default:work remains.
        let retry = restored.closed_sessions[0].resolve_owner(&sessions[1..], Socket::Private);
        assert_eq!(retry.target(), sessions[0].target());
        assert!(live_closed_session(&sessions[1..], &retry).is_none());
        let command = crate::ssh::kill_and_confirm_command(&retry.target());
        assert_eq!(
            command.matches("has-session").count(),
            1,
            "no new owner discovery: {command}"
        );
        assert!(command.contains("; set -- -L mxl;"));
    }

    #[test]
    fn confirmed_legacy_close_does_not_reap_a_namesake_on_reconnect() {
        let name = "muxel_p_12345678";
        let mut sessions = parse_sessions_on(&format!("{name}|/p|claude|\n"), Socket::Private);
        sessions.extend(parse_sessions_on(
            &format!("{name}|/p|zsh|\n"),
            Socket::Default,
        ));
        let legacy = ClosedSession {
            name: name.into(),
            socket: None,
            instance: Uuid::nil(),
        };
        // This resolved record is retained in the in-memory confirmed-close ledger.
        let reaped = legacy.resolve_owner(&sessions, Socket::Private);
        assert!(live_closed_session(&sessions[1..], &reaped).is_none());
        assert_eq!(
            reaped.resolve_owner(&sessions[1..], Socket::Private),
            reaped
        );
        assert_eq!(
            orphan_sessions(&sessions[1..], "/p", &[], &[reaped]),
            sessions[1..]
        );
    }

    #[test]
    fn an_absent_legacy_close_still_remembers_one_owner() {
        let legacy = ClosedSession {
            name: "work".into(),
            socket: None,
            instance: Uuid::nil(),
        };
        let reaped = legacy.resolve_owner(&[], Socket::Private);
        assert_eq!(reaped.socket, Some(Socket::Private));
        let later = parse_sessions_on("work|/p|zsh|\n", Socket::Default);
        assert!(live_closed_session(&later, &reaped).is_none());
        assert_eq!(reaped.resolve_owner(&later, Socket::Private), reaped);
    }

    #[test]
    fn a_legacy_close_prefers_its_exact_name_before_a_uuid_suffix() {
        let id = Uuid::parse_str("12345678-0000-4000-8000-000000000000").unwrap();
        let mut sessions = parse_sessions_on("muxel_old_12345678|/p|claude|\n", Socket::Private);
        sessions.extend(parse_sessions_on(
            "muxel_new_12345678|/p|zsh|\n",
            Socket::Default,
        ));
        let legacy = ClosedSession {
            name: "muxel_new_12345678".into(),
            socket: None,
            instance: id,
        };
        assert_eq!(
            legacy.resolve_owner(&sessions, Socket::Private).target(),
            sessions[1].target()
        );
    }

    /// The whole point: sessions still running in this project that nothing owns.
    #[test]
    fn orphans_are_this_projects_unowned_muxel_sessions() {
        let owned = vec!["muxel_sro_client_d0d464c4".into()];
        let orphans = orphan_sessions(&sessions(), "/home/ryan/Projects/sro_client", &owned, &[]);
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
        let orphans = orphan_sessions(&sessions(), "/home/ryan/Projects/Bot/Manager", &[], &[]);
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
        let orphans = orphan_sessions(&s, "/home/ryan/Projects/app", &[], &[]);
        assert_eq!(
            orphans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["muxel_p_00000001"]
        );
    }

    /// muxel adopts only its own sessions — never the user's own tmux.
    #[test]
    fn a_users_own_session_is_never_adopted() {
        let s = parse_sessions("work|/home/ryan/Projects/app|vim\n");
        assert!(orphan_sessions(&s, "/home/ryan/Projects/app", &[], &[]).is_empty());
    }

    fn closed(name: &str, id: Uuid) -> ClosedSession {
        ClosedSession {
            name: name.to_string(),
            socket: None,
            instance: id,
        }
    }

    /// The bug this exists for: a remote close whose kill didn't land left the
    /// session running, and the next connect adopted it straight back into a pane.
    #[test]
    fn a_session_whose_close_is_unconfirmed_is_never_adopted_back() {
        let id = Uuid::parse_str("90f9def0-0000-4000-8000-000000000000").unwrap();
        let c = [closed("muxel_sro_client_90f9def0", id)];
        let orphans = orphan_sessions(&sessions(), "/home/ryan/Projects/sro_client", &[], &c);
        assert_eq!(
            orphans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["muxel_sro_client_d0d464c4"],
            "only the session nobody closed comes back"
        );
    }

    /// The recorded name is derived from a slug that can drift (a renamed host, a
    /// session a peer created under its own slug). The instance suffix still pins it.
    #[test]
    fn a_close_matches_its_session_under_a_different_slug() {
        let id = Uuid::parse_str("ae15cabf-0000-4000-8000-000000000000").unwrap();
        let c = closed("muxel_oldhostname_ae15cabf", id);
        assert_eq!(
            live_closed_session(&sessions(), &c).map(|s| s.name.as_str()),
            Some("muxel_rhel_ae15cabf"),
            "a retry must target the name the session actually has"
        );
        assert!(
            orphan_sessions(&sessions(), "/home/ryan/Projects/Bot/Manager", &[], &[c]).is_empty()
        );
    }

    /// Once the host no longer lists it, the close is finished and forgettable.
    #[test]
    fn a_close_with_no_session_left_running_is_done() {
        let id = Uuid::parse_str("11111111-0000-4000-8000-000000000000").unwrap();
        assert!(live_closed_session(&sessions(), &closed("muxel_gone_11111111", id)).is_none());
    }

    /// Never reap a session that isn't muxel's, however the name lines up.
    #[test]
    fn a_close_never_matches_a_users_own_session() {
        let id = Uuid::parse_str("00000001-0000-4000-8000-000000000000").unwrap();
        let s = parse_sessions("work_00000001|/home/ryan|vim\n");
        assert!(live_closed_session(&s, &closed("muxel_p_00000001", id)).is_none());
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
    fn a_new_sessions_program_is_handed_its_variables_with_its_options() {
        let env = [("MUXEL_INSTANCE_ID".to_string(), "1a2b".to_string())];
        let unset = ["MUXEL_INSTANCE_ID", "OTHER"];
        let args = launch_session_args_with_env("s", Some("/w"), Some("claude"), &[], &env, &unset);
        assert_eq!(
            args[6..],
            [
                // Off the server first, in the same client's command list.
                "set-environment",
                "-g",
                "-u",
                "MUXEL_INSTANCE_ID",
                ";",
                "set-environment",
                "-g",
                "-u",
                "OTHER",
                ";",
                "new-session",
                "-A",
                "-s",
                "s",
                "-e",
                "MUXEL_INSTANCE_ID=1a2b",
                "-c",
                "/w",
                "--",
                "claude"
            ]
        );
        // Without variables it is exactly what it was.
        assert_eq!(
            launch_session_args_with_env("s", None, None, &[], &[], &[]),
            launch_session_args("s", None, None, &[])
        );
    }

    #[test]
    fn session_variables_need_tmux_3_2() {
        for yes in [
            "tmux 3.2",
            "tmux 3.2a",
            "tmux 3.4\n",
            "tmux 3.10",
            "tmux 4.0",
            "tmux next-3.6",
            "tmux master",
        ] {
            assert!(session_env_supported(yes), "{yes}");
        }
        for no in [
            "tmux 3.1c",
            "tmux 3.0a",
            "tmux 2.7",
            "tmux openbsd-7.4",
            "",
            "tmux",
        ] {
            assert!(!session_env_supported(no), "{no}");
        }
    }

    #[test]
    fn utf8_flag_leads_because_tmux_client_options_precede_the_command() {
        // `tmux set -u …` would be parsed as an argument to `set` and fail; the
        // client flag has to come before the first command.
        let args = launch_session_args("s", None, Some("claude"), &[]);
        assert_eq!(args.first().map(String::as_str), Some("-u"));
    }

    #[test]
    fn a_panes_title_is_parsed_and_may_contain_the_separator() {
        let out = "\
s1|/work|claude|✳ Review changes
s2|/work|claude|a|title|with|pipes
s3|/work|zsh
";
        let got = parse_sessions(out);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].title, "✳ Review changes");
        assert_eq!(
            got[1].title, "a|title|with|pipes",
            "the title is last precisely so it may contain the separator"
        );
        assert_eq!(
            got[2].title, "",
            "a line without a title is still a session"
        );
        assert_eq!(got[2].command, "zsh");
    }

    #[test]
    fn session_by_suffix_finds_a_peer_session_whatever_its_slug() {
        let id = Uuid::parse_str("1a2b3c4d-0000-4000-8000-000000000000").unwrap();
        let session = |name: &str| RemoteSession {
            name: name.into(),
            socket: Socket::Private,
            path: "/work".into(),
            command: "claude".into(),
            title: String::new(),
        };
        // A remote desktop names the session after its host, not the project.
        let sessions = vec![
            session("muxel_proj_99999999"),
            session("muxel_studio_1a2b3c4d"),
        ];
        assert_eq!(
            session_by_suffix(&sessions, id).map(|s| s.name.as_str()),
            Some("muxel_studio_1a2b3c4d")
        );
        // Only muxel's own sessions, and only a whole-id suffix, count.
        let others = vec![session("work_1a2b3c4d"), session("muxel_p_x1a2b3c4d")];
        assert!(session_by_suffix(&others, id).is_none());
    }
}
