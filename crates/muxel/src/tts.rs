//! Text-to-speech I/O: the voice muxel can answer in.
//!
//! **Nothing calls this today.** It was built for the spoken wake command, which
//! was cut; it is kept — whole, working and documented — because the next feature
//! that wants to say something out loud should not have to rediscover any of it.
//! To wire it up: build a [`VoiceConfig`] from `Settings` (the `tts_*` fields are
//! still persisted) and call [`speak`]. Hence the `dead_code` allowance below:
//! this module is deliberately parked, not accidentally orphaned.
//!
//! The Kokoro (Local) engine is behind the off-by-default `voice-local` cargo
//! feature, because onnxruntime links statically and costs ~63 MB of binary — too
//! much to carry for a feature nothing calls yet. System and Provider need no
//! feature: cpal is already here for the microphone, and ureq for HTTP.
//!
//! Three engines, mirroring the speech-to-text side:
//!
//! - **System** — the synthesizer the OS already ships (`say`, SAPI, `spd-say` /
//!   `espeak`). Needs no model, no key and no network, so it is the default and
//!   the floor everything else falls back to. It also sounds like 1998.
//! - **Local** — Kokoro-82M on onnxruntime, in-process and fully offline, with the
//!   weights downloaded once into the data dir exactly like the whisper model.
//! - **Provider** — a cloud OpenAI-compatible `/audio/speech` endpoint, reusing
//!   the Speech section's base URL and keychain key.
//!
//! Local and Provider both hand back f32 samples, which [`play_stream`] pushes to
//! the default output device through cpal — already in the build for mic capture,
//! so muxel needs no audio-playback dependency and no codec: the provider is asked
//! for raw PCM rather than MP3 precisely so that stays true.
//!
//! Synthesis and playback are separate threads joined by a channel, so sound
//! starts on the first chunk rather than the last. That is not a nicety: Kokoro
//! renders at ~1.4× real time, so rendering a whole greeting before playing it
//! would open with four seconds of silence.
//!
//! Speech should never be the only channel — whatever speaks next should still
//! report on screen. So every failure here degrades rather than raises: a provider
//! that 500s, a model that won't download, a machine with no voice at all — it
//! falls back to the system voice, and failing that, stays quiet.

// Parked, not orphaned — see the module docs. Remove this the moment something
// speaks again.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use muxel_core::TtsEngine;

/// Everything a single utterance needs, snapshotted off the settings so speech
/// can run on its own thread without touching the app entity.
#[derive(Clone)]
pub struct VoiceConfig {
    pub engine: TtsEngine,
    /// Kokoro voice + weights (Local).
    pub local_voice: String,
    pub local_model: String,
    /// Endpoint, key, model and voice (Provider). The URL and key are the ones
    /// the Speech section already stores — one provider serves both directions.
    pub provider_url: String,
    pub provider_model: String,
    pub provider_voice: String,
    pub api_key: String,
    /// Where downloaded models live (`None` if there is no data dir).
    pub models_dir: Option<PathBuf>,
}

/// Speak `text` aloud, off the UI thread. Returns immediately.
///
/// The returned channel fires once, when audio actually starts — or when the
/// attempt is abandoned. A caller pacing something around the voice can wait on it
/// *with a timeout* and carry on regardless; it must never be treated as a promise
/// that sound happened.
pub fn speak(text: &str, cfg: VoiceConfig) -> Receiver<()> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let text = text.trim().to_string();
    if text.is_empty() {
        let _ = tx.try_send(());
        return rx;
    }
    std::thread::spawn(move || {
        if cfg.engine == TtsEngine::System {
            system_voice(&text, &tx);
            let _ = tx.try_send(());
            return;
        }

        // Synthesis runs on its own thread and pushes finished chunks down the
        // channel, so playback can begin on the FIRST chunk instead of waiting for
        // the last. That is what makes the local voice usable: Kokoro takes ~4s to
        // render four sentences whole, but under a second to render the first — and
        // it renders faster than the device plays, so once the first sentence is
        // out, the rest stays ahead of the needle.
        let (chunk_tx, chunks) = std::sync::mpsc::channel::<Vec<f32>>();
        let synth_cfg = cfg.clone();
        let synth_text = text.clone();
        let producer = std::thread::spawn(move || produce(&synth_text, &synth_cfg, &chunk_tx));

        let played = match play_stream(&chunks, muxel_core::tts::SPEECH_RATE, &tx) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("speech playback failed: {e:#}");
                0
            }
        };
        let synth = producer
            .join()
            .unwrap_or_else(|_| bail!("speech thread panicked"));

        // Nothing came out — a dead provider, a model that won't download, no audio
        // device. Fall back to the OS voice, which needs none of those things.
        if played == 0 {
            if let Err(e) = synth {
                log::warn!("speech failed, falling back to the system voice: {e:#}");
            }
            system_voice(&text, &tx);
        } else if let Err(e) = synth {
            // It spoke, then broke: say what happened but don't repeat the line.
            log::warn!("speech ended early: {e:#}");
        }
        // Whatever happened, unblock anyone pacing around us.
        let _ = tx.try_send(());
    });
    rx
}

