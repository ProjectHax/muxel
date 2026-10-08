//! Shared test helpers for `integrations.rs` and `libraries.rs`. No
//! `use gpui::*` here, so `#[test]` in the modules that use them stays the built-in one.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// How a [`TestServer`] treats each connection it accepts.
#[derive(Clone, Copy)]
enum Behavior {
    /// Answer every HTTP request with `401` + `WWW-Authenticate: Basic`.
    Unauthorized,
    /// Accept the connection and never write anything back.
    Silent,
}

/// A local HTTP server on `127.0.0.1:<random port>`, stopped on drop.
pub struct TestServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    /// A server that asks for credentials (`401`) on every request.
    pub fn unauthorized() -> Self {
        Self::start(Behavior::Unauthorized)
    }

    /// A server that accepts connections and never answers (time limits).
    pub fn silent() -> Self {
        Self::start(Behavior::Silent)
    }

    pub fn url(&self, path: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}",
            self.port,
            path.trim_start_matches('/')
        )
    }

    fn start(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let port = listener.local_addr().expect("local addr").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = std::thread::spawn(move || {
            // Held open (silent) until the server stops.
            let mut held: Vec<TcpStream> = Vec::new();
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => match behavior {
                        Behavior::Silent => held.push(stream),
                        Behavior::Unauthorized => answer_unauthorized(stream),
                    },
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
            drop(held);
        });
        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }
}

fn answer_unauthorized(mut stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let _ = stream.write_all(
        b"HTTP/1.1 401 Unauthorized\r\n\
          WWW-Authenticate: Basic realm=\"muxel-test\"\r\n\
          Content-Length: 0\r\n\
          Connection: close\r\n\r\n",
    );
    let _ = stream.flush();
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A new, short path directly under the temp directory (not created), so clones
/// and pack files below it stay under the Windows `MAX_PATH` limit on CI.
pub fn short_temp_path() -> PathBuf {
    let mut hex = uuid::Uuid::new_v4().simple().to_string();
    hex.truncate(8);
    std::env::temp_dir().join(format!("mxl-{hex}"))
}

/// An empty `GIT_CONFIG_GLOBAL` for test git commands, so the user's global
/// config (autocrlf, signing, hooks…) never leaks in.
fn empty_git_config() -> &'static std::path::Path {
    static EMPTY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| {
        let path = short_temp_path();
        std::fs::write(&path, "").expect("write empty git config");
        path
    })
}

/// Run `git <args>` in `dir` with an isolated config and a fixed identity;
/// panics if git fails. Returns stdout, trimmed.
pub fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=muxel-test",
            "-c",
            "user.email=muxel-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_git_config())
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn file_url(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    format!("file:///{}", path.trim_start_matches('/'))
}

/// A temporary git repository (`main` branch) standing in for a library's
/// remote `R`; removed on drop.
pub struct TestRepo {
    path: PathBuf,
}

impl TestRepo {
    pub fn init() -> Self {
        let path = short_temp_path();
        std::fs::create_dir_all(&path).expect("create test repo dir");
        git_out(&path, &["init", "-q", "-b", "main"]);
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn git(&self, args: &[&str]) -> String {
        git_out(&self.path, args)
    }

    /// Write `files`, stage everything and commit. Returns the new HEAD.
    pub fn commit(&self, files: &[(&str, &str)], msg: &str) -> String {
        self.write(files);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Replace the tip commit as a force-push would, so a clone of the old tip
    /// can no longer fast-forward. Returns the new HEAD.
    pub fn force_push(&self, files: &[(&str, &str)], msg: &str) -> String {
        self.write(files);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--amend", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn write(&self, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let file = self.path.join(rel);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).expect("create parent dir");
            }
            std::fs::write(&file, content).expect("write test repo file");
        }
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// An askpass program that writes `marker` when git runs it and answers
/// with a dummy secret: a `.bat` on Windows, a `sh` script elsewhere.
pub fn askpass_script(dir: &Path, marker: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let script = dir.join("askpass.bat");
        let body = format!(
            "@echo off\r\necho called> \"{}\"\r\necho secret\r\n",
            marker.display()
        );
        std::fs::write(&script, body).unwrap();
        script
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("askpass.sh");
        let body = format!(
            "#!/bin/sh\necho called > '{}'\necho secret\n",
            marker.display()
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }
}
