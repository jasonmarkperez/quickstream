//! BPM detection and acoustic energy analysis using embedded tags or ffmpeg DSP.

use std::io::Read;
use std::path::{Path, PathBuf};
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

/// How acoustic analysis invokes ffmpeg (binary and per-call timeout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegConfig {
    /// ffmpeg executable; a bare name is resolved via `PATH`.
    pub binary: PathBuf,
    /// Hard limit for a single decode.
    pub timeout: Duration,
}

impl Default for FfmpegConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("ffmpeg"),
            timeout: FFMPEG_TIMEOUT,
        }
    }
}

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
        let samples = Self::decode_pcm_slice(
            path,
            ACOUSTIC_SEEK_SECS,
            ACOUSTIC_SLICE_SECS,
            ANALYSIS_SAMPLE_RATE,
            None,
        )?;
        Self::compute_bpm_from_samples(&samples, ANALYSIS_SAMPLE_RATE as f32)
    }

    /// Decode one slice and derive both tempo and energy from it (single ffmpeg call).
    /// `cancel` aborts the ffmpeg subprocess early (e.g. on application shutdown).
    ///
    /// Returns `None` if the decode failed (ffmpeg missing, error exit, timeout, cancelled) or
    /// produced no audio. A successful decode in which no tempo is found (e.g. silence, or a clip
    /// too short for tempo analysis) is a real result: `Some` with `bpm: None`.
    pub fn analyze_acoustic(path: &Path, cancel: Option<&AtomicBool>) -> Option<AcousticAnalysis> {
        Self::analyze_acoustic_with(&FfmpegConfig::default(), path, cancel)
    }

    /// [`Self::analyze_acoustic`] using an explicit ffmpeg binary and timeout.
    pub fn analyze_acoustic_with(
        ffmpeg: &FfmpegConfig,
        path: &Path,
        cancel: Option<&AtomicBool>,
    ) -> Option<AcousticAnalysis> {
        let samples = Self::decode_slice_unfiltered(
            ffmpeg,
            path,
            ACOUSTIC_SEEK_SECS,
            ACOUSTIC_SLICE_SECS,
            ANALYSIS_SAMPLE_RATE,
            cancel,
        )
        .filter(|s| !s.is_empty())?;
        let long_enough = samples.len() >= (ANALYSIS_SAMPLE_RATE * MIN_SLICE_SECS) as usize;
        Some(AcousticAnalysis {
            bpm: if long_enough {
                Self::compute_bpm_from_samples(&samples, ANALYSIS_SAMPLE_RATE as f32)
            } else {
                None
            },
            energy: Self::energy_from_samples(&samples),
        })
    }

    /// Decode `duration_secs` of mono PCM starting at `offset_secs`.
    /// If the track is shorter than the seek offset (too little audio returned), retry from 0.
    /// Returns `None` if decoding failed or yielded less than the minimum usable audio.
    pub fn decode_pcm_slice(
        path: &Path,
        offset_secs: u32,
        duration_secs: u32,
        sample_rate: u32,
        cancel: Option<&AtomicBool>,
    ) -> Option<Vec<f32>> {
        Self::decode_pcm_slice_with(
            &FfmpegConfig::default(),
            path,
            offset_secs,
            duration_secs,
            sample_rate,
            cancel,
        )
    }

    /// [`Self::decode_pcm_slice`] using an explicit ffmpeg binary and timeout.
    pub fn decode_pcm_slice_with(
        ffmpeg: &FfmpegConfig,
        path: &Path,
        offset_secs: u32,
        duration_secs: u32,
        sample_rate: u32,
        cancel: Option<&AtomicBool>,
    ) -> Option<Vec<f32>> {
        let min_samples = (sample_rate * MIN_SLICE_SECS) as usize;
        Self::decode_slice_unfiltered(ffmpeg, path, offset_secs, duration_secs, sample_rate, cancel)
            .filter(|s| s.len() >= min_samples)
    }

    /// Decode a slice, retrying from offset 0 only when the first decode *succeeded* but returned
    /// too little audio (track shorter than the seek offset). A failed or timed-out first decode is
    /// not retried, so a stalled file costs at most one timeout.
    /// Returns `None` only if a decode failed; the returned samples may be short or empty.
    fn decode_slice_unfiltered(
        ffmpeg: &FfmpegConfig,
        path: &Path,
        offset_secs: u32,
        duration_secs: u32,
        sample_rate: u32,
        cancel: Option<&AtomicBool>,
    ) -> Option<Vec<f32>> {
        let min_samples = (sample_rate * MIN_SLICE_SECS) as usize;
        let first = Self::decode_pcm_at(ffmpeg, path, offset_secs, duration_secs, sample_rate, cancel)?;
        if first.len() >= min_samples || offset_secs == 0 || cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Some(first);
        }
        Self::decode_pcm_at(ffmpeg, path, 0, duration_secs, sample_rate, cancel)
    }

    fn decode_pcm_at(
        ffmpeg: &FfmpegConfig,
        path: &Path,
        offset_secs: u32,
        duration_secs: u32,
        sample_rate: u32,
        cancel: Option<&AtomicBool>,
    ) -> Option<Vec<f32>> {
        let mut cmd = Command::new(&ffmpeg.binary);
        cmd.args([
            "-nostdin",
            "-ss",
            &offset_secs.to_string(),
            "-t",
            &duration_secs.to_string(),
            "-i",
        ])
        .arg(path)
        .args([
            "-vn",
            "-sn",
            "-ar",
            &sample_rate.to_string(),
            "-ac",
            "1",
            "-f",
            "s16le",
            "-loglevel",
            "error",
            "-",
        ]);
        let pcm_bytes = Self::run_with_timeout(cmd, ffmpeg.timeout, cancel)?;
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

    /// Estimate tempo (60–180 BPM) from mono samples.
    ///
    /// 1. Onset strength: half-wave rectified difference of a 10 ms RMS envelope, mean removed.
    /// 2. Autocorrelation normalized by overlap length (a mean, not a sum), so long lags are not
    ///    penalized just because fewer frame pairs overlap.
    /// 3. Normalized correlation is equal at a beat period and its multiples, so the octave is
    ///    chosen with a broad log-tempo prior centred on 120 BPM (σ = 1 octave): a 120 BPM click
    ///    reads as 120 rather than 60, while a genuine 90 BPM track has no competing peak.
    /// 4. The winning lag is refined with parabolic interpolation for sub-frame precision.
    ///
    /// Tempos from 60 to ~160 BPM are reported as-is (±2 BPM on click tracks). Faster material
    /// may be reported at half tempo (e.g. 170 BPM drum & bass as 85); mood/BPM matching
    /// already treats half and double tempo as equivalent.
    ///
    /// Returns `None` for silence or clips too short to analyse.
    pub fn compute_bpm_from_samples(samples: &[f32], sample_rate: f32) -> Option<f64> {
        const HOP_SECS: f32 = 0.010;
        const MIN_BPM: f32 = 60.0;
        const MAX_BPM: f32 = 180.0;
        const PRIOR_CENTER_BPM: f32 = 120.0;
        const PRIOR_OCTAVES_SIGMA: f32 = 1.0;

        if samples.is_empty() || sample_rate <= 0.0 {
            return None;
        }
        let hop_size = ((sample_rate * HOP_SECS) as usize).max(1); // ~110 samples @ 11025 Hz
        let fps = sample_rate / hop_size as f32; // ~100 frames per second

        let envelope: Vec<f32> = samples
            .chunks(hop_size)
            .map(|chunk| (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt())
            .collect();

        // Onset flux: half-wave rectified first difference, then mean-removed so a constant
        // background (noise floor) adds no baseline to every lag.
        let raw_flux: Vec<f32> = envelope.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
        // Light triangular smoothing (±2 frames) widens each onset so a beat period that is not
        // a whole number of frames does not split its correlation peak across two lag bins
        // (which otherwise lets the sharper half-tempo peak win).
        const KERNEL: [f32; 5] = [1.0, 2.0, 3.0, 2.0, 1.0];
        let mut flux: Vec<f32> = (0..raw_flux.len())
            .map(|i| {
                KERNEL
                    .iter()
                    .enumerate()
                    .filter_map(|(k, w)| (i + k).checked_sub(2).and_then(|j| raw_flux.get(j)).map(|f| f * w))
                    .sum::<f32>()
                    / 9.0
            })
            .collect();
        let min_lag = (fps * 60.0 / MAX_BPM).floor().max(1.0) as usize;
        let max_lag = ((fps * 60.0 / MIN_BPM).ceil() as usize).min(flux.len() / 2);
        if flux.len() < 50 || max_lag <= min_lag + 1 {
            return None;
        }
        let mean = flux.iter().sum::<f32>() / flux.len() as f32;
        if mean <= f32::EPSILON {
            return None; // silence / no onsets
        }
        flux.iter_mut().for_each(|f| *f -= mean);

        let prior = |lag: f32| -> f32 {
            let octaves = ((fps * 60.0 / lag) / PRIOR_CENTER_BPM).log2() / PRIOR_OCTAVES_SIGMA;
            (-0.5 * octaves * octaves).exp()
        };
        // Score each lag in [min_lag, max_lag]; one extra lag either side for interpolation.
        let lo = min_lag.saturating_sub(1).max(1);
        let hi = (max_lag + 1).min(flux.len() - 1);
        let scores: Vec<f32> = (lo..=hi)
            .map(|lag| normalized_autocorrelation(&flux, lag) * prior(lag as f32))
            .collect();
        let score_at = |lag: usize| scores[lag - lo];

        let best_lag = (min_lag..=max_lag).max_by(|&a, &b| score_at(a).total_cmp(&score_at(b)))?;
        if score_at(best_lag) <= 0.0 {
            return None;
        }

        // Parabolic interpolation around the peak for sub-frame precision.
        let mut lag = best_lag as f32;
        if best_lag > lo && best_lag < hi {
            let (a, b, c) = (score_at(best_lag - 1), score_at(best_lag), score_at(best_lag + 1));
            let denom = a - 2.0 * b + c;
            if denom.abs() > f32::EPSILON {
                lag += (0.5 * (a - c) / denom).clamp(-0.5, 0.5);
            }
        }

        let bpm = fps * 60.0 / lag;
        (MIN_BPM - 1.0..=MAX_BPM + 1.0)
            .contains(&bpm)
            .then(|| bpm.round() as f64)
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

/// Autocorrelation of `flux` at `lag`, divided by the number of overlapping frame pairs.
/// Dividing by the overlap removes the bias of a raw sum toward short lags.
fn normalized_autocorrelation(flux: &[f32], lag: usize) -> f32 {
    let overlap = flux.len().saturating_sub(lag);
    if overlap == 0 {
        return 0.0;
    }
    let sum: f32 = flux[..overlap].iter().zip(&flux[lag..]).map(|(a, b)| a * b).sum();
    sum / overlap as f32
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

    const SR: f32 = ANALYSIS_SAMPLE_RATE as f32;

    /// Click track: decaying 880 Hz bursts at exact (fractional-sample) beat positions over a
    /// faint deterministic noise floor.
    fn click_track(bpm: f64, seconds: f64) -> Vec<f32> {
        let sr = SR as f64;
        let total = (sr * seconds) as usize;
        let mut rng: u32 = 0x1234_5678;
        let mut samples: Vec<f32> = (0..total)
            .map(|_| {
                rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((rng >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.02
            })
            .collect();
        let beat = sr * 60.0 / bpm;
        let burst = (sr * 0.04) as usize;
        let mut k = 0usize;
        loop {
            let start = (k as f64 * beat).round() as usize;
            if start >= total {
                break;
            }
            for j in 0..burst.min(total - start) {
                let t = j as f64 / sr;
                let env = 1.0 - j as f64 / burst as f64;
                samples[start + j] += (0.8 * env * (2.0 * std::f64::consts::PI * 880.0 * t).sin()) as f32;
            }
            k += 1;
        }
        samples
    }

    /// Detection over the production slice length (20 s) must land within ±2 BPM of the true
    /// tempo, not on an octave (half / double) or a neighbouring coarse lag bin.
    #[test]
    fn test_click_tracks_detected_within_two_bpm() {
        for &true_bpm in &[90.0, 100.0, 120.0, 128.0, 140.0] {
            let samples = click_track(true_bpm, ACOUSTIC_SLICE_SECS as f64);
            let detected = BpmAnalyzer::compute_bpm_from_samples(&samples, SR)
                .unwrap_or_else(|| panic!("no tempo detected for {true_bpm} BPM click track"));
            assert!(
                (detected - true_bpm).abs() <= 2.0,
                "{true_bpm} BPM click track detected as {detected}"
            );
        }
    }

    /// Whole-range sweep: 60–160 BPM is reported at the true tempo; above that the detector may
    /// report the half tempo (octave-equivalent, e.g. 170 BPM drum & bass as 85), never anything
    /// else.
    #[test]
    fn test_tempo_sweep_is_exact_or_octave_equivalent() {
        for true_bpm in (60..=180).step_by(5).map(f64::from) {
            let detected = BpmAnalyzer::compute_bpm_from_samples(&click_track(true_bpm, 20.0), SR)
                .unwrap_or_else(|| panic!("no tempo for {true_bpm} BPM"));
            let exact = (detected - true_bpm).abs() <= 2.0;
            let half = (detected - true_bpm / 2.0).abs() <= 2.0;
            if true_bpm <= 160.0 {
                assert!(exact, "{true_bpm} BPM detected as {detected}");
            } else {
                assert!(exact || half, "{true_bpm} BPM detected as {detected}");
            }
        }
    }

    /// Short slices (the 3 s minimum used for very short tracks) still give a usable tempo.
    #[test]
    fn test_short_click_track_detected_within_two_bpm() {
        let samples = click_track(128.0, 8.0);
        let detected = BpmAnalyzer::compute_bpm_from_samples(&samples, SR).expect("tempo for 8 s clip");
        assert!(
            (detected - 128.0).abs() <= 2.0,
            "128 BPM 8 s clip detected as {detected}"
        );
    }

    #[test]
    fn test_silence_has_no_tempo() {
        let silence = vec![0.0f32; (SR * 20.0) as usize];
        assert_eq!(BpmAnalyzer::compute_bpm_from_samples(&silence, SR), None);
    }

    /// For a perfectly periodic onset train, correlation at the period and at twice the period
    /// must be equal: the score is a mean over the overlap, not a sum that shrinks with the lag.
    #[test]
    fn test_autocorrelation_is_normalized_by_overlap() {
        let mut flux = vec![0.0f32; 200];
        for i in (0..flux.len()).step_by(10) {
            flux[i] = 1.0;
        }
        let r10 = normalized_autocorrelation(&flux, 10);
        let r40 = normalized_autocorrelation(&flux, 40);
        assert!(r10 > 0.0);
        assert!(
            (r10 - r40).abs() / r10 < 0.05,
            "r(10)={r10}, r(40)={r40}: biased toward short lags"
        );
    }
}
