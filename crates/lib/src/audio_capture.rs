//! CPAL device discovery and capture callback (TECH_SPEC §4.1, tasks T3.1/T3.2).
//!
//! The callback performs bounded copies into preallocated SPSC rings only. It
//! does not run DSP, storage, logging, HTTP, or blocking work. The Pi hardware
//! path remains subject to the T0.2/T8.1 spike; this module compiles on the
//! developer host so the rest of the application can be tested without a Pi.

use crate::bounded_queue::SpscRing;
use crate::config::{AudioConfig, SampleFormat, SUPPORTED_CAPTURE_RATES, WIRE_CHANNELS, WIRE_RATE_HZ};
use crate::metrics::Metrics;
use crate::resampler::{Resampler, ResamplerError};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, Stream, SupportedStreamConfig};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const META_PADDING: usize = 16;

/// Callback block metadata paired with raw interleaved S16 samples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureMeta {
    pub frames: u32,
    pub channels: u16,
    pub rate_hz: u32,
    pub capture_seq: u64,
    pub mono_start_ns: u64,
    pub stream_generation: u32,
}

/// Two-ring capture buffer: one ring for sample frames and one for callback
/// boundaries/timestamps. Both are allocated before the stream starts.
pub struct CaptureRing {
    samples: SpscRing<i16>,
    blocks: SpscRing<CaptureMeta>,
    max_block_frames: usize,
    overflows: AtomicU64,
    max_depth_frames: AtomicU64,
}

impl CaptureRing {
    pub fn new(rate_hz: u32, channels: u16, capture_ring_ms: u32, period_ms: u32, buffer_ms: u32) -> Self {
        let duration_frames = (u64::from(rate_hz) * u64::from(capture_ring_ms) / 1000) as usize;
        let requested_buffer_frames = (u64::from(rate_hz) * u64::from(buffer_ms) / 1000) as usize;
        let max_block_frames = requested_buffer_frames.max(1);
        let sample_capacity = duration_frames.max(max_block_frames * 2).next_power_of_two();
        let blocks_needed = (capture_ring_ms / period_ms.max(1)) as usize + META_PADDING;
        CaptureRing {
            samples: SpscRing::with_capacity(sample_capacity.saturating_mul(channels.max(1) as usize)),
            blocks: SpscRing::with_capacity(blocks_needed),
            max_block_frames,
            overflows: AtomicU64::new(0),
            max_depth_frames: AtomicU64::new(0),
        }
    }

    /// Called by the single CPAL producer. Returns false for a whole block when
    /// either ring cannot accept it, preserving the metadata/sample boundary.
    pub fn callback_push(&self, samples: &[i16], meta: CaptureMeta) -> bool {
        if meta.frames as usize > self.max_block_frames
            || samples.len() > self.samples.free_space()
            || self.blocks.free_space() == 0
        {
            self.overflows.fetch_add(u64::from(meta.frames), Ordering::Relaxed);
            return false;
        }
        for sample in samples {
            if !self.samples.try_push_quiet(*sample) {
                // The preflight plus single producer guarantees this cannot occur
                // unless the ring's SPSC contract was violated.
                self.overflows.fetch_add(u64::from(meta.frames), Ordering::Relaxed);
                return false;
            }
        }
        if !self.blocks.try_push_quiet(meta) {
            // Capacity was checked above; don't publish an unpaired block.
            self.overflows.fetch_add(u64::from(meta.frames), Ordering::Relaxed);
            return false;
        }
        let depth = self.samples.len() / usize::from(meta.channels.max(1));
        self.max_depth_frames.fetch_max(depth as u64, Ordering::Relaxed);
        true
    }

    /// Dispatcher side: obtains metadata and copies exactly that block into the
    /// caller's reusable buffer.
    pub fn pop_into(&self, out: &mut Vec<i16>) -> Option<CaptureMeta> {
        let meta = self.blocks.try_pop()?;
        let sample_count = meta.frames as usize * meta.channels as usize;
        out.clear();
        out.reserve(sample_count.saturating_sub(out.capacity()));
        for _ in 0..sample_count {
            let Some(sample) = self.samples.try_pop() else {
                // Internal invariant failure: samples and metadata were published
                // together. Count a loss and return no partially valid block.
                self.overflows.fetch_add(u64::from(meta.frames), Ordering::Relaxed);
                out.clear();
                return None;
            };
            out.push(sample);
        }
        Some(meta)
    }

