//! Audio visualizer algorithms and beat synchronization engine.
//!
//! Inspired by `cli-viz` (https://github.com/sam1am/cli-viz) with 5 reactive visualizer styles:
//! - Spectrum Bars (frequency block equalizer with floating peak markers)
//! - Oscilloscope Wave (Braille / sinusoidal harmonic ripples)
//! - Particle Burst (fountain physics simulator bursting on kick transients)
//! - Dancing Flame (cellular automaton heat diffusion flaring on beats)
//! - Matrix Rain (cyberpunk digital streams surging with the rhythm)
//!
//! Synchronized to music playback via background 100Hz audio envelope extraction
//! using ffmpeg, with an adaptive harmonic rhythm fallback.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

/// Visualizer display modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VisualizerMode {
    #[default]
    Bars,
    Wave,
    Particles,
    Flame,
    MatrixRain,
}

impl std::str::FromStr for VisualizerMode {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "bars" => Self::Bars,
            "wave" => Self::Wave,
            "particles" => Self::Particles,
            "flame" => Self::Flame,
            "matrix_rain" => Self::MatrixRain,
            _ => Self::Bars,
        })
    }
}

impl VisualizerMode {
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Bars => "Spectrum Bars",
            Self::Wave => "Oscilloscope Wave",
            Self::Particles => "Particle Burst",
            Self::Flame => "Dancing Flame",
            Self::MatrixRain => "Matrix Rain",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            Self::Bars => Self::Wave,
            Self::Wave => Self::Particles,
            Self::Particles => Self::Flame,
            Self::Flame => Self::MatrixRain,
            Self::MatrixRain => Self::Bars,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bars => "bars",
            Self::Wave => "wave",
            Self::Particles => "particles",
            Self::Flame => "flame",
            Self::MatrixRain => "matrix_rain",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "bars" => Self::Bars,
            "wave" => Self::Wave,
            "particles" => Self::Particles,
            "flame" => Self::Flame,
            "matrix_rain" => Self::MatrixRain,
            _ => Self::Bars,
        }
    }

    pub fn all() -> &'static [VisualizerMode] {
        &[
            Self::Bars,
            Self::Wave,
            Self::Particles,
            Self::Flame,
            Self::MatrixRain,
        ]
    }
}

/// Instantaneous beat state used to drive all visualizers.
#[derive(Debug, Clone)]
pub struct BeatState {
    pub energy: f32,
    pub is_beat: bool,
    pub beat_intensity: f32,
    pub spectrum: [f32; 32],
}

impl Default for BeatState {
    fn default() -> Self {
        Self {
            energy: 0.0,
            is_beat: false,
            beat_intensity: 0.0,
            spectrum: [0.0; 32],
        }
    }
}

/// Background beat synchronization engine.
pub struct BeatSync {
    envelope_cache: Arc<RwLock<HashMap<PathBuf, Arc<Vec<f32>>>>>,
    loading_tracks: Arc<Mutex<HashMap<PathBuf, bool>>>,
    current_track: Option<PathBuf>,
    last_beat_time: f64,
    beat_intensity: f32,
    smoothed_energy: f32,
    rolling_avg: f32,
    pub peak_bars: [f32; 32],
    pub peak_hold: [u8; 32],
    pub smoothed_bars: [f32; 32],
}

impl Default for BeatSync {
    fn default() -> Self {
        Self::new()
    }
}

impl BeatSync {
    pub fn new() -> Self {
        Self {
            envelope_cache: Arc::new(RwLock::new(HashMap::new())),
            loading_tracks: Arc::new(Mutex::new(HashMap::new())),
            current_track: None,
            last_beat_time: -1.0,
            beat_intensity: 0.0,
            smoothed_energy: 0.0,
            rolling_avg: 0.2,
            peak_bars: [0.0; 32],
            peak_hold: [0; 32],
            smoothed_bars: [0.0; 32],
        }
    }

