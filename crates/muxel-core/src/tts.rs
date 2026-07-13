//! Text-to-speech: the pure, I/O-free pieces shared by the app's `tts` module.
//!
//! Playback (cpal), the cloud request (ureq) and Kokoro inference (onnxruntime)
//! live in the `muxel` crate. Everything here is deterministic and unit-tested:
//! the engine choice, the provider's request body, decoding its raw-PCM reply,
//! and the model/voice download URLs.

use serde::{Deserialize, Serialize};

/// Sample rate of every voice muxel speaks with: the OpenAI-compatible `pcm`
/// response format and Kokoro's output are both 24 kHz mono.
pub const SPEECH_RATE: u32 = 24_000;

/// Which synthesizer speaks muxel's replies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TtsEngine {
    /// The voice the OS already ships (`say`, SAPI, `spd-say`/`espeak`). Always
    /// available, needs no model and no key, and sounds like it.
    #[default]
    System,
    /// Kokoro-82M, in-process and fully offline (model in the data dir).
    Local,
    /// A cloud OpenAI-compatible `/audio/speech` endpoint.
    Provider,
}

/// Default cloud voice (OpenAI's set: alloy, echo, fable, onyx, nova, shimmer).
pub const DEFAULT_TTS_VOICE: &str = "onyx";
/// Default cloud model.
pub const DEFAULT_TTS_PROVIDER_MODEL: &str = "tts-1";
/// Default Kokoro voice — a British male read, which is the one that sounds like
/// a house AI rather than a phone menu.
pub const DEFAULT_KOKORO_VOICE: &str = "bm_george";
/// Default Kokoro weights: int8, ~89 MB. The fp32 `model.onnx` is ~325 MB for a
/// difference nobody notices on two sentences of speech.
pub const DEFAULT_KOKORO_MODEL: &str = "model_quantized";

/// The Kokoro voices muxel offers. Kokoro ships many more; these are the English
/// reads worth putting in a settings row.
pub const KOKORO_VOICES: &[&str] = &[
    "bm_george",
    "bm_lewis",
    "am_michael",
    "am_adam",
    "af_heart",
    "bf_emma",
];

/// Build the JSON body for an OpenAI-compatible `POST /audio/speech`.
///
/// `response_format: "pcm"` asks for raw 24 kHz 16-bit mono little-endian samples
/// rather than MP3 — the whole point being that raw PCM needs no audio decoder,
/// so muxel can play the reply without taking on a codec dependency.
pub fn build_speech_request(model: &str, voice: &str, text: &str) -> String {
    let body = serde_json::json!({
        "model": model,
        "voice": voice,
        "input": text,
        "response_format": "pcm",
    });
    body.to_string()
}

/// Decode raw 16-bit signed little-endian PCM into f32 samples in `-1.0..=1.0`.
/// A trailing odd byte is ignored rather than panicking — a truncated response
/// should cost you the last sample, not the process.
pub fn decode_pcm_s16le(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / -(i16::MIN as f32))
        .collect()
}