    pub fn overflow_frames(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }

    /// Adds frames lost somewhere *upstream* of this ring.
    ///
    /// The single-box path has two rings (device format, then wire format), so
    /// the producer that actually overflowed is the first one. The dispatcher
    /// reads the counter of the ring it pops, so a loss on the device ring has
    /// to be transferred here or the night would report a gap nobody accounted
    /// for — the exact failure the counter exists to prevent.
    pub fn note_overflow_frames(&self, frames: u64) {
        self.overflows.fetch_add(frames, Ordering::Relaxed);
    }

    /// Largest block this ring accepts in one `callback_push`.
    pub fn max_block_frames(&self) -> usize {
        self.max_block_frames
    }

    pub fn queued_frames(&self, channels: u16) -> usize {
        self.samples.len() / usize::from(channels.max(1))
    }

    pub fn high_water_frames(&self) -> u64 {
        self.max_depth_frames.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedFormat {
    pub device_name: String,
    pub rate_hz: u32,
    pub channels: u16,
    pub format: SampleFormat,
}

#[derive(Debug)]
pub struct CaptureSelection {
    pub device: Device,
    pub supported: SupportedStreamConfig,
    pub info: NegotiatedFormat,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no CPAL input device found")]
    NoDevice,
    #[error("multiple input devices found; specify audio.alsa_device (found {0:?})")]
    MultipleDevices(Vec<String>),
    #[error("requested input device {0:?} was not found")]
    DeviceNotFound(String),
    #[error("no supported S16 input configuration for device {device}: {details}")]
    NoSupportedConfig { device: String, details: String },
    #[error("CPAL error: {0}")]
    Cpal(String),
}

/// Discovers eligible capture devices and negotiates S16_LE in documented order:
/// configured rate, then 48 kHz, then 44.1 kHz; channel count is configured, then
/// mono if the device exposes it. Other formats/rates fail explicitly.
pub fn select_device(host: &Host, config: &AudioConfig) -> Result<CaptureSelection, CaptureError> {
    let mut devices = host.input_devices().map_err(|e| CaptureError::Cpal(e.to_string()))?;
    let all: Vec<Device> = devices.by_ref().collect();
    if all.is_empty() {
        return Err(CaptureError::NoDevice);
    }
    let names: Vec<(String, Device)> = all
        .into_iter()
        .map(|d| (d.to_string(), d))
        .collect();
    let candidates: Vec<(String, Device)> = if config.alsa_device.trim().is_empty() {
        if names.len() != 1 {
            return Err(CaptureError::MultipleDevices(names.iter().map(|(name, _)| name.clone()).collect()));
        }
        names
    } else {
        names
            .into_iter()
            .filter(|(name, _)| name == &config.alsa_device)
            .collect()
    };
    let Some((device_name, device)) = candidates.into_iter().next() else {
        return Err(CaptureError::DeviceNotFound(config.alsa_device.clone()));
    };

    let ranges = device
        .supported_input_configs()
        .map_err(|e| CaptureError::Cpal(e.to_string()))?
        .collect::<Vec<_>>();

    // The device's *default* configuration is the one value ALSA is guaranteed
    // to honour, and it is tried first.
    //
    // `supported_input_configs` reports the full range a codec can clock, not
    // what the endpoint will actually run at: it will happily accept a request
    // for 16 kHz on a microphone that only does 48 kHz and then let ALSA round
    // the rate without reporting an error. The samples that arrive are then
    // three times faster than the rate we advertise, every syllable is stretched,
    // and the recogniser on the other side hears rumble — which is exactly the
    // failure this ordering exists to prevent. Whatever rate comes back here is
    // converted down to the wire format by [`WireConverter`], so the only thing
    // that matters is that it is a rate the hardware really runs at.
    if let Ok(default) = device.default_input_config() {
        let rate = default.sample_rate();
        if default.sample_format() == cpal::SampleFormat::I16
            && SUPPORTED_CAPTURE_RATES.contains(&rate)
        {
            return Ok(CaptureSelection {
                info: NegotiatedFormat {
                    device_name,
                    rate_hz: rate,
                    channels: default.channels(),
                    format: SampleFormat::S16Le,
                },
                device,
                supported: default,
            });
        }
    }

    let attempts = [config.device_rate_hz, 48_000, 44_100];
    for desired_rate in attempts {
        if !SUPPORTED_CAPTURE_RATES.contains(&desired_rate) {
            continue;
        }
        for channels in [config.device_channels, 1].into_iter().filter(|c| *c > 0).collect::<Vec<_>>() {
            for range in &ranges {
                if range.channels() != channels || range.sample_format() != cpal::SampleFormat::I16 {
                    continue;
                }
                if desired_rate < range.min_sample_rate() || desired_rate > range.max_sample_rate() {
                    continue;
                }
                let supported = (*range).with_sample_rate(desired_rate);
                return Ok(CaptureSelection {
                    info: NegotiatedFormat {
                        device_name: device_name.clone(),
                        rate_hz: desired_rate,
                        channels,
                        format: SampleFormat::S16Le,
                    },
                    device,
                    supported,
                });
            }
        }
    }
    let details = ranges
        .iter()
        .map(|r| format!("{}ch {:?} {}-{} Hz", r.channels(), r.sample_format(), r.min_sample_rate(), r.max_sample_rate()))
        .collect::<Vec<_>>()
        .join(", ");
    Err(CaptureError::NoSupportedConfig { device: device_name, details })
}

/// Opens the selected CPAL stream and starts it. The callback copies only to the
/// bounded capture rings. Stream errors set an atomic flag and are later turned
/// into ordered gap/control messages by the dispatcher.
pub fn start_stream(
    selection: CaptureSelection,
    ring: Arc<CaptureRing>,
    metrics: Arc<Metrics>,
    shutting_down: Arc<AtomicBool>,
    stream_generation: u32,
) -> Result<Stream, CaptureError> {
    let channels = selection.info.channels;
    let rate_hz = selection.info.rate_hz;
    let seq = Arc::new(AtomicU64::new(0));
    let stream_origin = Instant::now();
    let data_ring = Arc::clone(&ring);
    let data_metrics = Arc::clone(&metrics);
    let data_seq = Arc::clone(&seq);
    let error_metrics = Arc::clone(&metrics);
    let error_shutdown = Arc::clone(&shutting_down);
    let stream_config = selection.supported.config();
    let err_fn = move |err: cpal::Error| {
        error_metrics.xrun_count.fetch_add(1, Ordering::Relaxed);
        error_metrics.set_last_error(format!("CPAL stream error: {err}"));
        if !error_shutdown.load(Ordering::Relaxed) {
            error_metrics.degraded.store(true, Ordering::Relaxed);
        }
    };
    let stream = selection
        .device
        .build_input_stream::<i16, _, _>(
            stream_config,
            move |data: &[i16], _info: &cpal::InputCallbackInfo| {
                let now_ns = stream_origin.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                let frame_count = data.len() / channels as usize;
                let start_seq = data_seq.fetch_add(frame_count as u64, Ordering::Relaxed);
                let meta = CaptureMeta {
                    frames: frame_count.min(u32::MAX as usize) as u32,
                    channels,
                    rate_hz,
                    capture_seq: start_seq,
                    mono_start_ns: now_ns,
                    stream_generation,
                };
                if !data_ring.callback_push(data, meta) {
                    data_metrics.capture_ring_overflow_frames.fetch_add(frame_count as u64, Ordering::Relaxed);
                }
                data_metrics.capture_ring_high_water_frames.fetch_max(data_ring.high_water_frames(), Ordering::Relaxed);
            },
            err_fn,
            None,
        )
        .map_err(|e| CaptureError::Cpal(e.to_string()))?;
    stream.play().map_err(|e| CaptureError::Cpal(e.to_string()))?;
    Ok(stream)
}

/// Backoff schedule for stream rebuild attempts: 1,2,4,8,16,30,30... seconds.
#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    next_seconds: u64,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        ReconnectBackoff { next_seconds: 1 }
    }
}

