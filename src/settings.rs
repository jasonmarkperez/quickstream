//! User preferences, configuration definitions, and JSON persistence.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// Playlist the add picker (`L`) highlights first: the one last added to.
    #[serde(default)]
    pub last_playlist: Option<String>,
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
            last_playlist: None,
        }
    }
}

impl Settings {
    pub fn mix_profile(&self) -> crate::analysis::MixProfile {
        mix_profile_for(&self.default_mix_profile)
    }
}

/// The mix profile a `default_mix_profile` key names (`diverse` for an unknown key).
pub fn mix_profile_for(key: &str) -> crate::analysis::MixProfile {
    match key {
        "bpm_flow" => crate::analysis::MixProfile::BpmFlow {
            target_bpm: 120.0,
            tolerance: 0.15,
        },
        "chill" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Chill),
        "workout" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Workout),
        "groove" => crate::analysis::MixProfile::MoodVibe(crate::analysis::MoodType::Groove),
        _ => crate::analysis::MixProfile::DiverseShuffle,
    }
}

/// The group a setting is listed under on the Settings tab (Telemetry mockup 2e).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingGroup {
    Playback,
    Library,
    Web,
}

impl SettingGroup {
    /// The group's heading: `PLAYBACK`, `LIBRARY`, `WEB`.
    pub fn label(self) -> &'static str {
        match self {
            SettingGroup::Playback => "PLAYBACK",
            SettingGroup::Library => "LIBRARY",
            SettingGroup::Web => "WEB",
        }
    }
}

pub struct SettingDefinition {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub group: SettingGroup,
}

/// Every setting, by index. The index is what `cycle_setting`, `get_display_value` and
/// `AppUI::settings_cursor` use; the Settings tab lists them in `SETTING_DISPLAY_ORDER`.
pub const SETTING_DEFINITIONS: &[SettingDefinition] = &[
    SettingDefinition {
        key: "metadata_view",
        label: "File List Display",
        description: "Show titles and artists from the tags in lists, or the file names.",
        group: SettingGroup::Library,
    },
    SettingDefinition {
        key: "player_backend",
        label: "Local Playback Engine",
        description: "The program that plays audio on this computer. If mpv fails, ffplay plays.",
        group: SettingGroup::Playback,
    },
    SettingDefinition {
        key: "seek_step_seconds",
        label: "Seek Jump Interval",
        description: "How far [ and ] (or ⇧← ⇧→) seek back or forward.",
        group: SettingGroup::Playback,
    },
    SettingDefinition {
        key: "volume_step",
        label: "Volume Step Size",
        description: "How much + and − change the volume.",
        group: SettingGroup::Playback,
    },
    SettingDefinition {
        key: "queue_mode",
        label: "Queue Playback Mode",
        description: "What plays when a track ends, and whether the queue loops or stops at its end.",
        group: SettingGroup::Playback,
    },
    SettingDefinition {
        key: "force_transcode",
        label: "Audio Transcoding",
        description: "Not used yet: meant to send 16-bit PCM WAV to older DLNA renderers.",
        group: SettingGroup::Playback,
    },
    SettingDefinition {
        key: "web_server_enabled",
        label: "Now Playing Web Server",
        description: "Serves the now-playing page and the web remote over HTTP.",
        group: SettingGroup::Web,
    },
    SettingDefinition {
        key: "home_dir",
        label: "Startup Home Directory",
        description: "The folder the library opens at startup, instead of the launch folder.",
        group: SettingGroup::Library,
    },
    SettingDefinition {
        key: "gapless_autopilot",
        label: "Gapless Continuous Stream",
        description: "On a DLNA renderer, autopilot plays as one gapless PCM stream.",
        group: SettingGroup::Playback,
    },
    SettingDefinition {
        key: "default_mix_profile",
        label: "Autopilot Mix Algorithm",
        description: "The mix profile autopilot starts with. M picks one for this session.",
        group: SettingGroup::Library,
    },
    SettingDefinition {
        key: "online_enrichment",
        label: "Online Tag Enrichment",
        description: "Not used yet: meant to add Last.fm genre tags (needs an API key).",
        group: SettingGroup::Library,
    },
    SettingDefinition {
        key: "web_server_lan",
        label: "Web Remote Access",
        description:
            "Lets phones and other devices on your network open the web remote. DLNA renderers always can while in use.",
        group: SettingGroup::Web,
    },
];

