//! Pure helpers for driving tmux. Building command arguments and names only —
//! the binary runs the actual `tmux` process.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Bind peer terminal metadata without converting a literal command into a tmux
/// command. Literal panes remain stopped until their explicit Run again action.
pub fn bind_peer_sessions(
    project: &str,
    instances: &mut [crate::Instance],
    sessions: &[RemoteSession],
    new_socket: Socket,
) {
    for instance in instances.iter_mut().filter(|i| i.needs_peer_session()) {
        instance.use_tmux = true;
    }
    bind_session_owners(
        project,
        instances.iter_mut().filter(|i| i.needs_peer_session()),
        sessions,
        new_socket,
    );
}

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
/// session muxel had. From outside muxel, only a command that names this socket
/// reaches muxel's server. Inside a pane, a bare `tmux …` reaches it too, because
/// tmux sets `TMUX` in every pane to that pane's own server (see
/// [`SERVER_MARK_ENV`]).
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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Socket {
    /// muxel's own server ([`SOCKET_NAME`]). Every session muxel creates goes here.
    Private,
    /// The default server: where muxel kept its sessions before it had its own, and
    /// where the user's own tmux runs. muxel only uses a session here that is already
    /// running and is nowhere else — a pane from an earlier muxel, or a session the
    /// user imported.
    Default,
    /// An isolated profile's server, locally or over SSH. No legacy fallback.
    Isolated(u64),
    /// A recorded server supplied by another muxel version or profile. Keep its
    /// identity intact; an unfamiliar owner is never legacy discovery metadata.
    Named(String),
}

impl From<Socket> for String {
    fn from(socket: Socket) -> Self {
        match socket {
            Socket::Private => SOCKET_NAME.to_string(),
            Socket::Default => "default".to_string(),
            Socket::Isolated(hash) => format!("mxl-{hash:016x}"),
            Socket::Named(name) => name,
        }
    }
}

impl TryFrom<String> for Socket {
    type Error = String;

    /// Any name tmux can use as `-L <name>`. A name that isn't a plain file name
    /// (empty, `.`, `..`, or holding a `/` or NUL) is refused: `-L` joins it onto
    /// tmux's socket directory, so a path in it would reach a server outside that
    /// directory. A recorded owner like that reads as no owner
    /// ([`recorded_socket`]). Ported to Swift as `TmuxCommands.isSocketName`.
    fn try_from(name: String) -> Result<Self, Self::Error> {
        match name.as_str() {
            SOCKET_NAME => Ok(Self::Private),
            "default" => Ok(Self::Default),
            "" | "." | ".." => Err("not a tmux socket name".to_string()),
            _ if name.contains(['/', '\0']) => Err("not a tmux socket name".to_string()),
            _ => name
                .strip_prefix("mxl-")
                .filter(|hash| {
                    hash.len() == 16
                        && hash
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
                .and_then(|hash| u64::from_str_radix(hash, 16).ok())
                .map(Self::Isolated)
                .map(Ok)
                .unwrap_or_else(|| Ok(Self::Named(name))),
        }
    }
}

/// `deserialize_with` for a recorded owner (`Option<Socket>`). A name that isn't
/// a socket name ([`Socket::try_from`]) reads as no owner, so its pane goes
/// through legacy discovery instead of failing the whole document. Ported to
/// Swift in `Instance`'s decoder.
pub fn recorded_socket<'de, D>(deserializer: D) -> Result<Option<Socket>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.and_then(|name| Socket::try_from(name).ok()))
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

    /// An explicit profile root always owns its own socket, regardless of older
    /// or peer metadata. The regular profile retains its recorded owner.
    pub fn for_profile(&self, root: Option<&std::path::Path>) -> Self {
        Self::new(
            self.name.clone(),
            root.filter(|root| !root.as_os_str().is_empty())
                .map(Socket::isolated)
                .or_else(|| self.socket.clone()),
        )
    }

    /// A recorded default owner can move only after both its session and an
    /// exact-name private collision have been confirmed absent. The caller's
    /// probe must check this target's exact name on the supplied server.
    /// `None` means an absent Default has a Private namesake: stop and refresh
    /// saved ownership rather than creating on Default or adopting that name.
    pub fn for_respawn<E>(
        &self,
        mut probe: impl FnMut(&Socket) -> Result<bool, E>,
    ) -> Result<Option<Self>, E> {
        if self.socket != Some(Socket::Default) || probe(&Socket::Default)? {
            return Ok(Some(self.clone()));
        }
        if probe(&Socket::Private)? {
            return Ok(None);
        }
        Ok(Some(Self::new(self.name.clone(), Some(Socket::Private))))
    }
}

impl std::fmt::Display for SessionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.socket {
            Some(socket) => write!(f, "{} ({})", self.name, socket.name()),
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

    /// The persisted socket name, including unfamiliar names exactly as received.
    pub fn name(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Private => SOCKET_NAME.into(),
            Self::Default => "default".into(),
            Self::Isolated(hash) => format!("mxl-{hash:016x}").into(),
            Self::Named(name) => name.as_str().into(),
        }
    }

    /// The client flags selecting this server. Like `-u`, they go before the first
    /// command of a `tmux …` argv.
    pub fn args(&self) -> Vec<String> {
        match self {
            Socket::Private => vec!["-L".into(), SOCKET_NAME.into()],
            Socket::Default => vec![],
            Socket::Isolated(hash) => vec!["-L".into(), format!("mxl-{hash:016x}")],
            Socket::Named(name) => vec!["-L".into(), name.clone()],
        }
    }

    /// Stable per-root name without putting the profile path in process argv.
    pub fn isolated(root: &std::path::Path) -> Self {
        // FNV-1a is specified here rather than DefaultHasher, whose output may
        // change with Rust releases and strand existing sessions after upgrades.
        let hash = root
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        Self::Isolated(hash)
    }

    /// Servers this profile's discovery, existence checks and cleanup may touch,
    /// on this machine or its SSH hosts.
    pub fn local(root: Option<&std::path::Path>) -> Vec<Self> {
        match root.filter(|root| !root.as_os_str().is_empty()) {
            Some(root) => vec![Self::isolated(root)],
            None => Self::BOTH.to_vec(),
        }
    }

    /// Isolated profiles do not probe either real-profile socket, even when the
    /// requested session is absent. Regular profiles retain legacy reattachment.
    pub fn locate_local(
        root: Option<&std::path::Path>,
        has_session: impl FnMut(Self) -> bool,
    ) -> Self {
        match root.filter(|root| !root.as_os_str().is_empty()) {
            Some(root) => Self::isolated(root),
            None => Self::locate(has_session),
        }
    }

