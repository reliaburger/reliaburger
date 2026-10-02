//! Staging an OS update on an appliance node (W6): what `os-stage` did by
//! hand, now done by bun when the leader says so.
//!
//! The node fetches the target release's `SHA256SUMS` and its signature
//! from the release beside the channel (`…/os-<version>-<arch>/`), checks
//! the signature against the release keys, downloads the UKI and the two
//! `/usr` images into a root-only staging directory, checking each against
//! `SHA256SUMS`, and lets `systemd-sysupdate` copy them into the spare slot
//! and onto the ESP. Then it reboots. systemd-boot gives the new version
//! three tries; if none reaches `boot-complete.target`, it falls back.
//!
//! What it's doing is kept in `os-update.json`, so after the reboot bun can
//! tell whether it came up on the version it staged or fell back, and say
//! so in `/v1/version`. The leader reads that, not a guess from a timeout.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::rollout::{OsDirective, OsUpdateState};
use super::{OsError, SumsEntry, check_asset, signed_sums};
use crate::upgrade::signing::PublicKey;

/// Where the appliance's sysupdate definitions live: only an appliance has
/// them, so they're also how bun knows it's running on one.
pub const DEFINITIONS: &str = "/usr/lib/reliaburger/sysupdate.d";

/// This machine's slot, once bun has looked. A machine runs one OS, so
/// the slot is process-wide: bun installs it at start-up (an appliance
/// only), and the API's handlers read it.
static INSTALLED: std::sync::OnceLock<Arc<OsSlot>> = std::sync::OnceLock::new();

/// Make `slot` this process's slot. Only the first call counts.
pub fn install(slot: Arc<OsSlot>) {
    let _ = INSTALLED.set(slot);
}

/// This process's slot, if it runs on an appliance.
pub fn installed() -> Option<Arc<OsSlot>> {
    INSTALLED.get().cloned()
}

