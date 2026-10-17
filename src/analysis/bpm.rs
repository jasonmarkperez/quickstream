//! BPM detection and acoustic energy analysis using embedded tags or ffmpeg DSP.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Hard limit for a single ffmpeg decode (a stalled network share must never hang a worker).
pub const FFMPEG_TIMEOUT: Duration = Duration::from_secs(20);
/// Preferred seek offset into the track for tempo analysis (skips intros).
const ACOUSTIC_SEEK_SECS: u32 = 30;
/// Length of the decoded analysis slice.
const ACOUSTIC_SLICE_SECS: u32 = 20;
const ANALYSIS_SAMPLE_RATE: u32 = 11025;
/// Minimum decoded audio needed for a usable analysis.
const MIN_SLICE_SECS: u32 = 3;

/// Tempo and energy derived from a single decoded slice.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AcousticAnalysis {
    pub bpm: Option<f64>,
    pub energy: Option<f64>,
}

/// Detect BPM and acoustic energy for an audio track.
pub struct BpmAnalyzer;

impl BpmAnalyzer {
    /// Extract BPM from embedded tags or acoustic analysis.
    pub fn detect_bpm(path: &Path) -> Option<f64> {
        // 1. Try reading embedded BPM tag first (pure metadata, zero DSP cost)
        if let Some(tag_bpm) = Self::read_embedded_bpm(path) {
            return Some(tag_bpm);
        }

        // 2. Perform fast local acoustic analysis via ffmpeg
        Self::analyze_acoustic_tempo(path)
    }

    /// Read embedded BPM tag via lofty (ID3 `TBPM`, MP4 `tmpo` / iTunes BPM, Vorbis `BPM`).
    pub fn read_embedded_bpm(path: &Path) -> Option<f64> {
        use lofty::config::ParseOptions;
        use lofty::prelude::*;
        use lofty::probe::Probe;

        let tagged_file = Probe::open(path)
            .ok()?
            .options(ParseOptions::new().read_cover_art(false))
            .read()
            .ok()?;
        tagged_file.tags().iter().find_map(Self::bpm_from_tag)
    }

    /// Extract a plausible BPM value from a single lofty tag.
    /// ID3v2 `TBPM` and MP4 `tmpo` map to `ItemKey::IntegerBpm`; others map to `ItemKey::Bpm`.
    pub fn bpm_from_tag(tag: &lofty::tag::Tag) -> Option<f64> {
        use lofty::tag::ItemKey;

        [ItemKey::Bpm, ItemKey::IntegerBpm].into_iter().find_map(|key| {
            let bpm = tag.get_string(key)?.trim().parse::<f64>().ok()?;
            (bpm > 30.0 && bpm < 300.0).then_some(bpm)
        })
    }

    /// Perform acoustic onset autocorrelation on a 20-second mono 11.025kHz slice.
    pub fn analyze_acoustic_tempo(path: &Path) -> Option<f64> {
        let samples = Self::decode_pcm_slice(path, ACOUSTIC_SEEK_SECS, ACOUSTIC_SLICE_SECS, ANALYSIS_SAMPLE_RATE, None)?;
        Self::compute_bpm_from_samples(&samples, ANALYSIS_SAMPLE_RATE as f32)
    }

    /// Decode one slice and derive both tempo and energy from it (single ffmpeg call).
    /// `cancel` aborts the ffmpeg subprocess early (e.g. on application shutdown).
    pub fn analyze_acoustic(path: &Path, cancel: Option<&AtomicBool>) -> AcousticAnalysis {
        match Self::decode_pcm_slice(path, ACOUSTIC_SEEK_SECS, ACOUSTIC_SLICE_SECS, ANALYSIS_SAMPLE_RATE, cancel) {
            Some(samples) => AcousticAnalysis {
                bpm: Self::compute_bpm_from_samples(&samples, ANALYSIS_SAMPLE_RATE as f32),
                energy: Self::energy_from_samples(&samples),
            },
            None => AcousticAnalysis::default(),
        }
    }

    /// Decode `duration_secs` of mono PCM starting at `offset_secs`.
    /// If the track is shorter than the seek offset (too little audio returned), retry from 0.
    pub fn decode_pcm_slice(
        path: &Path,
        offset_secs: u32,
        duration_secs: u32,
        sample_rate: u32,
        cancel: Option<&AtomicBool>,
    ) -> Option<Vec<f32>> {
        let min_samples = (sample_rate * MIN_SLICE_SECS) as usize;
        let samples = Self::decode_pcm_at(path, offset_secs, duration_secs, sample_rate, cancel);
        match samples {
            Some(s) if s.len() >= min_samples => Some(s),
            _ if offset_secs > 0 && !cancel.is_some_and(|c| c.load(Ordering::Relaxed)) => {
                Self::decode_pcm_at(path, 0, duration_secs, sample_rate, cancel)
                    .filter(|s| s.len() >= min_samples)
            }
            _ => None,
        }
    }