/// Whether `a` and `b` are the same string, at compile time.
const fn same_str(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// The index of the setting whose key is `key` in `SETTING_DEFINITIONS`, worked out at compile
/// time: a key that names no setting fails the build.
const fn setting_index(key: &str) -> usize {
    let mut i = 0;
    while i < SETTING_DEFINITIONS.len() {
        if same_str(SETTING_DEFINITIONS[i].key, key) {
            return i;
        }
        i += 1;
    }
    panic!("no setting with that key")
}

/// Each setting's index in `SETTING_DEFINITIONS`, named after its key, so no code writes a
/// setting's number.
pub mod setting {
    use super::setting_index;

    pub const METADATA_VIEW: usize = setting_index("metadata_view");
    pub const PLAYER_BACKEND: usize = setting_index("player_backend");
    pub const SEEK_STEP_SECONDS: usize = setting_index("seek_step_seconds");
    pub const VOLUME_STEP: usize = setting_index("volume_step");
    pub const QUEUE_MODE: usize = setting_index("queue_mode");
    pub const FORCE_TRANSCODE: usize = setting_index("force_transcode");
    pub const WEB_SERVER_ENABLED: usize = setting_index("web_server_enabled");
    pub const HOME_DIR: usize = setting_index("home_dir");
    pub const GAPLESS_AUTOPILOT: usize = setting_index("gapless_autopilot");
    pub const DEFAULT_MIX_PROFILE: usize = setting_index("default_mix_profile");
    pub const ONLINE_ENRICHMENT: usize = setting_index("online_enrichment");
    pub const WEB_SERVER_LAN: usize = setting_index("web_server_lan");

    /// The settings whose rows show the web address (web server, web remote access).
    pub const WEB: [usize; 2] = [WEB_SERVER_ENABLED, WEB_SERVER_LAN];
}

/// The order the Settings tab lists `SETTING_DEFINITIONS` in (mockup 2e): PLAYBACK (engine, seek,
/// volume step, queue mode, gapless, transcoding), LIBRARY (file list display, startup home, mix
/// algorithm, tag enrichment), WEB (web server, remote access). `j`/`k` move along it.
pub const SETTING_DISPLAY_ORDER: [usize; 12] = [
    setting::PLAYER_BACKEND,
    setting::SEEK_STEP_SECONDS,
    setting::VOLUME_STEP,
    setting::QUEUE_MODE,
    setting::GAPLESS_AUTOPILOT,
    setting::FORCE_TRANSCODE,
    setting::METADATA_VIEW,
    setting::HOME_DIR,
    setting::DEFAULT_MIX_PROFILE,
    setting::ONLINE_ENRICHMENT,
    setting::WEB_SERVER_ENABLED,
    setting::WEB_SERVER_LAN,
];

const _: () = assert!(SETTING_DISPLAY_ORDER.len() == SETTING_DEFINITIONS.len());

/// Where setting `index` is listed on the Settings tab (0-based), `None` for no setting.
pub fn display_position(index: usize) -> Option<usize> {
    SETTING_DISPLAY_ORDER.iter().position(|&i| i == index)
}

/// The setting listed after (`forward`) or before setting `index` on the Settings tab, wrapping
/// at either end. An index that is no setting moves to the first one.
pub fn step_in_display_order(index: usize, forward: bool) -> usize {
    let Some(pos) = display_position(index) else {
        return SETTING_DISPLAY_ORDER[0];
    };
    let n = SETTING_DISPLAY_ORDER.len();
    let next = if forward { (pos + 1) % n } else { (pos + n - 1) % n };
    SETTING_DISPLAY_ORDER[next]
}

/// Look the music folder up (`~/Music` on macOS). On Linux `directories` reads
/// `user-dirs.dirs` for it, so this touches the disk: the UI calls it on a worker at startup
/// (`AppUI::set_music_dir_lookup`) and passes the answer in [`SettingsContext::music_dir`].
pub fn lookup_music_dir() -> Option<PathBuf> {
    directories::UserDirs::new().and_then(|dirs| dirs.audio_dir().map(Path::to_path_buf))
}

/// What the settings' values and choices depend on besides config.json. The caller (the UI)
/// holds all of it in memory, so building values and choices never reads the disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct SettingsContext<'a> {
    /// The Music Homes; the startup home cycles through them (bookmarks.json is not read).
    pub bookmarks: &'a [String],
    /// The music folder, a startup home choice; `None` while it is still being looked up, or
    /// when there is none.
    pub music_dir: Option<&'a Path>,
    /// The home folder: a startup home choice, and shown as `~` in paths.
    pub home: Option<&'a Path>,
    /// The port the web server is bound to while it runs, which is not `web_server_port` when
    /// another program held that one; `None` while no server runs.
    pub web_port: Option<u16>,
}

