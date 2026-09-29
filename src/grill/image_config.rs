//! How an OCI image wants to be run, and how an app's own settings override it.
//!
//! Every image carries a small JSON config next to its layers: the
//! `Entrypoint` and `Cmd` to run, the `Env` it expects, the `WorkingDir` and
//! the `User`. Almost every public image relies on at least one of them.
//! Runtimes that unpack images themselves (runc) resolve a workload's
//! process against that config with the Kubernetes rules:
//!
//! - app `command` replaces `Entrypoint` (and drops `Cmd`);
//! - app `args` replaces `Cmd`;
//! - image `Env` sits under the app's env (the app wins on a clash);
//! - image `WorkingDir` applies unless the app sets one;
//! - image `User` applies unless the app sets `run_as_user`/`run_as_group`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::oci::{OciProcess, OciUser};

/// The `PATH` Docker and containerd give a container whose image sets none.
const DEFAULT_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Largest `/etc/passwd` or `/etc/group` we'll read from an image.
const MAX_ACCOUNT_FILE_BYTES: u64 = 1024 * 1024;

/// The process-related part of an OCI image config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageConfig {
    /// `config.Entrypoint`.
    pub entrypoint: Vec<String>,
    /// `config.Cmd`.
    pub cmd: Vec<String>,
    /// `config.Env`, as `KEY=value` strings.
    pub env: Vec<String>,
    /// `config.WorkingDir`, when set and non-empty.
    pub working_dir: Option<String>,
    /// `config.User` (`uid`, `uid:gid`, `name`, `name:group`, ...), when set.
    pub user: Option<String>,
}

/// Why an image config couldn't be read or applied.
#[derive(Debug, thiserror::Error)]
pub enum ImageConfigError {
    #[error("image config is not valid JSON: {0}")]
    Parse(String),

    #[error("the image sets no entrypoint or command, and the app sets no command")]
    NothingToRun,

    #[error("image user {user:?} is not in the image's /etc/passwd")]
    UnknownUser { user: String },

    #[error("image group {group:?} is not in the image's /etc/group")]
    UnknownGroup { group: String },

    #[error("failed to read {path:?} from the image: {reason}")]
    AccountFile { path: PathBuf, reason: String },
}

