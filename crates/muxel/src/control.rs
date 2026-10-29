//! `muxel ctl`: outside control of the running app over a loopback port.
//!
//! While Settings → Outside control → "Allow outside tools to control muxel" is on, the
//! app listens on `127.0.0.1:<random port>` and writes the port and a fresh random
//! token to `<data dir>/control.json`. Unix creates the file with mode 0600;
//! Windows inherits the parent directory's ACL, so the user data directory must
//! be private to the intended OS account. `--user-data-dir` does not impose an
//! owner-only ACL. `muxel ctl …` — this same binary, run with those arguments —
//! reads that file, connects, sends one JSON request line and prints the reply.
//! Knowing the token is the whole of the authorization; anyone able to read it
//! can exercise the enabled control authority.
//!
//! The protocol and the decisions in it live in `muxel_core::control`; the app
//! answers requests in `app/control_api.rs`. This file is only sockets and files.

use anyhow::{Context as _, Result};
use muxel_core::control::{
    self, AgentState, CliCommand, Command, Endpoint, ErrorCode, PROTOCOL_VERSION, Request,
    Response, WaitOutcome,
};
use serde_json::{Value, json};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Largest request line read; a prompt longer than this is refused.
const MAX_REQUEST_BYTES: u64 = 1 << 20;
/// Connections served at once; more are dropped rather than queued.
const MAX_CONNECTIONS: usize = 16;
/// How long a connection waits for the app to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// Socket read/write timeout, so a stalled peer can't hold a thread.
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// How often `wait` checks on the agent.
const WAIT_POLL: Duration = Duration::from_secs(1);

const NOT_RUNNING: &str = "muxel isn't running, or outside control is off: open muxel and \
     turn on Settings > Outside control > \"Allow outside tools to control muxel\". (If it is \
     running, this command may see a different home directory than muxel does: set \
     MUXEL_CONTROL to the path of muxel's control.json.)";

/// A request on its way to the app, with where its reply goes.
pub struct Incoming {
    pub command: Command,
    pub reply: std::sync::mpsc::Sender<Response>,
}

/// The listening side, alive while outside control is on. Dropping it stops
/// accepting and removes the endpoint file (if it is still this server's).
pub struct Server {
    stop: Arc<AtomicBool>,
    port: u16,
    path: PathBuf,
    token: String,
}

impl Server {
    /// Listen on a random loopback port, publish it in the endpoint file, and hand
    /// each authorized request to `tx`.
    pub fn start(tx: async_channel::Sender<Incoming>) -> Result<Self> {
        let dir = muxel_store::data_dir().context("no data directory")?;
        Self::start_in(&dir, tx)
    }

    /// [`Self::start`], publishing the endpoint file in `dir`.
    fn start_in(dir: &Path, tx: async_channel::Sender<Incoming>) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("opening a loopback port")?;
        let port = listener.local_addr()?.port();
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let path = dir.join(control::ENDPOINT_FILE);
        write_endpoint(
            &path,
            &Endpoint {
                version: PROTOCOL_VERSION,
                port,
                token: token.clone(),
                pid: std::process::id(),
                exe: Some(exe_path()),
            },
        )
        .with_context(|| format!("writing {}", path.display()))?;

        let stop = Arc::new(AtomicBool::new(false));
        let accept_stop = stop.clone();
        let accept_token = token.clone();
        std::thread::Builder::new()
            .name("muxel-control".into())
            .spawn(move || {
                let active = Arc::new(AtomicUsize::new(0));
                for stream in listener.incoming() {
                    if accept_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                        continue;
                    }
                    active.fetch_add(1, Ordering::Relaxed);
                    let (tx, token, done) = (tx.clone(), accept_token.clone(), active.clone());
                    let spawned = std::thread::Builder::new()
                        .name("muxel-control-conn".into())
                        .spawn(move || {
                            serve(stream, &token, &tx);
                            done.fetch_sub(1, Ordering::Relaxed);
                        });
                    if spawned.is_err() {
                        active.fetch_sub(1, Ordering::Relaxed);
                    }
                }
            })
            .context("starting the control thread")?;
        Ok(Self {
            stop,
            port,
            path,
            token,
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop so it notices `stop`.
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)),
            Duration::from_millis(200),
        );
        // Another muxel may have published its own endpoint since; leave that one.
        if read_endpoint(&self.path).is_some_and(|e| e.token == self.token) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Answer one connection: one request line in, one response line out.
