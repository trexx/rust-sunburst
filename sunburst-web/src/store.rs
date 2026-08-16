// SPDX-License-Identifier: GPL-2.0-or-later

//! On-disk state: `config.json` and `clients.json`.
//!
//! **Two files, not one.** Settings are rendered in the UI and can appear in a
//! log; pairing secrets must do neither. Keeping them apart means the config can
//! be handled freely without a secret ever passing through that path.
//!
//! **Writes are atomic.** Temp file, then rename over the original. A
//! half-written `clients.json` after a power cut means re-pairing every device
//! from a TV remote, which is the worst text-entry situation in the system.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::client::PairedClient;
use crate::config::{Config, ConfigError};

const CONFIG_FILE: &str = "config.json";
const CLIENTS_FILE: &str = "clients.json";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no configuration directory: set SUNBURST_CONFIG_DIR")]
    NoConfigDir,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Deliberately distinct from a missing file. A corrupt file is a problem to
    /// report, not a reason to carry on as though nothing was configured.
    #[error("{path} is not valid JSON: {source}")]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0}")]
    Invalid(#[from] ConfigError),
    /// The OS random source failed while minting the first-run token.
    #[error("{0}")]
    Random(#[from] crate::random::RandomError),
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Use an explicit directory. Tests point this at a temporary path.
    pub fn at(dir: impl Into<PathBuf>) -> Store {
        Store { dir: dir.into() }
    }

    /// The per-user location.
    ///
    /// Per-user rather than machine-wide because the server runs in the
    /// interactive session, not as a service — there is exactly one user it
    /// could belong to.
    pub fn default_dir() -> Result<PathBuf, StoreError> {
        if let Ok(explicit) = std::env::var("SUNBURST_CONFIG_DIR") {
            return Ok(PathBuf::from(explicit));
        }
        #[cfg(windows)]
        let base = std::env::var("APPDATA").ok().map(PathBuf::from);
        #[cfg(not(windows))]
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config"))
            });

        base.map(|b| b.join("sunburst"))
            .ok_or(StoreError::NoConfigDir)
    }

    pub fn open_default() -> Result<Store, StoreError> {
        Ok(Store::at(Store::default_dir()?))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    pub fn clients_path(&self) -> PathBuf {
        self.dir.join(CLIENTS_FILE)
    }

    /// Load settings, or the defaults if the file has never been written.
    pub fn load_config(&self) -> Result<Config, StoreError> {
        let path = self.config_path();
        let Some(text) = read_optional(&path)? else {
            return Ok(Config::default());
        };
        let config: Config = serde_json::from_str(&text).map_err(|source| StoreError::Corrupt {
            path: path.clone(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Validate, then write atomically.
    ///
    /// Validation happens before the write so a bad edit through the API is
    /// refused outright, rather than persisted and then discovered at the next
    /// start — when the UI that could fix it is the thing failing to come up.
    pub fn save_config(&self, config: &Config) -> Result<(), StoreError> {
        config.validate()?;
        let json = serde_json::to_string_pretty(config).expect("Config always serialises");
        write_atomic(&self.config_path(), json.as_bytes(), false)
    }

    /// Load paired clients, or an empty list if nothing has been paired.
    pub fn load_clients(&self) -> Result<Vec<PairedClient>, StoreError> {
        let path = self.clients_path();
        let Some(text) = read_optional(&path)? else {
            return Ok(Vec::new());
        };
        serde_json::from_str(&text).map_err(|source| StoreError::Corrupt { path, source })
    }

    pub fn save_clients(&self, clients: &[PairedClient]) -> Result<(), StoreError> {
        let json = serde_json::to_string_pretty(clients).expect("clients always serialise");
        write_atomic(&self.clients_path(), json.as_bytes(), true)
    }
}

/// `Ok(None)` for a file that is not there; an error for one that cannot be read.
fn read_optional(path: &Path) -> Result<Option<String>, StoreError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Write via a temporary file and rename, so a reader never sees a partial file.
///
/// `sensitive` restricts the mode to owner-only on unix. Windows inherits the
/// directory ACL, which for a per-user `%APPDATA%` path is already owner-only.
fn write_atomic(path: &Path, bytes: &[u8], sensitive: bool) -> Result<(), StoreError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|source| StoreError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    let tmp = path.with_extension("tmp");
    let io_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source| StoreError::Io { path, source }
    };

    fs::write(&tmp, bytes).map_err(io_err(&tmp))?;

    if sensitive {
        restrict(&tmp)?;
    }

    // std::fs::rename replaces an existing destination on both platforms:
    // MoveFileEx with MOVEFILE_REPLACE_EXISTING on Windows, rename(2) on unix.
    fs::rename(&tmp, path).map_err(io_err(path))?;
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::QuirksRecord;
    use crate::config::AppEntry;
    use std::net::{IpAddr, Ipv4Addr};

    /// A unique scratch directory, removed on drop.
    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Temp {
            let mut p = std::env::temp_dir();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            p.push(format!("sunburst-store-{tag}-{unique}"));
            fs::create_dir_all(&p).expect("create temp dir");
            Temp(p)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn client(id: u32) -> PairedClient {
        PairedClient {
            id,
            name: format!("client {id}"),
            model: "SHIELD Android TV".into(),
            abi: "arm64-v8a".into(),
            quirks: QuirksRecord::default(),
            paired_at: 1_700_000_000,
            last_seen: None,
            secret: [id as u8; 32],
        }
    }

    #[test]
    fn missing_files_yield_defaults_not_errors() {
        let dir = Temp::new("missing");
        let store = Store::at(&dir.0);
        assert_eq!(store.load_config().expect("config"), Config::default());
        assert_eq!(store.load_clients().expect("clients"), vec![]);
    }

    #[test]
    fn config_round_trips_through_disk() {
        let dir = Temp::new("config");
        let store = Store::at(&dir.0);

        let mut config = Config::default();
        config.web.token = "tok".into();
        config.next_app_id = 1;
        config.apps = vec![AppEntry {
            id: 0,
            name: "Big Picture".into(),
            exe: "steam://open/bigpicture".into(),
            ..Default::default()
        }];

        store.save_config(&config).expect("save");
        assert_eq!(store.load_config().expect("load"), config);
    }

    #[test]
    fn clients_round_trip_through_disk() {
        let dir = Temp::new("clients");
        let store = Store::at(&dir.0);
        let clients = vec![client(1), client(2)];
        store.save_clients(&clients).expect("save");
        assert_eq!(store.load_clients().expect("load"), clients);
    }

    #[test]
    fn a_truncated_clients_file_is_reported_not_read_as_empty() {
        // The failure this guards: a corrupt file read as "nothing is paired"
        // would silently unpair every device and invite a re-pair, which is
        // indistinguishable from an attacker having cleared it.
        let dir = Temp::new("truncated");
        let store = Store::at(&dir.0);
        store.save_clients(&[client(1)]).expect("save");

        let text = fs::read_to_string(store.clients_path()).expect("read");
        fs::write(store.clients_path(), &text[..text.len() / 2]).expect("truncate");

        match store.load_clients() {
            Err(StoreError::Corrupt { .. }) => {}
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn a_corrupt_config_is_reported_not_replaced_by_defaults() {
        let dir = Temp::new("corrupt-config");
        let store = Store::at(&dir.0);
        fs::write(store.config_path(), "{ this is not json").expect("write");
        match store.load_config() {
            Err(StoreError::Corrupt { .. }) => {}
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn an_invalid_config_is_refused_on_save_and_on_load() {
        let dir = Temp::new("invalid");
        let store = Store::at(&dir.0);

        let mut bad = Config::default();
        bad.web.bind = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 10));
        bad.web.token = String::new();

        // Refused before it can reach the disk.
        assert!(matches!(
            store.save_config(&bad),
            Err(StoreError::Invalid(_))
        ));
        assert!(!store.config_path().exists());

        // And refused if it gets there another way, such as a hand edit.
        fs::write(
            store.config_path(),
            serde_json::to_string(&bad).expect("serialise"),
        )
        .expect("write");
        assert!(matches!(store.load_config(), Err(StoreError::Invalid(_))));
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = Temp::new("tmp");
        let store = Store::at(&dir.0);
        store.save_config(&Config::default()).expect("save");
        store.save_clients(&[client(1)]).expect("save");

        let leftovers: Vec<_> = fs::read_dir(&dir.0)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn a_second_save_replaces_the_first() {
        // Rename-over-existing behaves differently on Windows and unix, so the
        // overwrite path is worth exercising rather than assuming.
        let dir = Temp::new("overwrite");
        let store = Store::at(&dir.0);
        store.save_clients(&[client(1)]).expect("first");
        store.save_clients(&[client(2), client(3)]).expect("second");

        let loaded = store.load_clients().expect("load");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, 2);
    }

    #[test]
    fn revoking_actually_removes_the_secret_from_disk() {
        // One of the two free mitigations for PIN-derived pairing: a revoked
        // client must not leave its secret recoverable in the file.
        let dir = Temp::new("revoke");
        let store = Store::at(&dir.0);
        store.save_clients(&[client(1), client(2)]).expect("save");

        let kept: Vec<_> = store
            .load_clients()
            .expect("load")
            .into_iter()
            .filter(|c| c.id != 1)
            .collect();
        store.save_clients(&kept).expect("save");

        let text = fs::read_to_string(store.clients_path()).expect("read");
        assert!(
            !text.contains(&"01".repeat(32)),
            "revoked secret still on disk"
        );
        assert!(text.contains(&"02".repeat(32)), "kept client was lost");
    }

    #[test]
    fn saving_creates_the_directory() {
        let dir = Temp::new("mkdir");
        let nested = dir.0.join("a").join("b");
        let store = Store::at(&nested);
        store.save_config(&Config::default()).expect("save");
        assert!(store.config_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_clients_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Temp::new("perms");
        let store = Store::at(&dir.0);
        store.save_clients(&[client(1)]).expect("save");

        let mode = fs::metadata(store.clients_path())
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "secrets readable by others: {mode:o}");
    }
}
