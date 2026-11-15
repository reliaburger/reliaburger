//! An app's revision: a SHA-256 over its stored spec and its stopped flag.
//!
//! It's how a proposal notices that someone else changed an app. A proposal
//! applies only while every touched app's revision still equals its
//! `before` one, and an undo only while it equals the `after` one. We don't
//! add a per-app version counter, because the hash needs no bookkeeping on
//! any of the existing write paths: it's computed from what's stored.

use sha2::Digest;

use crate::config::app::AppSpec;
use crate::council::types::DesiredState;
use crate::meat::AppId;

/// The revision of `spec` (`None`: the app doesn't exist) with `stopped`,
/// in hex.
///
/// The spec is serialised with `serde_json`, as the snapshot does. Every
/// map inside `AppSpec` is a `BTreeMap`, so the bytes, and the hash, are
/// the same on every node.
pub fn revision_of(spec: Option<&AppSpec>, stopped: bool) -> String {
    // Serialising plain data can't fail; an empty buffer would still hash
    // the same everywhere.
    let bytes = serde_json::to_vec(&(spec, stopped)).unwrap_or_default();
    hex::encode(sha2::Sha256::digest(bytes))
}

/// The revision of an app that doesn't exist.
pub fn absent_revision() -> String {
    revision_of(None, false)
}

/// `app`'s revision in `desired` now.
pub fn current_revision(desired: &DesiredState, app: &AppId) -> String {
    let spec = desired.apps.get(app);
    let stopped = spec.is_some() && desired.stopped_apps.contains(app);
    revision_of(spec, stopped)
}

/// `app` as `desired` holds it now.
pub fn current_app(desired: &DesiredState, app: &AppId) -> super::types::AppRevision {
    let spec = desired.apps.get(app).cloned();
    let stopped = spec.is_some() && desired.stopped_apps.contains(app);
    super::types::AppRevision::new(spec, stopped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(image: &str) -> AppSpec {
        toml::from_str(&format!("image = {image:?}")).unwrap()
    }

    #[test]
    fn identical_specs_hash_the_same() {
        assert_eq!(
            revision_of(Some(&spec("web:1")), false),
            revision_of(Some(&spec("web:1")), false)
        );
    }

    #[test]
    fn any_field_change_and_the_stopped_flag_change_the_hash() {
        let base = revision_of(Some(&spec("web:1")), false);
        assert_ne!(base, revision_of(Some(&spec("web:2")), false));
        assert_ne!(base, revision_of(Some(&spec("web:1")), true));
        let mut scaled = spec("web:1");
        scaled.replicas = crate::config::Replicas::Fixed(5);
        assert_ne!(base, revision_of(Some(&scaled), false));
        assert_ne!(base, absent_revision());
    }

    #[test]
    fn a_stopped_flag_on_a_missing_app_is_ignored() {
        let mut desired = DesiredState::default();
        let web = AppId::new("web", "shop");
        desired.stopped_apps.insert(web.clone());
        assert_eq!(current_revision(&desired, &web), absent_revision());
        assert!(!current_app(&desired, &web).exists());
    }
}