/// Synthesize `text` into the channel, a chunk at a time.
fn produce(text: &str, cfg: &VoiceConfig, out: &std::sync::mpsc::Sender<Vec<f32>>) -> Result<()> {
    match cfg.engine {
        // One request, one chunk: the whole reply arrives as a single PCM body.
        TtsEngine::Provider => {
            let _ = out.send(synth_provider(text, cfg)?);
            Ok(())
        }
        // Sentence by sentence, so the first word lands fast.
        TtsEngine::Local => synth_local_streaming(text, cfg, out),
        TtsEngine::System => Ok(()),
    }
}

// --- Playback ---------------------------------------------------------------

/// Play mono chunks (at `rate`) on the default output device as they arrive,
/// blocking until the producer is done and the buffer has drained. Returns how
/// many samples were played; signals `started` when the first sound goes out.
///
/// Building the device stream waits for the first chunk, so a synthesizer that
/// fails outright never opens (and never has to close) an audio device.
fn play_stream(chunks: &Receiver<Vec<f32>>, rate: u32, started: &SyncSender<()>) -> Result<usize> {
    let Ok(first) = chunks.recv() else {
        return Ok(0); // producer failed before it made a sound
    };

    let device = cpal::default_host()
        .default_output_device()
        .context("no audio output device")?;
    let supported = device.default_output_config().context("no output config")?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let dev_rate = config.sample_rate.0;
    let channels = config.channels.max(1) as usize;

    // The device rarely wants 24 kHz mono: resample each chunk to its rate, and
    // fan the mono signal out across however many channels it has.
    let queue: Shared =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let mut played = push(&queue, &first, rate, dev_rate)?;

    let starving = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stream = match format {
        cpal::SampleFormat::F32 => {
            out_stream::<f32>(&device, &config, &queue, channels, &starving)?
        }
        cpal::SampleFormat::I16 => {
            out_stream::<i16>(&device, &config, &queue, channels, &starving)?
        }
        cpal::SampleFormat::U16 => {
            out_stream::<u16>(&device, &config, &queue, channels, &starving)?
        }
        other => bail!("unsupported output sample format: {other:?}"),
    };
    stream.play().context("start output stream")?;
    let _ = started.try_send(());

    // Feed the queue until the producer hangs up.
    while let Ok(chunk) = chunks.recv() {
        played += push(&queue, &chunk, rate, dev_rate)?;
    }

    // Producer is done; wait for the device to drain what is left, with a ceiling
    // well past the audio's own length so a stalled device can't wedge the thread.
    let secs = played as f32 / dev_rate.max(1) as f32;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f32(secs + 5.0);
    loop {
        let empty = queue.lock().map(|q| q.is_empty()).unwrap_or(true);
        if empty && starving.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if std::time::Instant::now() > deadline {
            log::warn!("speech playback timed out waiting for the device");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // The last buffer is queued, not yet audible: let the device flush it before
    // the stream drops, or the final word is clipped.
    std::thread::sleep(std::time::Duration::from_millis(150));
    Ok(played)
}

type Shared = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<f32>>>;

/// Resample a chunk to the device rate and queue it. Returns its length.
fn push(queue: &Shared, chunk: &[f32], rate: u32, dev_rate: u32) -> Result<usize> {
    let resampled = muxel_core::audio::resample_linear(chunk, rate, dev_rate);
    let n = resampled.len();
    queue
        .lock()
        .map_err(|_| anyhow::anyhow!("speech queue poisoned"))?
        .extend(resampled);
    Ok(n)
}

fn out_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    queue: &Shared,
    channels: usize,
    starving: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let queue = queue.clone();
    let starving = starving.clone();
    device
        .build_output_stream(
            config,
            move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                let mut q = match queue.lock() {
                    Ok(q) => q,
                    Err(_) => return,
                };
                for frame in out.chunks_mut(channels) {
                    // An empty queue mid-utterance is an underrun: write silence
                    // rather than stopping, so a slow chunk costs a gap, not the
                    // rest of the sentence.
                    let s = q.pop_front().unwrap_or(0.0);
                    for slot in frame.iter_mut() {
                        *slot = T::from_sample(s);
                    }
                }
                starving.store(q.is_empty(), std::sync::atomic::Ordering::Relaxed);
            },
            |e| log::warn!("speech stream error: {e}"),
            None,
        )
        .context("build output stream")
}

