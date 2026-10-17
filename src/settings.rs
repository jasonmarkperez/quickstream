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
    #[serde(default = "default_gapless_autopilot")]
    pub gapless_autopilot: bool,
    #[serde(default = "default_mix_profile")]
    pub default_mix_profile: String,
    #[serde(default)]
    pub online_enrichment: bool,
    /// Let other devices on the network reach the web remote. DLNA renderers get LAN access
    /// while active regardless of this setting.
    #[serde(default)]
    pub web_server_lan: bool,
}

fn default_web_port() -> u16 {
    8080
}

fn default_gapless_autopilot() -> bool {
    true
}

fn default_mix_profile() -> String {
    "diverse".to_string()
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
            gapless_autopilot: true,
            default_mix_profile: "diverse".to_string(),
            online_enrichment: false,
            web_server_lan: false,
        }
    }
}

impl Settings {
    pub fn mix_profile(&self) -> crate::analysis::MixProfile {
        match self.default_mix_profile.as_str() {
            "bpm_flow" => crate::analysis::MixProfile::BpmFlow { target_bpm: 120.0, tolerance: 0.15 },
            "chill" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Chill),
            "workout" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Workout),
            "groove" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Groove),
            _ => crate::analysis::MixProfile::DiverseShuffle,
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
        key: "gapless_autopilot",
        label: "Gapless Continuous Stream",
        description: "Seamlessly splice tracks into a single DLNA PCM stream (Roon-style gapless playback).",
    },
    SettingDefinition {
        key: "default_mix_profile",
        label: "Autopilot Mix Algorithm",
        description: "Intelligent mix style: Diverse Shuffle, DJ BPM Flow, Chillout, Workout, or Groove.",
    },
    SettingDefinition {
        key: "online_enrichment",
        label: "Online Tag Enrichment",
        description: "Fetch community genre tags and similarity graphs from Last.fm (cached locally, library read-only).",
    },
    SettingDefinition {
        key: "web_server_lan",
        label: "Web Remote Access",
        description: "Allow phones and other devices on your network to open the web remote. DLNA speakers always get network access while active.",
    },
];

pub struct SettingsManager {
    pub path: PathBuf,
    pub settings: Settings,
    /// When set (the TUI), `save()` queues the write on the persistence worker instead of
    /// writing on the calling thread.
    persist: Option<crate::persist::PersistHandle>,
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self::with_path(paths::get_config_path())
    }
}

