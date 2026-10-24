//! Resource owners: the controller contract's rule for who changes an app
//! (#1125).
//!
//! An app may carry an `owner`, the name of the API token that manages it.
//! A controller is an ordinary app holding that token: it claims an app by
//! applying it with `owner = "<its token's name>"`, and from then on
//! `relish apply` and GitOps leave the app alone, the way GitOps leaves
//! alone what it didn't write (chapter 7). The rules, in full:
//!
//! | Who | What | Result |
//! |-----|------|--------|
//! | anyone | apply declaring `owner = X` | allowed only for token `X` or an admin, else 403 |
//! | anyone | apply of an app owned by `X`, declaring `owner = X` | written |
//! | anyone | apply of an app owned by `X`, declaring no owner | skipped, with a note |
//! | anyone | apply of an app owned by `X`, declaring `owner = Y` | 409: release it first |
//! | token `X` or an admin | stop, delete, rollback or release | allowed |
//! | anyone else | stop, delete, rollback or release | 409 |
//! | GitOps | an app owned by anyone | left out of the diff: no update, adoption or prune |
//! | GitOps | git declaring an `owner` | refused, named in the partial result |
//!
//! Every function here is pure. The API calls them on the leader before it
//! proposes a write; the state machine calls [`stored_owner_conflict`] as a
//! backstop, so two writers racing on one app can't slip past each other.

use crate::config::app::AppSpec;
use crate::meat::types::AppId;

/// The longest `owner` the cluster accepts, in bytes.
pub const MAX_OWNER_BYTES: usize = 128;

/// Who is asking, as ownership sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerCaller {
    /// The token's name. An app's `owner` is compared with it.
    pub name: String,
    /// An admin may claim an app for any owner and run every imperative
    /// change on an owned app.
    pub admin: bool,
}

impl OwnerCaller {
    /// A caller authenticated by the token called `name`.
    pub fn token(name: &str, admin: bool) -> Self {
        Self {
            name: name.to_string(),
            admin,
        }
    }

    /// Whether this caller may act as `owner`: it's that token, or an admin.
    fn speaks_for(&self, owner: &str) -> bool {
        self.admin || self.name == owner
    }
}

/// What an apply does with one app, given its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyVerdict {
    /// Write the app as declared.
    Write,
    /// Leave the app as it is: a controller owns it and the apply doesn't
    /// declare that owner.
    Skip {
        /// The app's current owner.
        owner: String,
    },
}

/// Why ownership refused a change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OwnerError {
    /// The apply declares an owner the caller doesn't speak for.
    #[error(
        "app {app}: owner {owner:?} must be the name of the token applying it, and this token is {caller:?}"
    )]
    NotTheCaller {
        app: AppId,
        owner: String,
        caller: String,
    },
    /// The apply declares a different owner from the one the app has.
    #[error("app {app} is owned by {current:?}, so {declared:?} can't own it until it's released")]
    Claimed {
        app: AppId,
        current: String,
        declared: String,
    },
    /// An imperative change from someone who isn't the owner or an admin.
    #[error("app {app} is owned by {owner:?}; only that token or an admin may {verb} it")]
    NotOwner {
        app: AppId,
        owner: String,
        verb: &'static str,
    },
    /// A release of an app nobody owns.
    #[error("app {app} has no owner to release")]
    NotOwned { app: AppId },
}

impl OwnerError {
    /// The HTTP status the API answers with: 403 when the caller can't
    /// claim that owner at all, 409 when the app's current owner is in the
    /// way.
    pub fn status(&self) -> u16 {
        match self {
            Self::NotTheCaller { .. } => 403,
            Self::Claimed { .. } | Self::NotOwner { .. } | Self::NotOwned { .. } => 409,
        }
    }
}

