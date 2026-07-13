//! Speech-to-text I/O: microphone capture (cpal), local whisper.cpp
//! transcription (whisper-rs), the OpenAI-compatible provider call (ureq), and
//! model download. The pure, testable pieces (resampling to 16 kHz, WAV +
//! multipart encoding, model URLs) live in `muxel_core::stt`.
//!
//! Everything here BLOCKS; callers run it off the UI thread via
//! `cx.background_executor().spawn(..)` (see the speech methods on `MuxelApp`).

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

/// No input device is attached. *Not* a permission problem: a mic the OS has
/// denied us is still enumerated (macOS then feeds it to us as silence, which
/// [`mic_access_denied`] catches first) — an empty device list means the machine
/// genuinely has nothing to record with, as on a Mac mini with no mic plugged in.
const NO_INPUT_DEVICE: &str = "no microphone found — connect an input device";

/// Permission refused. Short because the pill pairs it with a button that opens
/// the settings screen. Only macOS can tell us this up front (see
/// [`mic_access_denied`]).
const ACCESS_DENIED: &str = "muxel isn't allowed to use the microphone";

/// A device that won't open is how Windows surfaces a denied microphone — WASAPI
/// refuses to activate it — so there the permission screen is worth offering.
/// macOS never fails this way (it feeds a denied app silence instead), so a macOS
/// device that won't open is broken or busy, and settings can't help.
const OPEN_FAILURE_MAY_BE_PERMISSION: bool = cfg!(target_os = "windows");

/// A failure to get at the microphone *itself*: nothing attached, the OS denied
/// access, or the device wouldn't open. Distinct from a transcription failure so
/// the UI offers its "open microphone settings" shortcut only where that is the
/// actual remedy — it would be nonsense on a failed model download.
///
/// Carries the whole user-facing message, so it reads correctly both as a root
/// error and as the outer context on an underlying device error.
#[derive(Debug)]
pub struct MicUnavailable {
    message: String,
    /// Whether the OS microphone settings could fix this. False when the mic is
    /// merely absent: an app that has never *requested* the microphone isn't even
    /// listed on that screen, so we'd be sending the user to look for something
    /// that isn't there.
    settings: bool,
}

impl MicUnavailable {
    /// Build one as an `anyhow::Error`, which is all any caller here wants.
    fn err(message: impl Into<String>, settings: bool) -> anyhow::Error {
        anyhow!(Self {
            message: message.into(),
            settings,
        })
    }
}

impl std::fmt::Display for MicUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MicUnavailable {}

/// Whether `e` is a microphone failure that the OS permission screen could fix,
/// and so is worth offering an "Open Settings" shortcut for.
pub fn mic_settings_would_help(e: &anyhow::Error) -> bool {
    e.downcast_ref::<MicUnavailable>()
        .is_some_and(|m| m.settings)
}

// AVCaptureDevice lives in AVFoundation, which nothing else in the binary links.
#[cfg(target_os = "macos")]
#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {}

/// macOS: has the user *denied* muxel the microphone?
///
/// Worth asking outright, because a denied app is not told so — CoreAudio still
/// enumerates the mic, opens it, and streams silence. Without this, a denied mic
/// would surface as "no speech captured" and the user would have no idea why.
///
/// Only `denied`/`restricted` count. `notDetermined` must not: the OS prompt fires
/// on the first real capture, and an app that has never asked isn't listed under
/// Privacy & Security → Microphone at all, so there'd be nothing there to switch on.
#[cfg(target_os = "macos")]
fn mic_access_denied() -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;
    use objc2_foundation::NSString;

    // AVAuthorizationStatus
    const RESTRICTED: isize = 1;
    const DENIED: isize = 2;

    let Some(class) = AnyClass::get(c"AVCaptureDevice") else {
        return false; // Framework missing: assume nothing and let capture try.
    };
    let audio = NSString::from_str("soun"); // AVMediaTypeAudio
    // SAFETY: +[AVCaptureDevice authorizationStatusForMediaType:] takes an
    // AVMediaType (an NSString) and returns AVAuthorizationStatus, an NSInteger.
    let status: isize = unsafe { msg_send![class, authorizationStatusForMediaType: &*audio] };
    matches!(status, RESTRICTED | DENIED)
}

/// Everywhere else the OS reports a refused microphone as a device that won't
/// open, so there's nothing to ask up front.
#[cfg(not(target_os = "macos"))]
fn mic_access_denied() -> bool {
    false
}

/// A microphone capture in progress. The cpal stream is `!Send`, so it lives on
/// a dedicated worker thread and this handle only holds `Send` channels — safe
/// to store on `MuxelApp` and stop from the UI thread.
pub struct Recording {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    channels: u16,
    /// Carries a stream failure that happened after [`start_capture`] had already
    /// handed back this `Recording` — the capture then holds no samples.
    failure: mpsc::Receiver<String>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recording {
    /// Stop capture and return `(samples, source_rate, channels)`.
    pub fn stop(mut self) -> Result<(Vec<f32>, u32, u16)> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Ok(e) = self.failure.try_recv() {
            return Err(MicUnavailable::err(
                format!("microphone error: {e}"),
                OPEN_FAILURE_MAY_BE_PERMISSION,
            ));
        }
        let samples = std::mem::take(&mut *self.samples.lock().unwrap());
        Ok((samples, self.rate, self.channels))
    }
}