    /// Load or trigger background audio envelope extraction for the given track.
    pub fn load_track(&mut self, path: &Path) {
        let path_buf = path.to_path_buf();
        self.current_track = Some(path_buf.clone());

        // Check if already in cache
        if let Ok(cache) = self.envelope_cache.read() {
            if cache.contains_key(&path_buf) {
                return;
            }
        }

        // Check if already loading
        if let Ok(mut loading) = self.loading_tracks.lock() {
            if loading.get(&path_buf).copied().unwrap_or(false) {
                return;
            }
            loading.insert(path_buf.clone(), true);
        }

        let cache_clone = self.envelope_cache.clone();
        let loading_clone = self.loading_tracks.clone();

        thread::spawn(move || {
            let extracted = Self::extract_envelope_ffmpeg(&path_buf);
            if let Some(samples) = extracted {
                if let Ok(mut cache) = cache_clone.write() {
                    cache.insert(path_buf.clone(), Arc::new(samples));
                }
            }
            if let Ok(mut loading) = loading_clone.lock() {
                loading.remove(&path_buf);
            }
        });
    }

    /// Run ffmpeg to extract 100Hz audio amplitude envelope.
    fn extract_envelope_ffmpeg(path: &Path) -> Option<Vec<f32>> {
        // Attempt 1: rectified envelope resampled to 100Hz
        let output = Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(path)
            .arg("-vn")
            .arg("-af")
            .arg("aeval=abs(val(0)),aresample=100")
            .arg("-f")
            .arg("f32le")
            .arg("-")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok();

        let raw_bytes = if let Some(out) = output.filter(|o| o.status.success() && !o.stdout.is_empty()) {
            out.stdout
        } else {
            // Attempt 2: fallback simple mono 100Hz
            let fallback = Command::new("ffmpeg")
                .arg("-y")
                .arg("-i")
                .arg(path)
                .arg("-vn")
                .arg("-ac")
                .arg("1")
                .arg("-ar")
                .arg("100")
                .arg("-f")
                .arg("f32le")
                .arg("-")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            if !fallback.status.success() || fallback.stdout.is_empty() {
                return None;
            }
            fallback.stdout
        };

        let num_floats = raw_bytes.len() / 4;
        let mut samples = Vec::with_capacity(num_floats);
        for i in 0..num_floats {
            let chunk = [
                raw_bytes[i * 4],
                raw_bytes[i * 4 + 1],
                raw_bytes[i * 4 + 2],
                raw_bytes[i * 4 + 3],
            ];
            let f = f32::from_le_bytes(chunk).abs().clamp(0.0, 1.0);
            samples.push(f);
        }

        Some(samples)
    }

