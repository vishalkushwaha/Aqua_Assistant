use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use std::sync::mpsc::{channel, Receiver, Sender};
use tracing::{info, warn};

pub struct MicCapture {
    _stream: Stream,
    sample_rate: u32,
    channels: u16,
    rx: Receiver<Vec<f32>>,
}

impl MicCapture {
    /// Initialises microphone audio stream capture at 48000Hz (DeepFilterNet native sample rate) or device default.
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("No input microphone device found on system")?;

        let device_name = device.name().unwrap_or_else(|_| "Default Mic".to_string());
        info!("Selected microphone device: {}", device_name);

        let default_config = device.default_input_config()?;
        let sample_rate = default_config.sample_rate().0;
        let channels = default_config.channels();

        info!(
            "Microphone config: rate={}Hz, channels={}, format={:?}",
            sample_rate,
            channels,
            default_config.sample_format()
        );

        let (tx, rx) = channel::<Vec<f32>>();

        let stream = match default_config.sample_format() {
            SampleFormat::F32 => Self::build_stream_f32(&device, &default_config.into(), tx)?,
            SampleFormat::I16 => Self::build_stream_i16(&device, &default_config.into(), tx)?,
            format => anyhow::bail!("Unsupported microphone sample format: {:?}", format),
        };

        stream.play()?;

        Ok(Self {
            _stream: stream,
            sample_rate,
            channels,
            rx,
        })
    }

    fn build_stream_f32(
        device: &cpal::Device,
        config: &StreamConfig,
        tx: Sender<Vec<f32>>,
    ) -> Result<Stream> {
        let err_fn = |err| warn!("Microphone stream error: {}", err);

        let stream = device.build_input_stream(
            config,
            move |data: &[f32], _: &_| {
                let _ = tx.send(data.to_vec());
            },
            err_fn,
            None,
        )?;

        Ok(stream)
    }

    fn build_stream_i16(
        device: &cpal::Device,
        config: &StreamConfig,
        tx: Sender<Vec<f32>>,
    ) -> Result<Stream> {
        let err_fn = |err| warn!("Microphone stream error: {}", err);

        let stream = device.build_input_stream(
            config,
            move |data: &[i16], _: &_| {
                let float_samples: Vec<f32> = data
                    .iter()
                    .map(|&s| s as f32 / i16::MAX as f32)
                    .collect();
                let _ = tx.send(float_samples);
            },
            err_fn,
            None,
        )?;

        Ok(stream)
    }

    pub fn receiver(&self) -> &Receiver<Vec<f32>> {
        &self.rx
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}