const PLAYER_BACKENDS: [&str; 2] = ["mpv", "ffplay"];
const SEEK_STEPS: [f64; 4] = [5.0, 10.0, 15.0, 30.0];
const VOLUME_STEPS: [u8; 3] = [2, 5, 10];
const QUEUE_MODES: [&str; 3] = ["loop", "stop", "repeat_one"];
const MIX_PROFILES: [&str; 5] = ["diverse", "bpm_flow", "chill", "workout", "groove"];

/// A value one setting can hold.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Text(String),
    Seconds(f64),
    Percent(u8),
    /// The startup home directory; `None` is the launch directory.
    Path(Option<String>),
}

impl SettingValue {
    /// Whether a setting holding `self` is on `choice` (seek steps within 0.1 s, as before).
    fn is(&self, choice: &SettingValue) -> bool {
        match (self, choice) {
            (SettingValue::Seconds(a), SettingValue::Seconds(b)) => (a - b).abs() < 0.1,
            _ => self == choice,
        }
    }
}

/// One choice of a setting: its value and its compact label (`short_value` wording).
#[derive(Debug, Clone, PartialEq)]
pub struct SettingChoice {
    pub value: SettingValue,
    pub label: String,
}

/// Every choice a setting cycles through, in cycling order (`SettingsManager::options`).
#[derive(Debug, Clone, PartialEq)]
pub struct SettingOptions {
    pub choices: Vec<SettingChoice>,
    /// The choice the setting holds; `None` when it holds a value outside the list (a
    /// hand-edited config.json).
    pub current: Option<usize>,
    /// Where cycling starts from when `current` is `None`.
    fallback: usize,
}

impl SettingOptions {
    /// The choice cycling forward (or back) moves to, wrapping at either end.
    pub fn next(&self, forward: bool) -> usize {
        let n = self.choices.len().max(1);
        let at = self.current.unwrap_or(self.fallback);
        if forward {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        }
    }
}

/// How long a looked-up LAN address is used before it is looked up again (the network can
/// change while Quickstream runs).
const LAN_IP_REFRESH: Duration = Duration::from_secs(30);

type LanIpResolver = Arc<dyn Fn() -> String + Send + Sync>;

/// The LAN address in the web server's pairing link. Looking it up connects a UDP socket, so it
/// runs on a short-lived thread and the result is cached: drawing the Settings tab reads the
/// cache and never waits for the lookup.
struct LanIp {
    state: Arc<Mutex<LanIpState>>,
    resolver: LanIpResolver,
}

#[derive(Default)]
struct LanIpState {
    ip: Option<String>,
    resolved_at: Option<Instant>,
    resolving: bool,
}

impl LanIp {
    fn new(resolver: LanIpResolver) -> Self {
        Self {
            state: Arc::default(),
            resolver,
        }
    }

    /// The cached address, `None` until the first lookup has finished. Starts a lookup on a
    /// worker thread when there is no address yet or it is older than `LAN_IP_REFRESH`.
    fn get(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let stale = state.resolved_at.is_none_or(|at| at.elapsed() >= LAN_IP_REFRESH);
        if stale && !state.resolving {
            state.resolving = true;
            let (shared, resolver) = (self.state.clone(), self.resolver.clone());
            let spawned = std::thread::Builder::new().name("lan-ip".into()).spawn(move || {
                let ip = resolver();
                // The pairing link built from this address carries the session token, which
                // is read from (or first written to) a file: load it here, off the caller's
                // thread, before the address is published.
                let _ = crate::web_access::web_token();
                let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                state.ip = Some(ip);
                state.resolved_at = Some(Instant::now());
                state.resolving = false;
            });
            if spawned.is_err() {
                state.resolving = false;
            }
        }
        state.ip.clone()
    }
}