/// Start capturing from the default input device. The device is opened on the
/// calling thread, so a missing mic or a denied permission is reported here
/// rather than after the user has spoken into a recording that never ran; the
/// cpal stream itself then runs on a worker thread until [`Recording::stop`].
pub fn start_capture() -> Result<Recording> {
    // Off macOS this is a constant `false`; there a refused mic shows up as a
    // device that won't open, below.
    if mic_access_denied() {
        // Would otherwise "succeed" and record silence.
        return Err(MicUnavailable::err(ACCESS_DENIED, true));
    }
    let (device, supported) = find_input_device()?;
    let rate = supported.sample_rate().0;
    let channels = supported.channels();
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
    let (tx, rx) = mpsc::channel();

    let thread = {
        let stop = stop.clone();
        let samples = samples.clone();
        std::thread::Builder::new()
            .name("muxel-mic".into())
            .spawn(move || {
                // The stream is `!Send`: it has to be built, played and dropped
                // all on this thread.
                let open = || -> Result<cpal::Stream> {
                    let stream = build_input_stream(&device, &config, sample_format, &samples)?;
                    stream.play().context("start the microphone stream")?;
                    Ok(stream)
                };
                let stream = match open() {
                    Ok(stream) => stream,
                    Err(e) => {
                        let _ = tx.send(format!("{e:#}"));
                        return;
                    }
                };
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                drop(stream); // stops capture
            })
            .context("spawn mic thread")?
    };

    Ok(Recording {
        stop,
        samples,
        rate,
        channels,
        failure: rx,
        thread: Some(thread),
    })
}

/// Pick an input device we can actually record from.
///
/// `default_input_device()` alone can't be trusted: on a Mac with no input at all
/// (a Mac mini / Mac Studio with nothing plugged in) CoreAudio still answers the
/// default-device query, with a handle that then fails every call on it with an
/// opaque backend error. So a device only counts once it has given us a config,
/// and the enumerated inputs — not the default — decide whether a mic exists.
fn find_input_device() -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    let host = cpal::default_host();
    let listed: Vec<cpal::Device> = host
        .input_devices()
        .map(Iterator::collect)
        .unwrap_or_default();
    if listed.is_empty() {
        return Err(MicUnavailable::err(NO_INPUT_DEVICE, false));
    }

    // Try the OS default first — it's the one the user chose — then anything else.
    let mut last_err = None;
    for device in host.default_input_device().into_iter().chain(listed) {
        match device.default_input_config() {
            Ok(config) => return Ok((device, config)),
            Err(e) => last_err = Some(e),
        }
    }
    // A mic is listed but none would open: report why, rather than claiming there
    // is no microphone.
    match last_err {
        Some(e) => Err(anyhow!(e).context(MicUnavailable {
            message: "could not open the microphone".to_string(),
            settings: OPEN_FAILURE_MAY_BE_PERMISSION,
        })),
        None => Err(MicUnavailable::err(NO_INPUT_DEVICE, false)),
    }
}

fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    samples: &Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream> {
    let err_fn = |e| log::warn!("mic stream error: {e}");
    match sample_format {
        SampleFormat::F32 => build_stream::<f32>(device, config, samples.clone(), err_fn),
        SampleFormat::I16 => build_stream::<i16>(device, config, samples.clone(), err_fn),
        SampleFormat::U16 => build_stream::<u16>(device, config, samples.clone(), err_fn),
        other => bail!("unsupported microphone sample format: {other:?}"),
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: Arc<Mutex<Vec<f32>>>,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                if let Ok(mut buf) = samples.lock() {
                    buf.extend(data.iter().map(|&s| f32::from_sample(s)));
                }
            },
            err_fn,
            None,
        )
        .context("build input stream")
}

/// Route whisper.cpp's internal C logging through the `log` crate (once), so it
/// doesn't spam stderr.
//
// whisper.cpp can't build for Windows on ARM (ggml's CPU backend rejects MSVC),
// so `whisper-rs` is excluded on that target — see crates/muxel/Cargo.toml — and
// the two functions that use it are gated out. The caller (app.rs) bails early
// with a "use a Provider" message there, so local transcription is never reached.
#[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
fn quiet_whisper_logging() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(whisper_rs::install_logging_hooks);
}