impl SettingsManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load settings from `path`, falling back to defaults. A file that exists but fails to
    /// parse is copied to `config.json.bak` and logged first (`persist::load_json_or_backup`),
    /// so saving the defaults later doesn't destroy the user's settings.
    pub fn with_path(path: PathBuf) -> Self {
        let settings = crate::persist::load_json_or_backup(&path, "settings").unwrap_or_default();
        Self { path, settings, persist: None }
    }

    /// Route future `save()` calls through a persistence worker (off the calling thread).
    pub fn set_persistence(&mut self, handle: crate::persist::PersistHandle) {
        self.persist = Some(handle);
    }

    pub fn load_from(path: &Path) -> Option<Settings> {
        if !path.exists() {
            return None;
        }
        let content = fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Write the settings to disk atomically (temp file + fsync + rename).
    ///
    /// With a persistence worker attached (`set_persistence`) the write is queued and this
    /// returns `true` once queued; failures are logged by the worker.
    pub fn save(&self) -> bool {
        let Ok(json) = serde_json::to_string_pretty(&self.settings) else {
            return false;
        };
        match &self.persist {
            Some(handle) => {
                handle.save(self.path.clone(), json.into_bytes());
                true
            }
            None => crate::persist::atomic_write(&self.path, json.as_bytes()).is_ok(),
        }
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
                // gapless_autopilot
                self.settings.gapless_autopilot = !self.settings.gapless_autopilot;
            }
            9 => {
                // default_mix_profile
                let profiles = ["diverse", "bpm_flow", "chill", "workout", "groove"];
                let cur_idx = profiles.iter().position(|&p| p == self.settings.default_mix_profile).unwrap_or(0);
                let next_idx = if forward {
                    (cur_idx + 1) % profiles.len()
                } else {
                    (cur_idx + profiles.len() - 1) % profiles.len()
                };
                self.settings.default_mix_profile = profiles[next_idx].to_string();
            }
            10 => {
                // online_enrichment
                self.settings.online_enrichment = !self.settings.online_enrichment;
            }
            11 => {
                // web_server_lan
                self.settings.web_server_lan = !self.settings.web_server_lan;
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
                let ip = if self.settings.web_server_lan {
                    crate::discovery::get_local_ip_for_target("8.8.8.8", 80)
                } else {
                    "127.0.0.1".to_string()
                };
                if self.settings.web_server_lan {
                    // Other devices need the pairing link; it carries the session token.
                    let link = crate::web_access::pairing_url(
                        &ip,
                        self.settings.web_server_port,
                        crate::web_access::web_token(),
                    );
                    format!("Enabled ({})", link)
                } else {
                    format!("Enabled (http://{}:{})", ip, self.settings.web_server_port)
                }
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
            8 => if self.settings.gapless_autopilot { "Enabled (Seamless PCM)" } else { "Disabled (Track-by-track)" }.to_string(),
            9 => match self.settings.default_mix_profile.as_str() {
                "diverse" => "Diverse Library Shuffle",
                "bpm_flow" => "DJ BPM Flow (124 BPM)",
                "chill" => "Chill & Ambient (Downtempo)",
                "workout" => "Workout & High Energy",
                "groove" => "Late Night Groove",
                other => other,
            }.to_string(),
            10 => if self.settings.online_enrichment { "Enabled (Last.fm Cache)" } else { "Disabled (Local Analysis Only)" }.to_string(),
            11 => if self.settings.web_server_lan { "Any device on your network" } else { "This computer only" }.to_string(),
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
        assert!(s.gapless_autopilot);
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
        mgr.settings.gapless_autopilot = false;
        assert!(mgr.save());

        let loaded = SettingsManager::load_from(&path).expect("should load");
        assert!(!loaded.metadata_view);
        assert_eq!(loaded.volume_step, 10);
        assert_eq!(loaded.home_dir, Some("/custom/home".to_string()));
        assert!(!loaded.gapless_autopilot);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_web_server_lan_defaults_off_and_toggles() {
        assert!(!Settings::default().web_server_lan);
        let legacy: Settings = serde_json::from_str(
            r#"{"metadata_view":true,"default_device":"local-audio","seek_step_seconds":10.0,"volume_step":5,"queue_mode":"loop","force_transcode":false,"player_backend":"mpv"}"#,
        )
        .unwrap();
        assert!(!legacy.web_server_lan, "configs saved before this setting existed stay loopback-only");

        let temp_dir = env::temp_dir().join(format!("quickstream_test_lan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let mut mgr = SettingsManager::with_path(temp_dir.join("test_config.json"));
        let idx = SETTING_DEFINITIONS.iter().position(|d| d.key == "web_server_lan").expect("setting row");
        mgr.cycle_setting(idx, true);
        assert!(mgr.settings.web_server_lan);
        assert!(mgr.get_display_value(idx).contains("network"));
        mgr.cycle_setting(idx, true);
        assert!(!mgr.settings.web_server_lan);
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

        assert!(mgr.settings.gapless_autopilot);
        assert_eq!(mgr.get_display_value(8), "Enabled (Seamless PCM)");
        mgr.cycle_setting(8, true);
        assert!(!mgr.settings.gapless_autopilot);
        assert_eq!(mgr.get_display_value(8), "Disabled (Track-by-track)");

        // Setting 9: default_mix_profile
        assert_eq!(mgr.settings.default_mix_profile, "diverse");
        mgr.cycle_setting(9, true);
        assert_eq!(mgr.settings.default_mix_profile, "bpm_flow");
        assert_eq!(mgr.get_display_value(9), "DJ BPM Flow (124 BPM)");

        // Setting 10: online_enrichment
        assert!(!mgr.settings.online_enrichment);
        mgr.cycle_setting(10, true);
        assert!(mgr.settings.online_enrichment);
        assert_eq!(mgr.get_display_value(10), "Enabled (Last.fm Cache)");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
