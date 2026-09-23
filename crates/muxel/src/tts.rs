//! Text-to-speech I/O: the voice muxel reads agents' replies aloud in.
//!
//! Read-aloud (the toolbar speaker, the `ReadAloud` shortcut, and auto-read when
//! an agent finishes) builds a [`VoiceConfig`] from `Settings` and calls
//! [`speak`]; the [`Speech`] it gets back stops the voice mid-sentence and says
//! when it has finished. Everything that decides *what* is said — finding the
//! reply, dropping the code — is pure and lives in `muxel_core::readaloud`.
//!
//! The Kokoro (Local) engine is behind the off-by-default `voice-local` cargo
//! feature, because onnxruntime links statically and costs ~63 MB of binary.
//! System and Provider need no feature: cpal is already here for the microphone,
//! and ureq for HTTP.
//!
//! Three engines, mirroring the speech-to-text side:
//!
//! - **System** — the synthesizer the OS already ships (`say`, SAPI, `spd-say` /
//!   `espeak`). Needs no model, no key and no network, so it is the default and
//!   the floor everything else falls back to. Its voice and pace are the OS's, so
//!   they follow the user's system voice and the rate setting.
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
//! renders at ~1.4× real time, so rendering a whole reply before playing it would
//! open with seconds of silence.
//!
//! Speech is never the only channel — the reply is still on screen. So every
//! failure here degrades rather than raises: a provider that 500s, a model that
//! won't download, a machine with no voice at all — it falls back to the system
//! voice, and failing that, stays quiet.

#[cfg(feature = "voice-local")]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use muxel_core::TtsEngine;

/// Everything a single utterance needs, snapshotted off the settings so speech
/// can run on its own thread without touching the app entity.
#[derive(Clone)]
pub struct VoiceConfig {
    pub engine: TtsEngine,
    /// Speaking pace as a multiple of normal (`muxel_core::tts::clamp_rate`
    /// range). Honored by the System and Provider voices; Kokoro speaks at its
    /// own pace.
    pub rate: f32,
    /// The OS voice to use by name (System); empty = the OS default.
    pub system_voice: String,
    /// Kokoro voice + weights (Local). Only read when Kokoro is compiled in.
    #[cfg_attr(not(feature = "voice-local"), allow(dead_code))]
    pub local_voice: String,
    #[cfg_attr(not(feature = "voice-local"), allow(dead_code))]
    pub local_model: String,
    /// Endpoint, key, model and voice (Provider). The URL and key are the ones
    /// the Speech section already stores — one provider serves both directions.
    pub provider_url: String,
    pub provider_model: String,
    pub provider_voice: String,
    pub api_key: String,
    /// Where downloaded models live (`None` if there is no data dir).
    #[cfg_attr(not(feature = "voice-local"), allow(dead_code))]
    pub models_dir: Option<PathBuf>,
}

