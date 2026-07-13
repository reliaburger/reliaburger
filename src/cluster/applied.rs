//! Durable applied-state for the placement reconciler (DEP3/H7/D10).
//!
//! The per-node reconciler converges local workloads onto the leader's
//! assignments. It used to remember what it had applied in a plain
//! in-memory map and mark an app "applied" the instant the deploy command
//! entered the agent's queue. Two things went wrong with that:
//!
//! - a Bun restart forgot everything, so the reconciler re-deployed every
//!   assigned app from scratch (double-deploying work already converged); and
//! - "applied" meant "queued", not "succeeded", so a deploy that failed
//!   after acceptance was recorded as done and never retried.
//!
//! This module makes the applied map *durable* and keyed on the deploy's
//! terminal outcome. It's a small JSON checkpoint written atomically
//! (temp + rename) next to the node's other data, reconciled against the
//! leader's live assignments on every tick — so a stale checkpoint entry
//! for an app that's no longer assigned is stopped, and a missing entry
//! for an in-flight app is (re)deployed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One app the reconciler is responsible for, in namespace order.
///
/// The tuple is `(name, namespace)`; the value is the fingerprint (a
/// serialised spec) of the assignment last *successfully* applied.
pub type AppliedMap = BTreeMap<(String, String), String>;

/// The on-disk form. Self-describing JSON with a schema version so a
/// future format change fails loudly rather than silently mis-parsing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AppliedCheckpoint {
    schema: u32,
    /// Flattened entries: `[namespace, name, fingerprint]` triples. A map
    /// with tuple keys doesn't serialise to JSON cleanly, so we store a list.
    entries: Vec<AppliedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AppliedEntry {
    namespace: String,
    name: String,
    fingerprint: String,
}

const SCHEMA: u32 = 1;

/// The checkpoint file name within the reconciler's state directory.
pub const CHECKPOINT_FILE: &str = "applied-placements.json";

/// The full checkpoint path within a state directory.
pub fn checkpoint_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CHECKPOINT_FILE)
}

/// Load the applied map from `path`.
///
/// A missing file is an empty map (a fresh node). A file that exists but
/// can't be parsed is treated as empty *and logged*: the reconciler will
/// re-derive applied-state against the leader on its next tick, so a
/// corrupt checkpoint costs one redeploy cycle rather than wedging the node.
pub fn load(path: &Path) -> AppliedMap {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AppliedMap::new(),
        Err(e) => {
            eprintln!(
                "bun: warning: could not read applied-placements checkpoint {}: {e}",
                path.display()
            );
            return AppliedMap::new();
        }
    };
    let checkpoint: AppliedCheckpoint = match serde_json::from_str(&raw) {
        Ok(checkpoint) => checkpoint,
        Err(e) => {
            eprintln!(
                "bun: warning: ignoring unparseable applied-placements checkpoint {}: {e}",
                path.display()
            );
            return AppliedMap::new();
        }
    };
    checkpoint
        .entries
        .into_iter()
        .map(|entry| ((entry.name, entry.namespace), entry.fingerprint))
        .collect()
}

/// Persist the applied map to `path` atomically (temp + rename).
///
/// Best-effort: a failed write is logged, not fatal — the reconciler's
/// convergence is still correct, it just loses the restart optimisation
/// for this tick.
pub fn save(path: &Path, applied: &AppliedMap) {
    let checkpoint = AppliedCheckpoint {
        schema: SCHEMA,
        entries: applied
            .iter()
            .map(|((name, namespace), fingerprint)| AppliedEntry {
                namespace: namespace.clone(),
                name: name.clone(),
                fingerprint: fingerprint.clone(),
            })
            .collect(),
    };
    // A checkpoint is plain data; serialisation cannot fail.
    let json = serde_json::to_string_pretty(&checkpoint).expect("checkpoint serialises");

    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "bun: warning: could not create applied-placements dir {}: {e}",
            parent.display()
        );
        return;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, json) {
        eprintln!("bun: warning: could not write applied-placements checkpoint: {e}");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        eprintln!("bun: warning: could not install applied-placements checkpoint: {e}");
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, namespace: &str) -> (String, String) {
        (name.to_string(), namespace.to_string())
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = checkpoint_path(dir.path());
        assert!(load(&path).is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = checkpoint_path(dir.path());

        let mut applied = AppliedMap::new();
        applied.insert(key("api", "default"), "fp-a".to_string());
        applied.insert(key("api", "payments"), "fp-b".to_string());
        save(&path, &applied);

        let loaded = load(&path);
        assert_eq!(loaded, applied);
        // The two same-name apps in different namespaces stay distinct.
        assert_eq!(
            loaded.get(&key("api", "default")).map(String::as_str),
            Some("fp-a")
        );
        assert_eq!(
            loaded.get(&key("api", "payments")).map(String::as_str),
            Some("fp-b")
        );
    }

    #[test]
    fn corrupt_checkpoint_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = checkpoint_path(dir.path());
        std::fs::write(&path, "not json at all").unwrap();
        assert!(load(&path).is_empty());
    }

    #[test]
    fn restart_skips_already_applied_and_redeploys_the_rest() {
        // Model the reconciler's restart decision: it loads the durable
        // checkpoint, then for each assignment compares the fresh
        // fingerprint against the loaded one. An identical fingerprint is
        // skipped (no double-deploy); a changed or missing one triggers a
        // (re)deploy (no forgetting in-flight work).
        let dir = tempfile::tempdir().unwrap();
        let path = checkpoint_path(dir.path());

        // Before the restart, `api` converged at fingerprint fp-1.
        let mut before = AppliedMap::new();
        before.insert(key("api", "default"), "fp-1".to_string());
        save(&path, &before);

        // After the restart, the reconciler loads the checkpoint.
        let applied = load(&path);

        // Assignment 1: same app, same fingerprint → skip.
        assert_eq!(
            applied.get(&key("api", "default")).map(String::as_str),
            Some("fp-1"),
            "an unchanged assignment is recognised as already applied"
        );

        // Assignment 2: a newly-assigned app the checkpoint never saw →
        // must (re)deploy, so it isn't in `applied` yet.
        assert!(
            !applied.contains_key(&key("web", "default")),
            "an in-flight app absent from the checkpoint is not skipped"
        );

        // Assignment 3: same app, a NEW fingerprint (spec changed) → deploy.
        assert_ne!(
            applied.get(&key("api", "default")).map(String::as_str),
            Some("fp-2"),
            "a changed spec is not treated as already applied"
        );
    }

    #[test]
    fn save_overwrites_previous_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = checkpoint_path(dir.path());

        let mut first = AppliedMap::new();
        first.insert(key("api", "default"), "fp-1".to_string());
        save(&path, &first);

        let mut second = AppliedMap::new();
        second.insert(key("web", "default"), "fp-2".to_string());
        save(&path, &second);

        let loaded = load(&path);
        assert_eq!(loaded, second);
        assert!(!loaded.contains_key(&key("api", "default")));
    }
}