/// The version of the running image, from `os-release`'s `IMAGE_VERSION`.
pub fn image_version(os_release: &str) -> Option<String> {
    os_release
        .lines()
        .find_map(|line| line.strip_prefix("IMAGE_VERSION="))
        .map(|v| v.trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

/// The files sysupdate needs from a release's `SHA256SUMS`: the UKI and the
/// two `/usr` images (whose names carry their partition UUIDs).
pub fn wanted(entries: &[SumsEntry], version: &str) -> Result<Vec<String>, String> {
    let prefix = format!("reliaburger-os_{version}.");
    let is_image = |rest: &str, kind: &str| {
        rest.strip_prefix(kind)
            .and_then(|r| r.strip_suffix(".raw.zst"))
            .is_some_and(|uuid| {
                !uuid.is_empty() && uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
            })
    };
    let files: Vec<String> = entries
        .iter()
        .map(|e| e.name.clone())
        .filter(|name| {
            name.strip_prefix(&prefix).is_some_and(|rest| {
                rest == "efi" || is_image(rest, "usr.") || is_image(rest, "usr-verity.")
            })
        })
        .collect();
    if files.len() != 3 {
        return Err(format!(
            "the release doesn't list a UKI and both /usr images for {version}"
        ));
    }
    Ok(files)
}

/// The URL of `file` in the release `os-<version>-<arch>`, beside the
/// channel at `channel_url` (`…/releases/download/os-channel/os-channel.json`).
pub fn asset_url(
    channel_url: &str,
    version: &str,
    arch: &str,
    file: &str,
) -> Result<String, String> {
    let base = channel_url
        .rsplit_once("/os-channel/")
        .map(|(base, _)| base)
        .ok_or_else(|| format!("{channel_url} isn't …/os-channel/os-channel.json"))?;
    Ok(format!("{base}/os-{version}-{arch}/{file}"))
}

/// What the saved state means once the node is up again on `running`: a
/// reboot into the target that left us on another version was a fallback.
pub fn after_boot(saved: OsUpdateState, running: Option<&str>) -> OsUpdateState {
    match saved {
        OsUpdateState::Rebooting { target } if running == Some(target.as_str()) => {
            OsUpdateState::Idle
        }
        OsUpdateState::Rebooting { target } => OsUpdateState::Failed {
            reason: format!(
                "booted {} instead: {target} failed its boot checks, so systemd-boot fell back",
                running.unwrap_or("an unknown version")
            ),
            target,
        },
        OsUpdateState::Staging { target } => OsUpdateState::Failed {
            target,
            reason: "bun restarted while staging the update".to_string(),
        },
        other => other,
    }
}

/// What [`OsSlot::begin`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begun {
    /// Staging started; the node reboots when it's done.
    Started,
    /// This version is staging or rebooting already.
    InProgress,
    /// The node runs this version already.
    Running,
}

/// Paths and commands, overridable for tests.
#[derive(Debug, Clone)]
pub struct SlotPaths {
    /// `os-update.json`.
    pub state_file: PathBuf,
    /// Where verified files wait for sysupdate.
    pub staging: PathBuf,
    /// `systemd-sysupdate --definitions=… update <version>`, as argv.
    pub sysupdate: Vec<String>,
    /// The reboot, as argv.
    pub reboot: Vec<String>,
}

impl Default for SlotPaths {
    fn default() -> Self {
        Self {
            state_file: PathBuf::from("/var/lib/reliaburger/os-update.json"),
            staging: PathBuf::from("/var/lib/reliaburger/os-staging"),
            sysupdate: vec![
                "/usr/lib/systemd/systemd-sysupdate".into(),
                format!("--definitions={DEFINITIONS}"),
                "update".into(),
            ],
            reboot: vec!["systemctl".into(), "reboot".into()],
        }
    }
}

/// An appliance node's OS slots: the running version, and the update in
/// progress.
pub struct OsSlot {
    paths: SlotPaths,
    arch: String,
    keys: Vec<PublicKey>,
    running: Option<String>,
    state: tokio::sync::Mutex<OsUpdateState>,
}

impl OsSlot {
    /// The slot on this machine, or `None` if it isn't an appliance.
    pub fn detect(keys: Vec<PublicKey>) -> Option<Arc<Self>> {
        if !Path::new(DEFINITIONS).is_dir() {
            return None;
        }
        let running = image_version(&std::fs::read_to_string("/usr/lib/os-release").ok()?);
        Some(Self::new(
            SlotPaths::default(),
            std::env::consts::ARCH,
            keys,
            running,
        ))
    }

    /// A slot with explicit paths, reading the saved state from them.
    pub fn new(
        paths: SlotPaths,
        arch: &str,
        keys: Vec<PublicKey>,
        running: Option<String>,
    ) -> Arc<Self> {
        let saved = std::fs::read(&paths.state_file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let state = after_boot(saved, running.as_deref());
        let _ = save(&paths.state_file, &state);
        Arc::new(Self {
            paths,
            arch: arch.to_string(),
            keys,
            running,
            state: tokio::sync::Mutex::new(state),
        })
    }

    /// The running image version.
    pub fn running(&self) -> Option<&str> {
        self.running.as_deref()
    }

    /// The update in progress, or how the last one ended.
    pub async fn state(&self) -> OsUpdateState {
        self.state.lock().await.clone()
    }

    /// Start updating to `directive.version`, unless it's running or under
    /// way already. Staging carries on in the background and ends in a
    /// reboot, or in [`OsUpdateState::Failed`].
    pub async fn begin(self: &Arc<Self>, directive: &OsDirective) -> Result<Begun, String> {
        if self.running.as_deref() == Some(directive.version.as_str()) {
            return Ok(Begun::Running);
        }
        let mut state = self.state.lock().await;
        match &*state {
            OsUpdateState::Staging { target } | OsUpdateState::Rebooting { target } => {
                return if *target == directive.version {
                    Ok(Begun::InProgress)
                } else {
                    Err(format!("already updating to {target}"))
                };
            }
            OsUpdateState::Idle | OsUpdateState::Failed { .. } => {}
        }
        *state = OsUpdateState::Staging {
            target: directive.version.clone(),
        };
        save(&self.paths.state_file, &state).map_err(|e| e.to_string())?;
        drop(state);
        let slot = Arc::clone(self);
        let directive = directive.clone();
        tokio::spawn(async move { slot.update(directive).await });
        Ok(Begun::Started)
    }

    async fn update(&self, directive: OsDirective) {
        let target = directive.version.clone();
        let next = match self.stage(&directive).await {
            Ok(()) => OsUpdateState::Rebooting {
                target: target.clone(),
            },
            Err(reason) => OsUpdateState::Failed {
                target: target.clone(),
                reason,
            },
        };
        let rebooting = matches!(next, OsUpdateState::Rebooting { .. });
        *self.state.lock().await = next.clone();
        if let Err(error) = save(&self.paths.state_file, &next) {
            eprintln!("bun: os update: saving its state: {error}");
        }
        if rebooting {
            eprintln!("bun: os update: {target} staged; rebooting into it");
            if let Err(error) = run(&self.paths.reboot, &[]).await {
                let failed = OsUpdateState::Failed {
                    target,
                    reason: format!("rebooting: {error}"),
                };
                let _ = save(&self.paths.state_file, &failed);
                *self.state.lock().await = failed;
            }
        }
    }

    /// Download, check and hand `directive.version` to sysupdate.
    async fn stage(&self, directive: &OsDirective) -> Result<(), String> {
        let version = &directive.version;
        let url = |file: &str| asset_url(&directive.channel_url, version, &self.arch, file);
        let client = reqwest::Client::builder()
            .user_agent("bun")
            .build()
            .map_err(|e| e.to_string())?;
        let sums_name = format!("reliaburger-os_{version}.SHA256SUMS");
        let sums = fetch(&client, &url(&sums_name)?).await?;
        let signature = fetch(&client, &url(&format!("{sums_name}.sig"))?).await?;
        let entries = signed_sums(&sums_name, &sums, &signature, &self.keys)
            .map_err(|e: OsError| e.to_string())?;
        let files = wanted(&entries, version)?;

        std::fs::create_dir_all(&self.paths.staging).map_err(|e| e.to_string())?;
        set_private(&self.paths.staging)?;
        let incoming = self.paths.staging.join(".incoming");
        let _ = std::fs::remove_dir_all(&incoming);
        std::fs::create_dir_all(&incoming).map_err(|e| e.to_string())?;
        for file in &files {
            eprintln!("bun: os update: fetching {file}");
            let digest = download(&client, &url(file)?, &incoming.join(file)).await?;
            check_asset(&entries, &sums_name, file, &digest).map_err(|e| e.to_string())?;
        }
        // Only verified files, and only this version's, reach sysupdate.
        for entry in std::fs::read_dir(&self.paths.staging).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_file() {
                std::fs::remove_file(&path).map_err(|e| e.to_string())?;
            }
        }
        for file in &files {
            std::fs::rename(incoming.join(file), self.paths.staging.join(file))
                .map_err(|e| e.to_string())?;
        }
        let _ = std::fs::remove_dir_all(&incoming);
        eprintln!("bun: os update: {version} checked; running systemd-sysupdate");
        run(&self.paths.sysupdate, &[version.as_str()]).await
    }
}

fn save(path: &Path, state: &OsUpdateState) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    crate::appliance::prepare::write_atomic(path, &bytes, 0o600)
}

