//! The audio features a speaker model reads: an 80-band log-mel spectrogram ("fbank"), computed the way Kaldi and its
//! descendants compute it, because that is what the speaker models were trained on.
//!
//! The recipe (25 ms frames every 10 ms, no padding at the edges):
//! samples are scaled to 16-bit integer range, each frame has its average removed, is pre-emphasised (0.97), is multiplied by
//! a Hamming window, is padded to 512 points and Fourier-transformed; the power in each of 80 triangular mel bands between
//! 20 Hz and 8 kHz is taken and its natural log (floored at the float epsilon) is the feature. The mean of each band over
//! the whole recording is then subtracted ("cepstral mean normalisation"), which takes out the microphone and the room.
//!
//! The numbers are checked against the reference implementation (`kaldi-native-fbank`) in the tests below.

use anyhow::{bail, Result};

/// Frame length in samples (25 ms at 16 kHz).
const FRAME: usize = 400;
/// Step between frames in samples (10 ms).
const HOP: usize = 160;
/// FFT size: the frame length rounded up to a power of two.
const NFFT: usize = 512;
/// Number of mel bands, the width of one feature vector.
pub const MEL_BANDS: usize = 80;
const LOW_HZ: f64 = 20.0;
const HIGH_HZ: f64 = 8000.0;
const SAMPLE_RATE: f64 = 16_000.0;
const PREEMPHASIS: f64 = 0.97;
/// Smallest value allowed before the log (the 32-bit float epsilon), so silence gives a finite number.
const LOG_FLOOR: f64 = 1.192_092_9e-7;

/// The shortest recording that gives at least one frame.
pub const MIN_SAMPLES: usize = FRAME;

fn mel(hz: f64) -> f64 {
    1127.0 * (1.0 + hz / 700.0).ln()
}

