use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tracing::warn;

/// Real-time audio connection status reported to the UI and logger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStatus {
    Connected,
    NoInput,
    NoOutput,
    Error(String),
    Stopped,
}

impl std::fmt::Display for AudioStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected => write!(f, "Connected"),
            Self::NoInput => write!(f, "No Input Device"),
            Self::NoOutput => write!(f, "No Output Device"),
            Self::Error(msg) => write!(f, "Error: {msg}"),
            Self::Stopped => write!(f, "Stopped"),
        }
    }
}

/// Metadata about an active audio stream (for logging and UI inspection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStreamInfo {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

impl std::fmt::Display for AudioStreamInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "'{}' ({} ch, {} Hz, {:?})",
            self.device_name, self.channels, self.sample_rate, self.sample_format
        )
    }
}

/// Diagnostic state for monitoring the audio pipeline.
pub struct AudioDiagnostics {
    status_code: AtomicU32, // 0: Stopped, 1: Connected, 2: NoInput, 3: NoOutput, 4: Error
    last_error: RwLock<Option<String>>,
    input_info: RwLock<Option<AudioStreamInfo>>,
    output_info: RwLock<Option<AudioStreamInfo>>,
    underrun_counter: AtomicUsize,
    overrun_counter: AtomicUsize,
    last_underrun_log: Mutex<Option<Instant>>,
    last_overrun_log: Mutex<Option<Instant>>,
}

impl AudioDiagnostics {
    pub fn new() -> Self {
        Self {
            status_code: AtomicU32::new(0),
            last_error: RwLock::new(None),
            input_info: RwLock::new(None),
            output_info: RwLock::new(None),
            underrun_counter: AtomicUsize::new(0),
            overrun_counter: AtomicUsize::new(0),
            last_underrun_log: Mutex::new(None),
            last_overrun_log: Mutex::new(None),
        }
    }

    pub fn set_status(&self, status: AudioStatus) {
        let code = match &status {
            AudioStatus::Stopped => 0,
            AudioStatus::Connected => 1,
            AudioStatus::NoInput => 2,
            AudioStatus::NoOutput => 3,
            AudioStatus::Error(err) => {
                if let Ok(mut lock) = self.last_error.write() {
                    *lock = Some(err.clone());
                }
                4
            }
        };
        if code != 4 {
            if let Ok(mut lock) = self.last_error.write() {
                *lock = None;
            }
        }
        self.status_code.store(code, Ordering::Release);
    }

    pub fn status(&self) -> AudioStatus {
        match self.status_code.load(Ordering::Acquire) {
            1 => AudioStatus::Connected,
            2 => AudioStatus::NoInput,
            3 => AudioStatus::NoOutput,
            4 => AudioStatus::Error(
                self.last_error
                    .read()
                    .ok()
                    .and_then(|lock| lock.clone())
                    .unwrap_or_else(|| "unknown error".to_string()),
            ),
            _ => AudioStatus::Stopped,
        }
    }

    pub fn set_input_info(&self, info: Option<AudioStreamInfo>) {
        if let Ok(mut lock) = self.input_info.write() {
            *lock = info;
        }
    }

    pub fn set_output_info(&self, info: Option<AudioStreamInfo>) {
        if let Ok(mut lock) = self.output_info.write() {
            *lock = info;
        }
    }

    #[allow(dead_code)]
    pub fn input_info(&self) -> Option<AudioStreamInfo> {
        self.input_info.read().ok().and_then(|lock| lock.clone())
    }

    #[allow(dead_code)]
    pub fn output_info(&self) -> Option<AudioStreamInfo> {
        self.output_info.read().ok().and_then(|lock| lock.clone())
    }

