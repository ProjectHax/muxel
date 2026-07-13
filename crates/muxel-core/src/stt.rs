//! Speech-to-text: the pure, I/O-free pieces shared by the app's `stt` module.
//!
//! Audio capture (cpal) and transcription (whisper.cpp / a cloud provider) live
//! in the `muxel` crate. Everything here is deterministic and unit-tested: the
//! engine choice, resampling to whisper's 16 kHz mono, WAV encoding for the
//! provider upload, the `multipart/form-data` body, and model URLs.

use serde::{Deserialize, Serialize};

/// Sample rate whisper.cpp requires (16 kHz mono f32).
pub const WHISPER_RATE: u32 = 16_000;

/// Which transcription engine a speech-to-text run uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SttEngine {
    /// In-process whisper.cpp, fully offline (model in the data dir).
    #[default]
    Local,
    /// A cloud OpenAI-compatible `/audio/transcriptions` endpoint.
    Provider,
}

/// Downmix interleaved `channels`-channel `samples` to mono and resample from
/// `src_rate` to [`WHISPER_RATE`], which is what whisper.cpp will accept.
pub fn resample_to_16k_mono(samples: &[f32], src_rate: u32, channels: u16) -> Vec<f32> {
    if src_rate == 0 {
        return Vec::new();
    }
    let mono = crate::audio::downmix(samples, channels);
    crate::audio::resample_linear(&mono, src_rate, WHISPER_RATE)
}

/// Encode 16 kHz mono f32 `samples` as a 16-bit PCM WAV (44-byte header + data),
/// for uploading to a provider that wants a real audio file.
pub fn encode_wav_16k_mono(samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = WHISPER_RATE * 2; // mono, 2 bytes/sample
    let mut buf = Vec::with_capacity(44 + data_len as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_len).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // channels = mono
    buf.extend_from_slice(&WHISPER_RATE.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

const MULTIPART_BOUNDARY: &str = "----muxelSTTboundaryK9dQ2fLp7xVzR3nH";

fn push_text_field(body: &mut Vec<u8>, name: &str, value: &str) {
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(b"\r\n");
}

/// Build an OpenAI-compatible `POST /audio/transcriptions` body: the `model`
/// field, an optional `language`, and the WAV as the `file` part. Returns the
/// `Content-Type` header value (with boundary) and the body bytes.
pub fn build_transcription_multipart(
    wav: &[u8],
    model: &str,
    language: Option<&str>,
) -> (String, Vec<u8>) {
    let mut body = Vec::with_capacity(wav.len() + 256);
    push_text_field(&mut body, "model", model);
    if let Some(lang) = language.filter(|l| !l.is_empty()) {
        push_text_field(&mut body, "language", lang);
    }
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(wav);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());

    let content_type = format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}");
    (content_type, body)
}

/// Spoken phrase that triggers the wake command by default.
pub const DEFAULT_WAKE_PHRASE: &str = "wake up daddy's home";

/// Fold spoken text down to bare lowercase words for matching: whisper
/// capitalizes and punctuates freely ("Wake up, Daddy's home!"), and an
/// apostrophe joins rather than splits (`daddy's` → `daddys`).
pub fn normalize_spoken(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut gap = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.extend(ch.to_lowercase());
        } else if ch.is_whitespace() {
            gap = true;
        }
    }
    out
}

/// Whether `transcript` speaks the wake `phrase` — case-, punctuation- and
/// filler-insensitive, but on whole words ("homestead" is not "home"). An empty
/// phrase never matches, so a blank setting can't fire on every dictation.
pub fn matches_wake_phrase(transcript: &str, phrase: &str) -> bool {
    let needle = normalize_spoken(phrase);
    if needle.is_empty() {
        return false;
    }
    let haystack = normalize_spoken(transcript);
    if haystack.is_empty() {
        return false;
    }
    format!(" {haystack} ").contains(&format!(" {needle} "))
}

/// Which greeting the wake command opens with. The spoken wording itself lives
/// in the app (it goes through `t()`); only the choice is decided here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DayPart {
    Morning,
    Afternoon,
    Evening,
}

/// The part of the day a local `hour` (0–23) falls in. Out-of-range hours read
/// as evening rather than panicking — a greeting is never worth a crash.
pub fn day_part(hour: u32) -> DayPart {
    match hour {
        5..=11 => DayPart::Morning,
        12..=17 => DayPart::Afternoon,
        _ => DayPart::Evening,
    }
}

/// Local filename for a whisper.cpp ggml model (e.g. `"base"` → `ggml-base.bin`).
pub fn whisper_model_filename(model: &str) -> String {
    format!("ggml-{model}.bin")
}