// --- Provider (cloud) -------------------------------------------------------

/// Synthesize through an OpenAI-compatible `/audio/speech`, asking for raw PCM
/// so the reply needs no audio decoder.
fn synth_provider(text: &str, cfg: &VoiceConfig) -> Result<Vec<f32>> {
    if cfg.api_key.is_empty() {
        bail!("set a provider API key in Settings → Speech");
    }
    let body =
        muxel_core::tts::build_speech_request(&cfg.provider_model, &cfg.provider_voice, text);
    let url = muxel_core::tts::speech_endpoint(&cfg.provider_url);
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {}", cfg.api_key))
        .set("Content-Type", "application/json")
        .send_string(&body);
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let msg = r.into_string().unwrap_or_default();
            bail!("speech provider returned {code}: {}", msg.trim());
        }
        Err(e) => return Err(e).context("speech request failed"),
    };
    // Bounded: a runaway or hostile endpoint must not stream into memory forever.
    let mut pcm = Vec::new();
    let mut reader = std::io::Read::take(resp.into_reader(), 64 * 1024 * 1024);
    std::io::Read::read_to_end(&mut reader, &mut pcm).context("read speech response")?;
    let samples = muxel_core::tts::decode_pcm_s16le(&pcm);
    if samples.is_empty() {
        bail!("speech provider returned no audio");
    }
    Ok(samples)
}

// --- Local (Kokoro on onnxruntime) ------------------------------------------

/// Fetch `url` to `dest` via a `.part` file, so an interrupted download never
/// leaves a truncated model that later loads as garbage. Mirrors `stt::ensure_model`.
fn download_once(url: &str, dest: &Path) -> Result<()> {
    if dest.is_file() {
        return Ok(());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).context("create models dir")?;
    }
    let tmp = dest.with_extension("part");
    let resp = ureq::get(url).call().context("start voice download")?;
    let mut file = std::fs::File::create(&tmp).context("create voice file")?;
    std::io::copy(&mut resp.into_reader(), &mut file).context("write voice file")?;
    std::fs::rename(&tmp, dest).context("finalize voice download")?;
    Ok(())
}

/// Download (once) the Kokoro weights and the chosen voice pack, returning both
/// paths. ~89 MB for the int8 model, ~510 KB per voice.
#[cfg(feature = "voice-local")]
fn ensure_kokoro(cfg: &VoiceConfig) -> Result<(PathBuf, PathBuf)> {
    use muxel_core::tts::{
        KOKORO_VOICE_DIR, kokoro_model_filename, kokoro_model_url, kokoro_voice_filename,
        kokoro_voice_url,
    };
    let dir = cfg
        .models_dir
        .as_ref()
        .context("no data directory for the voice model")?;
    let model = dir.join(kokoro_model_filename(&cfg.local_model));
    download_once(&kokoro_model_url(&cfg.local_model), &model)?;
    // Kokoro finds a voice by file stem, so the pack must keep its bare name
    // (`bm_george.bin`) and live in its own folder — see `KOKORO_VOICE_DIR`.
    let voice = dir
        .join(KOKORO_VOICE_DIR)
        .join(kokoro_voice_filename(&cfg.local_voice));
    download_once(&kokoro_voice_url(&cfg.local_voice), &voice)?;
    Ok((model, voice))
}