/// Split spoken text into sentences, keeping their terminators.
///
/// This is what makes the local voice usable: Kokoro synthesizes at ~1.4× real
/// time, so waiting for a whole paragraph means several seconds of silence before
/// the first word. Synthesized a sentence at a time, the first one starts playing
/// almost immediately and the rest is produced faster than it is consumed.
pub fn sentences(text: &str) -> Vec<String> {
    fn ends_a_sentence(ch: char) -> bool {
        matches!(ch, '.' | '!' | '?' | '\n')
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        current.push(ch);
        // A *run* of terminators ("...", "?!") closes one sentence, not one per
        // mark — otherwise an ellipsis would hand the synthesizer a couple of bare
        // "." fragments to read out.
        let closing = ends_a_sentence(ch) && !chars.peek().copied().is_some_and(ends_a_sentence);
        if closing {
            let s = current.trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
            current.clear();
        }
    }
    let tail = current.trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

/// `POST` URL for a provider's speech endpoint, from its base URL.
pub fn speech_endpoint(base_url: &str) -> String {
    format!("{}/audio/speech", base_url.trim_end_matches('/'))
}

/// Local filename for a Kokoro model (`"model_quantized"` → `kokoro-model_quantized.onnx`).
/// Namespaced so it can't collide with the whisper `ggml-*.bin` files beside it.
pub fn kokoro_model_filename(model: &str) -> String {
    format!("kokoro-{model}.onnx")
}

/// Subdirectory (under the models dir) holding the Kokoro voice packs.
///
/// The voices live in their own folder rather than beside the whisper `ggml-*.bin`
/// files because Kokoro looks a voice up by its file *stem*: the pack for
/// `bm_george` has to be named exactly `bm_george.bin`, so it cannot carry a
/// disambiguating prefix. The directory does the disambiguating instead.
pub const KOKORO_VOICE_DIR: &str = "kokoro-voices";

/// Local filename for a Kokoro voice pack (`"bm_george"` → `bm_george.bin`).
pub fn kokoro_voice_filename(voice: &str) -> String {
    format!("{voice}.bin")
}

const KOKORO_REPO: &str = "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main";

/// HuggingFace download URL for a Kokoro model.
pub fn kokoro_model_url(model: &str) -> String {
    format!("{KOKORO_REPO}/onnx/{model}.onnx")
}

/// HuggingFace download URL for a Kokoro voice pack.
pub fn kokoro_voice_url(voice: &str) -> String {
    format!("{KOKORO_REPO}/voices/{voice}.bin")
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_KOKORO_MODEL, DEFAULT_KOKORO_VOICE, KOKORO_VOICES, build_speech_request,
        decode_pcm_s16le, kokoro_model_filename, kokoro_model_url, kokoro_voice_filename,
        kokoro_voice_url, speech_endpoint,
    };

    #[test]
    fn speech_request_asks_for_raw_pcm() {
        let body = build_speech_request("tts-1", "onyx", "All systems online.");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "tts-1");
        assert_eq!(v["voice"], "onyx");
        assert_eq!(v["input"], "All systems online.");
        // Raw PCM is what lets us play the reply with no audio decoder.
        assert_eq!(v["response_format"], "pcm");
    }

    #[test]
    fn speech_request_escapes_rather_than_breaks() {
        // The greeting has an apostrophe in it; quotes/newlines must not corrupt JSON.
        let body = build_speech_request("m", "v", "\"Daddy's\" home.\nOnline.");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["input"], "\"Daddy's\" home.\nOnline.");
    }

    #[test]
    fn pcm_decodes_to_normalized_floats() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0i16.to_le_bytes());
        bytes.extend_from_slice(&i16::MAX.to_le_bytes());
        bytes.extend_from_slice(&i16::MIN.to_le_bytes());
        let out = decode_pcm_s16le(&bytes);
        assert_eq!(out.len(), 3);
        assert!((out[0] - 0.0).abs() < 1e-6);
        assert!((out[1] - 1.0).abs() < 1e-3);
        assert!((out[2] + 1.0).abs() < 1e-6);
        // Never above full scale, whatever the input.
        assert!(out.iter().all(|s| (-1.0..=1.0).contains(s)));
    }

    #[test]
    fn pcm_ignores_a_truncated_trailing_byte() {
        // A cut-off response costs the last sample, not a panic.
        assert_eq!(decode_pcm_s16le(&[0, 0, 7]).len(), 1);
        assert!(decode_pcm_s16le(&[]).is_empty());
    }

    #[test]
    fn sentences_split_on_terminators_and_keep_them() {
        assert_eq!(
            super::sentences("Good evening. Two agents are offline. Bringing them back!"),
            vec![
                "Good evening.",
                "Two agents are offline.",
                "Bringing them back!"
            ]
        );
        // A trailing fragment with no terminator is still spoken, not dropped.
        assert_eq!(
            super::sentences("All online. Standing by"),
            vec!["All online.", "Standing by"]
        );
        assert!(super::sentences("   ").is_empty());
        assert!(super::sentences("").is_empty());
        // Runs of terminators don't produce empty sentences to synthesize.
        assert_eq!(super::sentences("Wait... go!"), vec!["Wait...", "go!"]);
    }

    #[test]
    fn endpoint_tolerates_a_trailing_slash() {
        let want = "https://api.openai.com/v1/audio/speech";
        assert_eq!(speech_endpoint("https://api.openai.com/v1"), want);
        assert_eq!(speech_endpoint("https://api.openai.com/v1/"), want);
    }

    #[test]
    fn kokoro_paths_are_namespaced_away_from_whisper() {
        assert_eq!(
            kokoro_model_filename(DEFAULT_KOKORO_MODEL),
            "kokoro-model_quantized.onnx"
        );
        // Kokoro resolves a voice by file stem, so the pack keeps its bare name and
        // the KOKORO_VOICE_DIR folder is what keeps it clear of the ggml-*.bin files.
        assert_eq!(kokoro_voice_filename(DEFAULT_KOKORO_VOICE), "bm_george.bin");
        assert_eq!(
            std::path::Path::new(&kokoro_voice_filename(DEFAULT_KOKORO_VOICE))
                .file_stem()
                .and_then(|s| s.to_str()),
            Some(DEFAULT_KOKORO_VOICE),
        );
        assert!(kokoro_model_url("model_quantized").ends_with("/onnx/model_quantized.onnx"));
        assert!(kokoro_voice_url("bm_george").ends_with("/voices/bm_george.bin"));
        assert!(KOKORO_VOICES.contains(&DEFAULT_KOKORO_VOICE));
    }
}
