//! `relish uninstall`: remove what `install.sh` and `setup --quickstart`
//! put on this machine.
//!
//! That is the CLI in `<RELIABURGER_HOME>/bin`, the `~/.local/bin/relish`
//! link if it points there, the private Lima distribution (`tools/`), the
//! downloaded guest images and binaries (`cache/`) and the managed Lima home
//! (`lima/`). The context lock (`context.lock`) goes too once no saved
//! context (`context.json`) is left for it to guard. Anything else under
//! `RELIABURGER_HOME` (node data from a server install, a saved context and
//! its lock) stays, and nothing outside it is touched apart from our own
//! link. Clusters come first: while a quickstart cluster or a
//! managed VM exists, uninstall refuses and names the command that removes it.
//!
//! Deleting the running executable is fine on Unix: the directory entry goes,
//! the process keeps its open inode until it exits.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Why uninstall stopped.
#[derive(Debug, thiserror::Error)]
pub enum UninstallError {
    /// A quickstart cluster still has saved state; destroy it first.
    #[error(
        "quickstart clusters still exist ({}); remove each with `relish local destroy --name NAME --yes` first",
        .names.join(", ")
    )]
    ClustersExist { names: Vec<String> },
    /// A VM still exists in the managed Lima home.
    #[error(
        "managed Lima VMs still exist in {} ({}); remove their clusters with `relish local destroy --name NAME --yes` first",
        .home.display(),
        .names.join(", ")
    )]
    VirtualMachinesExist { home: PathBuf, names: Vec<String> },
    /// No `--yes` and no terminal to ask on.
    #[error("uninstall removes the CLI and its downloads; pass --yes to run it without a terminal")]
    ConfirmationRequired,
    /// The answer at the prompt wasn't yes.
    #[error("uninstall cancelled; nothing was removed")]
    Cancelled,
    /// `RELIABURGER_HOME` is invalid or there is no home directory.
    #[error("cannot locate the Reliaburger home directory: {0}")]
    Home(String),
    /// Listing a directory or reading the answer failed.
    #[error("failed to read {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Deleting a planned path failed; later paths were not attempted.
    #[error("failed to remove {}: {source}", .path.display())]
    Remove {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// What uninstall will remove and what it leaves in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Paths to delete, in order. Directories go with their contents.
    pub remove: Vec<PathBuf>,
    /// Entries under the home directory that aren't ours to remove.
    pub keep: Vec<PathBuf>,
    /// The home directory itself, removed at the end only if it's empty.
    pub root: PathBuf,
}

impl Plan {
    /// Whether executing the plan deletes `path`, directly or through a link.
    /// Only meaningful before [`execute`]: afterwards there's nothing left to
    /// resolve the paths against.
    pub fn removes(&self, path: &Path) -> bool {
        self.remove.iter().any(|removed| same_file(removed, path))
    }
}

/// Files and directories under `RELIABURGER_HOME` that only the installer
/// and the quickstart create.
const OWNED: &[&str] = &["tools", "cache", "lima", "setup.lock"];

/// The saved managed-cluster context, kept by uninstall.
const CONTEXT: &str = "context.json";
/// The lock guarding [`CONTEXT`], removed only when no context is left.
const CONTEXT_LOCK: &str = "context.lock";

/// Work out what to remove from `root`, plus the link in `local_bin` if it
/// points at our binary. Refuses while clusters or managed VMs exist.
pub fn plan(root: &Path, local_bin: Option<&Path>) -> Result<Plan, UninstallError> {
    let clusters = live_cluster_names(&root.join("clusters"))?;
    if !clusters.is_empty() {
        return Err(UninstallError::ClustersExist { names: clusters });
    }
    // Lima keeps `_config` and `_networks` beside one directory per VM.
    let lima = root.join("lima");
    let machines: Vec<String> = directory_names(&lima)?
        .into_iter()
        .filter(|name| !name.starts_with('_'))
        .collect();
    if !machines.is_empty() {
        return Err(UninstallError::VirtualMachinesExist {
            home: lima,
            names: machines,
        });
    }

    let binary = root.join("bin").join("relish");
    let mut remove = Vec::new();
    if let Some(link) = local_bin.map(|directory| directory.join("relish"))
        && std::fs::read_link(&link).is_ok_and(|target| target == binary)
    {
        remove.push(link);
    }
    if exists(&binary) {
        remove.push(binary);
    }
    for name in OWNED {
        let path = root.join(name);
        if exists(&path) {
            remove.push(path);
        }
    }
    let clusters = root.join("clusters");
    if exists(&clusters) {
        remove.push(clusters);
    }
    // The lock only guards the context file; with no context it's litter.
    let context_lock = root.join(CONTEXT_LOCK);
    if exists(&context_lock) && !exists(&root.join(CONTEXT)) {
        remove.push(context_lock);
    }

    let mut keep = Vec::new();
    for entry in entries(root)? {
        let owned = remove.contains(&entry) || entry == root.join("bin");
        if !owned {
            keep.push(entry);
        }
    }
    for entry in entries(&root.join("bin"))? {
        if !remove.contains(&entry) {
            keep.push(entry);
        }
    }
    keep.sort();
    Ok(Plan {
        remove,
        keep,
        root: root.to_path_buf(),
    })
}