/// Transcribe 16 kHz mono f32 `samples` with a local whisper.cpp model file.
#[cfg(not(all(target_os = "windows", target_arch = "aarch64")))]
pub fn transcribe_local(samples16k: &[f32], model_path: &Path, language: &str) -> Result<String> {
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};
    if samples16k.is_empty() {
        return Ok(String::new());
    }
    quiet_whisper_logging();
    let path = model_path
        .to_str()
        .ok_or_else(|| anyhow!("model path is not valid UTF-8"))?;
    let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
        .context("load whisper model")?;
    let mut state = ctx.create_state().context("create whisper state")?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    if !language.is_empty() {
        params.set_language(Some(language));
    }
    state
        .full(params, samples16k)
        .context("whisper inference")?;
    let mut text = String::new();
    for i in 0..state.full_n_segments() {
        // Lossy: whisper splits on token boundaries, which can cut a multi-byte
        // character in half, and one mangled glyph beats dropping the segment that
        // holds it (what a strict `to_str` would do to the whole utterance).
        if let Some(Ok(seg)) = state.get_segment(i).map(|s| s.to_str_lossy()) {
            text.push_str(&seg);
        }
    }
    Ok(text.trim().to_string())
}

/// Windows-on-ARM stub: whisper.cpp can't build there (ggml rejects MSVC on ARM —
/// see crates/muxel/Cargo.toml), so local transcription is unavailable. Same
/// signature as the real one so the caller stays platform-agnostic; it points the
/// user at a Provider instead.
#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
pub fn transcribe_local(
    _samples16k: &[f32],
    _model_path: &Path,
    _language: &str,
) -> Result<String> {
    bail!(
        "local transcription isn't available on Windows on ARM — pick a Provider in Settings → Speech"
    )
}

/// Transcribe a WAV via an OpenAI-compatible `POST {base_url}/audio/transcriptions`.
pub fn transcribe_provider(
    wav: &[u8],
    base_url: &str,
    api_key: &str,
    model: &str,
    language: &str,
) -> Result<String> {
    let lang = (!language.is_empty()).then_some(language);
    let (content_type, body) = muxel_core::stt::build_transcription_multipart(wav, model, lang);
    let url = format!("{}/audio/transcriptions", base_url.trim_end_matches('/'));
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", &content_type)
        .send_bytes(&body);
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let msg = r.into_string().unwrap_or_default();
            bail!("transcription provider returned {code}: {}", msg.trim());
        }
        Err(e) => return Err(e).context("transcription request failed"),
    };
    let json: serde_json::Value = resp.into_json().context("parse provider response")?;
    let text = json
        .get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("provider response had no `text` field"))?;
    Ok(text.trim().to_string())
}

/// Ensure the whisper model for `model` exists under `models_dir`, downloading
/// it from HuggingFace if missing. Returns its path.
pub fn ensure_model(model: &str, models_dir: &Path) -> Result<PathBuf> {
    let path = models_dir.join(muxel_core::stt::whisper_model_filename(model));
    if path.is_file() {
        return Ok(path);
    }
    std::fs::create_dir_all(models_dir).context("create models dir")?;
    // Download to a temp file then rename, so an interrupted download never
    // leaves a truncated model that later loads as garbage.
    let tmp = path.with_extension("part");
    download_to(&muxel_core::stt::whisper_model_url(model), &tmp)?;
    std::fs::rename(&tmp, &path).context("finalize model download")?;
    Ok(path)
}

fn download_to(url: &str, dest: &Path) -> Result<()> {
    let resp = ureq::get(url).call().context("start model download")?;
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).context("create model file")?;
    std::io::copy(&mut reader, &mut file).context("write model file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MicUnavailable, mic_settings_would_help};
    use anyhow::{Context, anyhow};

    #[test]
    fn a_denied_microphone_offers_the_settings_shortcut() {
        let e = MicUnavailable::err("muxel isn't allowed to use the microphone", true);
        assert!(mic_settings_would_help(&e));
        assert_eq!(
            format!("{e:#}"),
            "muxel isn't allowed to use the microphone"
        );
    }

    #[test]
    fn a_missing_microphone_does_not() {
        // Nothing to fix in System Settings: an app that never requested the mic
        // isn't listed there at all, so the shortcut would lead to an empty list.
        let e = MicUnavailable::err("no microphone found — connect an input device", false);
        assert!(!mic_settings_would_help(&e));
    }

    #[test]
    fn the_marker_survives_being_wrapped_in_context() {
        let e: anyhow::Error = Err::<(), _>(anyhow!("device is busy"))
            .context(MicUnavailable {
                message: "could not open the microphone".to_string(),
                settings: true,
            })
            .unwrap_err();
        assert!(mic_settings_would_help(&e));
        // The cause survives the wrapping — that's what the pill shows.
        assert_eq!(
            format!("{e:#}"),
            "could not open the microphone: device is busy"
        );
    }

    #[test]
    fn other_failures_are_not_mic_failures() {
        // A transcription/model error must not get the "open mic settings" button.
        let e = anyhow!("write model file").context("download whisper model");
        assert!(!mic_settings_would_help(&e));
    }
}