    pub fn record_underrun(&self) {
        let count = self.underrun_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut last_log) = self.last_underrun_log.lock() {
            let now = Instant::now();
            let should_log = match *last_log {
                Some(t) => now.duration_since(t).as_secs() >= 5,
                None => true,
            };
            if should_log {
                warn!(
                    "audio output underrun (buffer starvation, total: {} occurrences)",
                    count
                );
                *last_log = Some(now);
            }
        }
    }

    pub fn record_overrun(&self) {
        let count = self.overrun_counter.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut last_log) = self.last_overrun_log.lock() {
            let now = Instant::now();
            let should_log = match *last_log {
                Some(t) => now.duration_since(t).as_secs() >= 5,
                None => true,
            };
            if should_log {
                warn!(
                    "audio input overrun (buffer overflow, dropped samples, total: {} occurrences)",
                    count
                );
                *last_log = Some(now);
            }
        }
    }
}

/// Convert any supported cpal sample to f32 in [-1.0, 1.0].
#[allow(dead_code)]
#[inline]
pub fn sample_to_f32<T: Sample>(sample: T) -> f32
where
    f32: FromSample<T>,
{
    f32::from_sample(sample)
}

/// Convert f32 in [-1.0, 1.0] to target sample type.
#[allow(dead_code)]
#[inline]
pub fn f32_to_sample<T: Sample + SizedSample + FromSample<f32>>(sample: f32) -> T {
    T::from_sample(sample.clamp(-1.0, 1.0))
}

/// Convert interleaved multi-channel audio from `in_channels` to `out_channels`.
/// Handles:
/// - mono -> stereo (duplicates sample to left and right)
/// - stereo -> mono (averages left and right: (L + R) * 0.5)
/// - pass-through when in_channels == out_channels
/// - multi-channel downmixing (N -> 2 or N -> 1)
pub fn convert_channels(
    input: &[f32],
    in_channels: usize,
    out_channels: usize,
    output: &mut Vec<f32>,
) {
    if in_channels == 0 || out_channels == 0 {
        return;
    }

    if in_channels == out_channels {
        output.extend_from_slice(input);
        return;
    }

    let frame_count = input.len() / in_channels;
    output.reserve(frame_count * out_channels);

    match (in_channels, out_channels) {
        // Mono to stereo: duplicate each sample
        (1, 2) => {
            for &s in input.iter().take(frame_count) {
                output.push(s);
                output.push(s);
            }
        }
        // Stereo to mono: average left and right
        (2, 1) => {
            for chunk in input.chunks_exact(2).take(frame_count) {
                output.push(0.5 * (chunk[0] + chunk[1]));
            }
        }
        // Mono to multi-channel (>2): replicate mono to all front channels
        (1, n) => {
            for &s in input.iter().take(frame_count) {
                for _ in 0..n {
                    output.push(s);
                }
            }
        }
        // Downmix multi-channel (>2) to stereo: take first two channels
        (n, 2) if n > 2 => {
            for chunk in input.chunks_exact(n).take(frame_count) {
                output.push(chunk[0]);
                output.push(chunk[1]);
            }
        }
        // Downmix multi-channel (>2) to mono: average all channels
        (n, 1) if n > 2 => {
            let inv = 1.0 / n as f32;
            for chunk in input.chunks_exact(n).take(frame_count) {
                let sum: f32 = chunk.iter().sum();
                output.push(sum * inv);
            }
        }
        // General arbitrary conversion: copy minimum channels, pad remaining with 0.0
        (n, m) => {
            let min_ch = n.min(m);
            for chunk in input.chunks_exact(n).take(frame_count) {
                for i in 0..min_ch {
                    output.push(chunk[i]);
                }
                for _ in min_ch..m {
                    output.push(0.0);
                }
            }
        }
    }
}

/// A high-quality, low-latency Catmull-Rom cubic Hermite audio resampler.
/// Interpolates between input frames to produce audio at the target output rate.
/// Preserves border history so chunk boundaries are completely seamless and click-free.
pub struct AudioResampler {
    in_rate: u32,
    out_rate: u32,
    channels: usize,
    ratio: f64, // in_rate / out_rate
    phase: f64, // Fractional frame position in the input stream [0.0, 1.0)
    // History buffer of previous input frames (up to 3 frames) to support 4-point cubic interpolation across chunk edges
    history: Vec<f32>,
    history_frames: usize,
}