/// HuggingFace download URL for a whisper.cpp ggml model.
pub fn whisper_model_url(model: &str) -> String {
    format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
        whisper_model_filename(model)
    )
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_WAKE_PHRASE, DayPart, WHISPER_RATE, build_transcription_multipart, day_part,
        encode_wav_16k_mono, matches_wake_phrase, normalize_spoken, resample_to_16k_mono,
        whisper_model_filename, whisper_model_url,
    };

    #[test]
    fn day_part_splits_the_clock_and_never_panics() {
        assert_eq!(day_part(5), DayPart::Morning);
        assert_eq!(day_part(11), DayPart::Morning);
        assert_eq!(day_part(12), DayPart::Afternoon);
        assert_eq!(day_part(17), DayPart::Afternoon);
        assert_eq!(day_part(18), DayPart::Evening);
        assert_eq!(day_part(4), DayPart::Evening); // small hours
        assert_eq!(day_part(99), DayPart::Evening); // nonsense hour, not a panic
    }

    #[test]
    fn resample_downmixes_and_hits_16k() {
        // 48 kHz stereo → 16 kHz mono: length scales by (16000/48000) = 1/3.
        let frames = 4800; // 0.1s at 48k
        let stereo: Vec<f32> = (0..frames).flat_map(|_| [0.5f32, -0.5f32]).collect();
        let out = resample_to_16k_mono(&stereo, 48_000, 2);
        // Downmix of (0.5, -0.5) is 0.0.
        assert!(out.iter().all(|s| s.abs() < 1e-6));
        assert_eq!(out.len(), frames * WHISPER_RATE as usize / 48_000);
    }

    #[test]
    fn resample_passthrough_when_already_16k_mono() {
        let mono = vec![0.1, 0.2, 0.3, 0.4];
        assert_eq!(resample_to_16k_mono(&mono, WHISPER_RATE, 1), mono);
    }

    #[test]
    fn resample_empty_and_degenerate_are_safe() {
        assert!(resample_to_16k_mono(&[], 48_000, 2).is_empty());
        assert!(resample_to_16k_mono(&[0.1, 0.2], 0, 1).is_empty());
        assert!(resample_to_16k_mono(&[0.1, 0.2], 48_000, 0).is_empty());
    }

    #[test]
    fn wav_header_is_well_formed() {
        let wav = encode_wav_16k_mono(&[0.0, 1.0, -1.0]);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        // 44-byte header + 3 samples * 2 bytes.
        assert_eq!(wav.len(), 44 + 6);
        // data chunk length field.
        assert_eq!(u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]), 6);
        // sample rate field.
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            WHISPER_RATE
        );
        // 1.0 → i16::MAX, -1.0 → -i16::MAX (clamped).
        assert_eq!(i16::from_le_bytes([wav[46], wav[47]]), i16::MAX);
        assert_eq!(i16::from_le_bytes([wav[48], wav[49]]), -i16::MAX);
    }

    #[test]
    fn multipart_contains_fields_and_file() {
        let (ct, body) = build_transcription_multipart(b"WAVDATA", "whisper-1", Some("en"));
        assert!(ct.starts_with("multipart/form-data; boundary="));
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"));
        assert!(s.contains("name=\"language\"\r\n\r\nen\r\n"));
        assert!(s.contains("name=\"file\"; filename=\"audio.wav\""));
        assert!(s.contains("WAVDATA"));
        assert!(s.trim_end().ends_with("--"));
    }

    #[test]
    fn multipart_omits_empty_language() {
        let (_, body) = build_transcription_multipart(b"x", "m", None);
        assert!(!String::from_utf8_lossy(&body).contains("language"));
        let (_, body2) = build_transcription_multipart(b"x", "m", Some(""));
        assert!(!String::from_utf8_lossy(&body2).contains("language"));
    }

    #[test]
    fn normalize_folds_case_punctuation_and_spacing() {
        assert_eq!(
            normalize_spoken("  Wake up,\n Daddy's home! "),
            "wake up daddys home"
        );
        assert_eq!(normalize_spoken("...!?"), "");
    }

    #[test]
    fn wake_phrase_matches_however_whisper_punctuates_it() {
        for said in [
            "wake up daddy's home",
            "Wake up, Daddy's home!",
            "  wake  up   daddys home  ",
            "Okay — wake up, daddy's home. Let's go.",
        ] {
            assert!(matches_wake_phrase(said, DEFAULT_WAKE_PHRASE), "{said:?}");
        }
    }

    #[test]
    fn wake_phrase_rejects_near_misses() {
        assert!(!matches_wake_phrase("wake up", DEFAULT_WAKE_PHRASE));
        assert!(!matches_wake_phrase(
            "write a wake up daddy test",
            DEFAULT_WAKE_PHRASE
        ));
        // Whole words only: "home" must not match inside "homestead".
        assert!(!matches_wake_phrase(
            "wake up daddys homestead",
            DEFAULT_WAKE_PHRASE
        ));
    }

    #[test]
    fn wake_phrase_is_configurable_and_blank_never_fires() {
        assert!(matches_wake_phrase("Rise and shine!", "rise and shine"));
        // A blank phrase would otherwise fire on every dictation.
        assert!(!matches_wake_phrase("anything at all", ""));
        assert!(!matches_wake_phrase("anything at all", "  ,, "));
        assert!(!matches_wake_phrase("", DEFAULT_WAKE_PHRASE));
    }

    #[test]
    fn model_url_and_filename() {
        assert_eq!(whisper_model_filename("base"), "ggml-base.bin");
        assert_eq!(
            whisper_model_url("small"),
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin"
        );
    }
}
