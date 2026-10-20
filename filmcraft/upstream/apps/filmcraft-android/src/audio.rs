//! Android audio output: [`AndroidAudioOut`] implements the [`filmcraft_ui_egui::AudioOut`]
//! trait with cpal. cpal 0.16's Android backend is AAudio (through the `ndk`
//! crate), so there is no oboe C++ build step.
//!
//! The known Android audio trap (fixed here): `source_playback` calls
//! [`AudioOut::sample_rate`] *before* [`AudioOut::start`] when it starts source
//! audio, and a zero rate means "no audio output device" — playback then goes
//! silent. [`AndroidAudioOut::new`] therefore pre-queries the default output
//! device's config up front and reports a real rate (or a safe 48 kHz fallback)
//! even when no stream exists yet.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use filmcraft_ui_egui::AudioOut;

/// Audio output for the Android port (AAudio via cpal).
pub struct AndroidAudioOut {
    stream: Option<cpal::Stream>,
    /// The frames played so far, for the playback master clock.
    played: Arc<AtomicU64>,
    /// Set while a stream is expected to be running (a started stream).
    active: Arc<AtomicBool>,
    /// Sample rate pre-queried from the default output device in `new()`.
    rate: u32,
    channels: u16,
}

impl AndroidAudioOut {
    /// Pre-query the default output device's config: `sample_rate()` must
    /// answer correctly before `start()` is ever called.
    pub fn new() -> Self {
        let (rate, channels) = default_output_config();
        Self {
            stream: None,
            played: Arc::new(AtomicU64::new(0)),
            active: Arc::new(AtomicBool::new(false)),
            rate,
            channels,
        }
    }
}

/// The default output device's (rate, channels), or a safe (48 kHz, stereo)
/// fallback. Never fails to an unusable zero rate.
fn default_output_config() -> (u32, u16) {
    let host = cpal::default_host();
    let outcome = (|| {
        let device = host.default_output_device()?;
        let config = device.default_output_config().ok()?;
        Some((config.sample_rate().0, config.channels()))
    })();
    match outcome {
        Some((rate, channels)) if rate > 0 && channels > 0 => {
            log::info!("Android audio: default output {rate} Hz, {channels} ch");
            (rate, channels)
        }
        _ => {
            log::warn!("Android audio: no output device config; reporting 48 kHz stereo");
            (48_000, 2)
        }
    }
}

impl Drop for AndroidAudioOut {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
        // Dropping the stream stops it.
        self.stream = None;
    }
}

impl AudioOut for AndroidAudioOut {
    /// Start output; `fill(buffer, channels)` is called on the audio thread
    /// with interleaved f32. Returns the device sample rate.
    fn start(&mut self, fill: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no audio output device".to_string())?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("no output config: {e}"))?;
        let channels = usize::from(supported.channels());
        let rate = supported.sample_rate().0;
        let config = cpal::StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let fill = Arc::new(Mutex::new(fill));
        let played = self.played.clone();
        let active = self.active.clone();
        // The mixer callback adds the frames it produced to the shared
        // `played` total (the playback master clock).
        let err_fn = |err| log::error!("Android audio stream error: {err}");

        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device
                .build_output_stream(
                    &config,
                    move |buf: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        if !active.load(Ordering::Relaxed) {
                            buf.fill(0.0);
                            return;
                        }
                        if let Ok(mut fill) = fill.lock() {
                            fill(buf, channels);
                            played.fetch_add(
                                (buf.len() / channels.max(1)) as u64,
                                Ordering::Relaxed,
                            );
                        } else {
                            buf.fill(0.0);
                        }
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| format!("build stream: {e}"))?,
            cpal::SampleFormat::I16 => device
                .build_output_stream(
                    &config,
                    move |buf: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        if !active.load(Ordering::Relaxed) {
                            buf.fill(0);
                            return;
                        }
                        let mut tmp = vec![0.0f32; buf.len()];
                        if let Ok(mut fill) = fill.lock() {
                            fill(&mut tmp, channels);
                            played.fetch_add(
                                (tmp.len() / channels.max(1)) as u64,
                                Ordering::Relaxed,
                            );
                        }
                        for (out, sample) in buf.iter_mut().zip(tmp.iter()) {
                            *out = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        }
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| format!("build stream: {e}"))?,
            cpal::SampleFormat::U16 => device
                .build_output_stream(
                    &config,
                    move |buf: &mut [u16], _: &cpal::OutputCallbackInfo| {
                        if !active.load(Ordering::Relaxed) {
                            buf.fill(u16::MAX / 2);
                            return;
                        }
                        let mut tmp = vec![0.0f32; buf.len()];
                        if let Ok(mut fill) = fill.lock() {
                            fill(&mut tmp, channels);
                            played.fetch_add(
                                (tmp.len() / channels.max(1)) as u64,
                                Ordering::Relaxed,
                            );
                        }
                        for (out, sample) in buf.iter_mut().zip(tmp.iter()) {
                            *out = (sample.clamp(-1.0, 1.0) * 32767.0 + 32768.0) as u16;
                        }
                    },
                    err_fn,
                    None,
                )
                .map_err(|e| format!("build stream: {e}"))?,
            fmt => return Err(format!("unsupported sample format: {fmt:?}")),
        };
        stream
            .play()
            .map_err(|e| format!("start stream: {e}"))?;
        self.active.store(true, Ordering::Relaxed);
        self.played.store(0, Ordering::Relaxed);
        self.stream = Some(stream);
        self.rate = rate;
        self.channels = supported.channels();
        Ok(rate)
    }

    fn stop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
        self.stream = None;
    }

    /// Device sample rate — pre-queried in `new()`, so it is correct even
    /// before the first `start()` (the app checks this to decide whether
    /// source audio exists).
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Device output channels — pre-queried in `new()`.
    fn channels(&self) -> usize {
        usize::from(self.channels)
    }

    /// Frames played since `start` (the playback master clock).
    fn played_frames(&self) -> Option<u64> {
        if self.stream.is_some() {
            Some(self.played.load(Ordering::Relaxed))
        } else {
            None
        }
    }
}