/// Judge one app in an apply.
///
/// `declared` is the owner the applied config gives the app, `current` the
/// owner the app has in desired state (`None` for a new or unowned app).
pub fn judge_apply(
    app: &AppId,
    declared: Option<&str>,
    current: Option<&str>,
    caller: &OwnerCaller,
) -> Result<ApplyVerdict, OwnerError> {
    if let Some(owner) = declared
        && !caller.speaks_for(owner)
    {
        return Err(OwnerError::NotTheCaller {
            app: app.clone(),
            owner: owner.to_string(),
            caller: caller.name.clone(),
        });
    }
    match (current, declared) {
        (None, _) => Ok(ApplyVerdict::Write),
        (Some(current), Some(declared)) if current == declared => Ok(ApplyVerdict::Write),
        (Some(current), Some(declared)) => Err(OwnerError::Claimed {
            app: app.clone(),
            current: current.to_string(),
            declared: declared.to_string(),
        }),
        (Some(current), None) => Ok(ApplyVerdict::Skip {
            owner: current.to_string(),
        }),
    }
}

/// Judge an imperative change (`verb`: stop, delete, roll back, release) to
/// an app whose owner is `current`.
pub fn judge_command(
    app: &AppId,
    current: Option<&str>,
    caller: &OwnerCaller,
    verb: &'static str,
) -> Result<(), OwnerError> {
    match current {
        Some(owner) if !caller.speaks_for(owner) => Err(OwnerError::NotOwner {
            app: app.clone(),
            owner: owner.to_string(),
            verb,
        }),
        _ => Ok(()),
    }
}

/// Judge a release, which hands an owned app back to `relish apply` and
/// GitOps.
pub fn judge_release(
    app: &AppId,
    current: Option<&str>,
    caller: &OwnerCaller,
) -> Result<(), OwnerError> {
    if current.is_none() {
        return Err(OwnerError::NotOwned { app: app.clone() });
    }
    judge_command(app, current, caller, "release")
}

/// The state machine's backstop: why an app write must not apply over the
/// app's stored owner, or `None` when it may.
///
/// Once an app has an owner, every write of it must carry the same owner;
/// only a release clears it. The state machine has no caller to check, so
/// this compares the two specs and nothing else, which keeps it
/// deterministic on every node.
pub fn stored_owner_conflict(
    app: &AppId,
    stored: Option<&str>,
    incoming: Option<&str>,
) -> Option<String> {
    let stored = stored?;
    if incoming == Some(stored) {
        return None;
    }
    Some(format!(
        "app {app} is owned by {stored:?}; a write must keep that owner until the app is released"
    ))
}

/// The progress line `relish apply` prints for an app it skipped.
pub fn skip_note(app: &AppId, owner: &str) -> String {
    format!(
        "app '{}' in namespace '{}' is owned by '{owner}'; skipped, so this apply doesn't fight it",
        app.name, app.namespace
    )
}

/// Check an `owner` value: a token name, so 1 to [`MAX_OWNER_BYTES`] bytes
/// of printable ASCII without spaces.
pub fn validate_owner(owner: &str) -> Result<(), String> {
    if owner.is_empty() || owner.len() > MAX_OWNER_BYTES {
        return Err(format!(
            "owner must be 1 to {MAX_OWNER_BYTES} bytes, the name of the controller's token"
        ));
    }
    if !owner.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!(
            "owner {owner:?} must be printable ASCII without spaces, the name of the controller's token"
        ));
    }
    Ok(())
}