    /// The argv for `tmux …` running `args` on this server.
    pub fn argv(&self, args: Vec<String>) -> Vec<String> {
        let mut v = self.args();
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

/// Whether a failed (exit 1) [`has_session_args`] said the session isn't there:
/// the server has no session of that name, has no sessions at all (tmux then
/// finds no "current target" before it gets to the name; muxel's own server
/// keeps running empty, `exit-empty off`), or isn't running
/// ([`confirmed_no_server`]). Any other failure is no answer.
pub fn has_session_absent(stderr: &str) -> bool {
    let message = stderr.trim();
    message.starts_with("can't find session:")
        || message == "no current target"
        || confirmed_no_server(message)
}

/// Only a definite absent server can authorize owner migration. In particular,
/// permission failures and incomplete listings are not evidence of absence.
pub fn confirmed_no_server(stderr: &str) -> bool {
    let message = stderr.trim();
    if message.lines().count() != 1 {
        return false;
    }
    if let Some(path) = message.strip_prefix("no server running on ") {
        return !path.is_empty();
    }
    let Some(error) = message.strip_prefix("error connecting to ") else {
        return false;
    };
    [" (No such file or directory)", " (Connection refused)"]
        .iter()
        .any(|suffix| {
            error
                .strip_suffix(suffix)
                .is_some_and(|path| !path.is_empty())
        })
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
        SessionTarget::new(self.name.clone(), Some(self.socket.clone()))
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
        for socket in [None, Some(&session.socket)] {
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
        if matches!(new_socket, Socket::Isolated(_)) {
            instance.tmux_socket = Some(new_socket.clone());
        }
        let suffix = instance.id.simple().to_string();
        let found = instance
            .tmux_session
            .as_deref()
            .and_then(|name| by_name.get(&(instance.tmux_socket.as_ref(), name)))
            .or_else(|| by_suffix.get(&(instance.tmux_socket.as_ref(), &suffix[..8])))
            .copied();
        let name = found
            .map(|session| session.name.clone())
            .unwrap_or_else(|| session_for(instance.tmux_session.as_deref(), slug, instance.id));
        instance.tmux_session = Some(name);
        instance.tmux_socket.get_or_insert_with(|| {
            found
                .map(|session| session.socket.clone())
                .unwrap_or_else(|| new_socket.clone())
        });
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
            socket: socket.clone(),
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
///
/// An owner whose recorded server isn't running its session resolves by name too.
/// That record is stale, not a claim on nothing: a restart moved the session off
/// Default after this copy of the pane was saved (a peer's layout, or this
/// machine's from before it was away). Read literally, it left the pane's own
/// running session looking unowned, and a connect adopted it into a second pane.
pub fn owned_session_keys<'a>(
    sessions: &'a [RemoteSession],
    owned: &'a [SessionTarget],
) -> HashSet<(&'a str, &'a Socket)> {
    let mut preferred = HashMap::new();
    for session in sessions {
        preferred
            .entry(session.name.as_str())
            .or_insert(&session.socket);
    }
    let listed = |name: &str, socket: &Socket| {
        sessions
            .iter()
            .any(|s| s.name == name && s.socket == *socket)
    };
    owned
        .iter()
        .filter_map(|target| {
            target
                .socket
                .as_ref()
                .filter(|socket| listed(&target.name, socket))
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
        .map(|s| (s.name.as_str(), &s.socket))
        .collect();
    sessions
        .iter()
        .filter(|s| s.name.starts_with(SESSION_PREFIX))
        .filter(|s| in_tree(&s.path, project_root))
        .filter(|s| !owned.contains(&(s.name.as_str(), &s.socket)))
        .filter(|s| !closed.contains(&(s.name.as_str(), &s.socket)))
        .cloned()
        .collect()
}

/// Whether closing a pane must leave its session `closed` running, because another
/// pane on the same host still shows it. `others` is the session of every other
/// pane there.
///
/// One session can be in two panes: one adopted beside a pane whose record had
/// gone stale, or an import. Closing either used to kill the session, and the
/// agent the other pane was still showing went with it. The last pane to close
/// is the one that stops it.
///
/// Matched by name alone, since a record of the server can be stale (see
/// [`owned_session_keys`]). Session names carry the pane's own id, so panes with
/// the same name are showing the same session.
pub fn session_still_shown(closed: &SessionTarget, others: &[SessionTarget]) -> bool {
    others.iter().any(|other| other.name == closed.name)
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
    #[serde(default, deserialize_with = "recorded_socket")]
    pub socket: Option<Socket>,
    /// The closed instance. The name is derived from a slug that can change under
    /// muxel's feet (a renamed host, a session a peer created under its own slug),
    /// so the instance's `_<uuid8>` suffix is the durable half of the match.
    pub instance: Uuid,
}

impl ClosedSession {
    pub fn target(&self) -> SessionTarget {
        SessionTarget::new(self.name.clone(), self.socket.clone())
    }

    /// Bind a legacy close before its first mutation. Keep this result in both
    /// ledgers and across retries, even when the chosen session disappears.
    pub fn resolve_owner(&self, sessions: &[RemoteSession], new_socket: Socket) -> Self {
        let mut resolved = self.clone();
        if matches!(new_socket, Socket::Isolated(_)) {
            resolved.socket = Some(new_socket.clone());
        }
        if let Some(live) = live_closed_session(sessions, &resolved) {
            resolved.name = live.name.clone();
            resolved.socket = Some(live.socket.clone());
        } else {
            resolved.socket = resolved.socket.or(Some(new_socket));
        }
        resolved
    }

    /// A missing row confirms absence only for a server the inventory queried.
    /// Unqueried recorded owners need a direct, owner-pinned kill/confirmation.
    pub fn retry_after_inventory(
        &self,
        sessions: &[RemoteSession],
        listed_sockets: &[Socket],
    ) -> Option<SessionTarget> {
        if let Some(live) = live_closed_session(sessions, self) {
            return Some(live.target());
        }
        match self.socket.as_ref() {
            Some(socket) if listed_sockets.contains(socket) => None,
            _ => Some(self.target()),
        }
    }

    /// Whether `session` is the one this close was meant to reap.
    pub fn matches(&self, session: &RemoteSession) -> bool {
        self.socket
            .as_ref()
            .is_none_or(|socket| socket == &session.socket)
            && (session.name == self.name
                || (session.name.starts_with(SESSION_PREFIX)
                    && session.name.ends_with(&instance_suffix(self.instance))))
    }
}

/// The session a remembered close still has running on the host, if any.
///
/// `None` means only that these rows contain no match; it does not prove the
/// recorded server was queried. Reconnect uses [`ClosedSession::retry_after_inventory`]
/// before forgetting a close. `Some` carries the session under its own name,
/// which may have drifted since the pane launched.
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
    match recorded.filter(|s| !s.is_empty()) {
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

/// Set on the client that runs [`start_server_args`] locally, so muxel can tell its
/// own server from one that something else started on the same socket.
///
/// tmux copies the environment of the client that *starts* a server into that
/// server's global environment, and never takes a later client's. So the marker is
/// there only if muxel's own `start-server` created the server. A process inside a
/// pane reaches muxel's server with a bare `tmux` (tmux points every pane at its
/// server through `TMUX`), and one that kills the server and then runs `tmux
/// new-session` starts a new one with its own environment: its `HOME`, its
/// `GIT_EDITOR`, its agent's variables. Every pane muxel opened there would start
/// from that environment. [`server_origin_args`] then flags the marker removed, so
/// no pane inherits it and hands it on to a server muxel didn't start.
///
/// An environment variable, not part of the command line: the server keeps the
/// command line of the client that started it, and that must not name muxel (see
/// [`start_server_args`]).
pub const SERVER_MARK_ENV: &str = "MUXEL_TMUX_SERVER";

/// Arguments for `tmux …` that print [`SERVER_MARK_ENV`] (read it with
/// [`ServerOrigin::from_probe`]) and then flag it removed, run right after
/// [`start_server_args`]. On a server muxel didn't start there is no marker to
/// print, and tmux skips the rest of a command sequence after a failed command, so
/// that server stays unmarked until [`reseed_environment_script`] has given it
/// muxel's environment.
pub fn server_origin_args() -> Vec<String> {
    [
        "show-environment",
        "-g",
        SERVER_MARK_ENV,
        ";",
        "set-environment",
        "-g",
        "-r",
        SERVER_MARK_ENV,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Arguments for `tmux …` that mark a server as muxel's without touching the rest
/// of its environment. For a tmux too old to run [`reseed_environment_script`]:
/// trying again on the same server would only fail again on every pane.
pub fn mark_server_args() -> Vec<String> {
    ["set-environment", "-g", "-r", SERVER_MARK_ENV]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Who started a tmux server, as [`server_origin_args`] reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerOrigin {
    /// muxel's `start-server` just started it.
    Started,
    /// muxel started it before, or has since given it muxel's environment.
    Muxel,
    /// Something else started it, or an earlier muxel that didn't mark its server.
    /// Its global environment is whatever that process had.
    Foreign,
}

impl ServerOrigin {
    /// Read [`server_origin_args`]' output: `NAME=1` while the marker is fresh,
    /// `-NAME` once it is flagged removed, and nothing (just an error) on a server
    /// muxel didn't start.
    pub fn from_probe(stdout: &[u8]) -> Self {
        let removed = format!("-{SERVER_MARK_ENV}");
        let set = format!("{SERVER_MARK_ENV}=");
        let mut lines = stdout.split(|&b| b == b'\n');
        if lines.clone().any(|line| line == removed.as_bytes()) {
            ServerOrigin::Muxel
        } else if lines.any(|line| line.starts_with(set.as_bytes())) {
            ServerOrigin::Started
        } else {
            ServerOrigin::Foreign
        }
    }
}

/// The variable names in `tmux show-environment -g` output (`NAME=value` lines),
/// leaving out the ones flagged removed (`-NAME`). A value containing a newline
/// carries on onto the next line, which can then look like a variable of its own.
/// That costs nothing in [`reseed_environment_script`]: unsetting a variable that
/// isn't there does nothing.
pub fn environment_names(show_environment: &[u8]) -> Vec<&[u8]> {
    show_environment
        .split(|&b| b == b'\n')
        .filter(|line| !line.starts_with(b"-"))
        .filter_map(|line| line.iter().position(|&b| b == b'=').map(|eq| &line[..eq]))
        .filter(|name| !name.is_empty())
        .collect()
}

/// A script for `tmux source-file -` that makes a server's global environment
/// `wanted`, the environment muxel would have started that server with. It unsets
/// every variable in `current` (see [`environment_names`]) that `wanted` lacks, sets
/// everything in `wanted`, and last marks the server as muxel's
/// ([`SERVER_MARK_ENV`]). `PWD` is left as it is, because tmux sets it itself when
/// it starts.
///
/// It goes in through stdin because these values must not appear in a command line,
/// where any process can read them. Each name and value is a double-quoted string
/// with every byte outside a small safe set written as an octal escape, so nothing
/// in it can expand (`$`, `~`), end the string, or start another command, and a
/// value that isn't UTF-8 arrives byte for byte. Octal escapes need tmux 3.0 and a
/// script on stdin needs 3.1. Older tmux runs none of the script (see
/// [`mark_server_args`]).
///
/// Variables a `tmux.conf` sets on the server can't be told apart from the starting
/// process's, so the script replaces them too. This only happens on a server muxel
/// didn't start, and they come back the next time that server starts.
pub fn reseed_environment_script(current: &[&[u8]], wanted: &[(&[u8], &[u8])]) -> String {
    use std::fmt::Write as _;
    let mut script = String::new();
    for name in environment_extras(current, wanted) {
        let _ = writeln!(script, "set-environment -g -u -- {}", tmux_string(name));
    }
    for &(name, value) in wanted {
        if !reseed_skips(name) {
            let _ = writeln!(
                script,
                "set-environment -g -- {} {}",
                tmux_string(name),
                tmux_string(value)
            );
        }
    }
    let _ = writeln!(script, "set-environment -g -r -- {SERVER_MARK_ENV}");
    script
}

/// The variables [`reseed_environment_script`] unsets: those in `current` that
/// `wanted` lacks, once each, in order.
pub fn environment_extras<'a>(current: &[&'a [u8]], wanted: &[(&[u8], &[u8])]) -> Vec<&'a [u8]> {
    let keep: HashSet<&[u8]> = wanted.iter().map(|(name, _)| *name).collect();
    let mut seen = HashSet::new();
    current
        .iter()
        .copied()
        .filter(|name| !reseed_skips(name) && !keep.contains(name) && seen.insert(*name))
        .collect()
}

fn reseed_skips(name: &[u8]) -> bool {
    name.is_empty() || name == b"PWD" || name == SERVER_MARK_ENV.as_bytes()
}

/// `bytes` as a double-quoted string in tmux's command syntax, with every byte
/// outside `[A-Za-z0-9_./:,+@=-]` as a three-digit octal escape (tmux 3.0+).
fn tmux_string(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"_./:,+@=-".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "\\{b:03o}");
        }
    }
    out.push('"');
    out
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
    let options = env.iter().flat_map(|(name, value)| {
        let mut setting = format!("{name}={value}");
        // tmux's argv parser treats a trailing semicolon as a command separator,
        // even inside a single argument. Escape it for tmux, which removes this
        // one backslash before setting the session environment.
        if setting.ends_with(';') {
            setting.insert(setting.len() - 1, '\\');
        }
        ["-e".to_string(), setting]
    });
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

/// Arguments for `tmux …` printing the title of a session's active pane — what
/// the program in it last set with OSC 0/2 — which a tmux client never forwards
/// to its own terminal unless `set-titles` is on. `=name:` is the exact-match
/// session's current window, so `muxel_p_1` never reads `muxel_p_12`. Read the
/// output with [`parse_pane_title`]. Reading the title never needs, and must
/// never use, `capture-pane`.
pub fn pane_title_args(session: &str) -> Vec<String> {
    vec![
        "display-message".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        format!("={session}:"),
        "#{session_name}:#{pane_title}".to_string(),
    ]
}

/// The title in [`pane_title_args`]' output, only when it is `session`'s own.
/// `display-message` may not fail on a target it can't find: it formats with
/// no target instead and succeeds, so the output names its session, and tmux
/// session names cannot contain `:`.
pub fn parse_pane_title(session: &str, out: &str) -> Option<String> {
    let line = out.strip_suffix('\n').unwrap_or(out);
    let line = line.strip_suffix('\r').unwrap_or(line);
    line.strip_prefix(session)?
        .strip_prefix(':')
        .map(str::to_string)
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

// --- Outside-control input through tmux ---------------------------------------------
//
// `muxel ctl` input for a pane behind tmux goes to its tmux pane through tmux
// commands, never through the attached client's input. The client decodes what it
// reads with its key tables (the prefix, `prefix2`, `bind -n`, extended keys), so
// bytes written there can run tmux commands; `paste-buffer` and `send-keys`
// without `-K` reach the pane directly.
//
// Caller text never appears in a tmux argument: an argument ending in `;` splits
// the command list (tmux 2.x `cmd_list_parse`, 3.x `cmd_parse_from_arguments`).
// Prompt text goes on standard input to `load-buffer -`, and a typed character
// goes as its code point (`0x3b`), or, outside ASCII, on standard input to a raw
// paste. Every other argument is a fixed word, a pane id (`%12`) or a buffer name
// muxel made. Version floors, from tmux's CHANGES and its 2.0 source:
// `display-message -p` 1.2, `load-buffer -` 1.3, `paste-buffer -p` and hex keys
// 1.7, `#{?…}` conditionals 1.8, named buffers and `#{pane_start_command}` 2.0,
// the `=` exact-session target 2.1. A hex key above 0x7f is a plain character
// only from 2.4, which is why those characters are pasted.

/// Arguments for `tmux …` listing every pane of `session` (all its windows) as
/// `session:%id:F`, where `F` is 1 when the pane was started with a command of
/// its own and 0 when it runs the default shell. Read it with
/// [`parse_control_pane`].
pub fn control_pane_args(session: &str) -> Vec<String> {
    vec![
        "list-panes".to_string(),
        "-s".to_string(),
        "-t".to_string(),
        format!("={session}:"),
        "-F".to_string(),
        "#{session_name}:#{pane_id}:#{?pane_start_command,1,0}".to_string(),
    ]
}

/// The pane ctl input for `session` goes to: the agent's own pane, which is the
/// pane the session was created with. tmux numbers panes in creation order and
/// never reuses an id on a live server, so that is the session's lowest pane id
/// for as long as it lives, whichever pane or window the user has made active.
///
/// A session muxel created with a program (`with_command`) started that pane
/// with a command. If its lowest pane has none, the agent's pane has gone and a
/// pane the user opened is left, so there is no pane to type into. Lines naming
/// another session, and anything that isn't a pane id, are ignored.
pub fn parse_control_pane(session: &str, out: &str, with_command: bool) -> Option<String> {
    let (id, started) = out
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix(session)?.strip_prefix(":%")?;
            let (digits, started) = rest.split_once(':')?;
            let started = match started.trim_end_matches('\r') {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            if !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((digits.parse::<u64>().ok()?, started))
        })
        .min_by_key(|(id, _)| *id)?;
    (started || !with_command).then(|| format!("%{id}"))
}

/// The paste buffer one ctl paste uses: its own, so two pastes never mix and
/// none touches the user's buffers.
pub fn control_buffer_name(nonce: Uuid) -> String {
    format!("muxel-ctl-{}", nonce.simple())
}

/// Arguments for `tmux …` that read text from standard input into `buffer` and
/// paste it into `pane` as one paste, byte for byte: `-r` stops `paste-buffer`
/// rewriting line breaks, so the caller decides them ([`control_paste_input`]).
/// With `bracket`, `-p` brackets the paste when the pane's program asked for
/// bracketed paste; without it, the bytes arrive as plain input. `-d` deletes
/// the buffer once it is pasted. A failed paste leaves the buffer: delete it
/// with [`delete_buffer_args`].
pub fn control_paste_args(buffer: &str, pane: &str, bracket: bool) -> Vec<String> {
    let mut args = vec![
        "load-buffer".to_string(),
        "-b".to_string(),
        buffer.to_string(),
        "-".to_string(),
        ";".to_string(),
        "paste-buffer".to_string(),
        "-d".to_string(),
    ];
    if bracket {
        args.push("-p".to_string());
    }
    args.extend(
        ["-r", "-b", buffer, "-t", pane]
            .iter()
            .map(|arg| (*arg).to_string()),
    );
    args
}

/// The bytes a ctl prompt is pasted as, the same whether or not the pane asked
/// for bracketed paste, since muxel can't see that pane's mode (it sees only the
/// mode of the pane the client shows). They are what tmux's own `paste-buffer
/// -p` sends: each line break as CR, which a program outside bracketed paste
/// reads as Enter, as typed, and one inside reads as a line break. ESC is
/// removed, so the text can't end a bracketed paste early (`\e[201~`) and have
/// the rest read as keys, and can't press keys in a pane outside one.
pub fn control_paste_input(text: &str) -> Vec<u8> {
    text.replace('\x1b', "")
        .replace("\r\n", "\r")
        .replace('\n', "\r")
        .into_bytes()
}

/// Arguments for `tmux …` deleting one of muxel's paste buffers.
pub fn delete_buffer_args(buffer: &str) -> Vec<String> {
    vec![
        "delete-buffer".to_string(),
        "-b".to_string(),
        buffer.to_string(),
    ]
}

/// Arguments for `tmux …` printing `pane`'s own id while it exists. Asked after
/// input to it was refused, to tell a pane that has gone from one that refused;
/// a pane that's gone gets an error, or an empty line.
pub fn control_pane_exists_args(pane: &str) -> Vec<String> {
    vec![
        "display-message".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        pane.to_string(),
        "#{pane_id}".to_string(),
    ]
}

/// The tmux key a ctl key presses. A character goes as its code point, which
/// tmux sends as that character (hex keys since tmux 1.7). Only an ASCII
/// character is that portable: tmux 2.1 to 2.3 read a code point above 0x7f as
/// a key with modifier bits, so [`control_key_steps`] pastes those instead.
pub fn key_name(key: crate::control::Key) -> String {
    use crate::control::Key;
    match key {
        Key::Enter => "Enter".into(),
        Key::Escape => "Escape".into(),
        Key::Tab => "Tab".into(),
        Key::BackTab => "BTab".into(),
        Key::Up => "Up".into(),
        Key::Down => "Down".into(),
        Key::Right => "Right".into(),
        Key::Left => "Left".into(),
        Key::Home => "Home".into(),
        Key::End => "End".into(),
        Key::Backspace => "BSpace".into(),
        Key::Delete => "DC".into(),
        Key::PageUp => "PPage".into(),
        Key::PageDown => "NPage".into(),
        Key::Space => "Space".into(),
        Key::Ctrl(c) => format!("C-{c}"),
        Key::Char(c) => format!("0x{:x}", u32::from(c)),
    }
}

/// Arguments for `tmux …` pressing `keys` in `pane`. Without `-K`, tmux gives
/// the keys to the pane (or to the mode the user left it in, such as copy mode)
/// and never to a client's key table.
pub fn control_keys_args(pane: &str, keys: &[crate::control::Key]) -> Vec<String> {
    let mut args = vec!["send-keys".to_string(), "-t".to_string(), pane.to_string()];
    args.extend(keys.iter().map(|&key| key_name(key)));
    args
}

/// One tmux command of a ctl key press.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyStep {
    /// Keys for [`control_keys_args`]: named keys and ASCII characters.
    Send(Vec<crate::control::Key>),
    /// Characters outside ASCII, pasted as their UTF-8 bytes without `-p`
    /// ([`control_paste_args`]), as typing them sends those bytes. `send-keys`
    /// takes their code points correctly only from tmux 2.4.
    Paste(String),
}

/// The tmux commands that press `keys` in order: runs of keys `send-keys` can
/// name on any supported tmux, and runs of other characters pasted raw.
pub fn control_key_steps(keys: &[crate::control::Key]) -> Vec<KeyStep> {
    use crate::control::Key;
    let mut steps: Vec<KeyStep> = Vec::new();
    for &key in keys {
        match (key, steps.last_mut()) {
            (Key::Char(c), Some(KeyStep::Paste(text))) if !c.is_ascii() => text.push(c),
            (Key::Char(c), _) if !c.is_ascii() => steps.push(KeyStep::Paste(c.to_string())),
            (key, Some(KeyStep::Send(run))) => run.push(key),
            (key, _) => steps.push(KeyStep::Send(vec![key])),
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_pane_is_the_sessions_first_whichever_pane_is_active() {
        assert_eq!(
            control_pane_args("muxel_p_1a2b3c4d"),
            [
                "list-panes",
                "-s",
                "-t",
                "=muxel_p_1a2b3c4d:",
                "-F",
                "#{session_name}:#{pane_id}:#{?pane_start_command,1,0}"
            ]
        );
        // The user split the agent's window (%14) and opened another window
        // (%20); list order follows windows, not age.
        let panes = "muxel_p_1:%20:0\nmuxel_p_1:%12:1\nmuxel_p_1:%14:0\n";
        for with_command in [true, false] {
            assert_eq!(
                parse_control_pane("muxel_p_1", panes, with_command).as_deref(),
                Some("%12")
            );
        }
        // Numeric order, not text order.
        assert_eq!(
            parse_control_pane("muxel_p_1", "muxel_p_1:%9:1\nmuxel_p_1:%10:0\n", true).as_deref(),
            Some("%9")
        );
        // The agent's pane has gone and the user's shell is left: nothing to
        // type into. A shell session (no command of its own) keeps its first pane.
        let left = "muxel_p_1:%14:0\nmuxel_p_1:%20:0\n";
        assert_eq!(parse_control_pane("muxel_p_1", left, true), None);
        assert_eq!(
            parse_control_pane("muxel_p_1", left, false).as_deref(),
            Some("%14")
        );
        // A namesake session, a window id, a missing flag or anything but a pane
        // id is not this session's pane.
        for out in [
            "",
            ":%3:1\n",
            "muxel_p_12:%3:1\n",
            "muxel_p_1:@3:1\n",
            "muxel_p_1:%:1\n",
            "muxel_p_1:%1;x:1\n",
            "muxel_p_1:%3\n",
            "muxel_p_1:%3:yes\n",
        ] {
            assert_eq!(parse_control_pane("muxel_p_1", out, false), None, "{out:?}");
        }
    }

    #[test]
    fn a_prompt_is_pasted_as_tmux_pastes_with_no_escape_left() {
        assert_eq!(
            control_paste_input("one\x1b[201~two\r\nthree\nfour ✓\x1b"),
            "one[201~two\rthree\rfour ✓".as_bytes()
        );
        assert_eq!(control_paste_input("plain"), b"plain");
    }

    #[test]
    fn control_input_keeps_text_out_of_arguments() {
        let buffer = control_buffer_name(Uuid::nil());
        assert_eq!(buffer, "muxel-ctl-00000000000000000000000000000000");
        assert_eq!(
            control_paste_args(&buffer, "%12", true),
            [
                "load-buffer",
                "-b",
                buffer.as_str(),
                "-",
                ";",
                "paste-buffer",
                "-d",
                "-p",
                "-r",
                "-b",
                buffer.as_str(),
                "-t",
                "%12"
            ]
        );
        assert_eq!(
            control_paste_args(&buffer, "%12", false),
            [
                "load-buffer",
                "-b",
                buffer.as_str(),
                "-",
                ";",
                "paste-buffer",
                "-d",
                "-r",
                "-b",
                buffer.as_str(),
                "-t",
                "%12"
            ]
        );
        assert_eq!(
            delete_buffer_args(&buffer),
            ["delete-buffer", "-b", buffer.as_str()]
        );
        assert_eq!(
            control_pane_exists_args("%12"),
            ["display-message", "-p", "-t", "%12", "#{pane_id}"]
        );
    }

    #[test]
    fn keys_outside_ascii_are_pasted_raw_and_the_rest_keep_their_order() {
        use crate::control::Key;
        let keys = [
            Key::Ctrl('b'),
            Key::Char('c'),
            Key::Char('é'),
            Key::Char('✓'),
            Key::Enter,
            Key::Char('𝄞'),
        ];
        assert_eq!(
            control_key_steps(&keys),
            [
                KeyStep::Send(vec![Key::Ctrl('b'), Key::Char('c')]),
                KeyStep::Paste("é✓".into()),
                KeyStep::Send(vec![Key::Enter]),
                KeyStep::Paste("𝄞".into()),
            ]
        );
        assert_eq!(
            control_key_steps(&[Key::Enter]),
            [KeyStep::Send(vec![Key::Enter])]
        );
        assert!(control_key_steps(&[]).is_empty());
    }

    #[test]
    fn every_ctl_key_presses_a_named_tmux_key_or_its_code_point() {
        use crate::control::Key;
        let keys: Vec<Key> = [
            "enter",
            "esc",
            "tab",
            "shift-tab",
            "up",
            "down",
            "right",
            "left",
            "home",
            "end",
            "backspace",
            "delete",
            "pageup",
            "pagedown",
            "space",
            "ctrl-b",
            ";",
            "~",
            "1",
        ]
        .iter()
        .map(|name| Key::parse(name).unwrap())
        .collect();
        assert_eq!(
            control_keys_args("%3", &keys),
            [
                "send-keys",
                "-t",
                "%3",
                "Enter",
                "Escape",
                "Tab",
                "BTab",
                "Up",
                "Down",
                "Right",
                "Left",
                "Home",
                "End",
                "BSpace",
                "DC",
                "PPage",
                "NPage",
                "Space",
                "C-b",
                "0x3b",
                "0x7e",
                "0x31"
            ]
        );
        // No argument a caller chose can end in `;` or hold a tmux format.
        for arg in control_keys_args("%3", &keys) {
            assert!(!arg.ends_with(';') && !arg.contains('#'), "{arg}");
        }
    }

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
    fn the_origin_probe_reads_the_marker_then_flags_it_removed() {
        assert_eq!(
            server_origin_args(),
            [
                "show-environment",
                "-g",
                "MUXEL_TMUX_SERVER",
                ";",
                "set-environment",
                "-g",
                "-r",
                "MUXEL_TMUX_SERVER"
            ]
        );
        assert_eq!(
            mark_server_args(),
            ["set-environment", "-g", "-r", "MUXEL_TMUX_SERVER"]
        );
        assert_eq!(
            ServerOrigin::from_probe(b"MUXEL_TMUX_SERVER=1\n"),
            ServerOrigin::Started
        );
        assert_eq!(
            ServerOrigin::from_probe(b"-MUXEL_TMUX_SERVER\n"),
            ServerOrigin::Muxel
        );
        // A server muxel didn't start has no marker: tmux prints only an error, to
        // stderr.
        assert_eq!(ServerOrigin::from_probe(b""), ServerOrigin::Foreign);
        assert_eq!(
            ServerOrigin::from_probe(b"MUXEL_TMUX_SERVER_OTHER=1\n"),
            ServerOrigin::Foreign
        );
    }

    #[test]
    fn environment_names_skip_removed_variables() {
        let out = b"HOME=/home/u\n-GONE\nEMPTY=\nF%%=() {  echo hi\n}\nPATH=/bin:/usr/bin\n";
        assert_eq!(
            environment_names(out),
            [&b"HOME"[..], b"EMPTY", b"F%%", b"PATH"]
        );
    }

    #[test]
    fn the_reseed_script_unsets_extras_sets_muxels_environment_and_marks_last() {
        let current: [&[u8]; 6] = [
            b"HOME",
            b"GIT_EDITOR",
            b"PWD",
            b"MUXEL_TMUX_SERVER",
            b"GIT_EDITOR",
            b"AGENT",
        ];
        let wanted: [(&[u8], &[u8]); 3] = [
            (b"HOME", b"/home/u"),
            (b"PWD", b"/elsewhere"),
            (b"LANG", b"en_US.UTF-8"),
        ];
        assert_eq!(
            environment_extras(&current, &wanted),
            [&b"GIT_EDITOR"[..], b"AGENT"]
        );
        assert_eq!(
            reseed_environment_script(&current, &wanted),
            "set-environment -g -u -- \"GIT_EDITOR\"\n\
             set-environment -g -u -- \"AGENT\"\n\
             set-environment -g -- \"HOME\" \"/home/u\"\n\
             set-environment -g -- \"LANG\" \"en_US.UTF-8\"\n\
             set-environment -g -r -- MUXEL_TMUX_SERVER\n"
        );
    }

    #[test]
    fn tmux_strings_cannot_expand_end_early_or_start_a_command() {
        assert_eq!(
            tmux_string(b"/usr/bin:/a-b_c.d,e+f@g=h"),
            "\"/usr/bin:/a-b_c.d,e+f@g=h\""
        );
        assert_eq!(
            tmux_string(b"$HOME ~\"\\;#{x}\n"),
            "\"\\044HOME\\040\\176\\042\\134\\073\\043\\173x\\175\\012\""
        );
        assert_eq!(tmux_string("é".as_bytes()), "\"\\303\\251\"");
        assert_eq!(tmux_string(b"\xff"), "\"\\377\"");
        assert_eq!(tmux_string(b""), "\"\"");
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
                Socket::Isolated(_) => panic!("legacy lookup probed an isolated socket"),
                Socket::Named(_) => unreachable!("locate only probes the standard servers"),
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
    fn isolated_local_sockets_never_probe_list_or_kill_regular_profile_sessions() {
        use std::path::Path;
        assert_eq!(Socket::local(None), Socket::BOTH);
        assert_eq!(Socket::local(Some(Path::new(""))), Socket::BOTH);
        let one = Path::new("/profile-one");
        let two = Path::new("/profile-two");
        let socket = Socket::isolated(one);
        assert_ne!(socket, Socket::isolated(two));
        assert_eq!(Socket::local(Some(one)), std::slice::from_ref(&socket));
        assert_eq!(
            Socket::locate_local(Some(one), |_| panic!("foreign socket probe")),
            socket
        );
        let mut probes = Vec::new();
        assert_eq!(
            Socket::locate_local(None, |socket| {
                probes.push(socket.clone());
                socket == Socket::Default
            }),
            Socket::Default
        );
        assert_eq!(probes, Socket::BOTH);
        for args in [
            start_server_args(),
            restore_exit_empty_args(),
            list_sessions_args(),
            has_session_args("same"),
            kill_session_args("same"),
        ] {
            let args = socket.argv(args);
            assert_eq!(&args[..2], socket.args());
            assert_eq!(args[0], "-L");
            assert_ne!(args[1], SOCKET_NAME);
            assert!(!args[1].contains("profile-one"));
        }
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

    #[test]
    fn respawn_migrates_only_after_two_confirmed_absences() {
        let target = SessionTarget::new("exact recorded name", Some(Socket::Default));
        for (default, private, expected, probes) in [
            (true, false, Some(Socket::Default), vec![Socket::Default]),
            (true, true, Some(Socket::Default), vec![Socket::Default]),
            (false, true, None, vec![Socket::Default, Socket::Private]),
            (
                false,
                false,
                Some(Socket::Private),
                vec![Socket::Default, Socket::Private],
            ),
        ] {
            let mut asked = Vec::new();
            let resolved = target
                .for_respawn(|socket| {
                    asked.push(socket.clone());
                    Ok::<_, ()>(if socket == &Socket::Default {
                        default
                    } else {
                        private
                    })
                })
                .unwrap();
            assert_eq!(
                resolved,
                expected.map(|socket| SessionTarget::new(&target.name, Some(socket)))
            );
            assert_eq!(asked, probes);
        }
        // The probe failed, rather than proving either server absent.
        for failing in [Socket::Default, Socket::Private] {
            let mut asked = Vec::new();
            assert_eq!(
                target.for_respawn(|socket| {
                    asked.push(socket.clone());
                    if socket == &failing {
                        Err("probe failed")
                    } else {
                        Ok(false)
                    }
                }),
                Err("probe failed")
            );
            assert_eq!(asked.last(), Some(&failing));
        }
        assert_eq!(
            target.socket,
            Some(Socket::Default),
            "decision does not mutate its input"
        );
    }

    #[test]
    fn respawn_never_probes_private_named_or_unresolved_owners() {
        for socket in [
            None,
            Some(Socket::Private),
            Some(Socket::Named("mxl-profile".into())),
        ] {
            let target = SessionTarget::new("work", socket);
            let result = target.for_respawn::<()>(|_| panic!("only Default may migrate"));
            assert_eq!(result, Ok(Some(target)));
        }
    }

    #[test]
    fn peer_restarts_cannot_recreate_default_after_private_migration() {
        let mut first = crate::Instance::shell(Uuid::new_v4());
        first.tmux_session = Some("owned".into());
        first.tmux_socket = Some(Socket::Default);
        // Independent clients admitted their restart while both knew Default.
        let second = first.clone();
        let stale_saved = serde_json::to_string(&second).unwrap();
        let mut running = HashSet::from([Socket::Default]);

        assert!(running.remove(&Socket::Default));
        let next = first
            .tmux_target("host")
            .for_respawn::<()>(|socket| Ok(running.contains(socket)))
            .unwrap()
            .unwrap();
        assert_eq!(next.socket, Some(Socket::Private));
        first.tmux_socket = next.socket;
        assert!(running.insert(Socket::Private));

        // B's delayed teardown succeeds because Default is already gone. Its
        // local generation is still valid, but A's replacement must stop it.
        assert!(!running.remove(&Socket::Default));
        let second_plan = second
            .tmux_target("host")
            .for_respawn::<()>(|socket| Ok(running.contains(socket)))
            .unwrap();
        assert_eq!(second_plan, None, "no Default create or Private adoption");
        assert_eq!(serde_json::to_string(&second).unwrap(), stale_saved);
        assert_eq!(running, HashSet::from([Socket::Private]));

        // Ordinary retry or app restart with stale saved ownership cannot
        // bypass the refusal. Inventory alone never proves a new binding.
        let mut reloaded: crate::Instance = serde_json::from_str(&stale_saved).unwrap();
        assert!(
            reloaded
                .tmux_target("host")
                .for_respawn::<()>(|socket| Ok(running.contains(socket)))
                .unwrap()
                .is_none()
        );
        // A separately refreshed, same-pane binding can attach to A's agent.
        assert_eq!(reloaded.id, first.id);
        reloaded.tmux_socket = first.tmux_socket.clone();
        assert_eq!(
            reloaded
                .tmux_target("host")
                .for_respawn::<()>(|_| panic!("recorded Private is already authoritative"))
                .unwrap()
                .unwrap()
                .socket,
            Some(Socket::Private)
        );
        assert_eq!(running.len(), 1);
    }

    #[test]
    fn has_session_absent_reads_only_tmuxs_answers_of_absence() {
        for message in [
            "can't find session: work\n",
            "no current target\n",
            "no server running on /tmp/tmux-1000/mxl\n",
            "error connecting to /tmp/tmux-1000/mxl (No such file or directory)\n",
        ] {
            assert!(has_session_absent(message), "{message}");
        }
        for message in [
            "",
            "lost server",
            "server exited unexpectedly",
            "error connecting to /tmp/tmux-1000/mxl (Permission denied)",
            "no current target\nPermission denied",
            "warning: no current target",
        ] {
            assert!(!has_session_absent(message), "{message}");
        }
    }

    #[test]
    fn migration_requires_specific_single_line_absence_errors() {
        for message in [
            "no server running on /tmp/tmux-1000/default\n",
            "error connecting to /tmp/tmux-1000/mxl (No such file or directory)\n",
            "error connecting to /tmp/tmux-1000/mxl (Connection refused)\r\n",
        ] {
            assert!(confirmed_no_server(message), "{message}");
        }
        for message in [
            "",
            "no server running",
            "no server running on ",
            "error connecting to /tmp/tmux-1000/mxl (Permission denied)",
            "error connecting to /tmp/tmux-1000/mxl (unknown error)",
            "error connecting to /tmp/tmux-1000/mxl",
            "warning\nno server running on /tmp/tmux-1000/default",
            "no server running on /tmp/tmux-1000/default\nPermission denied",
        ] {
            assert!(!confirmed_no_server(message), "{message}");
        }
    }

    #[test]
    fn named_owner_roundtrips_without_becoming_legacy() {
        for name in [
            "   ",
            "...",
            ".hidden",
            "mxl-profile",
            "default ",
            "MXL",
            "socket '名; $()",
            "mxl-0",
            "mxl-zzzzzzzzzzzzzzzz",
            "mxl-0000000000000000; echo bad",
            "mxl-0123456789ABCDEF",
            "MXL-0123456789abcdef",
        ] {
            let socket: Socket = serde_json::from_value(serde_json::json!(name)).unwrap();
            assert_eq!(socket, Socket::Named(name.into()));
            assert_eq!(socket.name(), name);
            assert_eq!(socket.args(), vec!["-L", name]);
            assert_eq!(
                socket.argv(has_session_args("work")),
                vec!["-L", name, "has-session", "-t", "=work"]
            );
            let closed = ClosedSession {
                name: "work".into(),
                socket: Some(socket),
                instance: Uuid::new_v4(),
            };
            let json = serde_json::to_string(&closed).unwrap();
            let restored: ClosedSession = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, closed);
            assert!(restored.socket.is_some());
            assert_eq!(serde_json::to_value(restored).unwrap()["socket"], name);
        }
        for (name, expected) in [("mxl", Socket::Private), ("default", Socket::Default)] {
            assert_eq!(
                serde_json::from_value::<Socket>(serde_json::json!(name)).unwrap(),
                expected
            );
        }
    }

    /// `tmux -L` joins the name onto its socket directory, so a recorded name that
    /// isn't a plain file name could reach a server somewhere else. It reads as no
    /// owner (legacy discovery on muxel's own servers) and the rest of the document
    /// still loads.
    #[test]
    fn a_recorded_owner_that_is_not_a_socket_name_reads_as_none() {
        for name in ["", ".", "..", "/tmp/elsewhere", "../mxl", "a/b", "nul\0x"] {
            let name = name.to_string();
            assert!(Socket::try_from(name.clone()).is_err(), "{name:?}");
            let closed: ClosedSession = serde_json::from_value(serde_json::json!({
                "name": "work",
                "socket": name,
                "instance": Uuid::nil(),
            }))
            .unwrap();
            assert_eq!(closed.socket, None, "{name:?}");
            let mut instance =
                serde_json::to_value(crate::Instance::shell(Uuid::new_v4())).unwrap();
            instance["tmux_session"] = serde_json::json!("work");
            instance["tmux_socket"] = serde_json::json!(name);
            let instance: crate::Instance = serde_json::from_value(instance).unwrap();
            assert_eq!(instance.tmux_socket, None, "{name:?}");
            assert_eq!(instance.tmux_session.as_deref(), Some("work"));
        }
        let closed: ClosedSession = serde_json::from_value(serde_json::json!({
            "name": "work",
            "socket": null,
            "instance": Uuid::nil(),
        }))
        .unwrap();
        assert_eq!(closed.socket, None);
    }

    /// The recorded name wins. muxel writes `muxel_<project>_<id>` onto an instance
    /// when tmux is enabled, and the iOS app launches from that; a desktop that
    /// recomputed `muxel_<host>_<id>` would attach to a *different* session and leave
    /// the real one orphaned on the host.
    #[test]
    fn a_recorded_session_wins_over_a_recomputed_one() {
        let id = Uuid::from_u128(0x1a2b3c4d_0000_0000_0000_000000000000);
        assert_eq!(
            session_for(Some("muxel_webapp_1a2b3c4d"), "rhel", id),
            "muxel_webapp_1a2b3c4d"
        );
    }

    #[test]
    fn recorded_session_names_remain_exact_before_migration() {
        let mut instance = crate::Instance::shell(Uuid::new_v4());
        instance.tmux_socket = Some(Socket::Default);
        for name in [" owned|with|pipes ", "   "] {
            instance.tmux_session = Some(name.into());
            let target = instance.tmux_target("host");
            assert_eq!(
                target.name, name,
                "a recorded name is an identity, not user input to trim"
            );
            let rows = crate::ssh::parse_session_listings_for_migration(&format!(
                ":muxel-listed:0\n{name}\n:muxel-listed:0\n"
            ))
            .unwrap();
            let prepared = target
                .for_respawn::<()>(|socket| {
                    Ok(rows
                        .iter()
                        .any(|row| &row.socket == socket && row.name == target.name))
                })
                .unwrap()
                .unwrap();
            assert_eq!(
                prepared, target,
                "the live Default owner must survive launch preparation"
            );
        }
    }

    /// With nothing recorded — the common remote case, where tmux comes from the
    /// host's default rather than the instance — both clients derive the same name.
    #[test]
    fn without_a_recorded_session_the_canonical_name_is_derived() {
        let id = Uuid::from_u128(0x1a2b3c4d_0000_0000_0000_000000000000);
        assert_eq!(session_for(None, "rhel", id), "muxel_rhel_1a2b3c4d");
        assert_eq!(session_for(Some(""), "rhel", id), "muxel_rhel_1a2b3c4d");
    }

    fn sessions() -> Vec<RemoteSession> {
        parse_sessions(concat!(
            "muxel_webapp_90f9def0|/home/me/Projects/webapp|claude\n",
            "muxel_webapp_d0d464c4|/home/me/Projects/webapp|claude\n",
            "muxel_rhel_ae15cabf|/home/me/Projects/work/api|claude\n",
            "muxel_notes_6b80c97f|/home/me/Projects/notes|claude\n",
            "work|/home/me|zsh\n",
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
    fn an_isolated_profile_overrides_foreign_owners_and_roundtrips_its_socket() {
        let root = std::path::Path::new("/isolated-test");
        let socket = Socket::isolated(root);
        let encoded = serde_json::to_string(&socket).unwrap();
        assert_eq!(serde_json::from_str::<Socket>(&encoded).unwrap(), socket);
        for prior in [
            None,
            Some(Socket::Private),
            Some(Socket::Default),
            Some(Socket::Isolated(1)),
            Some(Socket::Named("peer '名".into())),
        ] {
            let target = SessionTarget::new("work", prior.clone());
            let isolated = target.for_profile(Some(root));
            assert_eq!(isolated, SessionTarget::new("work", Some(socket.clone())));
            assert_eq!(
                isolated
                    .for_respawn::<()>(|_| panic!("isolated respawn must not probe global servers"))
                    .unwrap(),
                Some(isolated)
            );
            assert_eq!(target.for_profile(None), target);
            assert_eq!(target.for_profile(Some(std::path::Path::new(""))), target);
            let mut instance = crate::Instance::shell(Uuid::new_v4());
            instance.tmux_session = Some("work".into());
            instance.tmux_socket = prior.clone();
            let mut found = parse_sessions_on("work|/real|claude|\n", Socket::Private);
            found.extend(parse_sessions_on("work|/isolated|zsh|\n", socket.clone()));
            bind_session_owners("p", [&mut instance], &found, socket.clone());
            assert_eq!(instance.tmux_target("p"), found[1].target());
            let closed = ClosedSession {
                name: "work".into(),
                socket: prior,
                instance: instance.id,
            };
            assert_eq!(
                closed.resolve_owner(&found, socket.clone()).target(),
                found[1].target()
            );
            let pending = closed.resolve_owner(&[], socket.clone());
            assert!(pending.retry_after_inventory(&[], &Socket::BOTH).is_some());
            assert!(
                pending
                    .retry_after_inventory(&[], std::slice::from_ref(&socket))
                    .is_none()
            );
        }
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

    /// The pane's record still names Default, but a restart has since moved its
    /// session onto muxel's own server. The running session is the pane's, not an
    /// orphan to adopt into a second pane beside it.
    #[test]
    fn an_owner_whose_recorded_server_lacks_the_session_owns_its_namesake() {
        let name = "muxel_p_12345678";
        let found = parse_sessions_on(&format!("{name}|/p|claude|\n"), Socket::Private);
        let stale = [SessionTarget::new(name, Some(Socket::Default))];
        assert!(orphan_sessions(&found, "/p", &stale, &[]).is_empty());
        assert!(owned_session_keys(&found, &stale).contains(&(name, &Socket::Private)));
    }

    /// Two panes on one session: closing either leaves it to the other, even when
    /// their records disagree about the server. Only the last pane stops it.
    #[test]
    fn a_session_another_pane_still_shows_outlives_the_close() {
        let adopted = SessionTarget::new("muxel_p_12345678", Some(Socket::Private));
        let stale = SessionTarget::new("muxel_p_12345678", Some(Socket::Default));
        let unrelated = SessionTarget::new("muxel_p_87654321", Some(Socket::Private));
        assert!(session_still_shown(
            &adopted,
            &[unrelated.clone(), stale.clone()]
        ));
        assert!(session_still_shown(&stale, std::slice::from_ref(&adopted)));
        assert!(!session_still_shown(&adopted, &[unrelated]));
        assert!(!session_still_shown(&adopted, &[]));
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
    fn reconnect_retries_named_owner_after_persisted_failed_close() {
        let socket = Socket::Named("mxl-profile".into());
        let live = parse_sessions_on("owned|/p|claude|\n", socket.clone());
        let iid = Uuid::new_v4();
        let mut project = crate::Project::new("p", "/p");
        project.remember_closed_session(live[0].target(), iid);
        // The SSH close failed; its durable ledger is all a restarted client has.
        let saved = serde_json::to_string(&project).unwrap();
        let restored: crate::Project = serde_json::from_str(&saved).unwrap();
        let mut discovery = parse_sessions_on("owned|/p|zsh|\n", Socket::Private);
        discovery.extend(parse_sessions_on("owned|/p|vim|\n", Socket::Default));
        let pending = restored.closed_sessions[0].resolve_owner(&discovery, Socket::Private);
        assert_eq!(pending.socket, Some(socket.clone()));
        let retry = pending.retry_after_inventory(&discovery, &Socket::BOTH);
        assert_eq!(retry, Some(live[0].target()), "unqueried is not gone");
        assert_eq!(restored.closed_sessions.len(), 1);
        let command = crate::ssh::kill_and_confirm_command(&retry.unwrap());
        assert!(command.contains("mxl-profile"));
        assert_eq!(command.matches("has-session").count(), 1);
        assert!(pending.retry_after_inventory(&[], &Socket::BOTH).is_some());
        let unresolved = ClosedSession {
            socket: None,
            ..pending.clone()
        };
        assert!(
            unresolved
                .retry_after_inventory(&[], &Socket::BOTH)
                .is_some()
        );
        // A complete inventory of the recorded owner may confirm its absence.
        assert!(pending.retry_after_inventory(&[], &[socket]).is_none());
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
        let owned = vec!["muxel_webapp_d0d464c4".into()];
        let orphans = orphan_sessions(&sessions(), "/home/me/Projects/webapp", &owned, &[]);
        assert_eq!(
            orphans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["muxel_webapp_90f9def0"],
            "an owned session, another project's, and a non-muxel session are all left alone"
        );
        assert_eq!(orphans[0].command, "claude");
    }

    /// A session named for the *host* still belongs to whichever project it was
    /// started in — the path decides, not the slug.
    #[test]
    fn a_host_named_session_is_attributed_by_its_path() {
        let orphans = orphan_sessions(&sessions(), "/home/me/Projects/work/api", &[], &[]);
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
            "muxel_p_00000001|/home/me/Projects/app/.worktrees/feat|claude\n",
            "muxel_p_00000002|/home/me/Projects/app-other|claude\n",
        ));
        let orphans = orphan_sessions(&s, "/home/me/Projects/app", &[], &[]);
        assert_eq!(
            orphans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["muxel_p_00000001"]
        );
    }

    /// muxel adopts only its own sessions — never the user's own tmux.
    #[test]
    fn a_users_own_session_is_never_adopted() {
        let s = parse_sessions("work|/home/me/Projects/app|vim\n");
        assert!(orphan_sessions(&s, "/home/me/Projects/app", &[], &[]).is_empty());
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
        let c = [closed("muxel_webapp_90f9def0", id)];
        let orphans = orphan_sessions(&sessions(), "/home/me/Projects/webapp", &[], &c);
        assert_eq!(
            orphans.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["muxel_webapp_d0d464c4"],
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
        assert!(orphan_sessions(&sessions(), "/home/me/Projects/work/api", &[], &[c]).is_empty());
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
        let s = parse_sessions("work_00000001|/home/me|vim\n");
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
    fn session_environment_escapes_trailing_semicolons() {
        let env = vec![
            ("ROOT".into(), "/isolated;".into()),
            ("BACKSLASH".into(), r"/isolated\;".into()),
            ("MIDDLE".into(), "/isolated;other".into()),
        ];
        let args = launch_session_args_with_env("s", None, None, &[], &env, &[]);
        assert_eq!(
            &args[10..],
            [
                "-e",
                r"ROOT=/isolated\;",
                "-e",
                r"BACKSLASH=/isolated\\;",
                "-e",
                "MIDDLE=/isolated;other"
            ]
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
    fn pane_title_is_read_from_the_exact_session_without_capturing_it() {
        let args = pane_title_args("muxel_p_1");
        assert_eq!(
            args,
            [
                "display-message",
                "-p",
                "-t",
                "=muxel_p_1:",
                "#{session_name}:#{pane_title}"
            ]
        );
        assert!(!args.iter().any(|arg| arg.contains("capture")));
        assert_eq!(
            parse_pane_title("muxel_p_1", "muxel_p_1:scratch | Ready\n").as_deref(),
            Some("scratch | Ready")
        );
        // A title may itself contain the separator or be empty.
        assert_eq!(
            parse_pane_title("muxel_p_1", "muxel_p_1:a: b\n").as_deref(),
            Some("a: b")
        );
        assert_eq!(
            parse_pane_title("muxel_p_1", "muxel_p_1:\n").as_deref(),
            Some("")
        );
        // A missing target formats with no session; another session is not ours.
        for out in [
            "\n",
            ":\n",
            "",
            "muxel_p_12:scratch | Ready\n",
            "other:muxel_p_1:x\n",
        ] {
            assert_eq!(parse_pane_title("muxel_p_1", out), None, "{out:?}");
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