    fn decode_pcm_at(
        path: &Path,
        offset_secs: u32,
        duration_secs: u32,
        sample_rate: u32,
        cancel: Option<&AtomicBool>,
    ) -> Option<Vec<f32>> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-nostdin", "-ss", &offset_secs.to_string(), "-t", &duration_secs.to_string(), "-i"])
            .arg(path)
            .args([
                "-vn",
                "-sn",
                "-ar", &sample_rate.to_string(),
                "-ac", "1",
                "-f", "s16le",
                "-loglevel", "error",
                "-",
            ]);
        let pcm_bytes = Self::run_with_timeout(cmd, FFMPEG_TIMEOUT, cancel)?;
        Some(
            pcm_bytes
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                .collect(),
        )
    }

    /// Run a subprocess capturing stdout, killing it if it exceeds `timeout`
    /// or if `cancel` is raised. Returns `None` on failure, timeout or cancellation.
    pub fn run_with_timeout(mut cmd: Command, timeout: Duration, cancel: Option<&AtomicBool>) -> Option<Vec<u8>> {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        // Drain stdout on a helper thread so a full pipe buffer can never stall the child.
        let mut stdout = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });

        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    let cancelled = cancel.is_some_and(|c| c.load(Ordering::Relaxed));
                    if cancelled || Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };

        let output = reader.join().ok()?;
        match status {
            Some(s) if s.success() => Some(output),
            _ => None,
        }
    }

    /// Autocorrelate energy envelope across 60–180 BPM range.
    pub fn compute_bpm_from_samples(samples: &[f32], sample_rate: f32) -> Option<f64> {
        if samples.is_empty() {
            return None;
        }

        // Window size ~50ms for energy envelope
        let hop_size = (sample_rate * 0.050) as usize; // ~551 samples @ 11025Hz
        let mut envelope = Vec::new();

        for chunk in samples.chunks(hop_size) {
            let mut rms = 0.0f32;
            for &s in chunk {
                rms += s * s;
            }
            envelope.push((rms / chunk.len() as f32).sqrt());
        }

        // Compute first-order difference (onset flux)
        let mut flux = Vec::with_capacity(envelope.len());
        for i in 1..envelope.len() {
            let diff = envelope[i] - envelope[i - 1];
            flux.push(diff.max(0.0)); // half-wave rectification
        }

        if flux.len() < 50 {
            return None;
        }

        // Autocorrelation over BPM range 60 to 180 BPM
        let fps = sample_rate / hop_size as f32; // ~20 frames per sec
        let min_bpm = 60.0;
        let max_bpm = 180.0;
        let min_lag = (fps * 60.0 / max_bpm).round() as usize;
        let max_lag = (fps * 60.0 / min_bpm).round() as usize;

        let mut best_lag = 0;
        let mut max_corr = 0.0f32;

        for lag in min_lag..=max_lag.min(flux.len() / 2) {
            let mut corr = 0.0f32;
            for i in 0..(flux.len() - lag) {
                corr += flux[i] * flux[i + lag];
            }
            if corr > max_corr {
                max_corr = corr;
                best_lag = lag;
            }
        }

        if best_lag > 0 {
            let bpm = (fps * 60.0) / best_lag as f32;
            Some(bpm.round() as f64)
        } else {
            None
        }
    }

    /// Calculate RMS energy score (0.0 to 1.0).
    pub fn calculate_energy(path: &Path) -> Option<f64> {
        let samples = Self::decode_pcm_slice(path, 20, 10, 8000, None)?;
        Self::energy_from_samples(&samples)
    }

    /// Normalised RMS energy score (0.0 to 1.0) for a block of samples.
    pub fn energy_from_samples(samples: &[f32]) -> Option<f64> {
        if samples.is_empty() {
            return None;
        }
        let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
        let rms = (sum_sq / samples.len() as f64).sqrt();
        // Scale RMS (typically 0.05 to 0.40) to normalized 0.0 - 1.0 energy score
        let energy = (rms * 3.0).clamp(0.0, 1.0);
        Some((energy * 100.0).round() / 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_bpm_synthetic_pulses() {
        let sample_rate = 11025.0;
        let duration_secs = 10.0;
        let total_samples = (sample_rate * duration_secs) as usize;
        let mut samples = vec![0.0f32; total_samples];

        // Generate synthetic pulse at exactly 120 BPM (2 beats per second = every 0.500s = 5512.5 samples)
        let beat_interval = (sample_rate / 2.0) as usize;
        for i in (0..total_samples).step_by(beat_interval) {
            // Pulse of 50ms duration
            for j in 0..200 {
                if i + j < total_samples {
                    samples[i + j] = 0.8;
                }
            }
        }

        let bpm = BpmAnalyzer::compute_bpm_from_samples(&samples, sample_rate);
        assert!(bpm.is_some());
        let val = bpm.unwrap();
        assert!((115.0..=125.0).contains(&val), "Expected ~120 BPM, got {}", val);
    }
}
