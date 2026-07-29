//! Developer harness for observing agent OSC titles through Muxel's real PTY
//! and terminal parser. It records control evidence, never screen contents.

use muxel_terminal::{CommandSpec, PtyChunk, TerminalSession};
use serde_json::json;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Default)]
struct OscProgressScanner {
    pending: Vec<u8>,
}

impl OscProgressScanner {
    fn push(&mut self, bytes: &[u8]) -> Vec<(String, Option<i32>)> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            let Some(start) = self
                .pending
                .windows(6)
                .position(|part| part == b"\x1b]9;4;")
            else {
                let keep = self.pending.len().min(5);
                self.pending.drain(..self.pending.len() - keep);
                break;
            };
            if start > 0 {
                self.pending.drain(..start);
            }
            let Some(end) = self.pending[6..]
                .iter()
                .position(|byte| *byte == 0x07)
                .map(|end| end + 6)
            else {
                break;
            };
            let body = String::from_utf8_lossy(&self.pending[6..end]);
            let mut fields = body.split(';');
            let state = fields.next().unwrap_or_default().to_string();
            let value = fields.next().and_then(|value| value.parse().ok());
            events.push((state, value));
            self.pending.drain(..=end);
        }
        events
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: agent-title-probe --log <path> [--script <json-path>] \
         [--include-title-text] -- <program> [args...]"
    );
    std::process::exit(2);
}

fn record(log: &Arc<Mutex<File>>, started: Instant, value: serde_json::Value) {
    let mut value = value;
    value["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
    let mut log = log.lock().expect("probe log lock poisoned");
    serde_json::to_writer(&mut *log, &value).expect("write probe event");
    writeln!(log).expect("finish probe event");
    log.flush().expect("flush probe event");
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--log")) {
        usage();
    }
    let log_path = args.next().map(PathBuf::from).unwrap_or_else(|| usage());
    let mut script_path = None;
    let mut include_title_text = false;
    loop {
        match args.next().as_deref() {
            Some(value) if value == std::ffi::OsStr::new("--script") => {
                script_path = Some(args.next().map(PathBuf::from).unwrap_or_else(|| usage()));
            }
            Some(value) if value == std::ffi::OsStr::new("--include-title-text") => {
                include_title_text = true;
            }
            Some(value) if value == std::ffi::OsStr::new("--") => break,
            _ => usage(),
        }
    }
    let program = args.next().unwrap_or_else(|| usage());
    let program = program.to_string_lossy().into_owned();
    let child_args = args.map(|arg| arg.to_string_lossy().into_owned()).collect();

    let started = Instant::now();
    let log = Arc::new(Mutex::new(File::create(log_path)?));
    record(
        &log,
        started,
        json!({ "event": "launch", "program": program }),
    );
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let spec = CommandSpec::program(program, child_args).with_cwd(cwd);
    let (session, rx) = TerminalSession::spawn(spec, 120, 40)?;
    let stopping = Arc::new(AtomicBool::new(false));

    if let Some(script_path) = script_path {
        let script: serde_json::Value = serde_json::from_reader(File::open(script_path)?)?;
        let actions = script
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("probe script must be a JSON array"))?;
        let input_session = session.clone();
        let input_log = log.clone();
        let input_stopping = stopping.clone();
        std::thread::Builder::new()
            .name("muxel-title-probe-script".to_string())
            .spawn(move || {
                for action in actions {
                    let after_ms = action["after_ms"].as_u64().unwrap_or(0);
                    std::thread::sleep(std::time::Duration::from_millis(after_ms));
                    if action["kill"].as_bool() == Some(true) {
                        record(&input_log, started, json!({ "event": "kill" }));
                        input_session.kill();
                        input_stopping.store(true, Ordering::Release);
                        break;
                    }
                    if let Some(input) = action["input"].as_str() {
                        input_session.write_input(input.as_bytes());
                        record(
                            &input_log,
                            started,
                            json!({ "event": "input", "bytes": input.len() }),
                        );
                    }
                }
            })?;
    } else {
        let input_session = session.clone();
        let input_log = log.clone();
        std::thread::Builder::new()
            .name("muxel-title-probe-input".to_string())
            .spawn(move || {
                let mut input = std::io::stdin().lock();
                let mut bytes = [0u8; 256];
                while let Ok(count) = input.read(&mut bytes) {
                    if count == 0 {
                        break;
                    }
                    input_session.write_input(&bytes[..count]);
                    record(
                        &input_log,
                        started,
                        json!({ "event": "input", "bytes": count }),
                    );
                }
            })?;
    }

    let mut title_generation = 0;
    let mut progress_scanner = OscProgressScanner::default();
    let mut stdout = std::io::stdout().lock();
    let (chunk_tx, chunk_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("muxel-title-probe-output".to_string())
        .spawn(move || {
            while let Ok(chunk) = rx.recv_blocking() {
                if chunk_tx.send(chunk).is_err() {
                    break;
                }
            }
        })?;
    loop {
        let chunk = match chunk_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(chunk) => chunk,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if stopping.load(Ordering::Acquire) => {
                record(&log, started, json!({ "event": "probe_stopped" }));
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match chunk {
            PtyChunk::Output(bytes) => {
                for (state, value) in progress_scanner.push(&bytes) {
                    record(
                        &log,
                        started,
                        json!({ "event": "progress", "state": state, "value": value }),
                    );
                }
                // Preserve an interactive view while recording only byte counts.
                stdout.write_all(&bytes)?;
                stdout.flush()?;
                session.process_probe_output(&bytes);
                record(
                    &log,
                    started,
                    json!({ "event": "output", "bytes": bytes.len() }),
                );
                let (generation, title) = session.title_snapshot();
                if generation != title_generation {
                    title_generation = generation;
                    let leading_codepoint = title
                        .as_deref()
                        .and_then(|title| title.chars().next())
                        .map(|ch| format!("U+{:04X}", ch as u32));
                    let title_chars = title.as_deref().map(|title| title.chars().count());
                    record(
                        &log,
                        started,
                        json!({
                            "event": "title",
                            "generation": generation,
                            "title": include_title_text.then_some(title),
                            "leading_codepoint": leading_codepoint,
                            "chars": title_chars,
                        }),
                    );
                }
                if session.take_bell() {
                    record(&log, started, json!({ "event": "bell" }));
                }
            }
            PtyChunk::Exit {
                code,
                signal,
                read_error,
            } => {
                record(
                    &log,
                    started,
                    json!({
                        "event": "exit",
                        "code": code,
                        "signal": signal,
                        "read_error": read_error,
                    }),
                );
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::OscProgressScanner;

    #[test]
    fn progress_scanner_handles_split_sequences_and_clear() {
        let mut scanner = OscProgressScanner::default();
        assert!(scanner.push(b"noise\x1b]9;").is_empty());
        assert_eq!(
            scanner.push(b"4;1;-1\x07tail"),
            vec![("1".to_string(), Some(-1))]
        );
        assert_eq!(
            scanner.push(b"\x1b]9;4;0;0\x07"),
            vec![("0".to_string(), Some(0))]
        );
    }
}