fn serve(stream: TcpStream, token: &str, tx: &async_channel::Sender<Incoming>) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let response = match read_request(&stream) {
        Ok(request) => dispatch(request, token, tx),
        Err(error) => Response::err(error),
    };
    let mut line = serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"unencodable reply"}"#.to_string());
    line.push('\n');
    let _ = (&stream).write_all(line.as_bytes());
}

fn read_request(stream: &TcpStream) -> Result<Request, String> {
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST_BYTES))
        .read_line(&mut line)
        .map_err(|e| format!("reading the request: {e}"))?;
    serde_json::from_str(&line).map_err(|e| format!("not a muxel ctl request: {e}"))
}

fn dispatch(request: Request, token: &str, tx: &async_channel::Sender<Incoming>) -> Response {
    if !same_secret(request.token.as_bytes(), token.as_bytes()) {
        return Response::err("wrong token: run `muxel ctl` as the user running muxel");
    }
    let version = control::request_version(&request.command);
    if request.version != version {
        return Response::err(format!(
            "this command requires control protocol v{version}, the command sent \
             v{}: run the muxel binary that is running",
            request.version
        ));
    }
    let status_poll = matches!(&request.command, Command::NewStatus { .. });
    let unavailable = |message: &str| {
        if status_poll {
            Response::err(control::Error::new(ErrorCode::HostUnavailable, message))
        } else {
            Response::err(message)
        }
    };
    let (reply, answer) = std::sync::mpsc::channel();
    let incoming = Incoming {
        command: request.command,
        reply,
    };
    if tx.send_blocking(incoming).is_err() {
        return unavailable("muxel is shutting down");
    }
    answer
        .recv_timeout(REPLY_TIMEOUT)
        .unwrap_or_else(|_| unavailable("muxel didn't answer in time"))
}

/// Compare secrets without leaking, through timing, how much of a guess was right.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Write the endpoint file atomically. Unix creates mode 0600; Windows inherits
/// the parent ACL, so the user data directory must be private to the intended
/// account.
fn write_endpoint(path: &Path, endpoint: &Endpoint) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(serde_json::to_string(endpoint)?.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_endpoint(path: &Path) -> Option<Endpoint> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

// --- The client: `muxel ctl …` -------------------------------------------------

/// Run `muxel ctl <args>` against the running app, print the result as JSON, and
/// return the process exit code: 0 on success, 1 for a request failure (including
/// no running muxel), 2 for an invalid command line or stdin.
pub fn run_cli(args: &[String]) -> i32 {
    let command = match control::parse_cli(args) {
        Ok(command) => command,
        Err(error) => return fail(error.into(), 2),
    };
    let result = match command {
        CliCommand::Help => {
            emit(control::USAGE);
            return 0;
        }
        CliCommand::Skill => {
            emit(control::skill(&exe_path()).trim_end());
            return 0;
        }
        CliCommand::Request(Command::Send {
            agent,
            text,
            force,
            launch_id,
        }) if text == "-" => {
            let mut text = String::new();
            match std::io::stdin().read_to_string(&mut text) {
                Ok(_) if !text.trim().is_empty() => request(Command::Send {
                    agent,
                    text: text.trim_end_matches(['\r', '\n']).to_string(),
                    force,
                    launch_id,
                }),
                Ok(_) => return fail("`send AGENT -` read no text from stdin".into(), 2),
                Err(e) => return fail(format!("reading stdin: {e}").into(), 2),
            }
        }
        CliCommand::Request(Command::New {
            project,
            mut options,
        }) => {
            if options.prompt.as_deref() == Some("-") {
                let mut text = String::new();
                match std::io::stdin().read_to_string(&mut text) {
                    Ok(_) if !text.trim().is_empty() => {
                        options.prompt = Some(text.trim_end_matches(['\r', '\n']).to_string())
                    }
                    Ok(_) => return fail("`new --prompt -` read no text from stdin".into(), 2),
                    Err(error) => return fail(format!("reading stdin: {error}").into(), 2),
                }
            }
            options.requesting_pane = std::env::var(control::INSTANCE_ID_ENV)
                .ok()
                .and_then(|id| id.parse().ok());
            let no_wait = options.no_wait;
            request(Command::New { project, options }).and_then(|value| {
                if no_wait {
                    Ok(value)
                } else {
                    wait_creation(value)
                }
            })
        }
        CliCommand::Request(command) => request(command),
        CliCommand::Wait {
            agent,
            timeout_secs,
            full,
        } => wait(&agent, Duration::from_secs(timeout_secs), full),
    };
    match result {
        Ok(value) => {
            let value = with_host(value);
            emit(&serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()));
            0
        }
        Err(error) => fail(error, 1),
    }
}

