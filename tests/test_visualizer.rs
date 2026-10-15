use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use quickstream::settings::SettingsManager;
use quickstream::ui::visualizer::{
    render_oscilloscope_wave, render_spectrum_bars, BeatState, BeatSync, FlameSim, MatrixRainSim,
    ParticleSim, VisualizerMode,
};
use quickstream::ui::AppUI;

fn create_test_wav_with_beats(path: &Path) {
    let mut file = File::create(path).unwrap();
    let sample_rate: u32 = 44100;
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * (channels as u32) * ((bits_per_sample as u32) / 8);
    let block_align = channels * (bits_per_sample / 8);
    let num_samples: u32 = 44100 * 2; // 2 seconds
    let data_size = num_samples * (block_align as u32);

    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_size).to_le_bytes()).unwrap();
    file.write_all(b"WAVE").unwrap();
    file.write_all(b"fmt ").unwrap();
    file.write_all(&16u32.to_le_bytes()).unwrap();
    file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    file.write_all(&channels.to_le_bytes()).unwrap();
    file.write_all(&sample_rate.to_le_bytes()).unwrap();
    file.write_all(&byte_rate.to_le_bytes()).unwrap();
    file.write_all(&block_align.to_le_bytes()).unwrap();
    file.write_all(&bits_per_sample.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_size.to_le_bytes()).unwrap();

    let mut samples = Vec::with_capacity(num_samples as usize);
    for i in 0..num_samples {
        let t = i as f64 / sample_rate as f64;
        let val = if (0.5..=0.6).contains(&t) || (1.5..=1.6).contains(&t) {
            (2.0 * std::f64::consts::PI * 120.0 * t).sin() * 0.9
        } else {
            (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 0.05
        };
        let s16 = (val * 32767.0) as i16;
        samples.extend_from_slice(&s16.to_le_bytes());
    }
    file.write_all(&samples).unwrap();
}

#[test]
fn test_visualizer_mode_definitions() {
    let modes = VisualizerMode::all();
    assert_eq!(modes.len(), 5);

    assert_eq!(VisualizerMode::Bars.display_name(), "Spectrum Bars");
    assert_eq!(VisualizerMode::Wave.display_name(), "Oscilloscope Wave");
    assert_eq!(VisualizerMode::Particles.display_name(), "Particle Burst");
    assert_eq!(VisualizerMode::Flame.display_name(), "Dancing Flame");
    assert_eq!(VisualizerMode::MatrixRain.display_name(), "Matrix Rain");

    // Circular next()
    assert_eq!(VisualizerMode::Bars.next(), VisualizerMode::Wave);
    assert_eq!(VisualizerMode::Wave.next(), VisualizerMode::Particles);
    assert_eq!(VisualizerMode::Particles.next(), VisualizerMode::Flame);
    assert_eq!(VisualizerMode::Flame.next(), VisualizerMode::MatrixRain);
    assert_eq!(VisualizerMode::MatrixRain.next(), VisualizerMode::Bars);

    // Serialization & string round-trips
    for m in modes {
        let s = m.as_str();
        assert_eq!(VisualizerMode::from_str(s), *m);
    }
}

#[test]
fn test_beat_sync_harmonic_fallback() {
    let mut beat_sync = BeatSync::new();

    // When stopped, beat intensity decays
    let stopped_state = beat_sync.update(0.0, false);
    assert!(!stopped_state.is_beat);
    assert_eq!(stopped_state.energy, 0.0);

    // When playing without track, harmonic tempo generator activates
    let mut beat_detected = false;
    for step in 0..100 {
        let pos = step as f64 * 0.03; // ~3 seconds at 33 FPS
        let state = beat_sync.update(pos, true);
        assert!(state.energy >= 0.0 && state.energy <= 1.0);
        if state.is_beat {
            beat_detected = true;
            assert!(state.beat_intensity > 0.8);
        }
    }
    assert!(beat_detected, "Harmonic fallback should generate beat onsets");
}