/// Remove everything in `plan`, then the `bin` and home directories if
/// nothing else is left in them.
pub fn execute(plan: &Plan) -> Result<(), UninstallError> {
    for path in &plan.remove {
        if *path == plan.root.join(CONTEXT_LOCK) {
            // Unlink under the lock, re-checking that no context appeared,
            // so a concurrent `relish setup` can't be left on a stale file.
            crate::relish::local_context::remove_unused_lock(&plan.root.join(CONTEXT)).map_err(
                |error| UninstallError::Remove {
                    path: path.clone(),
                    source: std::io::Error::other(error.to_string()),
                },
            )?;
            continue;
        }
        let removal = match std::fs::symlink_metadata(path) {
            // `remove_dir_all` never follows a link, so a `tools` symlink
            // loses the link, not its target.
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        removal.map_err(|source| UninstallError::Remove {
            path: path.clone(),
            source,
        })?;
    }
    // `remove_dir` only succeeds on an empty directory, which is the point.
    let _ = std::fs::remove_dir(plan.root.join("bin"));
    let _ = std::fs::remove_dir(&plan.root);
    Ok(())
}

/// `relish uninstall`: plan, show, confirm (`--yes` skips the question), remove.
pub fn run(yes: bool) -> Result<(), UninstallError> {
    let root = crate::relish::local_context::root_directory()
        .map_err(|error| UninstallError::Home(error.to_string()))?;
    // An isolated RELIABURGER_HOME never gets a ~/.local/bin link, and the
    // plan only removes a link that points at this home's binary anyway.
    let local_bin = dirs::home_dir().map(|home| home.join(".local").join("bin"));
    let plan = plan(&root, local_bin.as_deref())?;
    if plan.remove.is_empty() {
        println!("nothing to uninstall in {}", root.display());
        return Ok(());
    }
    println!("relish uninstall will remove:");
    for path in &plan.remove {
        println!("  {}", path.display());
    }
    if !plan.keep.is_empty() {
        println!("and keep (not created by the installer):");
        for path in &plan.keep {
            println!("  {}", path.display());
        }
    }
    if !yes {
        confirm()?;
    }
    // Ask before removing: once our own binary is gone, Linux reports
    // current_exe() as "/…/relish (deleted)", which matches nothing.
    let running = std::env::current_exe().ok();
    let ours = running
        .as_deref()
        .is_some_and(|running| plan.removes(running));
    execute(&plan)?;
    println!("removed");

    if let Some(running) = running.filter(|_| !ours) {
        println!(
            "note: this relish ({}) wasn't installed by install.sh; remove it the way you installed it",
            running.display()
        );
    }
    let store = root.join("bin");
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|entry| entry == store));
    if on_path {
        println!(
            "note: remove the line that adds {} to PATH from your shell's rc file",
            store.display()
        );
    }
    Ok(())
}

