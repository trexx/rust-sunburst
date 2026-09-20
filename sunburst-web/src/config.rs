// SPDX-License-Identifier: GPL-2.0-or-later

//! Settings and the app catalogue.
//!
//! Serialised as JSON rather than TOML to keep the dependency to one crate; the
//! web UI is the intended editor, so the loss of comments costs little.
//!
//! Every field carries `#[serde(default)]`. A config written by an older build
//! must keep loading after a field is added, because the alternative is a server
//! that refuses to start after an update and takes its own web UI with it.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default web UI port.
///
/// Clear of Sunshine's range (47984, 47989–47990, 47998–48010) so both can be
/// installed on the same machine while this is being brought up.
pub const DEFAULT_WEB_PORT: u16 = 47810;

/// Default UDP port for the stream itself. One socket, per PROTOCOL.md.
pub const DEFAULT_STREAM_PORT: u16 = 47811;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub web: WebConfig,
    pub stream: StreamConfig,
    pub input: InputConfig,
    pub apps: Vec<AppEntry>,
    /// Next id to hand out. Monotonic, never reused, so a revoked app id in a
    /// client's cache cannot come back pointing at a different program.
    pub next_app_id: u32,
}

/// Server-side input tuning. All default to today's behaviour (no scaling, no
/// extra deadzone, EPP left as the OS has it).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    /// Multiplier applied to injected relative mouse deltas. 1.0 = 1:1.
    pub mouse_sensitivity: f32,
    /// Extra stick deadzone (0..1) applied on top of what the client reports.
    pub gamepad_deadzone: f32,
    /// Have the server turn Enhanced Pointer Precision off (`SystemParametersInfo`)
    /// while streaming, and restore it after. CLAUDE.md otherwise leaves this a
    /// manual OS checkbox; this makes the server own it, opt-in.
    pub disable_epp: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        InputConfig {
            mouse_sensitivity: 1.0,
            gamepad_deadzone: 0.0,
            disable_epp: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    /// Loopback by default. A LAN bind is opt-in and checked by
    /// [`Config::validate`].
    pub bind: IpAddr,
    pub port: u16,
    /// Bearer token for `/api/*`. Generated on first run.
    pub token: String,
    /// Where the built frontend lives. Relative paths resolve against the
    /// executable's directory, not the working directory, because a shortcut or
    /// a scheduled task sets that to somewhere unhelpful.
    pub assets_dir: PathBuf,
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_WEB_PORT,
            token: String::new(),
            assets_dir: PathBuf::from("web"),
        }
    }
}