impl AudioResampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            in_rate,
            out_rate,
            channels,
            ratio: in_rate as f64 / out_rate as f64,
            phase: 0.0,
            history: vec![0.0; channels * 4],
            history_frames: 0,
        }
    }

    #[allow(dead_code)]
    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    #[allow(dead_code)]
    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    #[allow(dead_code)]
    pub fn channels(&self) -> usize {
        self.channels
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.history.fill(0.0);
        self.history_frames = 0;
    }

    /// Resample interleaved multi-channel input samples into the output buffer.
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if self.in_rate == self.out_rate {
            // Identity: zero resampling required!
            output.extend_from_slice(input);
            return;
        }

        let in_frames = input.len() / self.channels;
        if in_frames == 0 {
            return;
        }

        // We combine the history frames with the new input frames to have a continuous sequence
        // Total sequence has (history_frames + in_frames) frames.
        let mut full_buffer = Vec::with_capacity((self.history_frames + in_frames) * self.channels);
        full_buffer.extend_from_slice(&self.history[..self.history_frames * self.channels]);
        full_buffer.extend_from_slice(&input[..in_frames * self.channels]);

        let total_frames = self.history_frames + in_frames;
        if total_frames < 2 {
            // Not enough frames yet to interpolate; store in history and wait for next chunk
            self.history[..full_buffer.len()].copy_from_slice(&full_buffer);
            self.history_frames = total_frames;
            return;
        }

        let channels = self.channels;
        let mut cur_pos = self.history_frames as f64 - 1.0 + self.phase;
        if cur_pos < 0.0 {
            cur_pos = 0.0;
        }

        let max_frame = total_frames - 1;

        while cur_pos < (total_frames - 1) as f64 {
            let i1 = cur_pos.floor() as usize;
            let i0 = if i1 > 0 { i1 - 1 } else { 0 };
            let i2 = (i1 + 1).min(max_frame);
            let i3 = (i1 + 2).min(max_frame);
            let t = (cur_pos - i1 as f64) as f32;

            for ch in 0..channels {
                let y0 = full_buffer[i0 * channels + ch];
                let y1 = full_buffer[i1 * channels + ch];
                let y2 = full_buffer[i2 * channels + ch];
                let y3 = full_buffer[i3 * channels + ch];

                // 4-point Catmull-Rom cubic interpolation
                let a = -0.5 * y0 + 1.5 * y1 - 1.5 * y2 + 0.5 * y3;
                let b = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
                let c = -0.5 * y0 + 0.5 * y2;
                let d = y1;

                let val = ((a * t + b) * t + c) * t + d;
                output.push(val);
            }

            cur_pos += self.ratio;
        }

        // Calculate phase for the next chunk relative to the remaining frames
        let consumed_frames = cur_pos.floor() as usize;
        if consumed_frames < total_frames {
            let remain = total_frames - consumed_frames;
            let keep_frames = remain.min(3);
            let start = total_frames - keep_frames;
            self.history[..keep_frames * channels]
                .copy_from_slice(&full_buffer[start * channels..]);
            self.history_frames = keep_frames;
            self.phase = cur_pos - (total_frames - 1) as f64;
            if self.phase < 0.0 {
                self.phase = 0.0;
            }
        } else {
            let keep_frames = total_frames.min(3);
            let start = total_frames - keep_frames;
            self.history[..keep_frames * channels]
                .copy_from_slice(&full_buffer[start * channels..]);
            self.history_frames = keep_frames;
            self.phase = cur_pos - total_frames as f64;
            if self.phase < 0.0 {
                self.phase = 0.0;
            }
        }
    }
}

/// A thread-safe, lock-free ring buffer for real-time audio sample streaming.
/// Samples are stored as bitwise f32 in AtomicU32.
pub struct AudioRingBuffer {
    data: Box<[AtomicU32]>,
    capacity: usize,
    read_index: AtomicUsize,
    write_index: AtomicUsize,
    diagnostics: Arc<AudioDiagnostics>,
}

