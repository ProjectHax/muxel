//! muxel's built-in alert sounds, synthesized rather than shipped as files.
//!
//! Each is a few struck notes with a soft mallet-on-glass timbre: every note is
//! a fundamental plus a handful of partials, each fading on its own (the higher
//! ones faster), after a 4 ms attack that keeps the onset from clicking.
//!
//! - **Done** — a rising fourth, G5 then C6 over a soft C5: settled, "finished".
//! - **Blocked** — the same bright B5 + E6 knocked twice: "come here".
//!
//! Both are normalized to the same peak, so the volume setting treats them alike.

use crate::sound_alert::AlertSound;
use std::f64::consts::{PI, TAU};

/// The sample rate the chimes are rendered at; playback resamples to the device's.
pub const RATE: u32 = 48_000;

const ATTACK_SECS: f64 = 0.004;
/// The tail tapers to silence over this long, so no chime ends on a cut.
const FADE_SECS: f64 = 0.25;
const PEAK: f64 = 0.89;
/// A note's partials: (frequency ratio, amplitude, decay relative to the
/// fundamental's). The 4.16 is deliberately off the harmonic series — it is
/// what makes the note ring like glass rather than an organ.
const PARTIALS: [(f64, f64, f64); 4] = [
    (1.0, 1.0, 1.0),
    (2.0, 0.42, 0.55),
    (3.0, 0.16, 0.33),
    (4.16, 0.07, 0.2),
];

/// One struck note.
struct Strike {
    /// Onset, in seconds from the start.
    at: f64,
    freq: f64,
    gain: f64,
    /// Time constant of the fundamental's exponential decay, in seconds.
    decay: f64,
    /// Scale on the overtones (0 = a pure tone).
    brightness: f64,
}

const fn strike(at: f64, freq: f64, gain: f64, decay: f64, brightness: f64) -> Strike {
    Strike {
        at,
        freq,
        gain,
        decay,
        brightness,
    }
}

const DONE_SECS: f64 = 1.45;
const DONE: [Strike; 3] = [
    strike(0.0, 783.99, 0.75, 0.28, 1.0),  // G5
    strike(0.11, 1046.5, 1.0, 0.42, 1.0),  // C6
    strike(0.11, 523.25, 0.22, 0.42, 0.0), // C5, underneath
];

const BLOCKED_SECS: f64 = 1.0;
const BLOCKED: [Strike; 4] = [
    strike(0.0, 1318.51, 0.85, 0.12, 1.3), // E6
    strike(0.0, 987.77, 0.47, 0.12, 1.3),  // B5
    strike(0.17, 1318.51, 1.0, 0.26, 1.3),
    strike(0.17, 987.77, 0.55, 0.26, 1.3),
];

/// `sound`'s built-in chime: mono samples at [`RATE`].
pub fn render(sound: AlertSound) -> Vec<f32> {
    let (secs, strikes): (f64, &[Strike]) = match sound {
        AlertSound::Done => (DONE_SECS, &DONE),
        AlertSound::Blocked => (BLOCKED_SECS, &BLOCKED),
    };
    let rate = f64::from(RATE);
    let len = (secs * rate) as usize;
    let mut out = vec![0.0f64; len];
    for s in strikes {
        let start = (s.at * rate) as usize;
        for (i, slot) in out.iter_mut().enumerate().skip(start) {
            let t = (i - start) as f64 / rate;
            let attack = if t < ATTACK_SECS {
                0.5 - 0.5 * (PI * t / ATTACK_SECS).cos()
            } else {
                1.0
            };
            let ring: f64 = PARTIALS
                .iter()
                .filter(|(ratio, _, _)| s.freq * ratio < rate / 2.0)
                .map(|&(ratio, amp, decay)| {
                    let amp = if ratio == 1.0 {
                        amp
                    } else {
                        amp * s.brightness
                    };
                    amp * (-t / (s.decay * decay)).exp() * (TAU * s.freq * ratio * t).sin()
                })
                .sum();
            *slot += s.gain * attack * ring;
        }
    }
    let fade = (FADE_SECS * rate) as usize;
    for (k, slot) in out[len - fade..].iter_mut().enumerate() {
        *slot *= 0.5 + 0.5 * (PI * k as f64 / fade as f64).cos();
    }
    let peak = out.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    out.iter().map(|v| (v * PEAK / peak) as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::{BLOCKED_SECS, DONE_SECS, PEAK, RATE, render};
    use crate::sound_alert::AlertSound;

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    #[test]
    fn chimes_are_their_length_and_normalized() {
        for (sound, secs) in [
            (AlertSound::Done, DONE_SECS),
            (AlertSound::Blocked, BLOCKED_SECS),
        ] {
            let samples = render(sound);
            assert_eq!(samples.len(), (secs * f64::from(RATE)) as usize);
            assert!(samples.iter().all(|v| v.is_finite()));
            assert!((peak(&samples) - PEAK as f32).abs() < 1e-4, "{sound:?}");
        }
    }

    #[test]
    fn chimes_start_and_end_without_a_click() {
        for sound in [AlertSound::Done, AlertSound::Blocked] {
            let samples = render(sound);
            assert_eq!(samples[0], 0.0, "{sound:?} starts at rest");
            // The attack ramps in rather than jumping.
            assert!(peak(&samples[..48]) < 0.1, "{sound:?} attack");
            assert!(
                samples.last().unwrap().abs() < 1e-6,
                "{sound:?} ends at rest"
            );
        }
    }

    #[test]
    fn blocked_knocks_twice_and_done_does_not_sound_like_it() {
        let blocked = render(AlertSound::Blocked);
        // The second knock (at 170 ms) lands well after the first has faded.
        let rate = RATE as usize;
        let before = peak(&blocked[rate * 15 / 100..rate * 17 / 100]);
        let after = peak(&blocked[rate * 17 / 100..rate * 19 / 100]);
        assert!(after > before * 1.5, "{before} → {after}");
        assert_ne!(render(AlertSound::Done)[..4800], blocked[..4800]);
    }
}
