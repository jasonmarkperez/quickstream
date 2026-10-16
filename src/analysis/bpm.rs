//! BPM detection and acoustic energy analysis using embedded tags or ffmpeg DSP.

use std::path::Path;
use std::process::Command;

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

    /// Read embedded TBPM / BPM tag via lofty.
    pub fn read_embedded_bpm(path: &Path) -> Option<f64> {
        use lofty::prelude::*;
        use lofty::probe::Probe;
        use lofty::tag::ItemKey;

        let tagged_file = Probe::open(path).ok()?.read().ok()?;
        let tag = tagged_file.primary_tag().or_else(|| tagged_file.first_tag())?;

        // 1. ItemKey::Bpm
        if let Some(val) = tag.get_string(ItemKey::Bpm) {
            if let Ok(bpm) = val.trim().parse::<f64>() {
                if bpm > 30.0 && bpm < 300.0 {
                    return Some(bpm);
                }
            }
        }

        // 2. Fallback to custom "TBPM" or "BPM" tag item
        for item in tag.items() {
            let key = item.key();
            if key == ItemKey::Bpm {
                if let Some(val) = item.value().text() {
                    if let Ok(bpm) = val.trim().parse::<f64>() {
                        if bpm > 30.0 && bpm < 300.0 {
                            return Some(bpm);
                        }
                    }
                }
            }
        }

        None
    }

    /// Perform acoustic onset autocorrelation on a 20-second mono 11.025kHz slice.
    pub fn analyze_acoustic_tempo(path: &Path) -> Option<f64> {
        // Extract 20 seconds from the middle of the track at 11,025 Hz mono 16-bit LE
        let output = Command::new("ffmpeg")
            .args([
                "-ss", "30",
                "-t", "20",
                "-i", path.to_str()?,
                "-vn",
                "-sn",
                "-ar", "11025",
                "-ac", "1",
                "-f", "s16le",
                "-loglevel", "error",
                "-",
            ])
            .output()
            .ok()?;

        if !output.status.success() || output.stdout.len() < 22050 * 2 {
            return None;
        }

        let pcm_bytes = output.stdout;
        let mut samples = Vec::with_capacity(pcm_bytes.len() / 2);
        for chunk in pcm_bytes.chunks_exact(2) {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32768.0;
            samples.push(sample);
        }

        Self::compute_bpm_from_samples(&samples, 11025.0)
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
        let output = Command::new("ffmpeg")
            .args([
                "-ss", "20",
                "-t", "10",
                "-i", path.to_str()?,
                "-vn",
                "-ar", "8000",
                "-ac", "1",
                "-f", "s16le",
                "-loglevel", "error",
                "-",
            ])
            .output()
            .ok()?;

        if !output.status.success() || output.stdout.is_empty() {
            return None;
        }

        let mut sum_sq = 0.0f64;
        let mut count = 0;
        for chunk in output.stdout.chunks_exact(2) {
            let s = i16::from_le_bytes([chunk[0], chunk[1]]) as f64 / 32768.0;
            sum_sq += s * s;
            count += 1;
        }

        if count > 0 {
            let rms = (sum_sq / count as f64).sqrt();
            // Scale RMS (typically 0.05 to 0.40) to normalized 0.0 - 1.0 energy score
            let energy = (rms * 3.0).clamp(0.0, 1.0);
            Some((energy * 100.0).round() / 100.0)
        } else {
            None
        }
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