impl ReconnectBackoff {
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next_seconds.min(30);
        self.next_seconds = (self.next_seconds * 2).min(30);
        Duration::from_secs(delay)
    }

    pub fn reset(&mut self) {
        self.next_seconds = 1;
    }
}

/// Turns whatever the operating system hands us into the one format everything
/// else assumes: 16 kHz mono S16.
///
/// Every consumer downstream — the UDP protocol, the local archive, the server's
/// detector and the recogniser — is written against that format, which is now
/// a constant ([`WIRE_RATE_HZ`] / [`WIRE_CHANNELS`]) rather than a configured
/// value. A microphone that only clocks at 48 kHz stereo used to be *described*
/// to all of them as "16 kHz mono" while its samples were neither: reading
/// interleaved stereo as mono doubles the sample count and mislabelling 48 kHz
/// as 16 kHz triples it, so a sentence arrives six times too slow and roughly
/// two octaves too low. The text on the other side is then nonsense, and
/// nothing in the system says so, because every component believes the format
/// it was told.
///
/// Converting here, once, next to the device, is what keeps the assumption true.
/// Which channel becomes the mono stream is `audio.mono_channel_index`, not an
/// average: averaging two capsules mixes a reflection into the signal instead of
/// removing noise, and the configuration already names the channel that matters.
#[derive(Debug)]
pub struct WireConverter {
    device_rate_hz: u32,
    channels: u16,
    channel_index: u16,
    resampler: Resampler,
    mono: Vec<f32>,
    output: Vec<i16>,
}

