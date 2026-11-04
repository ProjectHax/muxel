//! Sound alerts' I/O: the chime for an agent that finished or is blocked.
//!
//! An alert plays the user's own sound file when they chose one (decoded here
//! with symphonia: WAV, AIFF, CAF, MP3, OGG Vorbis, FLAC, M4A/AAC), else the
//! built-in chime synthesized by `muxel_core::chime`. Either way it goes out
//! through cpal like speech does (`tts::play_clip`). *When* an alert plays is
//! decided in `muxel_core::sound_alert`.
//!
//! A chime is a nicety on top of the status badge and the notification feed,
//! so nothing here raises: a file that won't decode falls back to the built-in
//! chime, and a machine with no audio device stays quiet.

use anyhow::{Context, Result, bail};
use muxel_core::sound_alert::AlertSound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as DecodeError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// A custom sound longer than this is cut short, so picking a song can't hold
/// the speakers for minutes on every alert.
pub const MAX_SECS: usize = 10;
/// A cut-short sound fades out over this long instead of stopping dead.
const CUT_FADE_SECS: f32 = 0.25;

/// Mono samples and their rate.
pub struct Clip {
    pub samples: Vec<f32>,
    pub rate: u32,
}

impl Clip {
    /// `sound`'s built-in chime.
    pub fn chime(sound: AlertSound) -> Self {
        Self {
            samples: muxel_core::chime::render(sound),
            rate: muxel_core::chime::RATE,
        }
    }
}

/// Decode the audio file at `path` to mono, at most [`MAX_SECS`] long.
pub fn decode(path: &Path) -> Result<Clip> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("not an audio format muxel can play")?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .context("no audio in the file")?;
    let track_id = track.id;
    let rate = track
        .codec_params
        .sample_rate
        .context("the file doesn't say its sample rate")?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .context("not an audio codec muxel can play")?;
    let limit = rate as usize * MAX_SECS;
    let mut samples = Vec::new();
    while samples.len() < limit {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            // The end of the stream (or of its first part, for a chained Ogg).
            Err(DecodeError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(DecodeError::ResetRequired) => break,
            Err(e) => return Err(e).context("read the audio"),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A damaged packet costs a moment of sound, not the whole clip.
            Err(DecodeError::DecodeError(_)) => continue,
            Err(e) => return Err(e).context("decode the audio"),
        };
        let spec = *decoded.spec();
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        samples.extend(muxel_core::audio::downmix(
            buf.samples(),
            spec.channels.count() as u16,
        ));
    }
    if samples.len() > limit {
        samples.truncate(limit);
    }
    if samples.len() == limit {
        let fade = ((rate as f32 * CUT_FADE_SECS) as usize).min(samples.len());
        let start = samples.len() - fade;
        for (k, s) in samples[start..].iter_mut().enumerate() {
            *s *= 1.0 - k as f32 / fade as f32;
        }
    }
    if samples.is_empty() {
        bail!("the file has no sound in it");
    }
    Ok(Clip { samples, rate })
}

/// Play `clip` at `gain` on its own thread; raising `stop` cuts it off.
pub fn play(clip: Clip, gain: f32, stop: Arc<AtomicBool>) {
    spawn(move || clip, gain, stop);
}

/// Ring `sound`: the user's `file` if it decodes, else the built-in chime.
/// Loads and plays on its own thread.
pub fn ring(sound: AlertSound, file: Option<PathBuf>, gain: f32, stop: Arc<AtomicBool>) {
    spawn(
        move || {
            file.and_then(|path| {
                decode(&path)
                    .map_err(|e| log::warn!("alert sound {}: {e:#}", path.display()))
                    .ok()
            })
            .unwrap_or_else(|| Clip::chime(sound))
        },
        gain,
        stop,
    );
}

fn spawn(load: impl FnOnce() -> Clip + Send + 'static, gain: f32, stop: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("muxel-alert-sound".into())
        .spawn(move || {
            let clip = load();
            let samples = clip
                .samples
                .iter()
                .map(|s| (s * gain).clamp(-1.0, 1.0))
                .collect();
            if let Err(e) = crate::tts::play_clip(samples, clip.rate, &stop) {
                log::warn!("alert sound: {e:#}");
            }
        });
    if let Err(e) = spawned {
        log::warn!("alert sound thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::{CUT_FADE_SECS, Clip, MAX_SECS, decode};
    use muxel_core::sound_alert::AlertSound;
    use std::path::{Path, PathBuf};

    /// A 16-bit PCM WAV of interleaved `frames` (one `[i16]` per frame).
    fn write_wav(path: &Path, rate: u32, frames: &[Vec<i16>]) {
        let channels = frames[0].len() as u16;
        let data: Vec<u8> = frames
            .iter()
            .flatten()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut out = Vec::new();
        out.extend(b"RIFF");
        out.extend((36 + data.len() as u32).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(channels.to_le_bytes());
        out.extend(rate.to_le_bytes());
        out.extend((rate * u32::from(channels) * 2).to_le_bytes());
        out.extend((channels * 2).to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        std::fs::write(path, out).unwrap();
    }

    fn temp_file(ext: &str) -> PathBuf {
        let dir = crate::test_support::short_temp_path();
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("sound.{ext}"))
    }

    #[test]
    fn decodes_a_stereo_wav_to_mono_at_its_rate() {
        let path = temp_file("wav");
        // Left and right cancel to silence, then agree at half scale.
        let frames: Vec<Vec<i16>> = (0..800)
            .map(|i| {
                if i < 400 {
                    vec![16384, -16384]
                } else {
                    vec![16384, 16384]
                }
            })
            .collect();
        write_wav(&path, 8000, &frames);
        let clip = decode(&path).expect("decode");
        assert_eq!(clip.rate, 8000);
        assert_eq!(clip.samples.len(), 800);
        assert!(clip.samples[..400].iter().all(|s| s.abs() < 1e-3));
        assert!(clip.samples[400..].iter().all(|s| (s - 0.5).abs() < 1e-3));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_long_file_is_cut_to_the_limit_and_faded_out() {
        let path = temp_file("wav");
        let rate = 4000;
        let frames = vec![vec![16384i16]; rate * (MAX_SECS + 2)];
        write_wav(&path, rate as u32, &frames);
        let clip = decode(&path).expect("decode");
        assert_eq!(clip.samples.len(), rate * MAX_SECS);
        assert!(
            clip.samples.last().unwrap().abs() < 0.01,
            "faded to silence"
        );
        let fade = (rate as f32 * CUT_FADE_SECS) as usize;
        let before_fade = clip.samples[clip.samples.len() - fade - 1];
        assert!(
            (before_fade - 0.5).abs() < 1e-3,
            "untouched before the fade"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_file_that_isnt_audio_is_an_error_not_a_panic() {
        let path = temp_file("mp3");
        std::fs::write(&path, b"definitely not an mp3").unwrap();
        assert!(decode(&path).is_err());
        assert!(decode(&path.with_file_name("missing.wav")).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_builtin_chimes_are_ready_to_play() {
        for sound in [AlertSound::Done, AlertSound::Blocked] {
            let clip = Clip::chime(sound);
            assert_eq!(clip.rate, muxel_core::chime::RATE);
            assert!(!clip.samples.is_empty());
        }
    }
}