/// One utterance in progress. Every clone controls the same utterance.
#[derive(Clone)]
pub struct Speech {
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl Speech {
    /// Silence it now: the OS voice is killed, queued audio is thrown away, and
    /// a synthesizer still rendering stops at its next chunk.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Whether it has finished — spoken to the end, stopped, or given up.
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

/// Speak `text` aloud, off the UI thread. Returns immediately with a handle to
/// stop it or ask whether it has finished.
pub fn speak(text: &str, cfg: VoiceConfig) -> Speech {
    let speech = Speech {
        stop: Arc::default(),
        done: Arc::default(),
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        speech.done.store(true, Ordering::Relaxed);
        return speech;
    }
    let handle = speech.clone();
    std::thread::spawn(move || {
        say_it(&text, &cfg, &handle.stop);
        handle.done.store(true, Ordering::Relaxed);
    });
    speech
}

/// The body of [`speak`]'s thread: synthesize and play, falling back to the OS
/// voice when nothing came out.
fn say_it(text: &str, cfg: &VoiceConfig, stop: &AtomicBool) {
    if cfg.engine == TtsEngine::System {
        system_voice(text, cfg, stop);
        return;
    }

    // Synthesis runs on its own thread and pushes finished chunks down the
    // channel, so playback can begin on the FIRST chunk instead of waiting for
    // the last. That is what makes the local voice usable: Kokoro takes seconds to
    // render a reply whole, but under a second to render its first sentence — and
    // it renders faster than the device plays, so the rest stays ahead.
    let (chunk_tx, chunks) = std::sync::mpsc::channel::<Vec<f32>>();
    let synth_cfg = cfg.clone();
    let synth_text = text.to_string();
    let producer = std::thread::spawn(move || produce(&synth_text, &synth_cfg, &chunk_tx));

    let played = match play_stream(&chunks, muxel_core::tts::SPEECH_RATE, stop) {
        Ok(n) => n,
        Err(e) => {
            log::warn!("speech playback failed: {e:#}");
            0
        }
    };
    // Hang up, so a synthesizer still rendering stops at its next chunk.
    drop(chunks);
    if stop.load(Ordering::Relaxed) {
        // Stopped: leave the producer to notice on its own rather than wait out
        // a slow provider request just to throw its answer away.
        return;
    }
    let synth = producer
        .join()
        .unwrap_or_else(|_| bail!("speech thread panicked"));

    // Nothing came out — a dead provider, a model that won't download, no audio
    // device. Fall back to the OS voice, which needs none of those things.
    if played == 0 {
        if let Err(e) = synth {
            log::warn!("speech failed, falling back to the system voice: {e:#}");
        }
        system_voice(text, cfg, stop);
    } else if let Err(e) = synth {
        // It spoke, then broke: say what happened but don't repeat the reply.
        log::warn!("speech ended early: {e:#}");
    }
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
/// blocking until the producer is done and the buffer has drained — or until
/// `stop` is raised, which silences it at once. Returns how many samples were
/// queued for playback.
///
/// Building the device stream waits for the first chunk, so a synthesizer that
/// fails outright never opens (and never has to close) an audio device.
fn play_stream(chunks: &Receiver<Vec<f32>>, rate: u32, stop: &AtomicBool) -> Result<usize> {
    // Short enough that Stop feels immediate, long enough to cost nothing.
    const POLL: Duration = Duration::from_millis(50);
    let stopped = || stop.load(Ordering::Relaxed);

    // A provider can take seconds to answer; keep an eye on Stop meanwhile.
    let first = loop {
        if stopped() {
            return Ok(0);
        }
        match chunks.recv_timeout(POLL) {
            Ok(chunk) => break chunk,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(0), // failed before a sound
        }
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

    // Feed the queue until the producer hangs up.
    while !stopped() {
        match chunks.recv_timeout(POLL) {
            Ok(chunk) => played += push(&queue, &chunk, rate, dev_rate)?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Producer is done; wait for the device to drain what is left, with a ceiling
    // well past the audio's own length so a stalled device can't wedge the thread.
    let secs = played as f32 / dev_rate.max(1) as f32;
    let deadline = std::time::Instant::now() + Duration::from_secs_f32(secs + 5.0);
    loop {
        if stopped() {
            // Silence now, rather than letting the queued audio play out.
            if let Ok(mut q) = queue.lock() {
                q.clear();
            }
            return Ok(played);
        }
        let empty = queue.lock().map(|q| q.is_empty()).unwrap_or(true);
        if empty && starving.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if std::time::Instant::now() > deadline {
            log::warn!("speech playback timed out waiting for the device");
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // The last buffer is queued, not yet audible: let the device flush it before
    // the stream drops, or the final word is clipped.
    std::thread::sleep(Duration::from_millis(150));
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
    let body = muxel_core::tts::build_speech_request(
        &cfg.provider_model,
        &cfg.provider_voice,
        text,
        cfg.rate,
    );
    let url = muxel_core::tts::speech_endpoint(&cfg.provider_url);
    // Bounded, so a stalled endpoint ends in the system-voice fallback rather than
    // a speaker button stuck on "reading" forever.
    let resp = ureq::post(&url)
        .timeout(Duration::from_secs(90))
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
#[cfg(feature = "voice-local")]
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

/// Speak through the synthesizer the OS ships. Blocks until it finishes or
/// `stop` is raised.
fn system_voice(text: &str, cfg: &VoiceConfig, stop: &AtomicBool) {
    for mut cmd in system_voices(text, cfg) {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut child) = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue; // not installed; try the next
        };
        // Waited on, never left to the scheduler: an unreaped synthesizer would
        // linger as a zombie for the life of the app, and muxel has been bitten
        // by leaked children before.
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return,
                Ok(Some(_)) => break, // it couldn't speak; try the next
                Ok(None) if stop.load(Ordering::Relaxed) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    cancel_system_voice();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
    }
    log::debug!("no speech synthesizer available; muxel stays quiet");
}

/// Silence a synthesizer that outlives the process that asked it to speak.
/// speech-dispatcher is one: `spd-say` hands the text to a daemon, so killing
/// `spd-say` would leave the daemon reading on. The others speak in-process.
fn cancel_system_voice() {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = Command::new("spd-say")
            .arg("--cancel")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The OS synthesizers to try, best first, set to the chosen voice and pace. A
/// box with none of them installed just runs out of candidates and stays quiet.
fn system_voices(text: &str, cfg: &VoiceConfig) -> Vec<Command> {
    let voice = cfg.system_voice.trim();
    #[cfg(target_os = "macos")]
    {
        // `say` falls back to the default voice itself when `-v` names one that
        // isn't installed. `--` so text that opens with a dash is spoken, not parsed.
        let wpm = muxel_core::tts::say_words_per_minute(cfg.rate).to_string();
        let mut say = Command::new("say");
        if !voice.is_empty() {
            say.args(["-v", voice]);
        }
        say.args(["-r", &wpm, "--", text]);
        vec![say]
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // Single quotes delimit the PowerShell strings, so an apostrophe in the
        // text ("don't") has to be doubled or it closes the string early.
        let escaped = text.replace('\'', "''");
        let select = if voice.is_empty() {
            String::new()
        } else {
            // A voice that isn't installed throws; keep the default instead.
            format!(
                "try {{ $s.SelectVoice('{}') }} catch {{}}; ",
                voice.replace('\'', "''")
            )
        };
        let rate = muxel_core::tts::rate_step(cfg.rate, 10);
        let mut ps = Command::new("powershell");
        ps.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "Add-Type -AssemblyName System.Speech; \
                 $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
                 {select}$s.Rate = {rate}; $s.Speak('{escaped}')"
            ),
        ]);
        // CREATE_NO_WINDOW: a GUI app must not flash a console per utterance.
        ps.creation_flags(0x0800_0000);
        vec![ps]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // `--` so a line that happens to start with a dash is spoken, not parsed
        // as a flag. spd-say routes through speech-dispatcher where it is set up;
        // espeak is the fallback for boxes without it.
        let step = muxel_core::tts::rate_step(cfg.rate, 100).to_string();
        let wpm = muxel_core::tts::espeak_words_per_minute(cfg.rate).to_string();
        let mut spd = Command::new("spd-say");
        spd.args(["--wait", "-r", &step]);
        if !voice.is_empty() {
            spd.args(["-y", voice]);
        }
        spd.args(["--", text]);
        let espeak = |program: &str| {
            let mut cmd = Command::new(program);
            cmd.args(["-s", &wpm]);
            if !voice.is_empty() {
                cmd.args(["-v", voice]);
            }
            cmd.args(["--", text]);
            cmd
        };
        vec![spd, espeak("espeak-ng"), espeak("espeak")]
    }
}

/// The voices the OS synthesizer offers, as `(name, locale)`, for the settings
/// picker. Blocking (it runs the OS's own listing), so call it off the UI thread.
/// Empty where there's no reliable way to ask — Linux, whose voice names depend on
/// which speech-dispatcher module is configured; there the name is typed instead.
pub fn system_voice_list() -> Vec<(String, String)> {
    #[cfg(target_os = "macos")]
    {
        Command::new("say")
            .args(["-v", "?"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|out| muxel_core::tts::parse_say_voices(&String::from_utf8_lossy(&out.stdout)))
            .unwrap_or_default()
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Add-Type -AssemblyName System.Speech; \
                 (New-Object System.Speech.Synthesis.SpeechSynthesizer).GetInstalledVoices() \
                 | ForEach-Object { $_.VoiceInfo.Name + '|' + $_.VoiceInfo.Culture.Name }",
            ])
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map(|out| muxel_core::tts::parse_voice_lines(&String::from_utf8_lossy(&out.stdout)))
            .unwrap_or_default()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Vec::new()
    }
}