/// `spec` without its owner: what the nodes run and what a rollout
/// compares. Claiming or releasing an app restarts nothing.
pub fn without_owner(spec: &AppSpec) -> AppSpec {
    let mut spec = spec.clone();
    spec.owner = None;
    spec
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppId {
        AppId::new("db", "pg")
    }

    fn controller() -> OwnerCaller {
        OwnerCaller::token("pg-failover", false)
    }

    fn alice() -> OwnerCaller {
        OwnerCaller::token("alice", false)
    }

    fn admin() -> OwnerCaller {
        OwnerCaller::token("root", true)
    }

    #[test]
    fn an_unowned_app_is_written_whatever_the_apply_declares() {
        assert_eq!(
            judge_apply(&app(), None, None, &alice()),
            Ok(ApplyVerdict::Write)
        );
        assert_eq!(
            judge_apply(&app(), Some("pg-failover"), None, &controller()),
            Ok(ApplyVerdict::Write),
            "a controller claims an app by applying it with its own name"
        );
    }

    #[test]
    fn only_the_named_token_or_an_admin_may_declare_an_owner() {
        let error = judge_apply(&app(), Some("pg-failover"), None, &alice()).unwrap_err();
        assert!(matches!(error, OwnerError::NotTheCaller { .. }));
        assert_eq!(error.status(), 403);
        assert!(error.to_string().contains("\"alice\""), "{error}");
        assert_eq!(
            judge_apply(&app(), Some("pg-failover"), None, &admin()),
            Ok(ApplyVerdict::Write)
        );
    }

    #[test]
    fn the_owner_keeps_writing_its_own_app() {
        assert_eq!(
            judge_apply(
                &app(),
                Some("pg-failover"),
                Some("pg-failover"),
                &controller()
            ),
            Ok(ApplyVerdict::Write)
        );
    }

    #[test]
    fn an_apply_without_the_owner_skips_an_owned_app() {
        for caller in [alice(), admin(), controller()] {
            assert_eq!(
                judge_apply(&app(), None, Some("pg-failover"), &caller),
                Ok(ApplyVerdict::Skip {
                    owner: "pg-failover".into()
                }),
                "{caller:?}: an apply never releases an app by leaving the owner out"
            );
        }
    }

    #[test]
    fn a_different_owner_is_refused_until_the_app_is_released() {
        let other = OwnerCaller::token("other-controller", false);
        let error = judge_apply(
            &app(),
            Some("other-controller"),
            Some("pg-failover"),
            &other,
        )
        .unwrap_err();
        assert!(matches!(error, OwnerError::Claimed { .. }));
        assert_eq!(error.status(), 409);
        let error = judge_apply(
            &app(),
            Some("other-controller"),
            Some("pg-failover"),
            &admin(),
        )
        .unwrap_err();
        assert!(
            matches!(error, OwnerError::Claimed { .. }),
            "even an admin releases first"
        );
    }

    #[test]
    fn imperative_changes_need_the_owner_or_an_admin() {
        for verb in ["stop", "delete", "roll back"] {
            assert_eq!(
                judge_command(&app(), Some("pg-failover"), &controller(), verb),
                Ok(())
            );
            assert_eq!(
                judge_command(&app(), Some("pg-failover"), &admin(), verb),
                Ok(())
            );
            let error = judge_command(&app(), Some("pg-failover"), &alice(), verb).unwrap_err();
            assert_eq!(error.status(), 409);
            assert!(error.to_string().contains(verb), "{error}");
            assert_eq!(judge_command(&app(), None, &alice(), verb), Ok(()));
        }
    }

    #[test]
    fn a_release_needs_an_owner_and_the_owner_or_an_admin() {
        assert_eq!(
            judge_release(&app(), Some("pg-failover"), &controller()),
            Ok(())
        );
        assert_eq!(judge_release(&app(), Some("pg-failover"), &admin()), Ok(()));
        assert!(matches!(
            judge_release(&app(), Some("pg-failover"), &alice()),
            Err(OwnerError::NotOwner { .. })
        ));
        assert!(matches!(
            judge_release(&app(), None, &admin()),
            Err(OwnerError::NotOwned { .. })
        ));
    }

    #[test]
    fn the_state_machine_refuses_any_write_that_drops_or_changes_the_owner() {
        let app = app();
        assert_eq!(stored_owner_conflict(&app, None, None), None);
        assert_eq!(stored_owner_conflict(&app, None, Some("x")), None);
        assert_eq!(stored_owner_conflict(&app, Some("x"), Some("x")), None);
        assert!(stored_owner_conflict(&app, Some("x"), None).is_some());
        assert!(stored_owner_conflict(&app, Some("x"), Some("y")).is_some());
    }

    #[test]
    fn an_owner_is_a_token_name() {
        assert!(validate_owner("pg-failover").is_ok());
        assert!(validate_owner("CI_bot.v2").is_ok());
        assert!(validate_owner("").is_err());
        assert!(validate_owner("two words").is_err());
        assert!(validate_owner("tab\there").is_err());
        assert!(validate_owner(&"x".repeat(MAX_OWNER_BYTES)).is_ok());
        assert!(validate_owner(&"x".repeat(MAX_OWNER_BYTES + 1)).is_err());
    }

    #[test]
    fn the_skip_note_names_the_app_and_its_owner() {
        assert_eq!(
            skip_note(&app(), "pg-failover"),
            "app 'db' in namespace 'pg' is owned by 'pg-failover'; skipped, so this apply \
             doesn't fight it"
        );
    }
}