/// Attach Windows GUI builds to the invoking shell's console so ctl can print.
/// Preserve inherited redirection and avoid sharing the shell's keyboard input.
/// If there is no parent console, or this process already has one, do nothing.
#[cfg(windows)]
pub fn attach_parent_console() {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };
    // SAFETY: plain Win32 calls; the handles put back are the ones this process
    // was started with.
    unsafe {
        let started = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .map(|id| (id, GetStdHandle(id).ok()));
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        // AttachConsole replaces redirected handles too. Restore them, and leave
        // absent stdin absent: the shell may already be reading its next prompt.
        for (id, handle) in started {
            match handle {
                Some(handle) => {
                    let _ = SetStdHandle(id, handle);
                }
                None if id == STD_INPUT_HANDLE => {
                    let _ = SetStdHandle(id, HANDLE(0));
                }
                None => {}
            }
        }
    }
}

fn fail(error: control::Error, status: i32) -> i32 {
    let reply = match error.code {
        Some(code) => json!({ "ok": false, "code": code, "error": error.message }),
        None => json!({ "ok": false, "error": error.message }),
    };
    emit(&with_host(reply).to_string());
    status
}

/// Print one reply. A reader that stops early (`muxel ctl panes | head`, `grep
/// -q`) closes the pipe; that's its business, not a crash — so write errors are
/// ignored where `println!` would panic.
fn emit(text: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}").and_then(|()| out.flush());
}

/// `value` with `host` — the computer whose muxel answered — first, so an agent
/// driving muxel on several computers can tell which one it is talking to. The
/// app is always on this computer: it only listens on loopback.
fn with_host(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut out = serde_json::Map::new();
            out.insert("host".into(), json!(local_hostname()));
            out.extend(fields);
            Value::Object(out)
        }
        other => json!({ "host": local_hostname(), "result": other }),
    }
}

/// This computer's name, as `hostname` prints it.
pub fn local_hostname() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        #[cfg(unix)]
        {
            let mut buf = [0u8; 256];
            // SAFETY: the buffer is valid for its length; gethostname writes at most
            // that many bytes and we stop at the first NUL (or the end).
            let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if ok && end > 0 {
                return String::from_utf8_lossy(&buf[..end]).into_owned();
            }
        }
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "unknown".to_string())
    })
}

/// The binary a skill should tell an agent to run: this one — `/usr/bin/muxel`
/// from a .deb or .rpm, wherever a tarball put it — or, when running from an
/// AppImage, the `.AppImage` file itself rather than its short-lived mount.
pub fn exe_path() -> String {
    crate::update::running_appimage()
        .or_else(|| std::env::current_exe().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "muxel".to_string())
}

/// Names the running app's endpoint file, for a `muxel ctl` whose environment
/// gives it a different home or `XDG_DATA_HOME` than the app has.
/// Without an isolated root it overrides the platform endpoint. With a root it
/// must name that root's endpoint; GUI runs clear it before starting panes.
pub const CONTROL_ENV: &str = "MUXEL_CONTROL";

/// An isolated root never routes control to a foreign endpoint.
fn endpoint_path() -> Result<Option<PathBuf>, String> {
    let default = muxel_store::data_dir().map(|dir| dir.join(control::ENDPOINT_FILE));
    let supplied = std::env::var_os(CONTROL_ENV)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    let isolated =
        std::env::var_os(muxel_store::USER_DATA_DIR_ENV).is_some_and(|root| !root.is_empty());
    let (default, supplied) = if isolated {
        (
            default.map(normalize_endpoint_path),
            supplied.map(normalize_endpoint_path),
        )
    } else {
        (default, supplied)
    };
    muxel_core::isolated_root::select_control_endpoint(default, isolated, supplied)
        .map_err(|error| error.to_string())
}

fn normalize_endpoint_path(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path)
        .or_else(|_| std::path::absolute(&path))
        .unwrap_or(path)
}

/// Whether the muxel that wrote the endpoint file is still running. A killed app
/// leaves its file behind; its port may since belong to something else, which
/// must not be sent the token or a prompt.
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // Signal 0 checks existence only. EPERM means it exists as another user.
        let signalled = unsafe { libc::kill(pid, 0) } == 0;
        signalled || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            WaitForSingleObject,
        };
        // SAFETY: plain Win32 calls on a handle this function opens and closes.
        unsafe {
            // Fail closed if either right is denied; synchronize access alone
            // can be granted to unrelated processes. This is a liveness check,
            // not proof of identity: an accessible reused PID can still pass.
            let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
            let Ok(process) = OpenProcess(access, false, pid) else {
                return false;
            };
            // Retained handles can keep exited processes open. Waiting avoids
            // confusing a real exit code of 259 with STILL_ACTIVE.
            let running = WaitForSingleObject(process, 0) == WAIT_TIMEOUT;
            let _ = CloseHandle(process);
            running
        }
    }
}

