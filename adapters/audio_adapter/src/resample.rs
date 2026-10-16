//! Getting audio into the one shape every speech stage expects: 16 kHz, mono, f32 in -1..1.
//!
//! Whisper, the speaker model and the silence check all assume it. A microphone usually delivers 44.1 or 48 kHz,
//! often stereo, and an uploaded WAV can be anything, so converting once at the edge keeps a 48 kHz recording from
//! being heard at three times the speed and a stereo one from being read as a mono recording of twice the length.

use anyhow::{bail, Result};

/// The rate every stage works at.
pub const TARGET_RATE: u32 = 16_000;

const MIN_RATE: u32 = 8_000;
const MAX_RATE: u32 = 192_000;
const MAX_CHANNELS: u16 = 8;

/// Convert interleaved samples at `rate` Hz with `channels` channels to 16 kHz mono. Channels are averaged. Going
/// down in rate averages the input samples that fall in each output sample (a simple low-pass, so high frequencies
/// do not fold back into the speech band); going up interpolates linearly.
pub fn to_16k_mono(samples: &[f32], rate: u32, channels: u16) -> Result<Vec<f32>> {
    if !(MIN_RATE..=MAX_RATE).contains(&rate) {
        bail!("a sample rate of {rate} Hz is not supported (use {MIN_RATE} to {MAX_RATE} Hz)");
    }
    if channels == 0 || channels > MAX_CHANNELS {
        bail!("{channels} channels is not supported (use 1 to {MAX_CHANNELS})");
    }
    let mono: Vec<f32> = if channels == 1 {
        samples.to_vec()
    } else {
        samples.chunks_exact(channels as usize).map(|f| f.iter().sum::<f32>() / channels as f32).collect()
    };
    if rate == TARGET_RATE || mono.is_empty() {
        return Ok(mono);
    }
    let ratio = rate as f64 / TARGET_RATE as f64;
    let out_len = ((mono.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        if ratio > 1.0 {
            let start = pos.floor() as usize;
            let end = ((pos + ratio).ceil() as usize).min(mono.len()).max(start + 1);
            out.push(mono[start..end].iter().sum::<f32>() / (end - start) as f32);
        } else {
            let a = pos.floor() as usize;
            let b = (a + 1).min(mono.len() - 1);
            let frac = (pos - a as f64) as f32;
            out.push(mono[a] * (1.0 - frac) + mono[b] * frac);
        }
    }
    Ok(out)
}

/// Mono 16-bit WAV bytes of `samples` (-1..1) at `rate` Hz.
pub fn mono_wav_bytes(samples: &[f32], rate: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cursor, spec)?;
        for s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        w.finalize()?;
    }
    Ok(cursor.into_inner())
}

/// Decode WAV bytes (8, 16, 24 or 32 bit integer, or 32 bit float; any rate and channel count) to 16 kHz mono.
pub fn wav_to_16k_mono(wav_bytes: &[u8]) -> Result<Vec<f32>> {
    use hound::{SampleFormat, WavReader};
    let mut reader = WavReader::new(std::io::Cursor::new(wav_bytes))?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        SampleFormat::Float => reader.samples::<f32>().collect::<std::result::Result<_, _>>()?,
        SampleFormat::Int => {
            // i64: for 32-bit audio `1 << 31` does not fit an i32 and would flip every sample's sign.
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.map(|v| v as f32 / max_val)).collect::<std::result::Result<_, _>>()?
        }
    };
    to_16k_mono(&samples, spec.sample_rate, spec.channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, secs: f32, hz: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize).map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5).collect()
    }

    #[test]
    fn audio_at_16k_mono_is_left_alone() {
        let s = tone(16_000, 0.1, 440.0);
        assert_eq!(to_16k_mono(&s, 16_000, 1).unwrap(), s);
    }

    #[test]
    fn higher_rates_come_down_to_the_right_length_and_keep_their_pitch() {
        for rate in [44_100u32, 48_000, 32_000, 22_050] {
            let out = to_16k_mono(&tone(rate, 1.0, 440.0), rate, 1).unwrap();
            assert!((out.len() as i64 - 16_000).abs() <= 2, "{rate}: {} samples", out.len());
            // 440 Hz in one second crosses zero about 880 times; a wrong rate would give a different count
            let crossings = out.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
            assert!((crossings as i64 - 880).abs() < 12, "{rate}: {crossings} zero crossings");
        }
    }

    #[test]
    fn lower_rates_go_up_and_stereo_is_averaged_not_read_as_twice_as_long() {
        let up = to_16k_mono(&tone(8_000, 1.0, 300.0), 8_000, 1).unwrap();
        assert!((up.len() as i64 - 16_000).abs() <= 2);
        let l = tone(48_000, 1.0, 440.0);
        let stereo: Vec<f32> = l.iter().flat_map(|s| [*s, *s]).collect();
        let mono = to_16k_mono(&stereo, 48_000, 2).unwrap();
        assert!((mono.len() as i64 - 16_000).abs() <= 2, "{}", mono.len());
        let left_only: Vec<f32> = l.iter().flat_map(|s| [*s, 0.0]).collect();
        let half = to_16k_mono(&left_only, 48_000, 2).unwrap();
        let peak = |v: &[f32]| v.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak(&half) < peak(&mono) * 0.6, "an unbalanced stereo file averages to about half");
    }

    #[test]
    fn samples_round_trip_through_a_wav() {
        let s = tone(24_000, 0.2, 300.0);
        let wav = mono_wav_bytes(&s, 24_000).unwrap();
        let r = hound::WavReader::new(std::io::Cursor::new(wav)).unwrap();
        assert_eq!((r.spec().channels, r.spec().sample_rate, r.len() as usize), (1, 24_000, s.len()));
    }

    #[test]
    fn nonsense_formats_are_refused_and_empty_audio_is_empty() {
        assert!(to_16k_mono(&[0.0; 100], 100, 1).is_err());
        assert!(to_16k_mono(&[0.0; 100], 48_000, 0).is_err());
        assert!(to_16k_mono(&[0.0; 100], 48_000, 9).is_err());
        assert!(to_16k_mono(&[], 48_000, 2).unwrap().is_empty());
    }

    #[test]
    fn a_wav_of_any_shape_decodes_to_16k_mono_and_32_bit_audio_keeps_its_sign() {
        let write = |rate: u32, channels: u16, bits: u16, samples: &[i32]| {
            let spec = hound::WavSpec { channels, sample_rate: rate, bits_per_sample: bits, sample_format: hound::SampleFormat::Int };
            let mut cur = std::io::Cursor::new(Vec::new());
            let mut w = hound::WavWriter::new(&mut cur, spec).unwrap();
            for s in samples {
                w.write_sample(*s).unwrap();
            }
            w.finalize().unwrap();
            cur.into_inner()
        };
        let pos32 = wav_to_16k_mono(&write(16_000, 1, 32, &[i32::MAX / 2; 160])).unwrap();
        assert!(pos32.iter().all(|s| *s > 0.4 && *s < 0.6), "positive 32-bit samples must stay positive: {:?}", &pos32[..3]);
        let s16 = (0..4800).map(|i| (((i as f32) * 0.05).sin() * 8000.0) as i32).collect::<Vec<_>>();
        let out = wav_to_16k_mono(&write(48_000, 1, 16, &s16)).unwrap();
        assert!((out.len() as i64 - 1600).abs() <= 2);
        assert!(wav_to_16k_mono(b"not a wav").is_err());
    }
}
