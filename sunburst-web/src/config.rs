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
    pub apps: Vec<AppEntry>,
    /// Next id to hand out. Monotonic, never reused, so a revoked app id in a
    /// client's cache cannot come back pointing at a different program.
    pub next_app_id: u32,
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
}

impl Default for StreamConfig {
    fn default() -> Self {
        StreamConfig {
            port: DEFAULT_STREAM_PORT,
            // 120 Mbps. CLAUDE.md puts HEVC at 100-150 and AV1 at 70-100, and
            // notes the Shield's decoder caps out before 1GbE does.
            bitrate_kbps: 120_000,
            codec: CodecPreference::Auto,
        }
    }
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
        let mut a = app(0);
        a.overrides.bitrate_kbps = Some(70_000);
        a.overrides.codec = Some(CodecPreference::Av1);

        let effective = c.effective(Some(&a));
        assert_eq!(effective.bitrate_kbps, 70_000);
        assert_eq!(effective.codec, CodecPreference::Av1);

        // And an app with no overrides inherits everything.
        assert_eq!(c.effective(Some(&app(1))).bitrate_kbps, 120_000);
        assert_eq!(c.effective(None).bitrate_kbps, 120_000);
    }
}