#[cfg(unix)]
fn set_private(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn set_private(_dir: &Path) -> Result<(), String> {
    Ok(())
}

async fn run(argv: &[String], extra: &[&str]) -> Result<(), String> {
    let (program, args) = argv.split_first().ok_or_else(|| "no command".to_string())?;
    let output = tokio::process::Command::new(program)
        .args(args)
        .args(extra)
        .output()
        .await
        .map_err(|e| format!("{program}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}: {}", response.status()));
    }
    Ok(response
        .bytes()
        .await
        .map_err(|e| format!("{url}: {e}"))?
        .to_vec())
}

/// Stream `url` into `path`, returning its SHA-256.
async fn download(client: &reqwest::Client, url: &str, path: &Path) -> Result<String, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url}: {}", response.status()));
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("{url}: {e}"))?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    file.sync_all()
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh Ed25519 key: a function that signs with it (raw signature),
    /// and its public half.
    fn keypair() -> (impl Fn(&[u8]) -> Vec<u8> + Clone, PublicKey) {
        use base64::Engine as _;
        let (pkcs8, public) = crate::upgrade::signing::generate_keypair().unwrap();
        let sign = move |bytes: &[u8]| {
            base64::engine::general_purpose::STANDARD
                .decode(crate::upgrade::signing::sign(&pkcs8, bytes).unwrap())
                .unwrap()
        };
        (sign, public)
    }

    fn entry(name: &str) -> SumsEntry {
        SumsEntry {
            sha256: "a".repeat(64),
            name: name.into(),
        }
    }

    #[test]
    fn the_image_version_comes_from_os_release() {
        assert_eq!(
            image_version("ID=ubuntu\nIMAGE_VERSION=\"2026.41.0\"\n").as_deref(),
            Some("2026.41.0")
        );
        assert_eq!(image_version("ID=ubuntu\n"), None);
    }

    #[test]
    fn sysupdate_gets_the_uki_and_both_usr_images_and_nothing_else() {
        let entries = [
            entry("reliaburger-os_2026.42.0.raw.zst"),
            entry("reliaburger-os_2026.42.0.efi"),
            entry("reliaburger-os_2026.42.0.usr.0a1b-2c.raw.zst"),
            entry("reliaburger-os_2026.42.0.usr-verity.3d4e.raw.zst"),
            entry("reliaburger-os_2026.41.0.efi"),
            entry("reliaburger-os-installer_2026.42.0.efi"),
            entry("boot.ipxe"),
        ];
        assert_eq!(
            wanted(&entries, "2026.42.0").unwrap(),
            [
                "reliaburger-os_2026.42.0.efi",
                "reliaburger-os_2026.42.0.usr.0a1b-2c.raw.zst",
                "reliaburger-os_2026.42.0.usr-verity.3d4e.raw.zst"
            ]
        );
        assert!(wanted(&entries[..2], "2026.42.0").is_err());
    }

    #[test]
    fn the_release_sits_beside_the_channel() {
        assert_eq!(
            asset_url(
                "https://github.com/r/r/releases/download/os-channel/os-channel.json",
                "2026.42.0",
                "x86_64",
                "f"
            )
            .unwrap(),
            "https://github.com/r/r/releases/download/os-2026.42.0-x86_64/f"
        );
        assert!(asset_url("https://example/channel.json", "v", "a", "f").is_err());
    }

    #[test]
    fn a_reboot_that_lands_elsewhere_is_a_fallback() {
        let rebooting = OsUpdateState::Rebooting {
            target: "2026.42.0".into(),
        };
        assert_eq!(
            after_boot(rebooting.clone(), Some("2026.42.0")),
            OsUpdateState::Idle
        );
        assert!(matches!(
            after_boot(rebooting, Some("2026.41.0")),
            OsUpdateState::Failed { reason, .. } if reason.starts_with("booted 2026.41.0 instead")
        ));
        assert!(matches!(
            after_boot(OsUpdateState::Staging { target: "x".into() }, Some("y")),
            OsUpdateState::Failed { .. }
        ));
        assert_eq!(
            after_boot(OsUpdateState::Idle, Some("y")),
            OsUpdateState::Idle
        );
    }

    /// Stage a signed release from a local server, through to sysupdate and
    /// the reboot (both stand-ins that record what they were given).
    #[tokio::test]
    async fn staging_checks_the_release_then_runs_sysupdate_and_reboots() {
        let dir = tempfile::tempdir().unwrap();
        let (signing, public) = keypair();
        let version = "2026.42.0";
        let files = [
            (format!("reliaburger-os_{version}.efi"), b"uki".to_vec()),
            (
                format!("reliaburger-os_{version}.usr.0a1b.raw.zst"),
                b"usr".to_vec(),
            ),
            (
                format!("reliaburger-os_{version}.usr-verity.2c3d.raw.zst"),
                b"verity".to_vec(),
            ),
        ];
        let sums: String = files
            .iter()
            .map(|(name, bytes)| format!("{}  {name}\n", hex::encode(Sha256::digest(bytes))))
            .collect();
        let sums_name = format!("reliaburger-os_{version}.SHA256SUMS");
        let mut served: std::collections::HashMap<String, Vec<u8>> =
            files.iter().cloned().collect();
        served.insert(format!("{sums_name}.sig"), signing(sums.as_bytes()));
        served.insert(sums_name, sums.into_bytes());
        let served = Arc::new(served);
        let app = axum::Router::new().route(
            "/releases/download/os-2026.42.0-x86_64/{file}",
            axum::routing::get({
                let served = Arc::clone(&served);
                move |axum::extract::Path(file): axum::extract::Path<String>| {
                    let served = Arc::clone(&served);
                    async move {
                        served
                            .get(&file)
                            .cloned()
                            .ok_or(axum::http::StatusCode::NOT_FOUND)
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });

        let log = dir.path().join("commands.log");
        let record = |name: &str| {
            vec![
                "sh".to_string(),
                "-c".to_string(),
                format!("echo {name} \"$@\" >> {}", log.display()),
                "sh".to_string(),
            ]
        };
        let paths = SlotPaths {
            state_file: dir.path().join("os-update.json"),
            staging: dir.path().join("staging"),
            sysupdate: record("sysupdate"),
            reboot: record("reboot"),
        };
        let slot = OsSlot::new(paths, "x86_64", vec![public], Some("2026.41.0".into()));
        let directive = OsDirective {
            rollout_id: "r1".into(),
            version: version.into(),
            channel_url: format!("http://{address}/releases/download/os-channel/os-channel.json"),
        };
        assert_eq!(slot.begin(&directive).await.unwrap(), Begun::Started);
        assert_eq!(slot.begin(&directive).await.unwrap(), Begun::InProgress);
        let mut other = directive.clone();
        other.version = "2026.43.0".into();
        assert!(slot.begin(&other).await.is_err(), "one update at a time");

        for _ in 0..100 {
            if matches!(
                slot.state().await,
                OsUpdateState::Rebooting { .. } | OsUpdateState::Failed { .. }
            ) && std::fs::read_to_string(&log).is_ok_and(|l| l.contains("reboot"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            slot.state().await,
            OsUpdateState::Rebooting {
                target: version.into()
            }
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            format!("sysupdate {version}\nreboot\n")
        );
        for (name, bytes) in &files {
            assert_eq!(
                &std::fs::read(dir.path().join("staging").join(name)).unwrap(),
                bytes
            );
        }

        // After the reboot, on the old version: a fallback.
        let paths = SlotPaths {
            state_file: dir.path().join("os-update.json"),
            ..SlotPaths::default()
        };
        let again = OsSlot::new(paths, "x86_64", Vec::new(), Some("2026.41.0".into()));
        assert!(matches!(again.state().await, OsUpdateState::Failed { .. }));
    }

    #[tokio::test]
    async fn a_release_signed_by_another_key_is_never_staged() {
        let dir = tempfile::tempdir().unwrap();
        let (signing, _) = keypair();
        let (_, other_public) = keypair();
        let sums = b"0000000000000000000000000000000000000000000000000000000000000000  reliaburger-os_1.efi\n".to_vec();
        let signature = signing(&sums);
        let app = axum::Router::new()
            .route(
                "/d/os-1-x86_64/reliaburger-os_1.SHA256SUMS",
                axum::routing::get(move || async move { sums }),
            )
            .route(
                "/d/os-1-x86_64/reliaburger-os_1.SHA256SUMS.sig",
                axum::routing::get(move || async move { signature }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let paths = SlotPaths {
            state_file: dir.path().join("os-update.json"),
            staging: dir.path().join("staging"),
            sysupdate: vec!["false".into()],
            reboot: vec!["false".into()],
        };
        let slot = OsSlot::new(paths, "x86_64", vec![other_public], Some("0".into()));
        let directive = OsDirective {
            rollout_id: "r".into(),
            version: "1".into(),
            channel_url: format!("http://{address}/d/os-channel/os-channel.json"),
        };
        slot.begin(&directive).await.unwrap();
        for _ in 0..100 {
            if matches!(slot.state().await, OsUpdateState::Failed { .. }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            matches!(slot.state().await, OsUpdateState::Failed { reason, .. } if reason.contains("signature")),
            "{:?}",
            slot.state().await
        );
        assert!(!dir.path().join("staging").exists());
    }
}