fn confirm() -> Result<(), UninstallError> {
    if !std::io::stdin().is_terminal() {
        return Err(UninstallError::ConfirmationRequired);
    }
    eprint!("Remove these? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|source| UninstallError::Read {
            path: PathBuf::from("/dev/stdin"),
            source,
        })?;
    match answer.trim() {
        "y" | "Y" | "yes" | "YES" | "Yes" => Ok(()),
        _ => Err(UninstallError::Cancelled),
    }
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Every entry directly inside `directory`; empty if it doesn't exist.
fn entries(directory: &Path) -> Result<Vec<PathBuf>, UninstallError> {
    match std::fs::read_dir(directory) {
        Ok(read) => read
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<_, _>>()
            .map_err(|source| UninstallError::Read {
                path: directory.to_path_buf(),
                source,
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(UninstallError::Read {
            path: directory.to_path_buf(),
            source,
        }),
    }
}

/// Names of the subdirectories of `directory`, sorted.
/// Cluster directories holding anything but the operation lock. `relish local
/// destroy` keeps the lock's inode so a concurrent setup can't lock a
/// different file, so a destroyed cluster leaves exactly that behind.
fn live_cluster_names(directory: &Path) -> Result<Vec<String>, UninstallError> {
    let mut live = Vec::new();
    for name in directory_names(directory)? {
        let contents = entries(&directory.join(&name))?;
        if contents
            .iter()
            .any(|path| path.file_name().is_none_or(|file| file != "operation.lock"))
        {
            live.push(name);
        }
    }
    Ok(live)
}

fn directory_names(directory: &Path) -> Result<Vec<String>, UninstallError> {
    let mut names: Vec<String> = entries(directory)?
        .into_iter()
        .filter(|path| path.is_dir())
        .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
        .collect();
    names.sort();
    Ok(names)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A home laid out the way install.sh and a finished quickstart leave it.
    fn installed_home() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".reliaburger");
        let local_bin = temp.path().join(".local/bin");
        for directory in [
            "bin",
            "tools/lima-2.1.0/bin",
            "cache",
            "lima/_config",
            "clusters",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(root.join("bin/relish"), "binary").unwrap();
        std::fs::write(root.join("tools/lima-2.1.0/bin/limactl"), "lima").unwrap();
        std::fs::write(root.join("cache/ubuntu.img"), "image").unwrap();
        std::fs::write(root.join("setup.lock"), "").unwrap();
        std::fs::create_dir_all(&local_bin).unwrap();
        std::os::unix::fs::symlink(root.join("bin/relish"), local_bin.join("relish")).unwrap();
        (temp, root, local_bin)
    }

    #[test]
    fn removes_the_cli_link_tools_cache_and_lima_home() {
        let (_temp, root, local_bin) = installed_home();
        let plan = plan(&root, Some(&local_bin)).unwrap();
        assert_eq!(
            plan.remove,
            vec![
                local_bin.join("relish"),
                root.join("bin/relish"),
                root.join("tools"),
                root.join("cache"),
                root.join("lima"),
                root.join("setup.lock"),
                root.join("clusters"),
            ]
        );
        assert!(plan.keep.is_empty());

        execute(&plan).unwrap();
        assert!(!root.exists(), "an emptied home is removed too");
        assert!(local_bin.is_dir(), "~/.local/bin itself is the user's");
        assert!(std::fs::symlink_metadata(local_bin.join("relish")).is_err());
    }

    #[test]
    fn recognises_its_own_binary_only_before_removing_it() {
        let (temp, root, local_bin) = installed_home();
        let plan = plan(&root, Some(&local_bin)).unwrap();
        let elsewhere = temp.path().join("cargo/bin/relish");
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        std::fs::write(&elsewhere, "binary").unwrap();

        assert!(plan.removes(&root.join("bin/relish")));
        // Running through ~/.local/bin/relish resolves to the same binary.
        assert!(plan.removes(&local_bin.join("relish")));
        assert!(!plan.removes(&elsewhere));

        // Afterwards the binary is gone, and Linux names it "(deleted)":
        // that's why `run` asks before it executes the plan.
        execute(&plan).unwrap();
        let deleted = PathBuf::from(format!("{} (deleted)", root.join("bin/relish").display()));
        assert!(!plan.removes(&deleted));
    }

    #[test]
    fn removes_the_context_lock_once_no_context_is_left() {
        let (_temp, root, local_bin) = installed_home();
        // What `relish local destroy` leaves: the lock, not the context.
        std::fs::write(root.join("context.lock"), "").unwrap();
        let plan = plan(&root, Some(&local_bin)).unwrap();
        assert!(plan.remove.contains(&root.join("context.lock")));
        assert!(plan.keep.is_empty());
        execute(&plan).unwrap();
        assert!(!root.exists(), "the lock no longer keeps the home alive");
    }

    #[test]
    fn a_context_lock_held_by_another_process_stops_uninstall() {
        let (_temp, root, local_bin) = installed_home();
        std::fs::write(root.join("context.lock"), "").unwrap();
        let plan = plan(&root, Some(&local_bin)).unwrap();
        let held = std::fs::File::open(root.join("context.lock")).unwrap();
        held.try_lock().unwrap();
        let error = execute(&plan).unwrap_err();
        assert!(
            matches!(error, UninstallError::Remove { ref path, .. } if path.ends_with("context.lock"))
        );
        assert!(root.join("context.lock").exists());
    }

    #[test]
    fn keeps_everything_the_installer_did_not_create() {
        let (_temp, root, local_bin) = installed_home();
        std::fs::write(root.join("bin/bun"), "node agent").unwrap();
        std::fs::write(root.join("context.json"), "{}").unwrap();
        std::fs::write(root.join("context.lock"), "").unwrap();
        std::fs::create_dir_all(root.join("images")).unwrap();

        let plan = plan(&root, Some(&local_bin)).unwrap();
        assert_eq!(
            plan.keep,
            vec![
                root.join("bin/bun"),
                root.join("context.json"),
                root.join("context.lock"),
                root.join("images")
            ]
        );
        execute(&plan).unwrap();
        assert!(root.join("bin/bun").is_file());
        assert!(root.join("context.json").is_file());
        assert!(root.join("context.lock").is_file());
        assert!(root.join("images").is_dir());
        assert!(!root.join("bin/relish").exists());
        assert!(!root.join("tools").exists());
    }

    #[test]
    fn refuses_while_a_quickstart_cluster_exists() {
        let (_temp, root, local_bin) = installed_home();
        std::fs::create_dir_all(root.join("clusters/laptop")).unwrap();
        std::fs::write(root.join("clusters/laptop/operation.lock"), "").unwrap();
        std::fs::write(root.join("clusters/laptop/state.json"), "{}").unwrap();
        let error = plan(&root, Some(&local_bin)).unwrap_err();
        assert!(
            matches!(error, UninstallError::ClustersExist { ref names } if names == &["laptop"])
        );
        assert!(error.to_string().contains("relish local destroy"));
        assert!(root.join("bin/relish").exists());
    }

    #[test]
    fn proceeds_after_destroy_leaves_only_the_operation_lock() {
        let (_temp, root, local_bin) = installed_home();
        std::fs::create_dir_all(root.join("clusters/laptop")).unwrap();
        std::fs::write(root.join("clusters/laptop/operation.lock"), "").unwrap();
        let plan = plan(&root, Some(&local_bin)).unwrap();
        assert!(plan.remove.contains(&root.join("clusters")));
    }

    #[test]
    fn refuses_while_a_managed_vm_exists() {
        let (_temp, root, local_bin) = installed_home();
        std::fs::create_dir_all(root.join("lima/rb-1a2b-node-1")).unwrap();
        let error = plan(&root, Some(&local_bin)).unwrap_err();
        assert!(
            matches!(error, UninstallError::VirtualMachinesExist { ref names, .. } if names == &["rb-1a2b-node-1"])
        );
    }

    #[test]
    fn leaves_a_local_bin_relish_that_is_not_our_link() {
        let (_temp, root, local_bin) = installed_home();
        std::fs::remove_file(local_bin.join("relish")).unwrap();
        std::fs::write(local_bin.join("relish"), "someone else's").unwrap();
        let first = plan(&root, Some(&local_bin)).unwrap();
        assert!(!first.remove.contains(&local_bin.join("relish")));

        std::fs::remove_file(local_bin.join("relish")).unwrap();
        std::os::unix::fs::symlink("/opt/relish/bin/relish", local_bin.join("relish")).unwrap();
        let second = plan(&root, Some(&local_bin)).unwrap();
        assert!(!second.remove.contains(&local_bin.join("relish")));

        execute(&second).unwrap();
        assert!(std::fs::symlink_metadata(local_bin.join("relish")).is_ok());
    }

    #[test]
    fn a_symlinked_tools_directory_loses_the_link_not_its_target() {
        let (temp, root, local_bin) = installed_home();
        let elsewhere = temp.path().join("precious");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep.txt"), "mine").unwrap();
        std::fs::remove_dir_all(root.join("cache")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join("cache")).unwrap();

        execute(&plan(&root, Some(&local_bin)).unwrap()).unwrap();
        assert!(elsewhere.join("keep.txt").is_file());
        assert!(std::fs::symlink_metadata(root.join("cache")).is_err());
    }

    #[test]
    fn an_empty_home_has_nothing_to_remove() {
        let temp = tempfile::tempdir().unwrap();
        let plan = plan(&temp.path().join("missing"), None).unwrap();
        assert!(plan.remove.is_empty());
        assert!(plan.keep.is_empty());
    }
}
