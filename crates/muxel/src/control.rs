//! `muxel ctl`: outside control of the running app over a loopback port.
//!
//! While Settings → Grok Bot → "Allow outside tools to control muxel" is on, the
//! app listens on `127.0.0.1:<random port>` and writes the port and a fresh random
//! token to `<data dir>/control.json`, readable only by its owner. `muxel ctl …` —
//! this same binary, run with those arguments — reads that file, connects, sends
//! one JSON request line and prints the reply. Knowing the token is the whole of
//! the authorization, so only the user running muxel (and what they run) gets in.
//!
//! The protocol and the decisions in it live in `muxel_core::control`; the app
//! answers requests in `app/control_api.rs`. This file is only sockets and files.

use anyhow::{Context as _, Result};
use muxel_core::control::{
    self, AgentState, CliCommand, Command, Endpoint, PROTOCOL_VERSION, Request, Response,
    WaitOutcome,
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
     turn on Settings > Grok Bot > \"Allow outside tools to control muxel\". (If it is \
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
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
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
    if request.version != PROTOCOL_VERSION {
        return Response::err(format!(
            "this muxel speaks control protocol v{PROTOCOL_VERSION}, the command sent \
             v{}: run the muxel binary that is running",
            request.version
        ));
    }
    let (reply, answer) = std::sync::mpsc::channel();
    let incoming = Incoming {
        command: request.command,
        reply,
    };
    if tx.send_blocking(incoming).is_err() {
        return Response::err("muxel is shutting down");
    }
    answer
        .recv_timeout(REPLY_TIMEOUT)
        .unwrap_or_else(|_| Response::err("muxel didn't answer in time"))
}

/// Compare secrets without leaking, through timing, how much of a guess was right.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Write the endpoint file atomically, readable only by its owner.
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
/// return the process exit code: 0 on success, 1 when muxel refused or couldn't be
/// reached, 2 for a bad command line.
pub fn run_cli(args: &[String]) -> i32 {
    let command = match control::parse_cli(args) {
        Ok(command) => command,
        Err(error) => return fail(error, 2),
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
        CliCommand::Request(Command::Send { agent, text, force }) if text == "-" => {
            let mut text = String::new();
            match std::io::stdin().read_to_string(&mut text) {
                Ok(_) if !text.trim().is_empty() => request(Command::Send {
                    agent,
                    text: text.trim_end_matches(['\r', '\n']).to_string(),
                    force,
                }),
                Ok(_) => return fail("`send AGENT -` read no text from stdin".into(), 2),
                Err(e) => return fail(format!("reading stdin: {e}"), 2),
            }
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

fn fail(error: String, code: i32) -> i32 {
    emit(&with_host(json!({ "ok": false, "error": error })).to_string());
    code
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

/// Where the running app's endpoint file is: `$MUXEL_CONTROL` when set — for an
/// agent whose environment gives it a different home or `XDG_DATA_HOME` than the
/// app has — else `control.json` in muxel's data directory.
fn endpoint_path() -> Option<PathBuf> {
    std::env::var_os("MUXEL_CONTROL")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(muxel_store::data_dir()?.join(control::ENDPOINT_FILE)))
}

/// Whether the muxel that wrote the endpoint file is still running. A killed app
/// leaves its file behind; its port may since belong to something else, which
/// must not be sent the token.
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
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// One request to the running app.
fn request(command: Command) -> Result<Value, String> {
    let path = endpoint_path().ok_or("no data directory")?;
    let endpoint = read_endpoint(&path)
        .filter(|e| process_alive(e.pid))
        .ok_or(NOT_RUNNING)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|_| NOT_RUNNING)?;
    let _ = stream.set_read_timeout(Some(REPLY_TIMEOUT + IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut line = serde_json::to_string(&Request {
        version: PROTOCOL_VERSION,
        token: endpoint.token,
        command,
    })
    .map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("sending to muxel: {e}"))?;
    let mut reply = String::new();
    BufReader::new(&stream)
        .read_line(&mut reply)
        .map_err(|e| format!("reading muxel's reply: {e}"))?;
    if reply.trim().is_empty() {
        return Err(NOT_RUNNING.to_string());
    }
    let response: Response =
        serde_json::from_str(&reply).map_err(|e| format!("unreadable reply from muxel: {e}"))?;
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "muxel refused the request".into()))
    }
}

/// `wait`: check on the agent until its turn ends (it finishes, blocks on a
/// question or exits) or `timeout` passes, then show it.
fn wait(agent: &str, timeout: Duration, full: bool) -> Result<Value, String> {
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
            return Err(format!("'{agent}' is not a terminal pane"));
        }
        let awaiting = info["awaiting_reply"].as_bool().unwrap_or(false);
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
    use super::{read_endpoint, same_secret, write_endpoint};
    use muxel_core::control::Endpoint;

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