impl AudioRingBuffer {
    pub fn new(capacity: usize, diagnostics: Arc<AudioDiagnostics>) -> Self {
        let mut values = Vec::with_capacity(capacity);
        values.resize_with(capacity, || AtomicU32::new(0));
        Self {
            data: values.into_boxed_slice(),
            capacity,
            read_index: AtomicUsize::new(0),
            write_index: AtomicUsize::new(0),
            diagnostics,
        }
    }

    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[allow(dead_code)]
    pub fn available(&self) -> usize {
        let write = self.write_index.load(Ordering::Acquire);
        let read = self.read_index.load(Ordering::Relaxed);
        write.wrapping_sub(read)
    }

    /// Pushes samples into the ring buffer. If buffer overflows, drops excess and logs.
    pub fn push_samples(&self, samples: &[f32]) -> usize {
        let mut written = 0;
        let mut write = self.write_index.load(Ordering::Relaxed);
        let read = self.read_index.load(Ordering::Acquire);
        let occupied = write.wrapping_sub(read);

        if occupied >= self.capacity {
            self.diagnostics.record_overrun();
            return 0;
        }

        let space = self.capacity - occupied;
        let to_write = samples.len().min(space);

        if to_write < samples.len() {
            self.diagnostics.record_overrun();
        }

        while written < to_write {
            let slot = write % self.capacity;
            self.data[slot].store(samples[written].to_bits(), Ordering::Relaxed);
            write = write.wrapping_add(1);
            written += 1;
        }

        if written > 0 {
            self.write_index.store(write, Ordering::Release);
        }

        written
    }

    /// Pops samples into output slice. If buffer underruns, fills missing samples with 0.0.
    pub fn pop_samples(&self, output: &mut [f32]) -> usize {
        let mut read = self.read_index.load(Ordering::Relaxed);
        let write = self.write_index.load(Ordering::Acquire);
        let available = write.wrapping_sub(read);
        let count = available.min(output.len());

        for sample in output.iter_mut().take(count) {
            let slot = read % self.capacity;
            *sample = f32::from_bits(self.data[slot].load(Ordering::Relaxed));
            read = read.wrapping_add(1);
        }

        if count < output.len() {
            // Buffer starvation / underrun
            for sample in &mut output[count..] {
                *sample = 0.0;
            }
            if available == 0 {
                self.diagnostics.record_underrun();
            }
        }

        if count > 0 {
            self.read_index.store(read, Ordering::Release);
        }

        count
    }
}

/// Volume and mute controller that can be safely updated from any thread and read in the audio callback.
#[derive(Clone)]
pub struct VolumeController {
    volume_bits: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
}

impl VolumeController {
    pub fn new(volume: f64, muted: bool) -> Self {
        let clamped = volume.clamp(0.0, 1.0) as f32;
        Self {
            volume_bits: Arc::new(AtomicU32::new(clamped.to_bits())),
            muted: Arc::new(AtomicBool::new(muted)),
        }
    }