/// Synthesize with Kokoro-82M, in-process and offline, one sentence at a time.
///
/// The loaded model is cached for the life of the process: it is ~89 MB of
/// weights and an onnxruntime session, which is far too much to rebuild for every
/// sentence. A settings change to the model or voice invalidates the cache.
#[cfg(feature = "voice-local")]
fn synth_local_streaming(
    text: &str,
    cfg: &VoiceConfig,
    out: &std::sync::mpsc::Sender<Vec<f32>>,
) -> Result<()> {
    use std::sync::Mutex;
    use std::sync::OnceLock;

    // (model, voice) the cached session was built for, so switching either in
    // Settings rebuilds it rather than speaking in the old voice forever.
    type Cached = (String, String, kokoro_en::KokoroTts);
    static SESSION: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();

    let (model_path, voice_path) = ensure_kokoro(cfg)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build speech runtime")?;

    let cell = SESSION.get_or_init(|| Mutex::new(None));
    let mut guard = cell
        .lock()
        .map_err(|_| anyhow::anyhow!("speech session poisoned"))?;

    let stale = guard
        .as_ref()
        .is_none_or(|(m, v, _)| m != &cfg.local_model || v != &cfg.local_voice);
    if stale {
        let tts = runtime
            .block_on(kokoro_en::KokoroTts::new(&model_path, &voice_path))
            .context("load the Kokoro voice model")?;
        *guard = Some((cfg.local_model.clone(), cfg.local_voice.clone(), tts));
    }
    let (_, _, tts) = guard.as_ref().expect("session just populated");

    for sentence in muxel_core::tts::sentences(text) {
        let (audio, took) = runtime
            .block_on(tts.synth(sentence.as_str(), cfg.local_voice.as_str()))
            .context("Kokoro synthesis")?;
        log::debug!("kokoro rendered {sentence:?} in {took:?}");
        // A closed channel means playback gave up (no device, app quitting): stop
        // rendering into the void.
        if out.send(audio).is_err() {
            break;
        }
    }
    Ok(())
}

/// Stub for builds without the `voice-local` feature (the default), where Kokoro
/// and onnxruntime are not compiled in at all. Same signature as the real one so
/// the caller stays build-agnostic; `speak` falls back to the system voice.
#[cfg(not(feature = "voice-local"))]
fn synth_local_streaming(
    _text: &str,
    _cfg: &VoiceConfig,
    _out: &std::sync::mpsc::Sender<Vec<f32>>,
) -> Result<()> {
    bail!("this build has no local voice (build with --features voice-local)")
}

/// Whether the local (Kokoro) engine is compiled into this build.
pub const fn local_voice_supported() -> bool {
    cfg!(feature = "voice-local")
}

// --- System (the OS voice) --------------------------------------------------

/// Speak through the synthesizer the OS ships. Blocks until it finishes.
fn system_voice(text: &str, started: &SyncSender<()>) {
    let _ = started.try_send(());
    for mut cmd in system_voices(text) {
        let spoken = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if spoken {
            return;
        }
    }
    log::debug!("no speech synthesizer available; muxel stays quiet");
}

/// The OS synthesizers to try, best first. A box with none of them installed just
/// runs out of candidates and stays quiet.
///
/// Each child is waited on (in [`system_voice`]) rather than left to the
/// scheduler: an unreaped synthesizer would linger as a zombie for the life of
/// the app, and muxel has been bitten by leaked children before.
fn system_voices(text: &str) -> Vec<Command> {
    #[cfg(target_os = "macos")]
    {
        let mut say = Command::new("say");
        say.arg(text);
        vec![say]
    }
    #[cfg(target_os = "windows")]
    {
        // Single quotes delimit the PowerShell string, so an apostrophe in the
        // text ("daddy's") has to be doubled or it closes the string early.
        let escaped = text.replace('\'', "''");
        let mut ps = Command::new("powershell");
        ps.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "Add-Type -AssemblyName System.Speech; \
                 (New-Object System.Speech.Synthesis.SpeechSynthesizer).Speak('{escaped}')"
            ),
        ]);
        vec![ps]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // `--` so a line that happens to start with a dash is spoken, not parsed
        // as a flag. spd-say routes through speech-dispatcher where it is set up;
        // espeak is the fallback for boxes without it.
        let mut spd = Command::new("spd-say");
        spd.args(["--wait", "--", text]);
        let mut espeak_ng = Command::new("espeak-ng");
        espeak_ng.args(["--", text]);
        let mut espeak = Command::new("espeak");
        espeak.args(["--", text]);
        vec![spd, espeak_ng, espeak]
    }
}