    /// Update beat state based on current playback timestamp and playing status.
    pub fn update(&mut self, position_secs: f64, is_playing: bool) -> BeatState {
        if !is_playing {
            self.beat_intensity *= 0.88;
            self.smoothed_energy *= 0.88;
            for i in 0..32 {
                self.smoothed_bars[i] *= 0.88;
                if self.peak_hold[i] > 0 {
                    self.peak_hold[i] -= 1;
                } else {
                    self.peak_bars[i] = (self.peak_bars[i] - 0.025).max(0.0);
                }
            }
            return BeatState {
                energy: self.smoothed_energy,
                is_beat: false,
                beat_intensity: self.beat_intensity,
                spectrum: self.smoothed_bars,
            };
        }

        let mut energy = 0.0f32;
        let mut is_beat = false;
        let mut envelope_hit = false;

        if let Some(ref track) = self.current_track {
            if let Ok(cache) = self.envelope_cache.read() {
                if let Some(env) = cache.get(track) {
                    envelope_hit = true;
                    let idx = (position_secs * 100.0).max(0.0) as usize;
                    if !env.is_empty() {
                        let start = idx.saturating_sub(2);
                        let end = (idx + 3).min(env.len());
                        let slice = &env[start..end];
                        let sum: f32 = slice.iter().sum();
                        energy = if !slice.is_empty() { sum / (slice.len() as f32) } else { 0.0 };

                        self.rolling_avg = 0.94 * self.rolling_avg + 0.06 * energy;
                        let threshold = (self.rolling_avg * 1.35).max(0.12);
                        let delta = position_secs - self.last_beat_time;
                        if energy > threshold && !(0.0..=0.22).contains(&delta) {
                            is_beat = true;
                            self.beat_intensity = 1.0;
                            self.last_beat_time = position_secs;
                        }
                    }
                }
            }
        }

        if !envelope_hit {
            // Adaptive harmonic rhythm fallback (125 BPM = 2.083 Hz)
            let cycle = position_secs * 2.083;
            let phase = cycle.fract();
            let delta = position_secs - self.last_beat_time;
            if phase < 0.09 && !(0.0..=0.25).contains(&delta) {
                is_beat = true;
                self.beat_intensity = 1.0;
                self.last_beat_time = position_secs;
            }
            let harmonic = (0.35 + 0.45 * (phase * std::f64::consts::PI * 2.0).cos() as f32).clamp(0.0, 1.0);
            energy = harmonic;
        }

        if !is_beat {
            self.beat_intensity *= 0.91;
        }

        self.smoothed_energy = 0.65 * self.smoothed_energy + 0.35 * energy;

        // Calculate 32 spectrum bins:
        // Lower bins (0..6): bass / kick reactive
        // Mid bins (7..18): rhythm / vocal harmonic body
        // High bins (19..31): treble / sparkle
        let t = position_secs as f32;
        for i in 0..32 {
            let i_f = i as f32;
            let freq_bias = (1.0 - (i_f / 32.0) * 0.4).max(0.2);

            let target = if i < 7 {
                // Bass frequencies: react heavily to beat spikes
                (self.smoothed_energy * 0.7 + self.beat_intensity * 0.8 * freq_bias
                    + (t * 4.0 + i_f * 0.5).sin() * 0.1)
                    .clamp(0.05, 1.0)
            } else if i < 19 {
                // Mid frequencies: harmonic waves
                let wave = ((t * 3.2 + i_f * 0.4).sin() * 0.2 + (t * 5.1 - i_f * 0.3).cos() * 0.15).abs();
                (self.smoothed_energy * 0.6 + self.beat_intensity * 0.35 + wave * freq_bias)
                    .clamp(0.04, 0.95)
            } else {
                // Treble frequencies: quick flutter
                let flutter = ((t * 8.0 + i_f * 0.8).sin() * 0.2).abs();
                (self.smoothed_energy * 0.4 + self.beat_intensity * 0.25 + flutter * freq_bias)
                    .clamp(0.02, 0.85)
            };

            // Attack vs Decay
            if target > self.smoothed_bars[i] {
                self.smoothed_bars[i] = 0.6 * self.smoothed_bars[i] + 0.4 * target;
            } else {
                self.smoothed_bars[i] = 0.85 * self.smoothed_bars[i] + 0.15 * target;
            }

            // Floating peak marker
            if self.smoothed_bars[i] >= self.peak_bars[i] {
                self.peak_bars[i] = self.smoothed_bars[i];
                self.peak_hold[i] = 4;
            } else if self.peak_hold[i] > 0 {
                self.peak_hold[i] -= 1;
            } else {
                self.peak_bars[i] = (self.peak_bars[i] - 0.028).max(0.0);
            }
        }

        BeatState {
            energy: self.smoothed_energy,
            is_beat,
            beat_intensity: self.beat_intensity,
            spectrum: self.smoothed_bars,
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Spectrum Bars Visualizer
// ---------------------------------------------------------------------------

pub fn render_spectrum_bars(
    area: Rect,
    buf: &mut Buffer,
    beat: &BeatState,
    peaks: &[f32; 32],
) {
    if area.width < 4 || area.height < 2 {
        return;
    }

    let bar_blocks = [' ', ' ', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let w = area.width as usize;
    let h = area.height as usize;

    // Determine bar spacing: 1 or 2 characters per bar, plus 1 gap
    let (bar_width, bar_count) = if w >= 64 {
        (2, (w / 3).min(32))
    } else {
        (1, (w / 2).clamp(4, 32))
    };

    let total_width = bar_count * (bar_width + 1) - 1;
    let start_x = area.x as usize + (w.saturating_sub(total_width)) / 2;

    for i in 0..bar_count {
        let spectrum_idx = (i * 32) / bar_count;
        let val = beat.spectrum[spectrum_idx.min(31)].clamp(0.0, 1.0);
        let peak_val = peaks[spectrum_idx.min(31)].clamp(0.0, 1.0);

        let bar_h_float = val * (h as f32);
        let bar_full_cells = bar_h_float.floor() as usize;
        let frac = (bar_h_float.fract() * 8.0).round() as usize;

        let peak_y = (peak_val * ((h.saturating_sub(1)) as f32)).round() as usize;

        let x_pos = start_x + i * (bar_width + 1);

        for row in 0..h {
            let y_from_bottom = row;
            let screen_y = (area.y as usize + h - 1).saturating_sub(row);

            // Compute gradient color based on vertical height
            let color = if y_from_bottom < h / 3 {
                Color::Cyan
            } else if y_from_bottom < (h * 2) / 3 {
                Color::Magenta
            } else if y_from_bottom < (h * 5) / 6 {
                Color::Yellow
            } else {
                Color::LightRed
            };

            for col in 0..bar_width {
                let screen_x = x_pos + col;
                if screen_x >= (area.x + area.width) as usize || screen_y >= (area.y + area.height) as usize {
                    continue;
                }

                if y_from_bottom < bar_full_cells {
                    if let Some(cell) = buf.cell_mut((screen_x as u16, screen_y as u16)) {
                        cell.set_char('█').set_fg(color);
                    }
                } else if y_from_bottom == bar_full_cells && frac > 0 {
                    if let Some(cell) = buf.cell_mut((screen_x as u16, screen_y as u16)) {
                        cell.set_char(bar_blocks[frac.min(8)]).set_fg(color);
                    }
                }

                // Render peak hold marker
                if y_from_bottom == peak_y && peak_y > bar_full_cells {
                    if let Some(cell) = buf.cell_mut((screen_x as u16, screen_y as u16)) {
                        cell.set_char('▪')
                            .set_fg(Color::White)
                            .set_style(Style::default().add_modifier(Modifier::BOLD));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Oscilloscope Wave Visualizer
// ---------------------------------------------------------------------------

pub fn render_oscilloscope_wave(
    area: Rect,
    buf: &mut Buffer,
    beat: &BeatState,
    position_secs: f64,
) {
    if area.width < 4 || area.height < 2 {
        return;
    }

    let w = area.width as usize;
    let h = area.height as usize;
    let center_y = (area.y as f32) + (h as f32) / 2.0;

    let braille_chars = ['⠂', '⠄', '⡀', '⣀', '⣤', '⠤', '⠶', '⠿', '⣿'];
    let t = position_secs as f32;
    let amp = (h as f32 * 0.42) * (0.35 + 0.65 * beat.energy + 0.5 * beat.beat_intensity);

    for col in 0..w {
        let screen_x = area.x + col as u16;
        let x_ratio = (col as f32) / (w as f32);

        // Multi-harmonic sine waves
        let wave1 = ((col as f32) * 0.08 + t * 4.2).sin();
        let wave2 = ((col as f32) * 0.16 - t * 3.1).cos() * 0.45;
        let wave3 = (x_ratio * std::f32::consts::PI * 4.0 + t * 1.8).sin() * 0.25;
        let combined = wave1 + wave2 + wave3;

        let y_float = center_y + combined * amp;
        let cell_y = y_float.round().clamp(area.y as f32, (area.y + area.height - 1) as f32) as u16;

        let harmonic_dist = (y_float - center_y).abs() / amp.max(1.0);
        let char_idx = ((harmonic_dist * 8.0).round() as usize).min(braille_chars.len() - 1);

        let color = if beat.beat_intensity > 0.5 {
            Color::LightMagenta
        } else if harmonic_dist > 0.6 {
            Color::Yellow
        } else {
            Color::Cyan
        };

        if screen_x < area.x + area.width && cell_y < area.y + area.height {
            if let Some(cell) = buf.cell_mut((screen_x, cell_y)) {
                cell.set_char(braille_chars[char_idx])
                    .set_fg(color)
                    .set_style(Style::default().add_modifier(Modifier::BOLD));
            }
        }

        // Secondary subtle harmonic ripple
        let wave_sub = ((col as f32) * 0.05 - t * 2.5).sin();
        let y_sub = (center_y - wave_sub * (amp * 0.6))
            .round()
            .clamp(area.y as f32, (area.y + area.height - 1) as f32) as u16;

        if screen_x < area.x + area.width && y_sub < area.y + area.height && y_sub != cell_y {
            if let Some(cell) = buf.cell_mut((screen_x, y_sub)) {
                cell.set_char('·').set_fg(Color::DarkGray);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Particle Burst Visualizer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Particle {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    pub life: f32,
    pub max_life: f32,
    pub glyph: char,
    pub color_idx: u8,
}

pub struct ParticleSim {
    pub particles: Vec<Particle>,
    rng: u64,
}

impl Default for ParticleSim {
    fn default() -> Self {
        Self::new()
    }
}

impl ParticleSim {
    pub fn new() -> Self {
        Self {
            particles: Vec::with_capacity(128),
            rng: 0x8543_2190_1234_5678,
        }
    }

    fn next_u32(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng & 0xFFFF_FFFF) as u32
    }

    fn next_f32(&mut self) -> f32 {
        (self.next_u32() % 1000) as f32 / 1000.0
    }

    pub fn update_and_render(&mut self, area: Rect, buf: &mut Buffer, beat: &BeatState) {
        if area.width < 4 || area.height < 2 {
            return;
        }

        let glyphs = ['✦', '★', '✱', '•', '·', '°', '▲', '◆'];
        let colors = [
            Color::Yellow,
            Color::LightMagenta,
            Color::Cyan,
            Color::LightRed,
            Color::White,
        ];

        let center_x = area.x as f32 + (area.width as f32) / 2.0;
        let origin_y = area.y as f32 + area.height as f32 - 1.0;

        // Spawn burst on beat
        if beat.is_beat {
            let count = 20 + (beat.beat_intensity * 20.0) as usize;
            for _ in 0..count {
                let angle = (self.next_f32() - 0.5) * 2.2 - (std::f32::consts::PI / 2.0);
                let speed = 0.5 + self.next_f32() * 1.5 * (0.8 + 0.6 * beat.beat_intensity);
                let vx = angle.cos() * speed * 2.0;
                let vy = angle.sin() * speed;
                let glyph_idx = (self.next_u32() as usize) % glyphs.len();
                let color_idx = (self.next_u32() as u8) % (colors.len() as u8);
                let px = center_x + (self.next_f32() - 0.5) * 6.0;
                let py = origin_y - self.next_f32() * 2.0;
                let max_life = 15.0 + self.next_f32() * 25.0;

                self.particles.push(Particle {
                    x: px,
                    y: py,
                    vx,
                    vy,
                    life: 0.0,
                    max_life,
                    glyph: glyphs[glyph_idx],
                    color_idx,
                });
            }
        }

        // Ambient trickle
        if self.next_f32() < 0.45 {
            let vx = (self.next_f32() - 0.5) * 1.2;
            let vy = -0.4 - self.next_f32() * 0.6;
            let px = center_x + (self.next_f32() - 0.5) * 10.0;
            let max_life = 20.0 + self.next_f32() * 15.0;
            self.particles.push(Particle {
                x: px,
                y: origin_y,
                vx,
                vy,
                life: 0.0,
                max_life,
                glyph: '·',
                color_idx: 2,
            });
        }

        // Advance particles and render
        let mut i = 0;
        while i < self.particles.len() {
            let p = &mut self.particles[i];
            p.x += p.vx;
            p.y += p.vy;
            p.vy += 0.045; // gravity
            p.vx *= 0.985; // drag
            p.life += 1.0;

            let px = p.x.round() as i32;
            let py = p.y.round() as i32;

            let in_bounds = px >= area.x as i32
                && px < (area.x + area.width) as i32
                && py >= area.y as i32
                && py < (area.y + area.height) as i32;

            if in_bounds && p.life < p.max_life {
                let color = colors[p.color_idx as usize % colors.len()];
                if let Some(cell) = buf.cell_mut((px as u16, py as u16)) {
                    cell.set_char(p.glyph).set_fg(color);
                }
                i += 1;
            } else {
                self.particles.swap_remove(i);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Dancing Flame Visualizer
// ---------------------------------------------------------------------------

pub struct FlameSim {
    grid: Vec<Vec<f32>>,
    rng: u64,
}

impl Default for FlameSim {
    fn default() -> Self {
        Self::new()
    }
}

impl FlameSim {
    pub fn new() -> Self {
        Self {
            grid: Vec::new(),
            rng: 0x9321_4321_5678_1234,
        }
    }

    fn next_f32(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        ((self.rng & 0xFFFF_FFFF) % 1000) as f32 / 1000.0
    }

    pub fn update_and_render(&mut self, area: Rect, buf: &mut Buffer, beat: &BeatState) {
        if area.width < 4 || area.height < 2 {
            return;
        }

        let w = area.width as usize;
        let h = area.height as usize;

        if self.grid.len() != h || self.grid.first().map(|r| r.len()).unwrap_or(0) != w {
            self.grid = vec![vec![0.0; w]; h];
        }

        // Bottom row heat source
        let base_heat = 0.35 + 0.45 * beat.energy + 0.55 * beat.beat_intensity;
        for col in 0..w {
            let flicker = (self.next_f32() - 0.4) * 0.4;
            self.grid[h - 1][col] = (base_heat + flicker).clamp(0.0, 1.0);
        }

        let decay = if beat.is_beat { 0.03 } else { 0.055 };

        // Cellular automaton heat diffusion upwards
        for row in 0..h - 1 {
            for col in 0..w {
                let left = if col > 0 { self.grid[row + 1][col - 1] } else { self.grid[row + 1][w - 1] };
                let center = self.grid[row + 1][col];
                let right = if col + 1 < w { self.grid[row + 1][col + 1] } else { self.grid[row + 1][0] };
                let below = if row + 2 < h { self.grid[row + 2][col] } else { center };

                let diffused = (left + center + right + below) / 4.02 - decay;
                self.grid[row][col] = diffused.max(0.0);
            }
        }

        let flame_chars = [' ', '.', ':', '*', 's', 'S', '#', '@'];

        for row in 0..h {
            for col in 0..w {
                let val = self.grid[row][col];
                if val < 0.05 {
                    continue;
                }

                let char_idx = ((val * 7.99).floor() as usize).min(flame_chars.len() - 1);
                let ch = flame_chars[char_idx];
                if ch == ' ' {
                    continue;
                }

                let color = if val > 0.82 {
                    Color::White
                } else if val > 0.60 {
                    Color::Yellow
                } else if val > 0.38 {
                    Color::LightRed
                } else if val > 0.20 {
                    Color::Red
                } else {
                    Color::DarkGray
                };

                let screen_x = area.x + col as u16;
                let screen_y = area.y + row as u16;
                if screen_x < area.x + area.width && screen_y < area.y + area.height {
                    if let Some(cell) = buf.cell_mut((screen_x, screen_y)) {
                        cell.set_char(ch).set_fg(color);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Matrix Rain Visualizer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct MatrixDrop {
    pub x: u16,
    pub y: f32,
    pub speed: f32,
    pub length: usize,
    pub glyphs: Vec<char>,
}

pub struct MatrixRainSim {
    drops: Vec<MatrixDrop>,
    rng: u64,
}

impl Default for MatrixRainSim {
    fn default() -> Self {
        Self::new()
    }
}

impl MatrixRainSim {
    pub fn new() -> Self {
        Self {
            drops: Vec::with_capacity(64),
            rng: 0x4829_1048_2910_4829,
        }
    }

    fn next_u32(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng & 0xFFFF_FFFF) as u32
    }

    fn next_f32(&mut self) -> f32 {
        (self.next_u32() % 1000) as f32 / 1000.0
    }

    pub fn update_and_render(&mut self, area: Rect, buf: &mut Buffer, beat: &BeatState) {
        if area.width < 4 || area.height < 2 {
            return;
        }

        let matrix_chars = [
            '0', '1', '2', '3', '7', '8', 'Z', 'X', 'K', 'Q',
            'λ', '§', 'Ψ', 'Ω', 'Ξ', 'Π', '✦', '·', '*', '=',
        ];

        let target_drops = ((area.width as usize) / 3).max(4);

        while self.drops.len() < target_drops {
            let col = (self.next_u32() as u16) % area.width;
            let len = 4 + (self.next_u32() as usize % 12);
            let mut glyphs = Vec::with_capacity(len);
            for _ in 0..len {
                glyphs.push(matrix_chars[(self.next_u32() as usize) % matrix_chars.len()]);
            }
            let y = -(self.next_f32() * (area.height as f32));
            let speed = 0.35 + self.next_f32() * 0.65;
            self.drops.push(MatrixDrop {
                x: area.x + col,
                y,
                speed,
                length: len,
                glyphs,
            });
        }

        // On beat hit: surge speed
        let speed_boost = if beat.is_beat { 0.45 * beat.beat_intensity } else { 0.0 };

        for idx in 0..self.drops.len() {
            self.drops[idx].y += self.drops[idx].speed + speed_boost;

            let head_y = self.drops[idx].y.round() as i32;
            let length = self.drops[idx].length;
            let drop_x = self.drops[idx].x;
            let drop_y_usize = self.drops[idx].y as usize;
            let glyphs_len = self.drops[idx].glyphs.len();

            for i in 0..length {
                let y = head_y - (i as i32);
                if y >= area.y as i32 && y < (area.y + area.height) as i32 {
                    let char_idx = (i + drop_y_usize) % glyphs_len;
                    let ch = self.drops[idx].glyphs[char_idx];

                    let color = if i == 0 {
                        Color::White
                    } else if i == 1 {
                        Color::LightCyan
                    } else if i < length / 2 {
                        Color::LightGreen
                    } else if i < (length * 3) / 4 {
                        Color::Green
                    } else {
                        Color::DarkGray
                    };

                    if let Some(cell) = buf.cell_mut((drop_x, y as u16)) {
                        cell.set_char(ch).set_fg(color);
                    }
                }
            }

            // Respawn drop once it has fallen off screen
            if (head_y - length as i32) >= (area.y + area.height) as i32 {
                let new_x = area.x + ((self.next_u32() as u16) % area.width);
                let new_y = -(self.next_f32() * 5.0);
                let new_speed = 0.35 + self.next_f32() * 0.65;
                self.drops[idx].x = new_x;
                self.drops[idx].y = new_y;
                self.drops[idx].speed = new_speed;
            }
        }
    }
}
