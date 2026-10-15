//! User preferences, configuration definitions, and JSON persistence.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub metadata_view: bool,
    pub default_device: String,
    pub seek_step_seconds: f64,
    pub volume_step: u8,
    pub queue_mode: String,
    pub force_transcode: bool,
    pub player_backend: String,
    #[serde(default)]
    pub web_server_enabled: bool,
    #[serde(default = "default_web_port")]
    pub web_server_port: u16,
    #[serde(default)]
    pub home_dir: Option<String>,
    #[serde(default = "default_visualizer_mode")]
    pub visualizer_mode: String,
}

fn default_web_port() -> u16 {
    8080
}

fn default_visualizer_mode() -> String {
    "bars".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            metadata_view: true,
            default_device: "local-audio".to_string(),
            seek_step_seconds: 10.0,
            volume_step: 5,
            queue_mode: "loop".to_string(),
            force_transcode: false,
            player_backend: "mpv".to_string(),
            web_server_enabled: false,
            web_server_port: 8080,
            home_dir: None,
            visualizer_mode: "bars".to_string(),
        }
    }
}

pub struct SettingDefinition {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

pub const SETTING_DEFINITIONS: &[SettingDefinition] = &[
    SettingDefinition {
        key: "metadata_view",
        label: "File List Display",
        description: "Display extracted ID3/FLAC tag metadata (Artist - Title) instead of filenames.",
    },
    SettingDefinition {
        key: "player_backend",
        label: "Local Playback Engine",
        description: "Audio player binary used for local playback (mpv with IPC or ffplay).",
    },
    SettingDefinition {
        key: "seek_step_seconds",
        label: "Seek Jump Interval",
        description: "Amount of time to jump forward/backward when using Shift+Arrow keys.",
    },
    SettingDefinition {
        key: "volume_step",
        label: "Volume Step Size",
        description: "Percentage to increase or decrease volume per hotkey press (+/-).",
    },
    SettingDefinition {
        key: "queue_mode",
        label: "Queue Playback Mode",
        description: "Playback behavior when reaching the end of the queue.",
    },
    SettingDefinition {
        key: "force_transcode",
        label: "Audio Transcoding",
        description: "Transcode audio into 16-bit PCM WAV via ffmpeg for legacy DLNA renderers.",
    },
    SettingDefinition {
        key: "web_server_enabled",
        label: "Now Playing Web Server",
        description: "Run local HTTP server (metadata, cover art & remote control) on configured port.",
    },
    SettingDefinition {
        key: "home_dir",
        label: "Startup Home Directory",
        description: "Default directory loaded on startup instead of current working directory.",
    },
    SettingDefinition {
        key: "visualizer_mode",
        label: "Visualizer Style",
        description: "Audio visualizer display mode on the Now Playing screen (press 'm' to cycle).",
    },
];

pub struct SettingsManager {
    pub path: PathBuf,
    pub settings: Settings,
}

impl Default for SettingsManager {
    fn default() -> Self {
        let path = paths::get_config_path();
        let settings = Self::load_from(&path).unwrap_or_default();
        Self { path, settings }
    }
}

impl SettingsManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_path(path: PathBuf) -> Self {
        let settings = Self::load_from(&path).unwrap_or_default();
        Self { path, settings }
    }

    pub fn load_from(path: &Path) -> Option<Settings> {
        if !path.exists() {
            return None;
        }
        let content = fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn save(&self) -> bool {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.settings) {
            return fs::write(&self.path, json).is_ok();
        }
        false
    }