    pub fn set_volume(&self, volume: f64) {
        let clamped = volume.clamp(0.0, 1.0) as f32;
        self.volume_bits.store(clamped.to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    /// Multiplier to apply to output samples (0.0 if muted, otherwise volume).
    #[inline]
    pub fn gain(&self) -> f32 {
        if self.is_muted() {
            0.0
        } else {
            self.volume()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_conversion_mono_to_stereo() {
        let input = [0.25f32, -0.5, 0.75];
        let mut output = Vec::new();
        convert_channels(&input, 1, 2, &mut output);
        assert_eq!(output.len(), 6);
        assert_eq!(output, vec![0.25, 0.25, -0.5, -0.5, 0.75, 0.75]);
    }

    #[test]
    fn channel_conversion_stereo_to_mono() {
        let input = [0.2f32, 0.4, -0.6, 0.2];
        let mut output = Vec::new();
        convert_channels(&input, 2, 1, &mut output);
        assert_eq!(output.len(), 2);
        assert!((output[0] - 0.3).abs() < 1e-5);
        assert!((output[1] - -0.2).abs() < 1e-5);
    }

    #[test]
    fn channel_conversion_identical_passthrough() {
        let input = [0.1f32, 0.2, 0.3, 0.4];
        let mut output = Vec::new();
        convert_channels(&input, 2, 2, &mut output);
        assert_eq!(output, input);
    }

    #[test]
    fn channel_conversion_multichannel_downmix() {
        // 4 channels downmix to 2 (take front stereo channels)
        let input = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let mut output = Vec::new();
        convert_channels(&input, 4, 2, &mut output);
        assert_eq!(output, vec![1.0, 2.0, 5.0, 6.0]);
    }

    #[test]
    fn resampler_identity_returns_exact_input() {
        let mut resampler = AudioResampler::new(48000, 48000, 2);
        let input = [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mut output = Vec::new();
        resampler.process(&input, &mut output);
        assert_eq!(output, input);
    }

    #[test]
    fn resampler_44100_to_48000_ratio_matches() {
        let mut resampler = AudioResampler::new(44100, 48000, 2);
        // Feed 4410 frames (100 ms of 44.1kHz audio)
        let in_frames = 4410;
        let mut input = Vec::with_capacity(in_frames * 2);
        for i in 0..in_frames {
            let t = i as f32 / 44100.0;
            let sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            input.push(sample);
            input.push(sample);
        }

        let mut output = Vec::new();
        resampler.process(&input, &mut output);
        let out_frames = output.len() / 2;
        // Expected ~4800 frames
        assert!(
            (out_frames as i32 - 4800).abs() <= 5,
            "expected ~4800 frames, got {out_frames}"
        );
    }

    #[test]
    fn resampler_96000_to_48000_ratio_matches() {
        let mut resampler = AudioResampler::new(96000, 48000, 2);
        let in_frames = 9600;
        let mut input = Vec::with_capacity(in_frames * 2);
        for i in 0..in_frames {
            let s = (i % 100) as f32 / 100.0;
            input.push(s);
            input.push(s);
        }

        let mut output = Vec::new();
        resampler.process(&input, &mut output);
        let out_frames = output.len() / 2;
        // Expected ~4800 frames
        assert!(
            (out_frames as i32 - 4800).abs() <= 5,
            "expected ~4800 frames, got {out_frames}"
        );
    }

    #[test]
    fn ring_buffer_push_pop_order() {
        let diag = Arc::new(AudioDiagnostics::new());
        let ring = AudioRingBuffer::new(16, diag);
        assert_eq!(ring.push_samples(&[1.0, 2.0, 3.0, 4.0]), 4);
        assert_eq!(ring.available(), 4);

        let mut out = [0.0f32; 4];
        assert_eq!(ring.pop_samples(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn ring_buffer_underrun_zero_fills() {
        let diag = Arc::new(AudioDiagnostics::new());
        let ring = AudioRingBuffer::new(8, diag.clone());
        assert_eq!(ring.push_samples(&[0.5, 0.6]), 2);

        let mut out = [1.0f32; 4];
        let count = ring.pop_samples(&mut out);
        assert_eq!(count, 2);
        assert_eq!(out[0], 0.5);
        assert_eq!(out[1], 0.6);
        assert_eq!(out[2], 0.0);
        assert_eq!(out[3], 0.0);
    }

    #[test]
    fn ring_buffer_overrun_limits_to_capacity() {
        let diag = Arc::new(AudioDiagnostics::new());
        let ring = AudioRingBuffer::new(4, diag.clone());
        assert_eq!(ring.push_samples(&[0.1, 0.2, 0.3, 0.4, 0.5]), 4);
        assert_eq!(ring.available(), 4);
    }

    #[test]
    fn volume_controller_gain() {
        let vol = VolumeController::new(0.8, false);
        assert!((vol.gain() - 0.8).abs() < 1e-4);

        vol.set_muted(true);
        assert_eq!(vol.gain(), 0.0);

        vol.set_muted(false);
        assert!((vol.gain() - 0.8).abs() < 1e-4);

        vol.set_volume(0.5);
        assert!((vol.gain() - 0.5).abs() < 1e-4);
    }
}