impl WebConfig {
    /// Whether this binding is reachable from anything but this machine.
    pub fn is_lan_exposed(&self) -> bool {
        !self.bind.is_loopback()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct StreamConfig {
    pub port: u16,
    pub bitrate_kbps: u32,
    pub codec: CodecPreference,
    /// Stream game audio (WASAPI loopback + Opus). On by default.
    pub audio: bool,
    /// Render endpoint to capture, matched by name substring. `None` (or empty)
    /// captures the system default endpoint; "Steam Streaming Speakers" silences
    /// the host while the client still gets audio.
    pub audio_device: Option<String>,
    /// Render endpoint the pads' headset mic is played into, matched by name
    /// substring. `None` (or empty) disables the pad-mic path; "Steam Streaming
    /// Microphone" is the intended target — a signed virtual mic Steam installs,
    /// consumed the same way "Steam Streaming Speakers" is on the capture side,
    /// so no driver of ours is required. Games read it as a microphone input.
    pub mic_device: Option<String>,
    /// Opus target bitrate in kbps.
    pub audio_bitrate_kbps: u32,
    /// Switch the server's display to the client's resolution for the session
    /// and restore it after. Off by default: changing the physical mode is
    /// disruptive, and the virtual display (below) supersedes it. Ignored while
    /// `virtual_display` is active.
    pub match_resolution: bool,
    /// Capture a virtual display (the installed MikeTheTech VDD) at the client's
    /// resolution instead of the physical one — headless, host untouched. Opt-in
    /// like NvFBC; falls back to the physical display when the driver is absent.
    pub virtual_display: bool,
    /// Which monitor to capture, as a DXGI output index. `None` = the primary
    /// (the default). Set it to the virtual display's index when the VDD is not
    /// the primary monitor.
    pub capture_output: Option<u32>,

    // ── Advanced video (defaults reproduce today's fixed behaviour) ──────────
    /// Stream HDR. On by default (the 4K60 HDR10 workload); off streams SDR on
    /// an HDR-capable codec. Ignored for H.264, which is always SDR.
    pub hdr: bool,
    /// NVENC preset P1–P4 (1 = P1, fastest/lowest-latency). Clamped to 1..=4.
    /// Stays inside `TUNING_INFO_ULTRA_LOW_LATENCY` — this is not a UHQ escape.
    pub preset: u8,
    pub rate_control: RateControl,
    /// HEVC slices / AV1 tiles-per-axis; `0` = the codec default (HEVC 4, AV1 2).
    pub slices: u8,
    /// Forced IDR period in frames; `0` = infinite GOP (recovery via intra-refresh).
    pub idr_period: u32,
    /// Reconstructed-frame DPB depth (`maxNumRefFramesInDPB`).
    pub dpb_depth: u8,
    pub capture_backend: CaptureBackend,
    /// Rate-control floor in kbps.
    pub min_bitrate_kbps: u32,
    /// Rate-control ceiling in kbps; `0` = use `bitrate_kbps` as the ceiling.
    pub max_bitrate_kbps: u32,
    /// Cap the encode frame rate; `0` = follow the client's refresh.
    pub fps_cap: u32,

    // ── Advanced audio ───────────────────────────────────────────────────────
    /// Opus frame duration in microseconds (2500/5000/10000/20000). Lower = lower
    /// latency, more overhead. 5 ms default.
    pub audio_frame_us: u32,
    /// Opus in-band FEC.
    pub audio_fec: bool,
    /// Opus complexity 0..=10.
    pub audio_complexity: u8,
}

impl Default for StreamConfig {
    fn default() -> Self {
        StreamConfig {
            port: DEFAULT_STREAM_PORT,
            // 120 Mbps. CLAUDE.md puts HEVC at 100-150 and AV1 at 70-100, and
            // notes the Shield's decoder caps out before 1GbE does.
            bitrate_kbps: 120_000,
            codec: CodecPreference::Auto,
            audio: true,
            audio_device: None,
            mic_device: None,
            audio_bitrate_kbps: 128,
            match_resolution: false,
            virtual_display: false,
            capture_output: None,
            hdr: true,
            preset: 1,
            rate_control: RateControl::Cbr,
            slices: 0,
            idr_period: 0,
            dpb_depth: 8,
            capture_backend: CaptureBackend::Auto,
            min_bitrate_kbps: 10_000,
            max_bitrate_kbps: 0,
            fps_cap: 0,
            audio_frame_us: 5_000,
            audio_fec: true,
            audio_complexity: 10,
        }
    }
}

/// NVENC rate-control mode, within the ULL envelope.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RateControl {
    /// Constant bitrate — steady wire load, the streaming default.
    #[default]
    Cbr,
    /// Variable bitrate — spends less on static frames.
    Vbr,
}

/// Which capture backend to use. `Auto` is the OS default (WGC on Win11, DDA on
/// Win10); `NvFbc` is opt-in resilience (see CLAUDE.md), never automatic.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CaptureBackend {
    #[default]
    Auto,
    Wgc,
    Dda,
    Nvfbc,
}

/// Which codec to negotiate.
///
/// `Auto` is the sane default and the only one that works across both clients:
/// the Shield has no AV1 block and the Homatics' HEVC decoder is broken, so
/// pinning either one breaks one device.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CodecPreference {
    #[default]
    Auto,
    Hevc,
    Av1,
    /// H.264 High, 8-bit SDR — a low-latency, opt-in choice; HDR needs HEVC/AV1.
    H264,
}

/// One launchable entry. Typed in by hand; nothing is scanned.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AppEntry {
    pub id: u32,
    pub name: String,
    /// Executable, or a URI such as `steam://open/bigpicture`.
    pub exe: String,
    pub args: Vec<String>,
    pub working_dir: Option<PathBuf>,
    /// Run before launch, undone after exit. Resolution changes, HDR toggles.
    pub prep: Vec<PrepCommand>,
    pub overrides: SessionOverrides,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PrepCommand {
    pub run: String,
    /// Run after the app exits. Optional, because not everything needs undoing.
    pub undo: Option<String>,
}

/// Per-app overrides. All optional; `None` means inherit from [`StreamConfig`].
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SessionOverrides {
    pub bitrate_kbps: Option<u32>,
    pub codec: Option<CodecPreference>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    /// NVENC preset P1–P4 for this app; `None` inherits [`StreamConfig::preset`].
    pub preset: Option<u8>,
}

