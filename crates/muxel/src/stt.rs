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

/// A microphone capture in progress. The cpal stream is `!Send`, so it lives on
/// a dedicated worker thread and this handle only holds `Send` channels — safe
/// to store on `MuxelApp` and stop from the UI thread.
pub struct Recording {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<f32>>>,
    config: mpsc::Receiver<std::result::Result<(u32, u16), String>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recording {
    /// Stop capture and return `(samples, source_rate, channels)`. `Err` if the
    /// device never started (e.g. no mic, or permission denied).
    pub fn stop(mut self) -> Result<(Vec<f32>, u32, u16)> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        match self.config.try_recv() {
            Ok(Ok((rate, channels))) => {
                let samples = std::mem::take(&mut *self.samples.lock().unwrap());
                Ok((samples, rate, channels))
            }
            Ok(Err(e)) => bail!("microphone error: {e}"),
            Err(_) => bail!("microphone did not start"),
        }
    }
}

/// Start capturing from the default input device. Returns immediately; the cpal
/// stream runs on a worker thread until [`Recording::stop`].
pub fn start_capture() -> Result<Recording> {
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
    let (tx, rx) = mpsc::channel();

    let thread = {
        let stop = stop.clone();
        let samples = samples.clone();
        std::thread::Builder::new()
            .name("muxel-mic".into())
            .spawn(move || {
                let stream = match open_input_stream(&samples) {
                    Ok((stream, rate, channels)) => {
                        let _ = tx.send(Ok((rate, channels)));
                        stream
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                        return;
                    }
                };
                if let Err(e) = stream.play() {
                    let _ = tx.send(Err(e.to_string()));
                    return;
                }
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
        config: rx,
        thread: Some(thread),
    })
}

fn open_input_stream(samples: &Arc<Mutex<Vec<f32>>>) -> Result<(cpal::Stream, u32, u16)> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no microphone / input device found"))?;
    let supported = device
        .default_input_config()
        .context("query default input config")?;
    let rate = supported.sample_rate().0;
    let channels = supported.channels();
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let err_fn = |e| log::warn!("mic stream error: {e}");
    let stream = match sample_format {
        SampleFormat::F32 => build_stream::<f32>(&device, &config, samples.clone(), err_fn)?,
        SampleFormat::I16 => build_stream::<i16>(&device, &config, samples.clone(), err_fn)?,
        SampleFormat::U16 => build_stream::<u16>(&device, &config, samples.clone(), err_fn)?,
        other => bail!("unsupported microphone sample format: {other:?}"),
    };
    Ok((stream, rate, channels))
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
fn quiet_whisper_logging() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(whisper_rs::install_logging_hooks);
}

/// Transcribe 16 kHz mono f32 `samples` with a local whisper.cpp model file.
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
    let n = state.full_n_segments().context("whisper segment count")?;
    let mut text = String::new();
    for i in 0..n {
        if let Ok(seg) = state.full_get_segment_text(i) {
            text.push_str(&seg);
        }
    }
    Ok(text.trim().to_string())
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