/// One request to the running app. For mutations, only failures before sending
/// are known to have done nothing. Read-only creation status can safely retry a
/// transport failure after sending as well.
fn request(command: Command) -> Result<Value, control::Error> {
    let path = endpoint_path()?
        .ok_or_else(|| control::Error::new(ErrorCode::HostUnavailable, "no data directory"))?;
    request_at(&path, command)
}

/// Each poll owns a fresh socket; approvals never occupy a connection slot.
fn wait_creation(value: Value) -> Result<Value, control::Error> {
    wait_creation_with(
        value,
        |request_id| request(Command::NewStatus { request_id }),
        || std::thread::sleep(WAIT_POLL),
    )
}

fn wait_creation_with(
    value: Value,
    mut status: impl FnMut(Uuid) -> Result<Value, control::Error>,
    mut pause: impl FnMut(),
) -> Result<Value, control::Error> {
    if value["pending"].as_bool() != Some(true) {
        return Ok(value);
    }
    let request_id = value["request_id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .ok_or("muxel returned an invalid creation request id")?;
    let mut retries = 0;
    loop {
        pause();
        match status(request_id) {
            Ok(value) if value["pending"].as_bool() != Some(true) => return Ok(value),
            Ok(_) => retries = 0,
            Err(error)
                if matches!(
                    error.code,
                    Some(ErrorCode::HostUnavailable | ErrorCode::StaleEndpoint)
                ) =>
            {
                if retries < 3 {
                    retries += 1;
                } else {
                    return Err(creation_poll_error(error, request_id));
                }
            }
            Err(error) if error.code.is_none() => {
                return Err(creation_poll_error(error, request_id));
            }
            Err(error) => return Err(error),
        }
    }
}

fn creation_poll_error(mut error: control::Error, request_id: Uuid) -> control::Error {
    error.message = format!(
        "{}; query this creation with `muxel ctl new-status {request_id}` when the host is available; do not repeat `new` because the original request may still be waiting",
        error.message,
    );
    error
}

/// [`request`] to the muxel whose endpoint file is `path`.
fn request_at(path: &Path, command: Command) -> Result<Value, control::Error> {
    let status_poll = matches!(&command, Command::NewStatus { .. });
    let transport_error = |message: String| {
        if status_poll {
            control::Error::new(ErrorCode::HostUnavailable, message)
        } else {
            message.into()
        }
    };
    let unavailable = |message| control::Error::new(ErrorCode::HostUnavailable, message);
    let endpoint = read_endpoint(path).ok_or_else(|| unavailable(NOT_RUNNING))?;
    if !process_alive(endpoint.pid) {
        return Err(control::Error::new(ErrorCode::StaleEndpoint, NOT_RUNNING));
    }
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|_| unavailable(NOT_RUNNING))?;
    let _ = stream.set_read_timeout(Some(REPLY_TIMEOUT + IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut line = serde_json::to_string(&Request {
        version: control::request_version(&command),
        token: endpoint.token,
        command,
    })
    .map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| transport_error(format!("sending to muxel: {e}")))?;
    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .map_err(|e| transport_error(format!("reading muxel's reply: {e}")))?;
    if reply.trim().is_empty() {
        return Err(transport_error(NOT_RUNNING.into()));
    }
    let response: Response = serde_json::from_str(&reply)
        .map_err(|e| transport_error(format!("unreadable reply from muxel: {e}")))?;
    response.into_result()
}

/// `wait`: check on the agent until its turn ends (it finishes, blocks on a
/// question or exits) or `timeout` passes, then show it.
fn wait(agent: &str, timeout: Duration, full: bool) -> Result<Value, control::Error> {
    let started = Instant::now();
    // Pin the agent by uuid: its name follows its title, which can change mid-turn.
    let mut target = agent.to_string();
    let outcome = loop {
        let info = request(Command::Status {
            agent: Some(target.clone()),
        })?;
        if let Some(uuid) = info["uuid"].as_str() {
            target = uuid.to_string();
        }
        let state: Option<AgentState> = serde_json::from_value(info["status"].clone()).ok();
        if state.is_none() {
            return Err(format!("'{agent}' is not a terminal pane").into());
        }
        let awaiting = info["awaiting_reply"].as_bool().unwrap_or(false);
        if info["startup_error"].is_string() {
            break WaitOutcome::SubmissionFailed;
        }
        if let Some(outcome) = control::wait_outcome(state, awaiting) {
            break outcome;
        }
        if started.elapsed() >= timeout {
            break WaitOutcome::TimedOut;
        }
        std::thread::sleep(WAIT_POLL);
    };
    let mut shown = request(Command::Show {
        agent: target,
        full,
    })?;
    shown["wait"] = json!({
        "outcome": outcome,
        "waited_secs": started.elapsed().as_secs(),
    });
    Ok(shown)
}

#[cfg(test)]
mod tests {
    use super::{
        Server, dispatch, process_alive, read_endpoint, request_at, same_secret,
        wait_creation_with, write_endpoint,
    };
    use muxel_core::control::{self, Command, Endpoint, ErrorCode, PROTOCOL_VERSION, Response};
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::{Ipv4Addr, TcpListener};
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxel-ctl-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn approval_poll_retries_keep_the_same_id_and_reset_after_a_pending_reply() {
        let id = uuid::Uuid::new_v4();
        let pending = serde_json::json!({"pending":true,"request_id":id});
        let outage = || {
            Err(control::Error::new(
                ErrorCode::HostUnavailable,
                "connection lost",
            ))
        };
        let mut replies = std::collections::VecDeque::from([
            outage(),
            outage(),
            Ok(serde_json::json!({"pending":true,"request_id":uuid::Uuid::new_v4()})),
            outage(),
            outage(),
            outage(),
            Ok(serde_json::json!({"uuid":"created-once"})),
        ]);
        let mut polls = 0;
        let mut pauses = 0;
        let result = wait_creation_with(
            pending,
            |request_id| {
                assert_eq!(request_id, id);
                polls += 1;
                replies.pop_front().expect("unexpected extra poll")
            },
            || pauses += 1,
        )
        .unwrap();
        assert_eq!(result["uuid"], "created-once");
        assert_eq!(polls, 7);
        assert_eq!(pauses, polls);
    }

    #[test]
    fn approval_poll_exhaustion_keeps_the_error_code_and_tells_callers_which_id_to_query() {
        let id = uuid::Uuid::new_v4();
        let mut polls = 0;
        let error = wait_creation_with(
            serde_json::json!({"pending":true,"request_id":id}),
            |request_id| {
                assert_eq!(request_id, id);
                polls += 1;
                Err(control::Error::new(
                    ErrorCode::HostUnavailable,
                    "outside control is off",
                ))
            },
            || {},
        )
        .unwrap_err();
        assert_eq!(polls, 4);
        assert_eq!(error.code, Some(ErrorCode::HostUnavailable));
        assert!(error.message.contains(&id.to_string()));
        assert!(error.message.contains("new-status"));
    }

    #[test]
    fn uncoded_approval_poll_error_keeps_the_request_id_and_recovery_guidance() {
        let id = uuid::Uuid::new_v4();
        let mut polls = 0;
        let error = wait_creation_with(
            serde_json::json!({"pending":true,"request_id":id}),
            |request_id| {
                assert_eq!(request_id, id);
                polls += 1;
                Err("muxel didn't answer in time".into())
            },
            || {},
        )
        .unwrap_err();
        assert_eq!(polls, 1);
        assert_eq!(error.code, None);
        assert!(error.message.starts_with("muxel didn't answer in time"));
        assert!(error.message.contains(&id.to_string()));
        assert!(error.message.contains("new-status"));
        assert!(error.message.contains("do not repeat `new`"));
    }

    #[test]
    fn approval_poll_never_retries_a_terminal_host_refusal() {
        for expected in [
            control::Error::new(ErrorCode::ApprovalDenied, "denied"),
            "unknown or expired creation request id".into(),
        ] {
            let mut polls = 0;
            let id = uuid::Uuid::new_v4();
            let error = wait_creation_with(
                serde_json::json!({"pending":true,"request_id":id}),
                |_| {
                    polls += 1;
                    Err(expected.clone())
                },
                || {},
            )
            .unwrap_err();
            assert_eq!(polls, 1);
            if expected.code.is_some() {
                assert_eq!(error, expected);
            } else {
                assert_eq!(error.code, None);
                assert!(error.message.starts_with(&expected.message));
                assert!(error.message.contains(&id.to_string()));
                assert!(error.message.contains("do not repeat `new`"));
            }
        }
    }

    #[test]
    fn a_lost_status_reply_is_retryable_without_claiming_a_mutation_did_nothing() {
        let dir = scratch("lost-status");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let path = dir.join(control::ENDPOINT_FILE);
        write_endpoint(
            &path,
            &Endpoint {
                version: PROTOCOL_VERSION,
                port: listener.local_addr().unwrap().port(),
                token: "test".into(),
                pid: std::process::id(),
                exe: None,
            },
        )
        .unwrap();
        let host = std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(&stream).read_line(&mut request).unwrap();
                // Lose the reply after receiving the entire request.
            }
        });
        let ambiguous = request_at(
            &path,
            Command::New {
                project: "repo".into(),
                options: Default::default(),
            },
        )
        .unwrap_err();
        let status = request_at(
            &path,
            Command::NewStatus {
                request_id: uuid::Uuid::new_v4(),
            },
        )
        .unwrap_err();
        host.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(ambiguous.code, None);
        assert_eq!(status.code, Some(ErrorCode::HostUnavailable));
    }

    #[test]
    fn a_disconnected_status_dispatch_is_retryable_without_reclassifying_creation() {
        let (tx, rx) = async_channel::unbounded();
        drop(rx);
        for (command, expected) in [
            (
                Command::New {
                    project: "repo".into(),
                    options: Default::default(),
                },
                None,
            ),
            (
                Command::NewStatus {
                    request_id: uuid::Uuid::new_v4(),
                },
                Some(ErrorCode::HostUnavailable),
            ),
        ] {
            let request = control::Request {
                version: control::request_version(&command),
                token: "test".into(),
                command,
            };
            assert_eq!(
                dispatch(request, "test", &tx)
                    .into_result()
                    .unwrap_err()
                    .code,
                expected
            );
        }
    }

    /// The path a refusal takes from the app to the caller: the server, the
    /// socket and `muxel ctl`'s reading of the reply.
    #[test]
    fn a_refusal_reaches_the_caller_with_its_code() {
        let dir = scratch("refusal");
        let (tx, rx) = async_channel::unbounded();
        let server = Server::start_in(&dir, tx).unwrap();
        // Stands in for the app: refuse the first send with a code, as a send to
        // a working agent is, then return an admitted paste receipt.
        let app = std::thread::spawn(move || {
            let first = rx.recv_blocking().unwrap();
            assert!(matches!(
                &first.command,
                Command::Send { launch_id: Some(launch), .. } if *launch == uuid::Uuid::from_u128(42)
            ));
            let busy = control::Error::new(ErrorCode::Working, "the agent is working");
            first.reply.send(Response::err(busy)).unwrap();
            let second = rx.recv_blocking().unwrap();
            let receipt =
                control::pasted_prompt_receipt(uuid::Uuid::from_u128(7), uuid::Uuid::from_u128(42));
            second.reply.send(Response::ok(receipt)).unwrap();
            let third = rx.recv_blocking().unwrap();
            assert!(matches!(third.command, Command::Projects));
            third
                .reply
                .send(Response::ok(serde_json::Value::Null))
                .unwrap();
        });
        let path = dir.join(control::ENDPOINT_FILE);
        let send = || Command::Send {
            agent: "claude".into(),
            text: "hi".into(),
            force: false,
            launch_id: Some(uuid::Uuid::from_u128(42)),
        };
        let refused = request_at(&path, send()).unwrap_err();
        assert_eq!(refused.code, Some(ErrorCode::Working));
        let receipt = request_at(&path, send()).unwrap();
        assert_eq!(receipt["pasted"], true);
        assert_eq!(receipt["launch_id"], uuid::Uuid::from_u128(42).to_string());
        assert_eq!(
            request_at(&path, Command::Projects).unwrap(),
            serde_json::Value::Null
        );
        app.join().unwrap();
        // Closing removes the file: there is no muxel to ask, and the code says so.
        drop(server);
        let gone = request_at(&path, Command::Projects).unwrap_err();
        assert_eq!(gone.code, Some(ErrorCode::HostUnavailable));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parameterized_creation_requires_exact_version_before_mutation() {
        let (tx, rx) = async_channel::unbounded();
        for (version, model, effort, required) in [
            (2, Some("sonnet"), None, 3),
            (2, None, Some("high"), 3),
            (3, None, None, 2),
        ] {
            let request: control::Request = serde_json::from_value(serde_json::json!({
                "version":version, "token":"test", "command": {
                    "cmd":"new", "project":"repo", "model":model, "effort":effort,
                }
            }))
            .unwrap();
            let error = dispatch(request, "test", &tx).into_result().unwrap_err();
            assert!(
                error
                    .message
                    .contains(&format!("requires control protocol v{required}"))
            );
            assert!(rx.is_empty(), "a mismatched request reached app mutation");
        }
        drop(rx);
        for (version, model) in [(2, None), (3, Some("sonnet"))] {
            let request: control::Request = serde_json::from_value(serde_json::json!({
                "version":version, "token":"test", "command": {
                    "cmd":"new", "project":"repo", "model":model,
                }
            }))
            .unwrap();
            let error = dispatch(request, "test", &tx).into_result().unwrap_err();
            assert_eq!(
                error.message, "muxel is shutting down",
                "matching version was refused before app dispatch"
            );
        }
    }

    #[test]
    fn a_v2_host_cannot_silently_drop_creation_parameters() {
        let dir = scratch("v2-parameters");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let path = dir.join(control::ENDPOINT_FILE);
        write_endpoint(
            &path,
            &Endpoint {
                version: PROTOCOL_VERSION,
                port: listener.local_addr().unwrap().port(),
                token: "test".into(),
                pid: std::process::id(),
                exe: None,
            },
        )
        .unwrap();
        let legacy = std::thread::spawn(move || {
            let mut mutations = 0;
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                // A v2 host ignores unknown option fields. If this client sends
                // v2, that host would create with defaults and lose the choices.
                let response = if request["version"] == 2 {
                    mutations += 1;
                    Response::ok(serde_json::json!({"created_with_defaults":true}))
                } else {
                    Response::err("v2 host refuses this request version")
                };
                writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
            }
            mutations
        });
        for options in [
            control::NewOptions {
                model: Some("sonnet".into()),
                ..Default::default()
            },
            control::NewOptions {
                effort: Some("high".into()),
                ..Default::default()
            },
        ] {
            let error = request_at(
                &path,
                Command::New {
                    project: "repo".into(),
                    options,
                },
            )
            .unwrap_err();
            assert!(error.message.contains("v2 host refuses"));
        }
        assert_eq!(legacy.join().unwrap(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_legacy_host_refuses_guarded_and_creation_requests_before_mutation() {
        let dir = scratch("legacy-guard");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let path = dir.join(control::ENDPOINT_FILE);
        write_endpoint(
            &path,
            &Endpoint {
                version: 1,
                port: listener.local_addr().unwrap().port(),
                token: "t".into(),
                pid: std::process::id(),
                exe: None,
            },
        )
        .unwrap();
        let commands = [
            Command::Send {
                agent: "a".into(),
                text: "hi".into(),
                force: false,
                launch_id: Some(uuid::Uuid::from_u128(42)),
            },
            Command::New {
                project: "repo".into(),
                options: Default::default(),
            },
            Command::New {
                project: "repo".into(),
                options: control::NewOptions {
                    prompt: Some("task".into()),
                    ..Default::default()
                },
            },
            Command::NewStatus {
                request_id: uuid::Uuid::new_v4(),
            },
        ];
        let count = commands.len();
        let legacy = std::thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["version"], 2);
                stream
                    .write_all(b"{\"ok\":false,\"error\":\"legacy host rejects request v2\"}\n")
                    .unwrap();
            }
        });
        for command in commands {
            let refused = request_at(&path, command).unwrap_err();
            assert!(refused.message.contains("legacy host rejects"));
        }
        legacy.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_left_by_a_muxel_that_exited_reaches_no_one() {
        let dir = scratch("stale");
        let mut gone = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "exit"])
                .spawn()
        } else {
            std::process::Command::new("true").spawn()
        }
        .unwrap();
        let pid = gone.id();
        gone.wait().unwrap();
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let path = dir.join(control::ENDPOINT_FILE);
        let endpoint = Endpoint {
            version: PROTOCOL_VERSION,
            port,
            token: "t".into(),
            pid,
            exe: None,
        };
        write_endpoint(&path, &endpoint).unwrap();
        let error = request_at(&path, Command::Projects).unwrap_err();
        // Both supported process checks reject the exited owner before connecting.
        let expected = if cfg!(any(unix, windows)) {
            ErrorCode::StaleEndpoint
        } else {
            ErrorCode::HostUnavailable
        };
        assert_eq!(error.code, Some(expected));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_running_process_is_alive_and_an_exited_one_is_not() {
        assert!(process_alive(std::process::id()));
        // 259 is also the exit code Windows reports for a process still running.
        for code in [3, 259] {
            let mut child = if cfg!(windows) {
                std::process::Command::new("cmd")
                    .args(["/C", &format!("exit {code}")])
                    .spawn()
            } else {
                std::process::Command::new("sh")
                    .args(["-c", &format!("exit {code}")])
                    .spawn()
            }
            .unwrap();
            let pid = child.id();
            let status = child.wait().unwrap();
            #[cfg(windows)]
            assert_eq!(status.code(), Some(code));
            // On Windows `child` still holds the process's handle, so the exited
            // process is still there to open — and must still read as not running.
            assert!(!process_alive(pid), "{status}");
        }
        // No process has this id.
        assert!(!process_alive(u32::MAX));
    }

    /// Refuse inaccessible processes even when the filename or wait access fits.
    #[cfg(windows)]
    #[test]
    fn a_process_we_may_not_open_is_not_alive() {
        // A copy of cmd.exe named like muxel's own program, which no one may open:
        // a check of the program's name would have let it through.
        let dir = std::env::temp_dir().join(format!("muxel-ctl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let muxel = dir.join("muxel.exe");
        std::fs::copy(std::env::var("ComSpec").unwrap(), &muxel).unwrap();
        let named_like_muxel = alive_once_locked(&muxel, "D:");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !named_like_muxel,
            "an unopenable muxel.exe counted as alive"
        );
        // cmd.exe, which lets everyone wait on it and do nothing else. Rights are
        // granted one by one, so being allowed to wait proves nothing.
        let cmd = std::path::Path::new("cmd");
        let wait_only = alive_once_locked(cmd, "D:(A;;0x00100000;;;WD)");
        assert!(!wait_only, "a process we may only wait on counted as alive");
    }

    /// Start `program` (cmd.exe or a copy, which runs until its input closes), give
    /// its process the access list `dacl` (in SDDL), and say whether
    /// `process_alive` then counts it as running. The child is ended either way.
    #[cfg(windows)]
    fn alive_once_locked(program: &std::path::Path, dacl: &str) -> bool {
        use std::os::windows::io::AsRawHandle as _;
        use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{
            AdjustTokenPrivileges, DACL_SECURITY_INFORMATION, ImpersonateSelf,
            PSECURITY_DESCRIPTOR, RevertToSelf, SecurityImpersonation, SetKernelObjectSecurity,
            TOKEN_ADJUST_PRIVILEGES,
        };
        use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};
        use windows::core::PCWSTR;
        let mut child = std::process::Command::new(program)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let open = process_alive(pid);
        let dacl: Vec<u16> = dacl.encode_utf16().chain([0]).collect();
        // `child` keeps the full-access handle it was created with, to end it below.
        // SAFETY: a NUL-terminated string that outlives the call, a descriptor
        // Windows allocates and that is freed here, and the child's handle, which
        // `child` owns and keeps open.
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(dacl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .unwrap();
            let set = SetKernelObjectSecurity(
                HANDLE(child.as_raw_handle() as isize),
                DACL_SECURITY_INFORMATION,
                sd,
            );
            LocalFree(HLOCAL(sd.0));
            set.unwrap();
        }
        // An enabled SeDebugPrivilege bypasses process DACLs. Restrict only a
        // fresh thread's token, leaving concurrent tests and the process alone.
        // SAFETY: the token belongs to this thread and is closed before use of
        // the liveness check. Impersonation ends here or when the thread exits.
        let locked = std::thread::spawn(move || unsafe {
            ImpersonateSelf(SecurityImpersonation).unwrap();
            let mut token = HANDLE::default();
            OpenThreadToken(
                GetCurrentThread(),
                TOKEN_ADJUST_PRIVILEGES,
                true,
                &mut token,
            )
            .unwrap();
            let adjusted = AdjustTokenPrivileges(token, true, None, 0, None, None);
            let _ = CloseHandle(token);
            adjusted.unwrap();
            assert!(process_alive(std::process::id()));
            let alive = process_alive(pid);
            RevertToSelf().unwrap();
            alive
        })
        .join();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            open,
            "{} wasn't alive before it was locked",
            program.display()
        );
        locked.unwrap()
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret(b"abc", b"abc"));
        assert!(!same_secret(b"abc", b"abd"));
        assert!(!same_secret(b"abc", b"abcd"));
    }

    #[test]
    fn endpoint_file_round_trips_owner_only() {
        let dir = std::env::temp_dir().join(format!("muxel-ctl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.json");
        let endpoint = Endpoint {
            version: 1,
            port: 4242,
            token: "secret".into(),
            pid: 7,
            exe: Some("/Applications/muxel.app/Contents/MacOS/muxel".into()),
        };
        write_endpoint(&path, &endpoint).unwrap();
        // Replacing it works too (a restarted server).
        write_endpoint(&path, &endpoint).unwrap();
        assert_eq!(read_endpoint(&path), Some(endpoint));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