/// Why a config was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error(
        "bind address {0} is reachable from the network but no token is set; \
         refusing to start an unauthenticated admin API on the LAN"
    )]
    LanWithoutToken(IpAddr),
    #[error("web port and stream port are both {0}")]
    PortCollision(u16),
    #[error("app id {0} appears more than once")]
    DuplicateAppId(u32),
    #[error("app id {0} is at or above next_app_id {1}, so it could be handed out again")]
    AppIdNotBelowNext(u32, u32),
    #[error("app {0} has an empty name")]
    EmptyAppName(u32),
    #[error("app {0} has an empty exe")]
    EmptyAppExe(u32),
}

impl Config {
    /// Reject configurations that are unsafe or self-contradictory.
    ///
    /// Called on load and before every save, so a bad edit through the API is
    /// refused rather than persisted and then failed on at the next start.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // The "never an open relay" rule, enforced by construction rather than
        // by remembering to set a token after changing the bind address.
        if self.web.is_lan_exposed() && self.web.token.is_empty() {
            return Err(ConfigError::LanWithoutToken(self.web.bind));
        }
        if self.web.port == self.stream.port {
            return Err(ConfigError::PortCollision(self.web.port));
        }

        let mut seen = Vec::with_capacity(self.apps.len());
        for app in &self.apps {
            if seen.contains(&app.id) {
                return Err(ConfigError::DuplicateAppId(app.id));
            }
            seen.push(app.id);

            if app.id >= self.next_app_id {
                return Err(ConfigError::AppIdNotBelowNext(app.id, self.next_app_id));
            }
            if app.name.trim().is_empty() {
                return Err(ConfigError::EmptyAppName(app.id));
            }
            if app.exe.trim().is_empty() {
                return Err(ConfigError::EmptyAppExe(app.id));
            }
        }
        Ok(())
    }

    /// Claim the next app id.
    pub fn allocate_app_id(&mut self) -> u32 {
        let id = self.next_app_id;
        self.next_app_id += 1;
        id
    }

    pub fn app(&self, id: u32) -> Option<&AppEntry> {
        self.apps.iter().find(|a| a.id == id)
    }

    pub fn app_mut(&mut self, id: u32) -> Option<&mut AppEntry> {
        self.apps.iter_mut().find(|a| a.id == id)
    }

    /// Merge an app's overrides over the global stream settings.
    pub fn effective(&self, app: Option<&AppEntry>) -> StreamConfig {
        let mut effective = self.stream.clone();
        if let Some(app) = app {
            if let Some(b) = app.overrides.bitrate_kbps {
                effective.bitrate_kbps = b;
            }
            if let Some(c) = app.overrides.codec {
                effective.codec = c;
            }
            if let Some(p) = app.overrides.preset {
                effective.preset = p;
            }
        }
        effective
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lan() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 0, 10))
    }

    fn app(id: u32) -> AppEntry {
        AppEntry {
            id,
            name: "Big Picture".into(),
            exe: "steam://open/bigpicture".into(),
            ..Default::default()
        }
    }

    #[test]
    fn defaults_are_loopback_and_valid() {
        let c = Config::default();
        assert!(c.web.bind.is_loopback());
        assert!(!c.web.is_lan_exposed());
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn a_lan_bind_without_a_token_is_refused() {
        // The rule that makes "never an open relay" structural. Changing the
        // bind address and forgetting the token is the obvious mistake, so it
        // has to fail loudly rather than come up unauthenticated.
        let mut c = Config::default();
        c.web.bind = lan();
        assert_eq!(c.validate(), Err(ConfigError::LanWithoutToken(lan())));

        c.web.token = "s3cret".into();
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn loopback_without_a_token_is_allowed() {
        // Nothing off this machine can reach it, so a token would be ceremony.
        let c = Config::default();
        assert!(c.web.token.is_empty());
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn colliding_ports_are_refused() {
        let mut c = Config::default();
        c.stream.port = c.web.port;
        assert_eq!(c.validate(), Err(ConfigError::PortCollision(c.web.port)));
    }

    #[test]
    fn duplicate_app_ids_are_refused() {
        let c = Config {
            next_app_id: 2,
            apps: vec![app(0), app(0)],
            ..Default::default()
        };
        assert_eq!(c.validate(), Err(ConfigError::DuplicateAppId(0)));
    }

    #[test]
    fn an_id_at_or_above_the_counter_is_refused() {
        // Otherwise the same id gets handed out twice, and a client's cached
        // entry silently starts pointing at a different program.
        let c = Config {
            next_app_id: 1,
            apps: vec![app(1)],
            ..Default::default()
        };
        assert_eq!(c.validate(), Err(ConfigError::AppIdNotBelowNext(1, 1)));
    }

    #[test]
    fn ids_are_never_reused() {
        let mut c = Config::default();
        let first = c.allocate_app_id();
        let second = c.allocate_app_id();
        assert_ne!(first, second);

        c.apps.push(app(first));
        c.apps.push(app(second));
        assert_eq!(c.validate(), Ok(()));

        // Removing an entry does not free its id.
        c.apps.retain(|a| a.id != first);
        assert_eq!(c.allocate_app_id(), second + 1);
    }

    #[test]
    fn empty_names_and_exes_are_refused() {
        let mut c = Config {
            next_app_id: 1,
            ..Default::default()
        };

        let mut blank = app(0);
        blank.name = "   ".into();
        c.apps = vec![blank];
        assert_eq!(c.validate(), Err(ConfigError::EmptyAppName(0)));

        let mut noexe = app(0);
        noexe.exe = "".into();
        c.apps = vec![noexe];
        assert_eq!(c.validate(), Err(ConfigError::EmptyAppExe(0)));
    }

    #[test]
    fn a_missing_exe_path_is_storable() {
        // Deliberately not validated. Drives get remapped and games get moved;
        // an entry that cannot be saved because its path is currently wrong is
        // worse than one that is saved and fails at launch.
        let mut c = Config {
            next_app_id: 1,
            ..Default::default()
        };
        let mut missing = app(0);
        missing.exe = r"D:\gone\nothere.exe".into();
        c.apps = vec![missing];
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn config_round_trips_through_json() {
        let mut c = Config::default();
        c.web.token = "tok".into();
        c.next_app_id = 1;
        // Non-default values across the new advanced/video/audio and input fields,
        // so a round trip proves every one survives serde.
        c.stream.hdr = false;
        c.stream.codec = CodecPreference::H264;
        c.stream.preset = 3;
        c.stream.rate_control = RateControl::Vbr;
        c.stream.slices = 2;
        c.stream.idr_period = 120;
        c.stream.capture_backend = CaptureBackend::Nvfbc;
        c.stream.max_bitrate_kbps = 90_000;
        c.stream.fps_cap = 60;
        c.stream.audio_frame_us = 10_000;
        c.stream.audio_fec = false;
        c.input.mouse_sensitivity = 1.5;
        c.input.disable_epp = true;
        c.apps = vec![AppEntry {
            id: 0,
            name: "Game".into(),
            exe: r"C:\game.exe".into(),
            args: vec!["-windowed".into()],
            working_dir: Some(PathBuf::from(r"C:\")),
            prep: vec![PrepCommand {
                run: "set-hdr on".into(),
                undo: Some("set-hdr off".into()),
            }],
            overrides: SessionOverrides {
                bitrate_kbps: Some(80_000),
                codec: Some(CodecPreference::Av1),
                ..Default::default()
            },
        }];

        let json = serde_json::to_string_pretty(&c).expect("serialise");
        let back: Config = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(back, c);
    }

    #[test]
    fn a_config_from_an_older_build_still_loads() {
        // Every field defaults, so a file written before a field existed must
        // load rather than strand the server with no way to start its own UI.
        let json = r#"{"web": {"port": 1234}}"#;
        let c: Config = serde_json::from_str(json).expect("should load");
        assert_eq!(c.web.port, 1234);
        assert!(c.web.bind.is_loopback(), "unspecified fields take defaults");
        assert_eq!(c.stream.port, DEFAULT_STREAM_PORT);
    }

    #[test]
    fn an_unknown_field_is_reported_rather_than_ignored() {
        // A typo in a hand-edited file should say so. Silently dropping it means
        // a setting that appears to be applied and is not.
        let json = r#"{"web": {"prot": 1234}}"#;
        assert!(serde_json::from_str::<Config>(json).is_err());
    }

    #[test]
    fn overrides_apply_over_the_global_settings() {
        let mut c = Config::default();
        c.stream.bitrate_kbps = 120_000;
        c.stream.preset = 1;
        let mut a = app(0);
        a.overrides.bitrate_kbps = Some(70_000);
        a.overrides.codec = Some(CodecPreference::Av1);
        a.overrides.preset = Some(4);

        let effective = c.effective(Some(&a));
        assert_eq!(effective.bitrate_kbps, 70_000);
        assert_eq!(effective.codec, CodecPreference::Av1);
        assert_eq!(effective.preset, 4);

        // And an app with no overrides inherits everything.
        assert_eq!(c.effective(Some(&app(1))).bitrate_kbps, 120_000);
        assert_eq!(c.effective(Some(&app(1))).preset, 1);
        assert_eq!(c.effective(None).bitrate_kbps, 120_000);
    }
}