    pub fn cycle_setting(&mut self, index: usize, forward: bool) {
        match index {
            0 => {
                // metadata_view
                self.settings.metadata_view = !self.settings.metadata_view;
            }
            1 => {
                // player_backend: "mpv", "ffplay"
                self.settings.player_backend = if self.settings.player_backend == "mpv" {
                    "ffplay".to_string()
                } else {
                    "mpv".to_string()
                };
            }
            2 => {
                // seek_step_seconds: [5.0, 10.0, 15.0, 30.0]
                let options = [5.0, 10.0, 15.0, 30.0];
                let current_idx = options
                    .iter()
                    .position(|&v| (v - self.settings.seek_step_seconds).abs() < 0.1)
                    .unwrap_or(1);
                let next_idx = if forward {
                    (current_idx + 1) % options.len()
                } else {
                    (current_idx + options.len() - 1) % options.len()
                };
                self.settings.seek_step_seconds = options[next_idx];
            }
            3 => {
                // volume_step: [2, 5, 10]
                let options = [2u8, 5, 10];
                let current_idx = options
                    .iter()
                    .position(|&v| v == self.settings.volume_step)
                    .unwrap_or(1);
                let next_idx = if forward {
                    (current_idx + 1) % options.len()
                } else {
                    (current_idx + options.len() - 1) % options.len()
                };
                self.settings.volume_step = options[next_idx];
            }
            4 => {
                // queue_mode: "loop", "stop", "repeat_one"
                let options = ["loop", "stop", "repeat_one"];
                let current_idx = options
                    .iter()
                    .position(|&v| v == self.settings.queue_mode)
                    .unwrap_or(0);
                let next_idx = if forward {
                    (current_idx + 1) % options.len()
                } else {
                    (current_idx + options.len() - 1) % options.len()
                };
                self.settings.queue_mode = options[next_idx].to_string();
            }
            5 => {
                // force_transcode
                self.settings.force_transcode = !self.settings.force_transcode;
            }
            6 => {
                // web_server_enabled
                self.settings.web_server_enabled = !self.settings.web_server_enabled;
            }
            7 => {
                // home_dir cycling:
                // None (Default launch folder), Music folder (if exists), Home folder, Bookmarks, and current custom path
                let mut options: Vec<Option<String>> = vec![None];
                if let Some(user_dirs) = directories::UserDirs::new() {
                    if let Some(audio) = user_dirs.audio_dir() {
                        let p = audio.to_string_lossy().to_string();
                        if !options.iter().any(|o| o.as_deref() == Some(&p)) {
                            options.push(Some(p));
                        }
                    }
                }
                if let Some(base) = directories::BaseDirs::new() {
                    let home = base.home_dir().to_string_lossy().to_string();
                    if !options.iter().any(|o| o.as_deref() == Some(&home)) {
                        options.push(Some(home));
                    }
                }
                let bookmarks_file = paths::get_bookmarks_path();
                if bookmarks_file.exists() {
                    if let Ok(content) = fs::read_to_string(&bookmarks_file) {
                        if let Ok(bm_list) = serde_json::from_str::<Vec<String>>(&content) {
                            for bm in bm_list {
                                if !options.iter().any(|o| o.as_deref() == Some(&bm)) {
                                    options.push(Some(bm));
                                }
                            }
                        }
                    }
                }
                if let Some(ref current) = self.settings.home_dir {
                    if !options.iter().any(|o| o.as_deref() == Some(current)) {
                        options.push(Some(current.clone()));
                    }
                }

                let current_idx = options
                    .iter()
                    .position(|o| o == &self.settings.home_dir)
                    .unwrap_or(0);
                let next_idx = if forward {
                    (current_idx + 1) % options.len()
                } else {
                    (current_idx + options.len() - 1) % options.len()
                };
                self.settings.home_dir = options[next_idx].clone();
            }
            8 => {
                // visualizer_mode: "bars", "wave", "particles", "flame", "matrix_rain"
                let options = ["bars", "wave", "particles", "flame", "matrix_rain"];
                let current_idx = options
                    .iter()
                    .position(|&v| v == self.settings.visualizer_mode)
                    .unwrap_or(0);
                let next_idx = if forward {
                    (current_idx + 1) % options.len()
                } else {
                    (current_idx + options.len() - 1) % options.len()
                };
                self.settings.visualizer_mode = options[next_idx].to_string();
            }
            _ => {}
        }
        self.save();
    }

