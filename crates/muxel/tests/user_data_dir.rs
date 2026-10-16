//! `muxel --user-data-dir DIR ctl …`, through the real binary: the flag is taken
//! at startup, the store puts the endpoint file under `DIR/data`, and an explicit
//! flag wins over an inherited `MUXEL_USER_DATA_DIR`. An effective isolated root
//! refuses a foreign `MUXEL_CONTROL` instead of routing to another profile.
//!
//! Each muxel here is a fake: a loopback listener and the endpoint file that
//! names it. The test answers whichever one `muxel ctl` connects to, and the
//! reply says which that was. No window opens: `ctl` exits before the app starts.

use muxel_core::control::{ENDPOINT_FILE, Endpoint, PROTOCOL_VERSION, Request, Response};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(30);

/// A fresh directory, removed afterwards.
struct Root(PathBuf);

impl Root {
    fn new() -> Root {
        let dir = std::env::temp_dir().join(format!("muxel-user-data-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create test root");
        Root(dir)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A stand-in for a running muxel with outside control on.
struct FakeMuxel {
    name: &'static str,
    listener: TcpListener,
    token: String,
}

impl FakeMuxel {
    /// Listen on a loopback port and publish it in an endpoint file at `path`.
    fn publish(name: &'static str, path: &Path) -> FakeMuxel {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("listen");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let token = format!("token-{name}-{}", uuid::Uuid::new_v4().simple());
        let endpoint = Endpoint {
            version: PROTOCOL_VERSION,
            port: listener.local_addr().expect("local addr").port(),
            token: token.clone(),
            pid: std::process::id(),
            exe: None,
        };
        std::fs::create_dir_all(path.parent().expect("endpoint dir")).expect("endpoint dir");
        std::fs::write(path, serde_json::to_vec(&endpoint).expect("encode")).expect("endpoint");
        FakeMuxel {
            name,
            listener,
            token,
        }
    }

    /// Answer a request waiting on this muxel, if there is one, with its name.
    fn answer(&self) {
        let Ok((stream, _)) = self.listener.accept() else {
            return;
        };
        stream.set_nonblocking(false).expect("blocking stream");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .expect("read request");
        let request: Request = serde_json::from_str(&line).expect("a request");
        assert_eq!(
            request.token, self.token,
            "{} got another's token",
            self.name
        );
        let mut reply = serde_json::to_string(&Response::ok(json!({ "muxel": self.name })))
            .expect("encode reply");
        reply.push('\n');
        (&stream).write_all(reply.as_bytes()).expect("reply");
    }
}

/// Run `muxel ARGS` with only `env` set among muxel's own variables, answer for
/// whichever fake it reaches, and return that fake's name.
fn response(
    args: &[OsString],
    env: &[(&str, &Path)],
    fakes: &[&FakeMuxel],
) -> (std::process::Output, Value) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_muxel"));
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // This test may itself run in a muxel pane.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MUXEL_") {
            command.env_remove(key);
        }
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("run muxel");
    let deadline = Instant::now() + WAIT;
    while child.try_wait().expect("muxel status").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("muxel {args:?} did not exit");
        }
        for fake in fakes {
            fake.answer();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("muxel output");
    let reply: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| json!({ "stdout": String::from_utf8_lossy(&output.stdout) }));
    (output, reply)
}