#[test]
fn test_beat_sync_synthetic_envelope() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_viz_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let wav_file = temp_dir.join("beat_test.wav");
    create_test_wav_with_beats(&wav_file);

    let mut beat_sync = BeatSync::new();
    beat_sync.load_track(&wav_file);

    // Give background extraction a moment to complete
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Update through 2 seconds of playback
    let mut beat_near_0_5 = false;
    let mut beat_near_1_5 = false;

    for step in 0..200 {
        let pos = step as f64 * 0.01; // 10ms steps
        let state = beat_sync.update(pos, true);
        if state.is_beat {
            if (0.45..=0.65).contains(&pos) {
                beat_near_0_5 = true;
            }
            if (1.45..=1.65).contains(&pos) {
                beat_near_1_5 = true;
            }
        }
    }

    assert!(beat_near_0_5, "Should detect transient near 0.5s beat burst");
    assert!(beat_near_1_5, "Should detect transient near 1.5s beat burst");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_particle_sim_physics() {
    let mut sim = ParticleSim::new();
    let area = Rect::new(0, 0, 80, 24);
    let mut buf = Buffer::empty(area);

    let beat = BeatState {
        energy: 0.8,
        is_beat: true,
        beat_intensity: 1.0,
        spectrum: [0.5; 32],
    };

    sim.update_and_render(area, &mut buf, &beat);
    assert!(!sim.particles.is_empty(), "Particles should spawn on beat");

    let initial_count = sim.particles.len();
    let initial_y = sim.particles[0].y;

    let calm_beat = BeatState {
        energy: 0.2,
        is_beat: false,
        beat_intensity: 0.0,
        spectrum: [0.1; 32],
    };

    sim.update_and_render(area, &mut buf, &calm_beat);
    // Position should update due to velocity and gravity
    assert!(sim.particles.len() <= initial_count + 2);
    assert_ne!(sim.particles[0].y, initial_y);
}

#[test]
fn test_flame_sim_cellular_automaton() {
    let mut sim = FlameSim::new();
    let area = Rect::new(0, 0, 60, 20);
    let mut buf = Buffer::empty(area);

    let beat = BeatState {
        energy: 0.9,
        is_beat: true,
        beat_intensity: 1.0,
        spectrum: [0.8; 32],
    };

    // Run several simulation iterations
    for _ in 0..10 {
        sim.update_and_render(area, &mut buf, &beat);
    }

    // Characters should be drawn into the bottom rows of the buffer
    let mut has_flame_chars = false;
    for y in 10..20 {
        for x in 0..60 {
            let ch = buf[(x, y)].symbol();
            if ch != " " {
                has_flame_chars = true;
                break;
            }
        }
    }
    assert!(has_flame_chars, "Flame cellular automaton should populate buffer");
}

#[test]
fn test_matrix_rain_sim() {
    let mut sim = MatrixRainSim::new();
    let area = Rect::new(0, 0, 60, 20);
    let mut buf = Buffer::empty(area);

    let beat = BeatState {
        energy: 0.7,
        is_beat: false,
        beat_intensity: 0.2,
        spectrum: [0.4; 32],
    };

    for _ in 0..15 {
        sim.update_and_render(area, &mut buf, &beat);
    }

    let mut has_rain = false;
    for y in 0..20 {
        for x in 0..60 {
            if buf[(x, y)].symbol() != " " {
                has_rain = true;
                break;
            }
        }
    }
    assert!(has_rain, "Matrix rain should render glyphs to buffer");
}

#[test]
fn test_now_playing_tab_key_navigation() {
    let temp_dir = std::env::temp_dir().join(format!("quickstream_viz_keys_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let config_path = temp_dir.join("config.json");
    let settings_mgr = SettingsManager::with_path(config_path);
    let mut app = AppUI::new(Some(temp_dir.clone()), settings_mgr);

    // Initial tab is 1 (Queue & Library)
    assert_eq!(app.active_tab, 1);

    // Press '1': switch to Now Playing (tab 0)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 0);

    // Press 'm': cycle visualizer mode
    assert_eq!(app.visualizer_mode, VisualizerMode::Bars);
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
    assert_eq!(app.visualizer_mode, VisualizerMode::Wave);
    assert_eq!(app.settings_mgr.settings.visualizer_mode, "wave");

    // Press 'n': toggle to Queue & Library (tab 1)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 1);

    // Press 'n' again: toggle back to Now Playing (tab 0)
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 0);

    // Press 'Esc': return to Queue & Library (tab 1)
    app.handle_tab_key(KeyCode::Esc);
    assert_eq!(app.active_tab, 1);

    // Numeric keys 2, 3, 4
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 2); // Devices
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 3); // Settings
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    assert_eq!(app.active_tab, 1); // Queue

    // Tab cycling
    let _ = app.handle_input_key(crossterm::event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.active_tab, 2);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_render_all_visualizers_to_buffer() {
    let area = Rect::new(0, 0, 80, 24);
    let mut buf = Buffer::empty(area);

    let beat = BeatState {
        energy: 0.75,
        is_beat: true,
        beat_intensity: 0.9,
        spectrum: [0.6; 32],
    };
    let peaks = [0.8; 32];

    // 1. Spectrum Bars
    render_spectrum_bars(area, &mut buf, &beat, &peaks);

    // 2. Oscilloscope Wave
    render_oscilloscope_wave(area, &mut buf, &beat, 1.25);

    // 3. Particles
    let mut p_sim = ParticleSim::new();
    p_sim.update_and_render(area, &mut buf, &beat);

    // 4. Flame
    let mut f_sim = FlameSim::new();
    f_sim.update_and_render(area, &mut buf, &beat);

    // 5. Matrix Rain
    let mut m_sim = MatrixRainSim::new();
    m_sim.update_and_render(area, &mut buf, &beat);
}