/// The JSON shape we read. Everything is optional: `Entrypoint` and `Cmd`
/// are `null` in plenty of real images.
#[derive(Deserialize)]
struct RawImageConfig {
    #[serde(default)]
    config: Option<RawProcessConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawProcessConfig {
    #[serde(default)]
    entrypoint: Option<Vec<String>>,
    #[serde(default)]
    cmd: Option<Vec<String>>,
    #[serde(default)]
    env: Option<Vec<String>>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

impl ImageConfig {
    /// Parse an OCI (or Docker v2) image config blob.
    ///
    /// The caller must pass the digest-verified bytes: whatever this
    /// returns decides what runs, as which user.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ImageConfigError> {
        let raw: RawImageConfig =
            serde_json::from_slice(bytes).map_err(|e| ImageConfigError::Parse(e.to_string()))?;
        let Some(config) = raw.config else {
            return Ok(Self::default());
        };
        Ok(Self {
            entrypoint: config.entrypoint.unwrap_or_default(),
            cmd: config.cmd.unwrap_or_default(),
            env: config.env.unwrap_or_default(),
            working_dir: config.working_dir.filter(|dir| !dir.is_empty()),
            user: config.user.filter(|user| !user.is_empty()),
        })
    }
}

/// Resolve a provisional process against its image config.
///
/// Consumes `process.overrides`. When it's `None` the process is already
/// final and nothing changes. `rootfs` is the unpacked image, read for
/// `/etc/passwd` and `/etc/group` when the image names its user.
pub fn resolve_process(
    process: &mut OciProcess,
    image: &ImageConfig,
    rootfs: &Path,
) -> Result<(), ImageConfigError> {
    let Some(overrides) = process.overrides.take() else {
        return Ok(());
    };

    process.args = if overrides.command.is_empty() {
        let cmd = if overrides.args.is_empty() {
            &image.cmd
        } else {
            &overrides.args
        };
        image.entrypoint.iter().chain(cmd).cloned().collect()
    } else {
        overrides
            .command
            .iter()
            .chain(&overrides.args)
            .cloned()
            .collect()
    };
    if process.args.is_empty() {
        return Err(ImageConfigError::NothingToRun);
    }

    process.cwd = overrides
        .working_dir
        .clone()
        .or_else(|| image.working_dir.clone())
        .unwrap_or_else(|| "/".to_string());

    let accounts = Accounts::new(rootfs);
    let (uid, gid, home) = resolve_user(&accounts, image.user.as_deref(), &overrides)?;
    process.user = OciUser { uid, gid };

    process.env = merge_env(&image.env, &process.env, home.as_deref());
    Ok(())
}

/// Image env first, minus anything the app sets, then the app's env.
/// A container always gets a `PATH` and a `HOME`, like under Docker.
fn merge_env(image: &[String], app: &[String], home: Option<&str>) -> Vec<String> {
    fn key(entry: &str) -> &str {
        entry.split_once('=').map_or(entry, |(key, _)| key)
    }
    let mut env: Vec<String> = image
        .iter()
        .filter(|entry| !app.iter().any(|own| key(own) == key(entry)))
        .chain(app)
        .cloned()
        .collect();
    if !env.iter().any(|entry| key(entry) == "PATH") {
        env.push(DEFAULT_PATH.to_string());
    }
    if !env.iter().any(|entry| key(entry) == "HOME") {
        env.push(format!("HOME={}", home.unwrap_or("/")));
    }
    env
}

/// Work out the uid, gid and home directory to run as.
///
/// Docker's rules for the image `User`: a name is looked up in the image's
/// `/etc/passwd`; a numeric uid without a group takes that user's primary
/// group when the image knows it, else 0. The app's `run_as_user` and
/// `run_as_group` then replace whichever half they name.
fn resolve_user(
    accounts: &Accounts<'_>,
    image_user: Option<&str>,
    overrides: &super::oci::ProcessOverrides,
) -> Result<(u32, u32, Option<String>), ImageConfigError> {
    let (user_part, group_part) = match image_user {
        Some(spec) => match spec.split_once(':') {
            Some((user, group)) => (Some(user), Some(group)),
            None => (Some(spec), None),
        },
        None => (None, None),
    };

    // A numeric uid takes its passwd entry's group and home when there is one.
    let by_id = |uid: u32| -> Result<(u32, u32, Option<String>), ImageConfigError> {
        Ok(match accounts.user_by_id(uid)? {
            Some(entry) => (uid, entry.gid, Some(entry.home)),
            None => (uid, 0, None),
        })
    };

    let (mut uid, mut gid, mut home) = match user_part {
        None => by_id(0)?,
        Some(user) => match user.parse::<u32>() {
            Ok(uid) => by_id(uid)?,
            Err(_) => {
                let entry =
                    accounts
                        .user_by_name(user)?
                        .ok_or_else(|| ImageConfigError::UnknownUser {
                            user: user.to_string(),
                        })?;
                (entry.uid, entry.gid, Some(entry.home))
            }
        },
    };

    if let Some(group) = group_part {
        gid = match group.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => accounts
                .group_id(group)?
                .ok_or_else(|| ImageConfigError::UnknownGroup {
                    group: group.to_string(),
                })?,
        };
    }

    if let Some(run_as_user) = overrides.user {
        (uid, gid, home) = by_id(run_as_user)?;
    }
    if let Some(run_as_group) = overrides.group {
        gid = run_as_group;
    }
    Ok((uid, gid, home))
}

/// One `/etc/passwd` line.
struct PasswdEntry {
    uid: u32,
    gid: u32,
    home: String,
}

/// Read-only access to an unpacked image's account files.
struct Accounts<'a> {
    rootfs: &'a Path,
}

impl<'a> Accounts<'a> {
    fn new(rootfs: &'a Path) -> Self {
        Self { rootfs }
    }

    fn user_by_name(&self, name: &str) -> Result<Option<PasswdEntry>, ImageConfigError> {
        self.find_user(|fields| fields[0] == name)
    }

    fn user_by_id(&self, uid: u32) -> Result<Option<PasswdEntry>, ImageConfigError> {
        let wanted = uid.to_string();
        self.find_user(|fields| fields[2] == wanted)
    }