fn reached(args: &[OsString], env: &[(&str, &Path)], fakes: &[&FakeMuxel]) -> String {
    let (output, reply) = response(args, env, fakes);
    assert!(output.status.success(), "{reply}");
    reply["muxel"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| {
            panic!(
                "muxel {args:?} reached no fake: {reply}\nstderr: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
}

#[test]
fn version_and_help_exit_without_creating_the_isolated_profile() {
    let root = Root::new();
    let profile = root.0.join("unused-profile");
    for flag in ["--version", "-V", "--help", "-h"] {
        let (output, _) = response(
            &[
                "--user-data-dir".into(),
                profile.clone().into(),
                flag.into(),
            ],
            &[],
            &[],
        );
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .starts_with(&format!("muxel {}", env!("CARGO_PKG_VERSION")))
        );
        assert!(!profile.exists(), "{flag} initialized the profile");
    }
}

#[test]
fn an_inherited_user_data_dir_refuses_a_foreign_endpoint() {
    let root = Root::new();
    let inherited = root.0.join("inherited");
    let expected = inherited.join("data").join(ENDPOINT_FILE);
    let foreign = root.0.join("foreign").join(ENDPOINT_FILE);
    let a = FakeMuxel::publish("foreign", &foreign);
    let b = FakeMuxel::publish("isolated", &expected);
    let (output, reply) = response(
        &["ctl".into(), "projects".into()],
        &[
            ("MUXEL_USER_DATA_DIR", &inherited),
            ("MUXEL_CONTROL", &foreign),
        ],
        &[&a, &b],
    );
    assert!(
        !output.status.success(),
        "foreign endpoint was accepted: {reply}"
    );
    let error = reply["error"].as_str().expect("typed error");
    assert!(error.contains("isolated_endpoint_conflict"), "{error}");
    assert!(
        error.contains("foreign") && error.contains("inherited"),
        "{error}"
    );
    let args = ["ctl".into(), "projects".into()];
    assert_eq!(
        reached(&args, &[("MUXEL_USER_DATA_DIR", &inherited)], &[&a, &b]),
        "isolated"
    );
    let matching = inherited.join("data").join(".").join(ENDPOINT_FILE);
    assert_eq!(
        reached(
            &args,
            &[
                ("MUXEL_USER_DATA_DIR", &inherited),
                ("MUXEL_CONTROL", &matching)
            ],
            &[&a, &b]
        ),
        "isolated"
    );
    assert_eq!(
        reached(&args, &[("MUXEL_CONTROL", &foreign)], &[&a, &b]),
        "foreign"
    );
}

#[test]
fn an_explicit_user_data_dir_picks_the_muxel_ctl_reaches() {
    let root = Root::new();
    let flagged = root.0.join("flagged");
    let inherited = root.0.join("inherited");
    let control = root.0.join("elsewhere").join(ENDPOINT_FILE);
    let flagged_endpoint = flagged.join("data").join(ENDPOINT_FILE);
    let b = FakeMuxel::publish("flagged", &flagged_endpoint);
    let c = FakeMuxel::publish("inherited", &inherited.join("data").join(ENDPOINT_FILE));
    let a = FakeMuxel::publish("muxel_control", &control);
    let fakes = [&a, &b, &c];
    let env = [
        ("MUXEL_CONTROL", control.as_path()),
        ("MUXEL_USER_DATA_DIR", inherited.as_path()),
    ];

    let mut joined = OsString::from("--user-data-dir=");
    joined.push(&flagged);
    for flag in [
        vec![OsString::from("--user-data-dir"), flagged.clone().into()],
        vec![joined],
    ] {
        let mut args = flag.clone();
        args.extend(["ctl".into(), "projects".into()]);
        let (output, reply) = response(&args, &env, &fakes);
        assert_eq!(output.status.code(), Some(1), "{reply}");
        let error = reply["error"].as_str().expect("typed conflict");
        assert!(
            error.contains("isolated_endpoint_conflict")
                && error.contains("flagged")
                && error.contains("elsewhere"),
            "{error}"
        );
        assert_eq!(
            reached(&args, &[("MUXEL_USER_DATA_DIR", &inherited)], &fakes),
            "flagged",
            "{flag:?}"
        );
        assert_eq!(
            reached(
                &args,
                &[
                    ("MUXEL_USER_DATA_DIR", &inherited),
                    ("MUXEL_CONTROL", &flagged_endpoint)
                ],
                &fakes
            ),
            "flagged",
            "{flag:?}"
        );
    }
}

#[test]
fn invalid_user_data_dir_flags_exit_before_startup() {
    let root = Root::new();
    for args in [
        vec!["--user-data-dir".into()],
        vec!["--user-data-dir=".into()],
        vec!["--user-data-dir".into(), "".into()],
        vec!["--user-data-dir".into(), "--version".into()],
        vec!["--user-data-dir:root".into()],
        vec!["--user-data-directory=root".into()],
        vec!["--user-data-dir=a".into(), "--user-data-dir=b".into()],
    ] {
        // A regression must still be confined to a disposable profile.
        let (output, _) = response(&args, &[("MUXEL_USER_DATA_DIR", &root.0)], &[]);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid_user_data_dir"));
    }
}