/// In-place iterative radix-2 FFT of `re`/`im` (length a power of two).
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    // bit reversal
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            for k in 0..len / 2 {
                let (a, b) = (i + k, i + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

/// The triangular mel filters as (first fft bin, weights) per band, built the Kaldi way (triangles are straight in mel space,
/// only the first `NFFT / 2` bins are used, the Nyquist bin is not).
fn mel_filters() -> Vec<(usize, Vec<f64>)> {
    let bin_hz = SAMPLE_RATE / NFFT as f64;
    let (mel_low, mel_high) = (mel(LOW_HZ), mel(HIGH_HZ));
    let delta = (mel_high - mel_low) / (MEL_BANDS as f64 + 1.0);
    (0..MEL_BANDS)
        .map(|b| {
            let left = mel_low + b as f64 * delta;
            let center = mel_low + (b as f64 + 1.0) * delta;
            let right = mel_low + (b as f64 + 2.0) * delta;
            let mut first = None;
            let mut weights = Vec::new();
            for i in 0..NFFT / 2 {
                let m = mel(bin_hz * i as f64);
                if m > left && m < right {
                    first.get_or_insert(i);
                    weights.push(if m <= center { (m - left) / (center - left) } else { (right - m) / (right - center) });
                }
            }
            (first.unwrap_or(0), weights)
        })
        .collect()
}

/// The log-mel features of 16 kHz mono samples in -1..1, one `MEL_BANDS`-wide row per frame, **without** the mean
/// subtraction (see [`normalised_fbank`]). Fewer than [`MIN_SAMPLES`] samples is an error.
pub fn log_mel_fbank(samples: &[f32]) -> Result<Vec<[f32; MEL_BANDS]>> {
    if samples.len() < MIN_SAMPLES {
        bail!("the recording is too short for a voice check ({} samples; at least {MIN_SAMPLES} are needed)", samples.len());
    }
    let frames = 1 + (samples.len() - FRAME) / HOP;
    let window: Vec<f64> = (0..FRAME).map(|n| 0.54 - 0.46 * (2.0 * std::f64::consts::PI * n as f64 / (FRAME as f64 - 1.0)).cos()).collect();
    let filters = mel_filters();
    let mut out = Vec::with_capacity(frames);
    let (mut re, mut im) = (vec![0.0f64; NFFT], vec![0.0f64; NFFT]);
    for f in 0..frames {
        let start = f * HOP;
        let mut x: Vec<f64> = samples[start..start + FRAME].iter().map(|s| *s as f64 * 32768.0).collect();
        let mean = x.iter().sum::<f64>() / FRAME as f64;
        x.iter_mut().for_each(|v| *v -= mean);
        for i in (1..FRAME).rev() {
            x[i] -= PREEMPHASIS * x[i - 1];
        }
        x[0] -= PREEMPHASIS * x[0];
        re.iter_mut().for_each(|v| *v = 0.0);
        im.iter_mut().for_each(|v| *v = 0.0);
        for i in 0..FRAME {
            re[i] = x[i] * window[i];
        }
        fft(&mut re, &mut im);
        let power: Vec<f64> = (0..NFFT / 2).map(|i| re[i] * re[i] + im[i] * im[i]).collect();
        let mut row = [0.0f32; MEL_BANDS];
        for (b, (first, weights)) in filters.iter().enumerate() {
            let energy: f64 = weights.iter().enumerate().map(|(k, w)| w * power[first + k]).sum();
            row[b] = energy.max(LOG_FLOOR).ln() as f32;
        }
        out.push(row);
    }
    Ok(out)
}

/// The features a speaker model is given: [`log_mel_fbank`] with each band's mean over the recording subtracted. Flattened
/// row by row (`frames * MEL_BANDS` values), with the number of frames.
pub fn normalised_fbank(samples: &[f32]) -> Result<(usize, Vec<f32>)> {
    let rows = log_mel_fbank(samples)?;
    let mut mean = [0.0f64; MEL_BANDS];
    for r in &rows {
        for b in 0..MEL_BANDS {
            mean[b] += r[b] as f64;
        }
    }
    mean.iter_mut().for_each(|m| *m /= rows.len() as f64);
    let mut flat = Vec::with_capacity(rows.len() * MEL_BANDS);
    for r in &rows {
        for b in 0..MEL_BANDS {
            flat.push((r[b] as f64 - mean[b]) as f32);
        }
    }
    Ok((rows.len(), flat))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signal the reference numbers below were made from: two tones and a little deterministic noise, 1 s at 16 kHz.
    fn reference_signal() -> Vec<f32> {
        let mut state: u64 = 12345;
        (0..16_000)
            .map(|i| {
                state = (state * 1_103_515_245 + 12_345) % (1u64 << 31);
                let noise = (state as f64 / (1u64 << 31) as f64) * 2.0 - 1.0;
                let t = i as f64 / 16_000.0;
                (0.3 * (2.0 * std::f64::consts::PI * 220.0 * t).sin() + 0.2 * (2.0 * std::f64::consts::PI * 1500.0 * t).sin() + 0.05 * noise) as f32
            })
            .collect()
    }

    #[test]
    fn the_features_match_the_reference_implementation() {
        let rows = log_mel_fbank(&reference_signal()).unwrap();
        assert_eq!(rows.len(), 98, "1 s of audio is 98 frames");
        // From kaldi-native-fbank with a Hamming window, 80 bands, 20 Hz to 8 kHz, no dither, no padding at the edges.
        let expected: [(usize, [f32; 8], f32); 4] = [
            (0, [15.2773, 13.5479, 11.4090, 12.7922, 17.0121, 20.3453, 21.9052, 22.8681], 22.4739),
            (10, [15.2328, 12.5399, 12.7964, 12.9747, 16.8103, 20.2892, 21.8638, 22.8473], 22.6304),
            (50, [15.2973, 13.4519, 12.4973, 14.0062, 16.9418, 20.3419, 21.9048, 22.8573], 22.1845),
            (97, [14.1119, 12.8629, 13.7199, 13.6620, 16.9870, 20.3233, 21.8928, 22.8869], 22.0267),
        ];
        for (frame, first8, last) in expected {
            for (b, want) in first8.iter().enumerate() {
                assert!((rows[frame][b] - want).abs() < 0.02, "frame {frame} band {b}: got {} want {want}", rows[frame][b]);
            }
            assert!((rows[frame][79] - last).abs() < 0.02, "frame {frame} band 79: got {} want {last}", rows[frame][79]);
        }
        let sum: f64 = rows.iter().flat_map(|r| r.iter()).map(|v| *v as f64).sum();
        assert!((sum - 149_372.859).abs() < 30.0, "sum of all features {sum}");
    }

    #[test]
    fn each_band_has_zero_mean_after_normalisation_and_the_shape_is_frames_by_bands() {
        let (frames, flat) = normalised_fbank(&reference_signal()).unwrap();
        assert_eq!((frames, flat.len()), (98, 98 * MEL_BANDS));
        for b in 0..MEL_BANDS {
            let mean: f64 = (0..frames).map(|f| flat[f * MEL_BANDS + b] as f64).sum::<f64>() / frames as f64;
            assert!(mean.abs() < 1e-3, "band {b} mean {mean}");
        }
    }

    #[test]
    fn silence_gives_finite_features_and_a_too_short_recording_is_an_error() {
        let rows = log_mel_fbank(&vec![0.0; 16_000]).unwrap();
        assert!(rows.iter().flat_map(|r| r.iter()).all(|v| v.is_finite()));
        assert!(log_mel_fbank(&vec![0.1; MIN_SAMPLES - 1]).is_err());
        assert_eq!(log_mel_fbank(&vec![0.1; MIN_SAMPLES]).unwrap().len(), 1);
        assert_eq!(log_mel_fbank(&vec![0.1; MIN_SAMPLES + HOP]).unwrap().len(), 2);
    }

    #[test]
    fn the_fft_finds_a_pure_tone_in_the_right_bin() {
        let n = 512;
        let mut re: Vec<f64> = (0..n).map(|i| (2.0 * std::f64::consts::PI * 8.0 * i as f64 / n as f64).sin()).collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        let mags: Vec<f64> = (0..n / 2).map(|i| (re[i] * re[i] + im[i] * im[i]).sqrt()).collect();
        let peak = mags.iter().enumerate().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).unwrap().0;
        assert_eq!(peak, 8);
        assert!((mags[8] - n as f64 / 2.0).abs() < 1e-6);
    }
}