impl WireConverter {
    /// The rate every downstream consumer expects.
    pub const RATE_HZ: u32 = WIRE_RATE_HZ;

    /// Fails when the device rate cannot be brought onto the wire rate exactly,
    /// or when `channel_index` is not one of the device's channels. A silent
    /// fallback here would reintroduce the very bug this type exists to prevent,
    /// so the caller is expected to abort startup instead.
    pub fn new(
        device_rate_hz: u32,
        channels: u16,
        channel_index: u16,
    ) -> Result<Self, WireConversionError> {
        let channels = channels.max(1);
        if channel_index >= channels {
            return Err(WireConversionError::ChannelIndex {
                index: channel_index,
                channels,
            });
        }
        Ok(WireConverter {
            device_rate_hz,
            channels,
            channel_index,
            resampler: Resampler::new(device_rate_hz, WIRE_RATE_HZ)?,
            mono: Vec::new(),
            output: Vec::new(),
        })
    }

    pub fn device_rate_hz(&self) -> u32 {
        self.device_rate_hz
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// True when the device already speaks the wire format, so `convert` is a
    /// copy rather than a channel pick and a resample.
    pub fn is_passthrough(&self) -> bool {
        self.device_rate_hz == WIRE_RATE_HZ && self.channels == WIRE_CHANNELS
    }

    /// Converts one interleaved block, keeping filter state across calls.
    ///
    /// Output length is `input_frames * 16000 / device_rate` plus or minus one
    /// frame depending on where the rational phase lands, which is why callers
    /// must count the frames they actually get rather than assume a ratio.
    pub fn convert(&mut self, block: &[i16]) -> &[i16] {
        if self.is_passthrough() {
            self.output.clear();
            self.output.extend_from_slice(block);
            return &self.output;
        }

        // `process` borrows the input and the filter at the same time, so the
        // scratch buffer is taken out of `self` for the duration.
        let mut mono = std::mem::take(&mut self.mono);
        mono.clear();
        let channels = usize::from(self.channels);
        let index = usize::from(self.channel_index);
        mono.reserve(block.len() / channels + 1);
        for frame in block.chunks_exact(channels) {
            mono.push(f32::from(frame[index]) / 32_768.0);
        }

        self.output.clear();
        {
            let resampled = self.resampler.process(&mono);
            self.output.reserve(resampled.len());
            for sample in resampled {
                let clamped = sample.clamp(-1.0, 1.0) * 32_767.0;
                self.output.push(clamped.round() as i16);
            }
        }
        self.mono = mono;
        &self.output
    }
}

/// Why a device format could not be turned into the wire format.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireConversionError {
    #[error("audio.mono_channel_index: {index} is out of range for the {channels} channel(s) the device opened")]
    ChannelIndex { index: u16, channels: u16 },
    #[error("cannot start the format-conversion thread: {0}")]
    Thread(String),
    #[error("{0}")]
    Resampler(#[from] ResamplerError),
}

/// A local microphone normalised onto the wire format before it reaches
/// anything else.
///
/// The split deployment converts in the node, so the server receives wire-format
/// audio and the dispatcher never has to know what the microphone clocked at.
/// This is the same arrangement for the single-box build: the CPAL callback
/// still pushes *device* blocks into a ring (it must never resample — that
/// allocates and filters, which is not allowed on an audio thread), and one
/// dedicated thread converts them and publishes wire blocks into the ring the
/// dispatcher pops.
///
/// ```text
/// CPAL callback ──device frames──► device ring ──► converter thread ──wire frames──► wire ring ──► dispatcher
/// ```
///
/// The consequence downstream is the point of the whole arrangement: `WavFormat`,
/// the detector's resampler, the STT queue and the sidecar's audio hub all see
/// one format, so the device's rate is a fact about the device rather than a
/// property of the recording.
pub struct LocalWireCapture {
    wire: Arc<CaptureRing>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl LocalWireCapture {
    /// Starts the conversion thread. `wire` must have been built for
    /// [`WIRE_RATE_HZ`] × [`WIRE_CHANNELS`].
    pub fn spawn(
        device: Arc<CaptureRing>,
        wire: Arc<CaptureRing>,
        negotiated: &NegotiatedFormat,
        channel_index: u16,
        metrics: Arc<Metrics>,
    ) -> Result<Self, WireConversionError> {
        let converter = WireConverter::new(negotiated.rate_hz, negotiated.channels, channel_index)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_metrics = Arc::clone(&metrics);
        let thread_wire = Arc::clone(&wire);
        let thread_device = Arc::clone(&device);
        let join = std::thread::Builder::new()
            .name("steno-wire-convert".into())
            .spawn(move || {
                convert_loop(thread_device, thread_wire, converter, thread_metrics, thread_stop);
            })
            // A failed spawn would leave the wire ring permanently empty, which
            // the dispatcher cannot distinguish from a silent room. Fail loudly.
            .map_err(|error| WireConversionError::Thread(error.to_string()))?;
        Ok(LocalWireCapture {
            wire,
            stop,
            join: Some(join),
        })
    }

    /// The ring the dispatcher reads: always wire format.
    pub fn wire_ring(&self) -> &Arc<CaptureRing> {
        &self.wire
    }

    /// Stops the converter and waits for it, so no block is lost in flight.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for LocalWireCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Drains device blocks, converts them, publishes wire blocks.
///
/// The wire block's `capture_seq` is counted in *wire* frames, so a downstream
/// consumer comparing it with `stored_frames` is comparing like with like — the
/// device counter would be at a different rate and every gap would look wrong.
fn convert_loop(
    device: Arc<CaptureRing>,
    wire: Arc<CaptureRing>,
    mut converter: WireConverter,
    metrics: Arc<Metrics>,
    stop: Arc<AtomicBool>,
) {
    let mut block: Vec<i16> = Vec::new();
    let mut out: Vec<i16> = Vec::new();
    let mut wire_frames: u64 = 0;
    // Wire frames the ring refused; handed to the wire ring as its own overflow
    // so the dispatcher registers one gap with the right duration, instead of
    // two counters that each half-describe the same loss.
    let mut rejected_frames: u64 = 0;
    loop {
        let stopping = stop.load(Ordering::Relaxed);
        let Some(meta) = device.pop_into(&mut block) else {
            if stopping {
                // Dragon: the device has stopped and its ring is empty, so
                // nothing can arrive after this point.
                break;
            }
            // Same idle shape as the dispatcher's own loop: a 20 ms block
            // arrives every 20 ms, so 2 ms of sleep costs no latency.
            std::thread::sleep(Duration::from_millis(2));
            continue;
        };
        if block.is_empty() {
            continue;
        }
        out.clear();
        out.extend_from_slice(converter.convert(&block));
        if out.is_empty() {
            continue;
        }
        let frames = out.len().min(u32::MAX as usize) as u32;
        let wire_meta = CaptureMeta {
            frames,
            channels: WIRE_CHANNELS,
            rate_hz: WIRE_RATE_HZ,
            capture_seq: wire_frames,
            // The device timestamp is preserved: it is the only clock the audio
            // actually has, and re-stamping it here would make every converted
            // block look like it happened when the converter ran.
            mono_start_ns: meta.mono_start_ns,
            stream_generation: meta.stream_generation,
        };
        wire_frames = wire_frames.saturating_add(u64::from(frames));
        if !wire.callback_push(&out, wire_meta) {
            rejected_frames = rejected_frames.saturating_add(u64::from(frames));
            wire.note_overflow_frames(u64::from(frames));
        }
        metrics
            .capture_ring_high_water_frames
            .fetch_max(wire.high_water_frames(), Ordering::Relaxed);
    }
    if rejected_frames > 0 {
        tracing::warn!(
            rejected_frames,
            "the wire ring refused converted audio; those frames are a recorded gap"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_ring_keeps_block_boundaries_and_exact_samples() {
        let ring = CaptureRing::new(16_000, 1, 500, 10, 200);
        let data = [1i16, -2, 3, -4];
        let meta = CaptureMeta { frames: 4, channels: 1, rate_hz: 16_000, capture_seq: 0, mono_start_ns: 123, stream_generation: 1 };
        assert!(ring.callback_push(&data, meta));
        let mut out = Vec::new();
        assert_eq!(ring.pop_into(&mut out), Some(meta));
        assert_eq!(out, data);
        assert_eq!(ring.pop_into(&mut out), None);
    }

    #[test]
    fn ring_overflow_drops_entire_block_and_counts_frames() {
        let ring = CaptureRing::new(1_000, 1, 10, 10, 10);
        let samples = vec![7i16; 20]; // larger than allocated max block
        let meta = CaptureMeta { frames: 20, channels: 1, rate_hz: 1_000, capture_seq: 0, mono_start_ns: 0, stream_generation: 0 };
        assert!(!ring.callback_push(&samples, meta));
        assert_eq!(ring.overflow_frames(), 20);
        assert!(ring.pop_into(&mut Vec::new()).is_none());
    }

    #[test]
    fn reconnect_backoff_caps_at_thirty_seconds_and_resets() {
        let mut b = ReconnectBackoff::default();
        let delays: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30, 30]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }

    /// A stereo 48 kHz block is what a desktop USB microphone actually delivers,
    /// and it is the case that used to reach the server mislabelled. One second
    /// in must come out as one second, mono, at 16 kHz — not as six seconds of
    /// rumble.
    #[test]
    fn a_stereo_48k_block_becomes_one_second_of_16k_mono() {
        let rate = 48_000u32;
        let frames = usize::try_from(rate).unwrap();
        // 1 kHz tone, identical on both channels, at half scale.
        let tone: Vec<i16> = (0..frames)
            .flat_map(|n| {
                let phase = 2.0 * std::f32::consts::PI * 1_000.0 * n as f32 / rate as f32;
                let value = (phase.sin() * 16_384.0) as i16;
                [value, value]
            })
            .collect();

        let mut converter = WireConverter::new(rate, 2, 0).unwrap();
        assert!(!converter.is_passthrough());
        let out = converter.convert(&tone).to_vec();

        // The polyphase filter has a group delay and the block boundary is not a
        // whole number of output frames, so the length is one second to within a
        // frame or two rather than exactly 16000.
        let expected = 16_000usize;
        assert!(
            out.len().abs_diff(expected) <= 2,
            "expected about {expected} output frames, got {}",
            out.len()
        );

        // A 1 kHz tone must still be a 1 kHz tone: count zero crossings.
        let crossings = out.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        let measured_hz = crossings as f32 / 2.0 * (16_000.0 / out.len() as f32);
        assert!(
            (measured_hz - 1_000.0).abs() < 60.0,
            "the tone must survive at 1 kHz, measured {measured_hz:.0} Hz"
        );

        // And at the right level: taking one channel of a stereo pair must not
        // halve or double the amplitude.
        let peak = out.iter().map(|s| i32::from(*s).abs()).max().unwrap_or(0);
        assert!(
            (12_000..=17_500).contains(&peak),
            "peak {peak} is not the level of the input"
        );
    }

    /// `mono_channel_index` has to actually select, not average. A stereo device
    /// with one capsule pointed at the room and one at a wall would otherwise
    /// mix a reflection into the recogniser's input.
    #[test]
    fn the_configured_channel_is_taken_and_the_other_one_ignored() {
        // Left = +8000, right = -8000. An average would be silence; picking
        // channel 1 must give -8000.
        let block: Vec<i16> = (0..640).flat_map(|_| [8_000i16, -8_000i16]).collect();

        let mut left = WireConverter::new(16_000, 2, 0).unwrap();
        let left_out = left.convert(&block).to_vec();
        assert!(!left_out.is_empty());
        assert!(
            left_out.iter().all(|sample| *sample == 8_000),
            "channel 0 must win outright, got {:?}",
            &left_out[..4]
        );

        let mut right = WireConverter::new(16_000, 2, 1).unwrap();
        let right_out = right.convert(&block).to_vec();
        assert!(
            right_out.iter().all(|sample| *sample == -8_000),
            "channel 1 must win outright, got {:?}",
            &right_out[..4]
        );
    }

    /// A device that opened with fewer channels than the configured index is an
    /// error, not a clamp to 0: silently analysing the wrong capsule is exactly
    /// the kind of "it looks fine but the text is nonsense" failure this project
    /// treats as a bug.
    #[test]
    fn an_index_the_device_does_not_have_is_refused() {
        assert_eq!(
            WireConverter::new(16_000, 1, 1).unwrap_err(),
            WireConversionError::ChannelIndex {
                index: 1,
                channels: 1
            }
        );
    }

    /// The same tone played into a mono 16 kHz microphone must pass through
    /// untouched — no filtering, no level change, no latency.
    #[test]
    fn a_16k_mono_block_is_passed_through_untouched() {
        let block: Vec<i16> = (0..320).map(|n| (n as i16 - 160) * 100).collect();
        let mut converter = WireConverter::new(16_000, 1, 0).unwrap();
        assert!(converter.is_passthrough());
        assert_eq!(converter.convert(&block), block.as_slice());
    }

    /// 44.1 kHz is in the supported set only because the resampler expresses the
    /// ratio exactly (441:160); this pins that it still ends up on the wire rate.
    #[test]
    fn a_44k_block_also_lands_on_the_wire_rate() {
        let mut converter = WireConverter::new(44_100, 1, 0).unwrap();
        let block = vec![1_000i16; 44_100];
        let out = converter.convert(&block);
        assert!(out.len().abs_diff(16_000) <= 2, "got {} frames", out.len());
        // The filter's own warm-up is at the start of the block; the steady
        // state must carry the input level unchanged.
        let settled = &out[200..out.len() - 200];
        assert!(
            settled.iter().all(|sample| (sample - 1_000).abs() <= 2),
            "a constant input must stay constant, got {:?}",
            &settled[..8]
        );
    }

    /// The single-box path's converter publishes wire format, its own frame
    /// counter, and the device's original timestamp.
    #[test]
    fn a_local_capture_publishes_wire_format_blocks() {
        use crate::config::SUPPORTED_CAPTURE_RATES;
        assert!(SUPPORTED_CAPTURE_RATES.contains(&48_000));

        let device = Arc::new(CaptureRing::new(48_000, 1, 500, 10, 200));
        let wire = Arc::new(CaptureRing::new(WIRE_RATE_HZ, WIRE_CHANNELS, 500, 10, 200));
        let negotiated = NegotiatedFormat {
            device_name: "test".to_string(),
            rate_hz: 48_000,
            channels: 1,
            format: SampleFormat::S16Le,
        };
        let mut capture = LocalWireCapture::spawn(
            Arc::clone(&device),
            Arc::clone(&wire),
            &negotiated,
            0,
            Arc::new(Metrics::new()),
        )
        .expect("converter thread");

        // 100 ms of device audio with a distinctive timestamp.
        let block = vec![4_000i16; 4_800];
        let pushed = device.callback_push(
            &block,
            CaptureMeta {
                frames: 4_800,
                channels: 1,
                rate_hz: 48_000,
                capture_seq: 0,
                mono_start_ns: 7_000_000,
                stream_generation: 1,
            },
        );
        assert!(pushed);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut out = Vec::new();
        let mut meta = None;
        while Instant::now() < deadline {
            if let Some(popped) = wire.pop_into(&mut out) {
                meta = Some(popped);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let meta = meta.expect("the converter never published a block");

        assert_eq!(meta.channels, WIRE_CHANNELS);
        assert_eq!(meta.rate_hz, WIRE_RATE_HZ);
        assert_eq!(meta.mono_start_ns, 7_000_000, "the device clock is preserved");
        assert_eq!(meta.capture_seq, 0, "the first wire block starts at frame 0");
        assert!(
            meta.frames.abs_diff(1_600) <= 2,
            "100 ms at 48 kHz is 1600 wire frames, got {}",
            meta.frames
        );
        assert_eq!(out.len(), meta.frames as usize);
        capture.stop();
    }
}