    fn find_user(
        &self,
        matches: impl Fn(&[&str]) -> bool,
    ) -> Result<Option<PasswdEntry>, ImageConfigError> {
        let Some(passwd) = self.read("etc/passwd")? else {
            return Ok(None);
        };
        for line in passwd.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() < 7 || !matches(&fields) {
                continue;
            }
            if let (Ok(uid), Ok(gid)) = (fields[2].parse(), fields[3].parse()) {
                return Ok(Some(PasswdEntry {
                    uid,
                    gid,
                    home: fields[5].to_string(),
                }));
            }
        }
        Ok(None)
    }

    fn group_id(&self, name: &str) -> Result<Option<u32>, ImageConfigError> {
        let Some(group) = self.read("etc/group")? else {
            return Ok(None);
        };
        Ok(group.lines().find_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.len() >= 3 && fields[0] == name)
                .then(|| fields[2].parse().ok())
                .flatten()
        }))
    }

    /// Read a small text file from the image, following its symlinks the
    /// way the container would see them: an image controls its own links,
    /// and `/etc/passwd -> /etc/shadow` must name the image's file, not the
    /// host's.
    fn read(&self, relative: &str) -> Result<Option<String>, ImageConfigError> {
        let failure = |reason: String| ImageConfigError::AccountFile {
            path: PathBuf::from("/").join(relative),
            reason,
        };
        let resolved = match resolve_in_root(self.rootfs, Path::new(relative)) {
            Ok(resolved) => resolved,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(failure(e.to_string())),
        };
        let metadata = std::fs::symlink_metadata(&resolved).map_err(|e| failure(e.to_string()))?;
        if !metadata.is_file() || metadata.len() > MAX_ACCOUNT_FILE_BYTES {
            return Err(failure("not a small regular file".to_string()));
        }
        std::fs::read_to_string(&resolved)
            .map(Some)
            .map_err(|e| failure(e.to_string()))
    }
}