    pub fn get_display_value(&self, index: usize) -> String {
        match index {
            0 => if self.settings.metadata_view { "Metadata (Tags)" } else { "Filename" }.to_string(),
            1 => if self.settings.player_backend == "mpv" { "MPV (IPC socket)" } else { "FFplay (subprocess)" }.to_string(),
            2 => format!("{} seconds", self.settings.seek_step_seconds),
            3 => format!("{}%", self.settings.volume_step),
            4 => match self.settings.queue_mode.as_str() {
                "loop" => "Continuous (Loop Queue)",
                "stop" => "Stop at End",
                "repeat_one" => "Repeat Current Track",
                other => other,
            }.to_string(),
            5 => if self.settings.force_transcode { "Force 16-bit PCM WAV" } else { "Direct Stream (Auto)" }.to_string(),
            6 => if self.settings.web_server_enabled {
                format!("Enabled (Port {})", self.settings.web_server_port)
            } else {
                "Disabled".to_string()
            },
            7 => {
                if let Some(ref h) = self.settings.home_dir {
                    if let Some(base) = directories::BaseDirs::new() {
                        let home_str = base.home_dir().to_string_lossy();
                        if h.starts_with(home_str.as_ref()) {
                            format!("~{}", &h[home_str.len()..])
                        } else {
                            h.clone()
                        }
                    } else {
                        h.clone()
                    }
                } else {
                    "Default (Launch Directory)".to_string()
                }
            }
            8 => match self.settings.visualizer_mode.as_str() {
                "bars" => "Spectrum Bars",
                "wave" => "Oscilloscope Wave",
                "particles" => "Particle Burst",
                "flame" => "Dancing Flame",
                "matrix_rain" => "Matrix Rain",
                other => other,
            }.to_string(),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_default_settings() {
        let s = Settings::default();
        assert!(s.metadata_view);
        assert_eq!(s.player_backend, "mpv");
        assert_eq!(s.seek_step_seconds, 10.0);
        assert_eq!(s.volume_step, 5);
        assert_eq!(s.queue_mode, "loop");
        assert_eq!(s.home_dir, None);
        assert_eq!(s.visualizer_mode, "bars");
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let temp_dir = env::temp_dir().join(format!("quickstream_test_settings_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let path = temp_dir.join("test_config.json");

        let mut mgr = SettingsManager::with_path(path.clone());
        mgr.settings.metadata_view = false;
        mgr.settings.volume_step = 10;
        mgr.settings.home_dir = Some("/custom/home".to_string());
        mgr.settings.visualizer_mode = "flame".to_string();
        assert!(mgr.save());

        let loaded = SettingsManager::load_from(&path).expect("should load");
        assert!(!loaded.metadata_view);
        assert_eq!(loaded.volume_step, 10);
        assert_eq!(loaded.home_dir, Some("/custom/home".to_string()));
        assert_eq!(loaded.visualizer_mode, "flame");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_cycle_settings() {
        let temp_dir = env::temp_dir().join(format!("quickstream_test_cycle_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let path = temp_dir.join("test_config.json");
        let mut mgr = SettingsManager::with_path(path);

        assert!(mgr.settings.metadata_view);
        mgr.cycle_setting(0, true);
        assert!(!mgr.settings.metadata_view);

        assert_eq!(mgr.settings.player_backend, "mpv");
        mgr.cycle_setting(1, true);
        assert_eq!(mgr.settings.player_backend, "ffplay");

        assert!(!mgr.settings.web_server_enabled);
        mgr.cycle_setting(6, true);
        assert!(mgr.settings.web_server_enabled);

        assert_eq!(mgr.settings.home_dir, None);
        assert_eq!(mgr.get_display_value(7), "Default (Launch Directory)");
        mgr.cycle_setting(7, true);
        mgr.cycle_setting(7, false);
        assert_eq!(mgr.settings.home_dir, None);

        assert_eq!(mgr.settings.visualizer_mode, "bars");
        assert_eq!(mgr.get_display_value(8), "Spectrum Bars");
        mgr.cycle_setting(8, true);
        assert_eq!(mgr.settings.visualizer_mode, "wave");
        assert_eq!(mgr.get_display_value(8), "Oscilloscope Wave");
        mgr.cycle_setting(8, false);
        assert_eq!(mgr.settings.visualizer_mode, "bars");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