fn default_lan_ip_resolver() -> LanIpResolver {
    Arc::new(|| crate::discovery::get_local_ip_for_target("8.8.8.8", 80))
}

pub struct SettingsManager {
    pub path: PathBuf,
    pub settings: Settings,
    /// When set (the TUI), `save()` queues the write on the persistence worker instead of
    /// writing on the calling thread.
    persist: Option<crate::persist::PersistHandle>,
    lan_ip: LanIp,
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self::with_path(paths::get_config_path())
    }
}

// The named indexes, as match patterns.
use setting::{
    DEFAULT_MIX_PROFILE, FORCE_TRANSCODE, GAPLESS_AUTOPILOT, HOME_DIR, METADATA_VIEW, ONLINE_ENRICHMENT,
    PLAYER_BACKEND, QUEUE_MODE, SEEK_STEP_SECONDS, VOLUME_STEP, WEB_SERVER_ENABLED, WEB_SERVER_LAN,
};

impl SettingsManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load settings from `path`, falling back to defaults. A file that exists but fails to
    /// parse is copied to `config.json.bak` and logged first (`persist::load_json_or_backup`),
    /// so saving the defaults later doesn't destroy the user's settings.
    pub fn with_path(path: PathBuf) -> Self {
        let settings = crate::persist::load_json_or_backup(&path, "settings").unwrap_or_default();
        Self {
            path,
            settings,
            persist: None,
            lan_ip: LanIp::new(default_lan_ip_resolver()),
        }
    }

    /// Route future `save()` calls through a persistence worker (off the calling thread).
    pub fn set_persistence(&mut self, handle: crate::persist::PersistHandle) {
        self.persist = Some(handle);
    }

    /// Replace how the LAN address is looked up (test seam). The lookup still runs on a worker
    /// thread; the cached address is dropped.
    pub fn set_lan_ip_resolver(&mut self, resolver: impl Fn() -> String + Send + Sync + 'static) {
        self.lan_ip = LanIp::new(Arc::new(resolver));
    }

    /// Start looking the LAN address up on a worker thread, unless it is cached and fresh, so
    /// the pairing link is ready when it is shown. Never waits for the lookup.
    pub fn prefetch_lan_address(&self) {
        self.lan_ip.get();
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

    /// Move setting `index` to its next (or previous) choice in `options(index, context)`,
    /// wrapping, and save.
    pub fn cycle_setting(&mut self, index: usize, forward: bool, context: &SettingsContext) {
        let options = self.options(index, context);
        if let Some(choice) = options.choices.get(options.next(forward)) {
            self.set_value(index, choice.value.clone());
        }
        self.save();
    }

    /// The value setting `index` holds, `None` for no setting.
    pub fn value(&self, index: usize) -> Option<SettingValue> {
        let s = &self.settings;
        Some(match index {
            METADATA_VIEW => SettingValue::Bool(s.metadata_view),
            PLAYER_BACKEND => SettingValue::Text(s.player_backend.clone()),
            SEEK_STEP_SECONDS => SettingValue::Seconds(s.seek_step_seconds),
            VOLUME_STEP => SettingValue::Percent(s.volume_step),
            QUEUE_MODE => SettingValue::Text(s.queue_mode.clone()),
            FORCE_TRANSCODE => SettingValue::Bool(s.force_transcode),
            WEB_SERVER_ENABLED => SettingValue::Bool(s.web_server_enabled),
            HOME_DIR => SettingValue::Path(s.home_dir.clone()),
            GAPLESS_AUTOPILOT => SettingValue::Bool(s.gapless_autopilot),
            DEFAULT_MIX_PROFILE => SettingValue::Text(s.default_mix_profile.clone()),
            ONLINE_ENRICHMENT => SettingValue::Bool(s.online_enrichment),
            WEB_SERVER_LAN => SettingValue::Bool(s.web_server_lan),
            _ => return None,
        })
    }

    /// Set setting `index` to `value`; a value of the wrong kind is ignored.
    fn set_value(&mut self, index: usize, value: SettingValue) {
        let s = &mut self.settings;
        match (index, value) {
            (METADATA_VIEW, SettingValue::Bool(v)) => s.metadata_view = v,
            (PLAYER_BACKEND, SettingValue::Text(v)) => s.player_backend = v,
            (SEEK_STEP_SECONDS, SettingValue::Seconds(v)) => s.seek_step_seconds = v,
            (VOLUME_STEP, SettingValue::Percent(v)) => s.volume_step = v,
            (QUEUE_MODE, SettingValue::Text(v)) => s.queue_mode = v,
            (FORCE_TRANSCODE, SettingValue::Bool(v)) => s.force_transcode = v,
            (WEB_SERVER_ENABLED, SettingValue::Bool(v)) => s.web_server_enabled = v,
            (HOME_DIR, SettingValue::Path(v)) => s.home_dir = v,
            (GAPLESS_AUTOPILOT, SettingValue::Bool(v)) => s.gapless_autopilot = v,
            (DEFAULT_MIX_PROFILE, SettingValue::Text(v)) => s.default_mix_profile = v,
            (ONLINE_ENRICHMENT, SettingValue::Bool(v)) => s.online_enrichment = v,
            (WEB_SERVER_LAN, SettingValue::Bool(v)) => s.web_server_lan = v,
            _ => {}
        }
    }

    /// Every choice setting `index` cycles through, in order, and which one it holds. The
    /// startup home lists the launch directory, the context's music folder (left out while it is
    /// unknown), home folder and bookmarks, then a custom path it holds, each once.
    /// `cycle_setting` moves along this list, so the two never disagree.
    pub fn options(&self, index: usize, context: &SettingsContext) -> SettingOptions {
        let bools = || vec![SettingValue::Bool(false), SettingValue::Bool(true)];
        let texts =
            |list: &[&str]| -> Vec<SettingValue> { list.iter().map(|v| SettingValue::Text(v.to_string())).collect() };
        let values: Vec<SettingValue> = match index {
            METADATA_VIEW => vec![SettingValue::Bool(true), SettingValue::Bool(false)],
            PLAYER_BACKEND => texts(&PLAYER_BACKENDS),
            SEEK_STEP_SECONDS => SEEK_STEPS.iter().map(|&v| SettingValue::Seconds(v)).collect(),
            VOLUME_STEP => VOLUME_STEPS.iter().map(|&v| SettingValue::Percent(v)).collect(),
            QUEUE_MODE => texts(&QUEUE_MODES),
            FORCE_TRANSCODE | WEB_SERVER_ENABLED | GAPLESS_AUTOPILOT | ONLINE_ENRICHMENT | WEB_SERVER_LAN => bools(),
            HOME_DIR => {
                let mut paths: Vec<Option<String>> = vec![None];
                let listed = context
                    .music_dir
                    .into_iter()
                    .chain(context.home)
                    .map(|p| p.to_string_lossy().into_owned())
                    .chain(context.bookmarks.iter().cloned())
                    .chain(self.settings.home_dir.clone());
                for path in listed {
                    if !paths.iter().any(|p| p.as_deref() == Some(path.as_str())) {
                        paths.push(Some(path));
                    }
                }
                paths.into_iter().map(SettingValue::Path).collect()
            }
            DEFAULT_MIX_PROFILE => texts(&MIX_PROFILES),
            _ => Vec::new(),
        };
        let current = self
            .value(index)
            .and_then(|held| values.iter().position(|choice| held.is(choice)));
        // A value outside the list cycles on from the default's choice, as it always has
        // (any engine other than mpv goes back to mpv).
        let fallback = match index {
            PLAYER_BACKEND | SEEK_STEP_SECONDS | VOLUME_STEP => 1,
            _ => 0,
        };
        let choices = values
            .into_iter()
            .map(|value| SettingChoice {
                label: self.value_label(index, &value, context),
                value,
            })
            .collect();
        SettingOptions {
            choices,
            current,
            fallback,
        }
    }

    /// The compact value of setting `index` for the Settings list (`◂ VALUE ▸`, mockup 2e):
    /// uppercase (`MPV`, `10 S`, `CONTINUOUS (LOOP)`, `ON  :8080` with the context's web port,
    /// `ON` while no server runs), paths in their own case with `~` for the context's home. The
    /// same wording labels the choices in `options`.
    pub fn short_value(&self, index: usize, context: &SettingsContext) -> String {
        self.value(index)
            .map(|value| self.value_label(index, &value, context))
            .unwrap_or_default()
    }

    fn value_label(&self, index: usize, value: &SettingValue, context: &SettingsContext) -> String {
        let on_off = |on: bool| if on { "ON" } else { "OFF" }.to_string();
        match (index, value) {
            (METADATA_VIEW, SettingValue::Bool(tags)) => if *tags { "TAGS" } else { "FILES" }.to_string(),
            (QUEUE_MODE, SettingValue::Text(mode)) => match mode.as_str() {
                "loop" => "CONTINUOUS (LOOP)".to_string(),
                "stop" => "STOP AT END".to_string(),
                "repeat_one" => "REPEAT TRACK".to_string(),
                other => other.to_uppercase(),
            },
            (FORCE_TRANSCODE, SettingValue::Bool(pcm)) => if *pcm { "PCM WAV" } else { "OFF" }.to_string(),
            (WEB_SERVER_ENABLED, SettingValue::Bool(true)) => match context.web_port {
                Some(port) => format!("ON  :{}", port),
                None => "ON".to_string(),
            },
            (DEFAULT_MIX_PROFILE, SettingValue::Text(key)) if MIX_PROFILES.contains(&key.as_str()) => {
                mix_profile_for(key).short_name().to_uppercase()
            }
            (WEB_SERVER_LAN, SettingValue::Bool(lan)) => if *lan { "NETWORK" } else { "THIS COMPUTER" }.to_string(),
            (_, SettingValue::Bool(on)) => on_off(*on),
            (_, SettingValue::Text(text)) => text.to_uppercase(),
            (_, SettingValue::Seconds(s)) => format!("{} S", s),
            (_, SettingValue::Percent(p)) => format!("{} %", p),
            (_, SettingValue::Path(None)) => "LAUNCH DIRECTORY".to_string(),
            (_, SettingValue::Path(Some(path))) => paths::display_path(Path::new(path), context.home),
        }
    }

    /// The web remote's address while the web server setting is on and the server runs on
    /// `web_port` (the port it bound; `None` while none runs), in its own case:
    /// `http://127.0.0.1:<port>` for this computer, or with network access the pairing link (LAN
    /// address and session token). The LAN address comes from the cache (looked up on a
    /// worker), so the link is `None` until it is known rather than looked up while drawing.
    pub fn web_url(&self, web_port: Option<u16>) -> Option<String> {
        if !self.settings.web_server_enabled {
            return None;
        }
        let port = web_port?;
        if !self.settings.web_server_lan {
            return Some(format!("http://127.0.0.1:{}", port));
        }
        // Other devices need the pairing link; it carries the session token.
        let ip = self.lan_ip.get()?;
        Some(crate::web_access::pairing_url(
            &ip,
            port,
            crate::web_access::web_token(),
        ))
    }

    /// The long wording of setting `index` (status messages): `Continuous (Loop Queue)`,
    /// `Enabled (<web address>)`, a path with `~` for the context's home.
    pub fn get_display_value(&self, index: usize, context: &SettingsContext) -> String {
        match index {
            METADATA_VIEW => if self.settings.metadata_view {
                "Metadata (Tags)"
            } else {
                "Filename"
            }
            .to_string(),
            PLAYER_BACKEND => if self.settings.player_backend == "mpv" {
                "MPV (IPC socket)"
            } else {
                "FFplay (subprocess)"
            }
            .to_string(),
            SEEK_STEP_SECONDS => format!("{} seconds", self.settings.seek_step_seconds),
            VOLUME_STEP => format!("{}%", self.settings.volume_step),
            QUEUE_MODE => match self.settings.queue_mode.as_str() {
                "loop" => "Continuous (Loop Queue)",
                "stop" => "Stop at End",
                "repeat_one" => "Repeat Current Track",
                other => other,
            }
            .to_string(),
            FORCE_TRANSCODE => if self.settings.force_transcode {
                "Force 16-bit PCM WAV"
            } else {
                "Direct Stream (Auto)"
            }
            .to_string(),
            WEB_SERVER_ENABLED if !self.settings.web_server_enabled => "Disabled".to_string(),
            WEB_SERVER_ENABLED => match self.web_url(context.web_port) {
                Some(url) => format!("Enabled ({})", url),
                None => "Enabled".to_string(),
            },
            HOME_DIR => match self.settings.home_dir {
                Some(ref h) => paths::display_path(Path::new(h), context.home),
                None => "Default (Launch Directory)".to_string(),
            },
            GAPLESS_AUTOPILOT => if self.settings.gapless_autopilot {
                "Enabled (Seamless PCM)"
            } else {
                "Disabled (Track-by-track)"
            }
            .to_string(),
            DEFAULT_MIX_PROFILE => match self.settings.default_mix_profile.as_str() {
                "diverse" => "Diverse Library Shuffle",
                "bpm_flow" => "DJ BPM Flow (124 BPM)",
                "chill" => "Chill & Ambient (Downtempo)",
                "workout" => "Workout & High Energy",
                "groove" => "Late Night Groove",
                other => other,
            }
            .to_string(),
            ONLINE_ENRICHMENT => if self.settings.online_enrichment {
                "Enabled (Last.fm Cache)"
            } else {
                "Disabled (Local Analysis Only)"
            }
            .to_string(),
            WEB_SERVER_LAN => if self.settings.web_server_lan {
                "Any device on your network"
            } else {
                "This computer only"
            }
            .to_string(),
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
        assert!(
            !legacy.web_server_lan,
            "configs saved before this setting existed stay loopback-only"
        );

        let temp_dir = env::temp_dir().join(format!("quickstream_test_lan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let mut mgr = SettingsManager::with_path(temp_dir.join("test_config.json"));
        let idx = SETTING_DEFINITIONS
            .iter()
            .position(|d| d.key == "web_server_lan")
            .expect("setting row");
        mgr.cycle_setting(idx, true, &SettingsContext::default());
        assert!(mgr.settings.web_server_lan);
        assert!(mgr
            .get_display_value(idx, &SettingsContext::default())
            .contains("network"));
        mgr.cycle_setting(idx, true, &SettingsContext::default());
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
        mgr.cycle_setting(METADATA_VIEW, true, &SettingsContext::default());
        assert!(!mgr.settings.metadata_view);

        assert_eq!(mgr.settings.player_backend, "mpv");
        mgr.cycle_setting(PLAYER_BACKEND, true, &SettingsContext::default());
        assert_eq!(mgr.settings.player_backend, "ffplay");

        assert!(!mgr.settings.web_server_enabled);
        mgr.cycle_setting(WEB_SERVER_ENABLED, true, &SettingsContext::default());
        assert!(mgr.settings.web_server_enabled);

        assert_eq!(mgr.settings.home_dir, None);
        assert_eq!(
            mgr.get_display_value(HOME_DIR, &SettingsContext::default()),
            "Default (Launch Directory)"
        );
        mgr.cycle_setting(HOME_DIR, true, &SettingsContext::default());
        mgr.cycle_setting(HOME_DIR, false, &SettingsContext::default());
        assert_eq!(mgr.settings.home_dir, None);

        assert!(mgr.settings.gapless_autopilot);
        assert_eq!(
            mgr.get_display_value(GAPLESS_AUTOPILOT, &SettingsContext::default()),
            "Enabled (Seamless PCM)"
        );
        mgr.cycle_setting(GAPLESS_AUTOPILOT, true, &SettingsContext::default());
        assert!(!mgr.settings.gapless_autopilot);
        assert_eq!(
            mgr.get_display_value(GAPLESS_AUTOPILOT, &SettingsContext::default()),
            "Disabled (Track-by-track)"
        );

        assert_eq!(mgr.settings.default_mix_profile, "diverse");
        mgr.cycle_setting(DEFAULT_MIX_PROFILE, true, &SettingsContext::default());
        assert_eq!(mgr.settings.default_mix_profile, "bpm_flow");
        assert_eq!(
            mgr.get_display_value(DEFAULT_MIX_PROFILE, &SettingsContext::default()),
            "DJ BPM Flow (124 BPM)"
        );

        assert!(!mgr.settings.online_enrichment);
        mgr.cycle_setting(ONLINE_ENRICHMENT, true, &SettingsContext::default());
        assert!(mgr.settings.online_enrichment);
        assert_eq!(
            mgr.get_display_value(ONLINE_ENRICHMENT, &SettingsContext::default()),
            "Enabled (Last.fm Cache)"
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
