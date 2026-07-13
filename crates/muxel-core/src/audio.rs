//! Audio sample plumbing shared by speech-to-text and text-to-speech.
//!
//! Capture hands us interleaved samples at the device's rate and channel count;
//! whisper wants 16 kHz mono; a synthesizer hands us 24 kHz mono and the output
//! device wants its own rate again. All of it is the same two operations, so they
//! live here once, pure and tested, rather than in each direction's I/O module.

/// Downmix interleaved `channels`-channel `samples` to mono by averaging.
pub fn downmix(samples: &[f32], channels: u16) -> Vec<f32> {
    let channels = channels as usize;
    if samples.is_empty() || channels == 0 {
        return Vec::new();
    }
    if channels == 1 {
        return samples.to_vec();
    }
    (0..samples.len() / channels)
        .map(|frame| {
            let base = frame * channels;
            samples[base..base + channels].iter().sum::<f32>() / channels as f32
        })
        .collect()
}

/// Resample mono `samples` from `src_rate` to `dst_rate` by linear interpolation.
///
/// Linear is adequate both for speech recognition and for playing back speech;
/// swap for a sinc resampler (`rubato`) if either ever proves it isn't.
pub fn resample_linear(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if samples.is_empty() || src_rate == 0 || dst_rate == 0 {
        return Vec::new();
    }
    if src_rate == dst_rate || samples.len() < 2 {
        return samples.to_vec();
    }
    let dst_len = ((samples.len() as u64 * dst_rate as u64) / src_rate as u64) as usize;
    if dst_len == 0 {
        return Vec::new();
    }
    let step = src_rate as f64 / dst_rate as f64;
    (0..dst_len)
        .map(|i| {
            let pos = i as f64 * step;
            let idx = pos.floor() as usize;
            let frac = (pos - idx as f64) as f32;
            let a = samples[idx.min(samples.len() - 1)];
            let b = samples[(idx + 1).min(samples.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{downmix, resample_linear};

    #[test]
    fn downmix_averages_channels() {
        assert_eq!(downmix(&[0.5, -0.5, 1.0, 0.0], 2), vec![0.0, 0.5]);
        // Mono passes straight through; degenerate input is empty, not a panic.
        assert_eq!(downmix(&[0.1, 0.2], 1), vec![0.1, 0.2]);
        assert!(downmix(&[0.1], 0).is_empty());
        assert!(downmix(&[], 2).is_empty());
    }

    #[test]
    fn resample_scales_length_by_the_rate_ratio() {
        let src: Vec<f32> = (0..300).map(|i| i as f32 / 300.0).collect();
        // Down: 48k → 16k is a third of the samples.
        assert_eq!(resample_linear(&src, 48_000, 16_000).len(), 100);
        // Up: 24k → 48k is twice as many.
        assert_eq!(resample_linear(&src, 24_000, 48_000).len(), 600);
    }

    #[test]
    fn resample_passes_through_and_survives_nonsense() {
        let mono = vec![0.1, 0.2, 0.3];
        assert_eq!(resample_linear(&mono, 24_000, 24_000), mono);
        assert!(resample_linear(&[], 24_000, 48_000).is_empty());
        assert!(resample_linear(&mono, 0, 48_000).is_empty());
        assert!(resample_linear(&mono, 24_000, 0).is_empty());
    }

    #[test]
    fn resample_interpolates_between_neighbours() {
        // 2 samples at 1 Hz → 4 samples at 2 Hz: the odd taps land midway.
        let out = resample_linear(&[0.0, 1.0], 1, 2);
        assert_eq!(out.len(), 4);
        assert!((out[0] - 0.0).abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6);
    }
}