/// Resolve `relative` inside `root` as if `root` were `/`.
///
/// Absolute symlink targets restart from `root`, and `..` stops at it, so
/// the result never names a file outside the image.
fn resolve_in_root(root: &Path, relative: &Path) -> std::io::Result<PathBuf> {
    use std::path::Component;

    const MAX_SYMLINKS: usize = 40;
    // Components still to walk, last first, so `pop` yields the next one.
    let mut pending: Vec<std::ffi::OsString> = relative
        .components()
        .rev()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_os_string()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect();
    let mut resolved = PathBuf::new();
    let mut symlinks = 0;
    while let Some(part) = pending.pop() {
        if part == ".." {
            resolved.pop();
            continue;
        }
        let candidate = root.join(&resolved).join(&part);
        if !std::fs::symlink_metadata(&candidate)?.is_symlink() {
            resolved.push(part);
            continue;
        }
        symlinks += 1;
        if symlinks > MAX_SYMLINKS {
            return Err(std::io::Error::other("too many symlinks"));
        }
        let target = std::fs::read_link(&candidate)?;
        if target.is_absolute() {
            resolved = PathBuf::new();
        }
        for component in target.components().rev() {
            match component {
                Component::Normal(next) => pending.push(next.to_os_string()),
                Component::ParentDir => pending.push("..".into()),
                _ => {}
            }
        }
    }
    Ok(root.join(resolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::oci::ProcessOverrides;

    fn image(json: &str) -> ImageConfig {
        ImageConfig::from_json(json.as_bytes()).unwrap()
    }

    fn podinfo_like() -> ImageConfig {
        image(
            r#"{"architecture":"arm64","os":"linux","config":{
                "User":"app","WorkingDir":"/home/app",
                "Env":["PATH=/usr/local/bin:/usr/bin:/bin","COLOUR=blue"],
                "Entrypoint":["./podinfo"],"Cmd":["--port=9898"]}}"#,
        )
    }

    /// An image rootfs with Alpine-style account files.
    fn rootfs() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::fs::write(
            dir.path().join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/sh\n\
             nobody:x:65534:65534:nobody:/:/sbin/nologin\n\
             app:x:100:101:Linux User,,,:/home/app:/sbin/nologin\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("etc/group"),
            "root:x:0:root\napp:x:101:app\nstaff:x:50:\n",
        )
        .unwrap();
        dir
    }

    fn provisional(app_env: &[&str], overrides: ProcessOverrides) -> OciProcess {
        OciProcess {
            args: Vec::new(),
            env: app_env.iter().map(|entry| entry.to_string()).collect(),
            cwd: "/".to_string(),
            user: OciUser {
                uid: 65534,
                gid: 65534,
            },
            capabilities: None,
            overrides: Some(overrides),
        }
    }

    fn resolved(
        image: &ImageConfig,
        app_env: &[&str],
        overrides: ProcessOverrides,
    ) -> Result<OciProcess, ImageConfigError> {
        let rootfs = rootfs();
        let mut process = provisional(app_env, overrides);
        resolve_process(&mut process, image, rootfs.path())?;
        Ok(process)
    }

    #[test]
    fn parses_the_process_fields_and_ignores_the_rest() {
        let config = podinfo_like();
        assert_eq!(config.entrypoint, ["./podinfo"]);
        assert_eq!(config.cmd, ["--port=9898"]);
        assert_eq!(config.working_dir.as_deref(), Some("/home/app"));
        assert_eq!(config.user.as_deref(), Some("app"));
        assert_eq!(config.env.len(), 2);
    }

    #[test]
    fn null_and_empty_fields_parse_as_absent() {
        let config = image(
            r#"{"config":{"Entrypoint":null,"Cmd":null,"Env":null,"WorkingDir":"","User":""}}"#,
        );
        assert_eq!(config, ImageConfig::default());
        assert_eq!(image(r#"{"os":"linux"}"#), ImageConfig::default());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(matches!(
            ImageConfig::from_json(b"not json"),
            Err(ImageConfigError::Parse(_))
        ));
    }

    #[test]
    fn no_command_runs_the_image_entrypoint_and_cmd() {
        let process = resolved(&podinfo_like(), &[], ProcessOverrides::default()).unwrap();
        assert_eq!(process.args, ["./podinfo", "--port=9898"]);
    }

    #[test]
    fn app_args_replace_cmd_but_keep_the_entrypoint() {
        let overrides = ProcessOverrides {
            args: vec!["--port=8080".into(), "--level=debug".into()],
            ..ProcessOverrides::default()
        };
        let process = resolved(&podinfo_like(), &[], overrides).unwrap();
        assert_eq!(process.args, ["./podinfo", "--port=8080", "--level=debug"]);
    }

    #[test]
    fn app_command_replaces_the_entrypoint_and_drops_cmd() {
        let overrides = ProcessOverrides {
            command: vec!["/bin/sh".into(), "-c".into(), "env".into()],
            ..ProcessOverrides::default()
        };
        let process = resolved(&podinfo_like(), &[], overrides).unwrap();
        assert_eq!(process.args, ["/bin/sh", "-c", "env"]);
    }

    #[test]
    fn app_command_and_args_run_together() {
        let overrides = ProcessOverrides {
            command: vec!["./podinfo".into()],
            args: vec!["--port=9999".into()],
            ..ProcessOverrides::default()
        };
        let process = resolved(&podinfo_like(), &[], overrides).unwrap();
        assert_eq!(process.args, ["./podinfo", "--port=9999"]);
    }

    #[test]
    fn nothing_to_run_is_an_error() {
        let result = resolved(&ImageConfig::default(), &[], ProcessOverrides::default());
        assert!(matches!(result, Err(ImageConfigError::NothingToRun)));
    }

    #[test]
    fn image_env_sits_under_the_app_env() {
        let process = resolved(
            &podinfo_like(),
            &["COLOUR=red", "EXTRA=1"],
            ProcessOverrides::default(),
        )
        .unwrap();
        assert_eq!(
            process.env,
            [
                "PATH=/usr/local/bin:/usr/bin:/bin",
                "COLOUR=red",
                "EXTRA=1",
                "HOME=/home/app"
            ]
        );
    }

    #[test]
    fn a_container_always_gets_path_and_home() {
        let config = image(r#"{"config":{"Cmd":["sh"]}}"#);
        let process = resolved(&config, &[], ProcessOverrides::default()).unwrap();
        assert_eq!(process.env, [DEFAULT_PATH, "HOME=/root"]);
    }

    #[test]
    fn working_dir_comes_from_the_image_unless_the_app_sets_one() {
        let process = resolved(&podinfo_like(), &[], ProcessOverrides::default()).unwrap();
        assert_eq!(process.cwd, "/home/app");

        let overrides = ProcessOverrides {
            working_dir: Some("/srv".into()),
            ..ProcessOverrides::default()
        };
        let process = resolved(&podinfo_like(), &[], overrides).unwrap();
        assert_eq!(process.cwd, "/srv");

        let config = image(r#"{"config":{"Cmd":["sh"]}}"#);
        let process = resolved(&config, &[], ProcessOverrides::default()).unwrap();
        assert_eq!(process.cwd, "/");
    }

    #[test]
    fn an_image_without_a_user_runs_as_container_root() {
        let config = image(r#"{"config":{"Cmd":["sh"]}}"#);
        let process = resolved(&config, &[], ProcessOverrides::default()).unwrap();
        assert_eq!(process.user, OciUser { uid: 0, gid: 0 });
    }

    #[test]
    fn a_named_image_user_resolves_through_the_image_passwd() {
        let process = resolved(&podinfo_like(), &[], ProcessOverrides::default()).unwrap();
        assert_eq!(process.user, OciUser { uid: 100, gid: 101 });
    }

    #[test]
    fn user_and_group_forms_follow_docker_rules() {
        let cases = [
            ("100", 100, 101),
            ("100:50", 100, 50),
            ("app:staff", 100, 50),
            ("app:7", 100, 7),
            ("4242", 4242, 0),
            ("4242:4242", 4242, 4242),
        ];
        for (user, uid, gid) in cases {
            let config = ImageConfig {
                cmd: vec!["sh".into()],
                user: Some(user.into()),
                ..ImageConfig::default()
            };
            let process = resolved(&config, &[], ProcessOverrides::default()).unwrap();
            assert_eq!(process.user, OciUser { uid, gid }, "User {user:?}");
        }
    }

    #[test]
    fn unknown_user_or_group_names_are_errors() {
        for (user, missing_user) in [("ghost", true), ("app:ghosts", false)] {
            let config = ImageConfig {
                cmd: vec!["sh".into()],
                user: Some(user.into()),
                ..ImageConfig::default()
            };
            let result = resolved(&config, &[], ProcessOverrides::default());
            if missing_user {
                assert!(matches!(result, Err(ImageConfigError::UnknownUser { .. })));
            } else {
                assert!(matches!(result, Err(ImageConfigError::UnknownGroup { .. })));
            }
        }
    }

    #[test]
    fn run_as_user_and_group_replace_the_image_user() {
        let overrides = ProcessOverrides {
            user: Some(0),
            ..ProcessOverrides::default()
        };
        let process = resolved(&podinfo_like(), &[], overrides).unwrap();
        assert_eq!(process.user, OciUser { uid: 0, gid: 0 });
        assert!(process.env.contains(&"HOME=/root".to_string()));

        let overrides = ProcessOverrides {
            user: Some(1234),
            group: Some(5678),
            ..ProcessOverrides::default()
        };
        let process = resolved(&podinfo_like(), &[], overrides).unwrap();
        assert_eq!(
            process.user,
            OciUser {
                uid: 1234,
                gid: 5678
            }
        );

        let overrides = ProcessOverrides {
            group: Some(50),
            ..ProcessOverrides::default()
        };
        let process = resolved(&podinfo_like(), &[], overrides).unwrap();
        assert_eq!(process.user, OciUser { uid: 100, gid: 50 });
    }

    #[test]
    fn a_final_process_is_left_alone() {
        let rootfs = rootfs();
        let mut process = provisional(&["A=1"], ProcessOverrides::default());
        process.overrides = None;
        process.args = vec!["/bin/true".into()];
        let before = process.clone();
        resolve_process(&mut process, &podinfo_like(), rootfs.path()).unwrap();
        assert_eq!(process, before);
    }

    #[test]
    fn resolution_consumes_the_overrides() {
        let process = resolved(&podinfo_like(), &[], ProcessOverrides::default()).unwrap();
        assert!(process.overrides.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn account_file_symlinks_resolve_inside_the_image() {
        // A host file the image's absolute symlink names. Following it on the
        // host would make `app` uid 0.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("passwd"), "app:x:0:0::/root:/bin/sh\n").unwrap();
        let rootfs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(rootfs.path().join("etc")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("passwd"),
            rootfs.path().join("etc/passwd"),
        )
        .unwrap();
        let mut process = provisional(&[], ProcessOverrides::default());
        let result = resolve_process(&mut process, &podinfo_like(), rootfs.path());
        assert!(
            matches!(result, Err(ImageConfigError::UnknownUser { .. })),
            "{result:?}"
        );

        // The same absolute link, when its target exists inside the image,
        // resolves to the image's copy; `..` can't climb out either.
        let image_root = rootfs.path();
        let inside = image_root.join(outside.path().strip_prefix("/").unwrap());
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join("passwd"), "app:x:100:101::/home/app:/bin/sh\n").unwrap();
        let mut process = provisional(&[], ProcessOverrides::default());
        resolve_process(&mut process, &podinfo_like(), image_root).unwrap();
        assert_eq!(process.user, OciUser { uid: 100, gid: 101 });

        std::fs::remove_file(image_root.join("etc/passwd")).unwrap();
        std::os::unix::fs::symlink(
            "../../../../../../../../etc/shadow-of-the-host",
            image_root.join("etc/passwd"),
        )
        .unwrap();
        let resolved = resolve_in_root(image_root, Path::new("etc/passwd"));
        assert!(resolved.is_err(), "{resolved:?}");
    }

    #[test]
    fn an_image_without_account_files_still_runs_numeric_users() {
        let rootfs = tempfile::tempdir().unwrap();
        let config = ImageConfig {
            cmd: vec!["/app".into()],
            user: Some("1000".into()),
            ..ImageConfig::default()
        };
        let mut process = provisional(&[], ProcessOverrides::default());
        resolve_process(&mut process, &config, rootfs.path()).unwrap();
        assert_eq!(process.user, OciUser { uid: 1000, gid: 0 });
    }
}
