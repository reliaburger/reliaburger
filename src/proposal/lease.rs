//! Leases between agents (#1686): a short, visible claim on an app or a
//! namespace, held in the council so it survives a leader change.
//!
//! An agent takes a lease before it changes something, so a second agent,
//! or a human, sees who's working on it, why, and for how long, and waits or
//! gives up. A lease owns nothing. It isn't a test lease
//! ([`crate::testkit::lease`]), which owns resources and cleans them up; we
//! reuse only that machinery's shape: times stamped by the leader in the
//! request, the holder named by its credential, renewal only by the holder,
//! no reviving an expired lease, and a leader-only reaper.
//!
//! Every rule here is a pure function of the stored leases and numbers in
//! the request, so every replica applying the same Raft entry reaches the
//! same answer. Nothing in this file reads a clock.
//!
//! # The seam for proposals (#1682)
//!
//! A proposal takes and checks leases inside the state machine, in the same
//! Raft entry that creates or applies it, through four methods on
//! [`AgentLeases`]:
//!
//! - [`AgentLeases::take`] when the proposal is created, for each touched
//!   app the author doesn't already cover;
//! - [`AgentLeases::pin`] straight after, so the lease lives as long as the
//!   proposal does;
//! - [`AgentLeases::blocker`] when it's approved, for each touched app; a
//!   `Some` fails the proposal;
//! - [`AgentLeases::end_pinned`] when the proposal reaches a terminal state.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::meat::AppId;

/// A lease's lifetime when the caller names none: two minutes.
pub const DEFAULT_LEASE_TTL_MS: u64 = 2 * 60 * 1000;

/// The shortest lease: 30 seconds, so a few seconds of clock skew between
/// leaders stays small beside it.
pub const MIN_LEASE_TTL_MS: u64 = 30 * 1000;

/// The longest lease: 15 minutes. Longer work renews.
pub const MAX_LEASE_TTL_MS: u64 = 15 * 60 * 1000;

/// The most live leases one holder may have.
pub const MAX_LEASES_PER_HOLDER: usize = 16;

/// The most live leases in the cluster.
pub const MAX_LEASES: usize = 256;

/// The longest `purpose`, in bytes.
pub const MAX_PURPOSE_BYTES: usize = 256;

/// The longest app or namespace name a target may carry, in bytes.
const MAX_NAME_BYTES: usize = 253;

/// What a lease is on: one app, or every app in a namespace.
// For the Rust reader: `tag = "kind"` makes serde write the variant's name
// as a `kind` field beside the variant's own fields, so the JSON reads
// `{"kind":"app","namespace":"shop","app":"web"}` rather than nesting.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeaseTarget {
    /// One app.
    App {
        /// The app's namespace.
        namespace: String,
        /// The app's name. It needn't exist yet.
        app: String,
    },
    /// A whole namespace: every app in it, including apps created while
    /// the lease is held.
    Namespace {
        /// The namespace.
        namespace: String,
    },
}

impl LeaseTarget {
    /// A lease on `app` in `namespace`.
    pub fn app(namespace: impl Into<String>, app: impl Into<String>) -> Self {
        LeaseTarget::App {
            namespace: namespace.into(),
            app: app.into(),
        }
    }

    /// A lease on the whole of `namespace`.
    pub fn namespace(namespace: impl Into<String>) -> Self {
        LeaseTarget::Namespace {
            namespace: namespace.into(),
        }
    }

    /// The namespace the target is in, or is.
    pub fn namespace_name(&self) -> &str {
        match self {
            LeaseTarget::App { namespace, .. } | LeaseTarget::Namespace { namespace } => namespace,
        }
    }

    /// The app, for an app lease.
    pub fn app_name(&self) -> Option<&str> {
        match self {
            LeaseTarget::App { app, .. } => Some(app),
            LeaseTarget::Namespace { .. } => None,
        }
    }

    /// The key a lease is stored under, and how answers name it:
    /// `app:shop/web` or `namespace:shop`.
    pub fn key(&self) -> String {
        self.to_string()
    }

    /// Whether this target covers `app`.
    pub fn covers(&self, app: &AppId) -> bool {
        match self {
            LeaseTarget::App {
                namespace,
                app: name,
            } => *namespace == app.namespace && *name == app.name,
            LeaseTarget::Namespace { namespace } => *namespace == app.namespace,
        }
    }

    /// Whether a lease on `self` and a lease on `other` can't both be held
    /// by different holders: the same target, or a namespace and an app in
    /// it.
    pub fn overlaps(&self, other: &LeaseTarget) -> bool {
        match (self, other) {
            (LeaseTarget::App { .. }, LeaseTarget::App { .. }) => self == other,
            (LeaseTarget::Namespace { namespace }, other)
            | (other, LeaseTarget::Namespace { namespace }) => other.namespace_name() == namespace,
        }
    }

    /// Check the names are usable: present, not too long, and free of the
    /// separators the key uses and of whitespace and control characters.
    pub fn validate(&self) -> Result<(), LeaseRefusal> {
        let names: &[&str] = match self {
            LeaseTarget::App { namespace, app } => &[namespace, app],
            LeaseTarget::Namespace { namespace } => &[namespace],
        };
        for name in names {
            let bad = name.is_empty()
                || name.len() > MAX_NAME_BYTES
                || name
                    .chars()
                    .any(|c| c == '/' || c == ':' || c.is_whitespace() || c.is_control());
            if bad {
                return Err(LeaseRefusal::Invalid {
                    reason: format!("{name:?} isn't an app or namespace name"),
                });
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for LeaseTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseTarget::App { namespace, app } => write!(f, "app:{namespace}/{app}"),
            LeaseTarget::Namespace { namespace } => write!(f, "namespace:{namespace}"),
        }
    }
}

/// Who holds a lease: the credential that took it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseHolder {
    /// The token's name, for people to read.
    pub name: String,
    /// The exact credential's stable id (`AuthContext::principal_id`). Only
    /// this credential renews or releases the lease.
    pub principal_id: String,
}

/// One live claim on an app or a namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLease {
    /// What it's on.
    pub target: LeaseTarget,
    /// Who holds it.
    pub holder: LeaseHolder,
    /// What the holder said it's doing. Untrusted text: redacted, without
    /// control characters, at most [`MAX_PURPOSE_BYTES`].
    pub purpose: String,
    /// The pending proposal that pins it, whose expiry it follows.
    pub proposal: Option<u64>,
    /// When it was taken, on the leader's clock.
    pub acquired_at_unix_ms: u64,
    /// When it lapses, on the leader's clock.
    pub expires_at_unix_ms: u64,
}

impl AgentLease {
    /// Whether it's still held at `now_unix_ms`.
    pub fn is_live_at(&self, now_unix_ms: u64) -> bool {
        self.expires_at_unix_ms > now_unix_ms
    }

    /// The facts its audit records carry: target, holder, purpose, expiry,
    /// and the pinning proposal when there is one.
    pub fn audit_details(&self) -> BTreeMap<String, String> {
        let mut details = BTreeMap::from([
            ("target".to_string(), self.target.key()),
            ("holder".to_string(), self.holder.name.clone()),
            ("purpose".to_string(), self.purpose.clone()),
            (
                "expires_at_unix_ms".to_string(),
                self.expires_at_unix_ms.to_string(),
            ),
        ]);
        if let Some(proposal) = self.proposal {
            details.insert("proposal".to_string(), proposal.to_string());
        }
        details
    }

    /// How it looks to a reader at `now_unix_ms`.
    pub fn view(&self, now_unix_ms: u64) -> LeaseView {
        LeaseView {
            target: self.target.key(),
            kind: match self.target {
                LeaseTarget::App { .. } => LeaseKind::App,
                LeaseTarget::Namespace { .. } => LeaseKind::Namespace,
            },
            namespace: self.target.namespace_name().to_string(),
            app: self.target.app_name().map(str::to_string),
            holder: self.holder.name.clone(),
            purpose: self.purpose.clone(),
            proposal: self.proposal,
            acquired_at_unix_ms: self.acquired_at_unix_ms,
            expires_at_unix_ms: self.expires_at_unix_ms,
            expires_in_seconds: self.expires_at_unix_ms.saturating_sub(now_unix_ms) / 1000,
        }
    }
}

/// What a caller asks for when it takes a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRequest {
    /// What to lease.
    pub target: LeaseTarget,
    /// Who's asking.
    pub holder: LeaseHolder,
    /// What they're doing, already passed through [`clean_purpose`].
    pub purpose: String,
    /// How long, from `now`, between [`MIN_LEASE_TTL_MS`] and
    /// [`MAX_LEASE_TTL_MS`].
    pub ttl_ms: u64,
}

/// A lease granted by [`AgentLeases::take`] or [`AgentLeases::renew`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseGrant {
    /// The lease as it now stands.
    pub lease: AgentLease,
    /// `true` when the caller already held it, so this only extended it.
    pub renewed: bool,
}

/// Why a lease operation changed nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum LeaseRefusal {
    /// Someone else holds the target, or a lease that overlaps it.
    #[error("{} is held by {} until {} ({})", lease.target, lease.holder.name, lease.expires_at_unix_ms, lease.purpose)]
    Held { lease: Box<AgentLease> },
    /// There's no lease on the target.
    #[error("there's no lease on {target}")]
    NotFound { target: String },
    /// The caller's lease lapsed before it renewed; take it again.
    #[error("the lease on {target} expired; take it again")]
    Expired { target: String },
    /// A pending proposal pins the lease; it ends with the proposal.
    #[error("proposal {proposal} pins the lease on {target}; it ends when the proposal does")]
    Pinned { target: String, proposal: u64 },
    /// The holder has as many live leases as it may.
    #[error("{holder} already holds {limit} leases, the most one holder may")]
    HolderLimit { holder: String, limit: usize },
    /// The cluster has as many live leases as it may.
    #[error("the cluster already has {limit} leases, the most it holds")]
    ClusterLimit { limit: usize },
    /// The request itself is wrong.
    #[error("{reason}")]
    Invalid { reason: String },
}

/// What a lease Raft entry did. Carried back to the API in
/// `CouncilResponse::AgentLease`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LeaseOutcome {
    /// A new lease.
    Taken { lease: AgentLease },
    /// The holder's own lease, extended.
    Renewed { lease: AgentLease },
    /// The holder gave it up.
    Released { lease: AgentLease },
    /// Someone else ended it.
    Broken { lease: AgentLease },
    /// The reaper ended these, whose time had passed.
    Expired { leases: Vec<AgentLease> },
    /// Nothing changed, and why.
    Refused { refusal: LeaseRefusal },
}

impl From<Result<LeaseGrant, LeaseRefusal>> for LeaseOutcome {
    fn from(result: Result<LeaseGrant, LeaseRefusal>) -> Self {
        match result {
            Ok(LeaseGrant {
                lease,
                renewed: true,
            }) => LeaseOutcome::Renewed { lease },
            Ok(LeaseGrant {
                lease,
                renewed: false,
            }) => LeaseOutcome::Taken { lease },
            Err(refusal) => LeaseOutcome::Refused { refusal },
        }
    }
}

/// Every agent lease in the cluster, and how many times the set of holders
/// has changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLeases {
    /// Leases by [`LeaseTarget::key`]. A lease past its expiry stays until
    /// the reaper's [`AgentLeases::expire`] removes it, but no rule counts
    /// it as held.
    pub leases: BTreeMap<String, AgentLease>,
    /// Moves on every take, release, break and expiry, never on a renewal.
    /// A write fenced on leases carries the generation it was checked at,
    /// so a renewing agent never makes a human's write retry.
    pub generation: u64,
}

impl AgentLeases {
    /// Take `request.target` for `request.holder` at `now_unix_ms`.
    ///
    /// Free (or lapsed): granted. Held by the same credential: renewed, with
    /// the new purpose. Held by anyone else, or overlapping a lease someone
    /// else holds (a namespace and an app in it): refused with
    /// [`LeaseRefusal::Held`], naming that lease.
    pub fn take(
        &mut self,
        request: &LeaseRequest,
        now_unix_ms: u64,
    ) -> Result<LeaseGrant, LeaseRefusal> {
        request.target.validate()?;
        check_ttl(request.ttl_ms)?;
        check_purpose(&request.purpose)?;
        let key = request.target.key();
        let principal = &request.holder.principal_id;
        if let Some(blocking) = self.live(now_unix_ms).find(|lease| {
            lease.holder.principal_id != *principal && lease.target.overlaps(&request.target)
        }) {
            return Err(LeaseRefusal::Held {
                lease: Box::new(blocking.clone()),
            });
        }
        let expires_at_unix_ms = now_unix_ms.saturating_add(request.ttl_ms);
        if let Some(lease) = self.leases.get_mut(&key)
            && lease.is_live_at(now_unix_ms)
        {
            // Only the same holder gets here: anyone else was refused above.
            lease.purpose = request.purpose.clone();
            lease.expires_at_unix_ms = lease.expires_at_unix_ms.max(expires_at_unix_ms);
            return Ok(LeaseGrant {
                lease: lease.clone(),
                renewed: true,
            });
        }
        let held_by_holder = self
            .live(now_unix_ms)
            .filter(|lease| lease.holder.principal_id == *principal)
            .count();
        if held_by_holder >= MAX_LEASES_PER_HOLDER {
            return Err(LeaseRefusal::HolderLimit {
                holder: request.holder.name.clone(),
                limit: MAX_LEASES_PER_HOLDER,
            });
        }
        if self.live(now_unix_ms).count() >= MAX_LEASES {
            return Err(LeaseRefusal::ClusterLimit { limit: MAX_LEASES });
        }
        let lease = AgentLease {
            target: request.target.clone(),
            holder: request.holder.clone(),
            purpose: request.purpose.clone(),
            proposal: None,
            acquired_at_unix_ms: now_unix_ms,
            expires_at_unix_ms,
        };
        // Replaces a lapsed lease on the same target, if there was one.
        self.leases.insert(key, lease.clone());
        self.generation = self.generation.saturating_add(1);
        Ok(LeaseGrant {
            lease,
            renewed: false,
        })
    }

    /// Extend the holder's lease on `target` to `now_unix_ms + ttl_ms`. It
    /// never shortens one. Refused for anyone but the holder, and for a
    /// lease that has already lapsed: an expired lease isn't revived.
    pub fn renew(
        &mut self,
        target: &LeaseTarget,
        holder: &LeaseHolder,
        ttl_ms: u64,
        now_unix_ms: u64,
    ) -> Result<LeaseGrant, LeaseRefusal> {
        check_ttl(ttl_ms)?;
        let lease = self.held_by(target, holder, now_unix_ms)?;
        lease.expires_at_unix_ms = lease
            .expires_at_unix_ms
            .max(now_unix_ms.saturating_add(ttl_ms));
        Ok(LeaseGrant {
            lease: lease.clone(),
            renewed: true,
        })
    }

    /// Give up the holder's lease on `target`. Refused for anyone else, and
    /// while a proposal pins it.
    pub fn release(
        &mut self,
        target: &LeaseTarget,
        holder: &LeaseHolder,
        now_unix_ms: u64,
    ) -> Result<AgentLease, LeaseRefusal> {
        let lease = self.held_by(target, holder, now_unix_ms)?;
        if let Some(proposal) = lease.proposal {
            return Err(LeaseRefusal::Pinned {
                target: target.key(),
                proposal,
            });
        }
        self.remove(target)
    }

    /// End whoever's lease is on `target`, pinned or not. The API decides
    /// who may; here it only has to exist and still be live.
    pub fn break_lease(
        &mut self,
        target: &LeaseTarget,
        now_unix_ms: u64,
    ) -> Result<AgentLease, LeaseRefusal> {
        match self.leases.get(&target.key()) {
            Some(lease) if lease.is_live_at(now_unix_ms) => self.remove(target),
            _ => Err(LeaseRefusal::NotFound {
                target: target.key(),
            }),
        }
    }

    /// Remove every lease lapsed by `now_unix_ms`, and return them.
    pub fn expire(&mut self, now_unix_ms: u64) -> Vec<AgentLease> {
        let lapsed: Vec<String> = self
            .leases
            .iter()
            .filter(|(_, lease)| !lease.is_live_at(now_unix_ms))
            .map(|(key, _)| key.clone())
            .collect();
        let expired: Vec<AgentLease> = lapsed
            .iter()
            .filter_map(|key| self.leases.remove(key))
            .collect();
        if !expired.is_empty() {
            self.generation = self.generation.saturating_add(1);
        }
        expired
    }

    /// Whether any lease has lapsed by `now_unix_ms`, so the reaper has
    /// something to write.
    pub fn any_lapsed(&self, now_unix_ms: u64) -> bool {
        self.leases
            .values()
            .any(|lease| !lease.is_live_at(now_unix_ms))
    }

    /// The live lease that stops `principal_id` from changing `app` at
    /// `now_unix_ms`: one on the app, or on its namespace, held by anyone
    /// else. `None` means the way is clear.
    pub fn blocker(
        &self,
        app: &AppId,
        principal_id: &str,
        now_unix_ms: u64,
    ) -> Option<&AgentLease> {
        self.live(now_unix_ms)
            .find(|lease| lease.holder.principal_id != principal_id && lease.target.covers(app))
    }

    /// Tie the live lease on `target` to `proposal`: it now lapses at
    /// `expires_at_unix_ms`, the proposal's own expiry, and its holder can't
    /// release it while the proposal waits.
    pub fn pin(
        &mut self,
        target: &LeaseTarget,
        proposal: u64,
        expires_at_unix_ms: u64,
        now_unix_ms: u64,
    ) -> Result<(), LeaseRefusal> {
        match self.leases.get_mut(&target.key()) {
            Some(lease) if lease.is_live_at(now_unix_ms) => {
                lease.proposal = Some(proposal);
                lease.expires_at_unix_ms = expires_at_unix_ms;
                Ok(())
            }
            _ => Err(LeaseRefusal::NotFound {
                target: target.key(),
            }),
        }
    }

    /// End every lease `proposal` pins, once it reaches a terminal state,
    /// and return them.
    pub fn end_pinned(&mut self, proposal: u64) -> Vec<AgentLease> {
        let pinned: Vec<String> = self
            .leases
            .iter()
            .filter(|(_, lease)| lease.proposal == Some(proposal))
            .map(|(key, _)| key.clone())
            .collect();
        let ended: Vec<AgentLease> = pinned
            .iter()
            .filter_map(|key| self.leases.remove(key))
            .collect();
        if !ended.is_empty() {
            self.generation = self.generation.saturating_add(1);
        }
        ended
    }

    /// Every lease still held at `now_unix_ms`, in key order.
    pub fn live(&self, now_unix_ms: u64) -> impl Iterator<Item = &AgentLease> {
        self.leases
            .values()
            .filter(move |lease| lease.is_live_at(now_unix_ms))
    }

    /// The holder's live lease on `target`, for renew and release.
    fn held_by(
        &mut self,
        target: &LeaseTarget,
        holder: &LeaseHolder,
        now_unix_ms: u64,
    ) -> Result<&mut AgentLease, LeaseRefusal> {
        let Some(lease) = self.leases.get_mut(&target.key()) else {
            return Err(LeaseRefusal::NotFound {
                target: target.key(),
            });
        };
        if !lease.is_live_at(now_unix_ms) {
            return Err(LeaseRefusal::Expired {
                target: target.key(),
            });
        }
        if lease.holder.principal_id != holder.principal_id {
            return Err(LeaseRefusal::Held {
                lease: Box::new(lease.clone()),
            });
        }
        Ok(lease)
    }

    fn remove(&mut self, target: &LeaseTarget) -> Result<AgentLease, LeaseRefusal> {
        let lease = self
            .leases
            .remove(&target.key())
            .ok_or_else(|| LeaseRefusal::NotFound {
                target: target.key(),
            })?;
        self.generation = self.generation.saturating_add(1);
        Ok(lease)
    }
}

fn check_ttl(ttl_ms: u64) -> Result<(), LeaseRefusal> {
    if (MIN_LEASE_TTL_MS..=MAX_LEASE_TTL_MS).contains(&ttl_ms) {
        return Ok(());
    }
    Err(LeaseRefusal::Invalid {
        reason: format!(
            "a lease lasts between {} and {} seconds, not {}",
            MIN_LEASE_TTL_MS / 1000,
            MAX_LEASE_TTL_MS / 1000,
            ttl_ms / 1000
        ),
    })
}

fn check_purpose(purpose: &str) -> Result<(), LeaseRefusal> {
    if purpose.len() > MAX_PURPOSE_BYTES || purpose.chars().any(char::is_control) {
        return Err(LeaseRefusal::Invalid {
            reason: format!(
                "a purpose is at most {MAX_PURPOSE_BYTES} bytes with no control characters"
            ),
        });
    }
    Ok(())
}

/// Make a caller's `purpose` safe to store and show: control characters
/// become spaces, secrets are redacted, and it's cut to
/// [`MAX_PURPOSE_BYTES`] on a character boundary.
pub fn clean_purpose(purpose: &str) -> String {
    let flat: String = purpose
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut clean = crate::redact::redact_secrets(flat.trim());
    if clean.len() > MAX_PURPOSE_BYTES {
        let mut end = MAX_PURPOSE_BYTES;
        while !clean.is_char_boundary(end) {
            end -= 1;
        }
        clean.truncate(end);
    }
    clean
}

/// The kind of a lease's target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LeaseKind {
    /// One app.
    App,
    /// A whole namespace.
    Namespace,
}

/// One lease as `GET /v1/leases` and `relish agent context` show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LeaseView {
    /// `app:shop/web` or `namespace:shop`.
    pub target: String,
    /// `app` or `namespace`.
    pub kind: LeaseKind,
    /// The namespace.
    pub namespace: String,
    /// The app, for an app lease.
    pub app: Option<String>,
    /// The holder's token name.
    pub holder: String,
    /// What the holder said it's doing, in its own words.
    pub purpose: String,
    /// The pending proposal that pins it, if one does.
    pub proposal: Option<u64>,
    /// When it was taken, Unix milliseconds on the leader's clock.
    pub acquired_at_unix_ms: u64,
    /// When it lapses, Unix milliseconds on the leader's clock.
    pub expires_at_unix_ms: u64,
    /// Whole seconds left when the leader answered.
    pub expires_in_seconds: u64,
}

/// What `GET /v1/leases` answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseListing {
    /// Live leases the caller may see, in target order.
    pub leases: Vec<LeaseView>,
    /// The lease generation the listing was read at.
    pub generation: u64,
}

/// `POST /v1/leases`: what to take, why, and for how long.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TakeLeaseRequest {
    /// The app or namespace.
    pub target: LeaseTarget,
    /// What the caller is doing, for whoever finds the lease held. It's
    /// cleaned of control characters and secrets, and cut to 256 bytes.
    #[serde(default)]
    pub purpose: String,
    /// How long, 30 to 900 seconds; 120 when absent.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// `POST /v1/leases/renew`: which lease, for how long from now.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RenewLeaseRequest {
    /// The app or namespace.
    pub target: LeaseTarget,
    /// How long from now, 30 to 900 seconds; 120 when absent.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// `POST /v1/leases/release` and `/v1/leases/break`: which lease.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseTargetRequest {
    /// The app or namespace.
    pub target: LeaseTarget,
}

/// What a lease write answers when it changed something.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LeaseAnswer {
    /// `taken`, `renewed`, `released` or `broken`.
    pub outcome: String,
    /// The lease as it stands, or as it was when it ended.
    pub lease: LeaseView,
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn holder(name: &str) -> LeaseHolder {
        LeaseHolder {
            name: name.to_string(),
            principal_id: format!("token:{name}"),
        }
    }

    fn request(target: LeaseTarget, who: &str) -> LeaseRequest {
        LeaseRequest {
            target,
            holder: holder(who),
            purpose: format!("{who} is investigating"),
            ttl_ms: DEFAULT_LEASE_TTL_MS,
        }
    }

    fn web() -> LeaseTarget {
        LeaseTarget::app("shop", "web")
    }

    fn held(result: Result<LeaseGrant, LeaseRefusal>) -> AgentLease {
        match result {
            Err(LeaseRefusal::Held { lease }) => *lease,
            other => panic!("expected a held refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_free_target_is_granted_and_moves_the_generation() {
        let mut leases = AgentLeases::default();
        let grant = leases.take(&request(web(), "holmes"), NOW).unwrap();
        assert!(!grant.renewed);
        assert_eq!(grant.lease.acquired_at_unix_ms, NOW);
        assert_eq!(grant.lease.expires_at_unix_ms, NOW + DEFAULT_LEASE_TTL_MS);
        assert_eq!(grant.lease.proposal, None);
        assert_eq!(leases.generation, 1);
        assert_eq!(leases.leases.len(), 1);
        assert!(leases.leases.contains_key("app:shop/web"));
    }

    #[test]
    fn taking_your_own_lease_again_renews_it_without_moving_the_generation() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        let mut again = request(web(), "holmes");
        again.purpose = "rolling back".to_string();
        let grant = leases.take(&again, NOW + 60_000).unwrap();
        assert!(grant.renewed);
        assert_eq!(grant.lease.acquired_at_unix_ms, NOW);
        assert_eq!(
            grant.lease.expires_at_unix_ms,
            NOW + 60_000 + DEFAULT_LEASE_TTL_MS
        );
        assert_eq!(grant.lease.purpose, "rolling back");
        assert_eq!(leases.generation, 1);
    }

    #[test]
    fn another_holder_is_refused_with_who_holds_it_and_until_when() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        let lease = held(leases.take(&request(web(), "capacity"), NOW + 1));
        assert_eq!(lease.holder.name, "holmes");
        assert_eq!(lease.expires_at_unix_ms, NOW + DEFAULT_LEASE_TTL_MS);
        assert_eq!(lease.purpose, "holmes is investigating");
        let message = LeaseRefusal::Held {
            lease: Box::new(lease),
        }
        .to_string();
        assert!(
            message.contains("app:shop/web is held by holmes"),
            "{message}"
        );
        assert_eq!(leases.generation, 1, "a refusal changes nothing");
    }

    #[test]
    fn a_namespace_lease_conflicts_with_every_app_lease_inside_it() {
        let mut leases = AgentLeases::default();
        leases
            .take(&request(LeaseTarget::namespace("shop"), "alice"), NOW)
            .unwrap();
        let lease = held(leases.take(&request(web(), "holmes"), NOW));
        assert_eq!(lease.target, LeaseTarget::namespace("shop"));
        // Another namespace is free.
        leases
            .take(&request(LeaseTarget::app("billing", "web"), "holmes"), NOW)
            .unwrap();
        // The namespace's holder may take an app inside it too.
        leases.take(&request(web(), "alice"), NOW).unwrap();
    }

    #[test]
    fn an_app_lease_blocks_a_namespace_lease_on_its_namespace() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        let lease = held(leases.take(&request(LeaseTarget::namespace("shop"), "alice"), NOW));
        assert_eq!(lease.target, web());
        leases
            .take(&request(LeaseTarget::namespace("billing"), "alice"), NOW)
            .unwrap();
    }

    #[test]
    fn a_lapsed_lease_is_free_to_anyone_and_counts_as_a_new_take() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        let after = NOW + DEFAULT_LEASE_TTL_MS;
        let grant = leases.take(&request(web(), "capacity"), after).unwrap();
        assert!(!grant.renewed);
        assert_eq!(grant.lease.holder.name, "capacity");
        assert_eq!(grant.lease.acquired_at_unix_ms, after);
        assert_eq!(leases.generation, 2);
    }

    #[test]
    fn only_the_holder_renews_and_a_renewal_never_shortens() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        let refused = leases.renew(&web(), &holder("capacity"), DEFAULT_LEASE_TTL_MS, NOW + 1);
        assert_eq!(held(refused).holder.name, "holmes");
        let grant = leases
            .renew(
                &web(),
                &holder("holmes"),
                DEFAULT_LEASE_TTL_MS,
                NOW + 90_000,
            )
            .unwrap();
        assert!(grant.renewed);
        assert_eq!(
            grant.lease.expires_at_unix_ms,
            NOW + 90_000 + DEFAULT_LEASE_TTL_MS
        );
        let short = leases
            .renew(&web(), &holder("holmes"), MIN_LEASE_TTL_MS, NOW + 91_000)
            .unwrap();
        assert_eq!(
            short.lease.expires_at_unix_ms,
            NOW + 90_000 + DEFAULT_LEASE_TTL_MS
        );
        assert_eq!(leases.generation, 1, "renewing never moves the generation");
    }

    #[test]
    fn an_expired_lease_is_never_revived_by_a_renewal() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        let refused = leases.renew(
            &web(),
            &holder("holmes"),
            DEFAULT_LEASE_TTL_MS,
            NOW + DEFAULT_LEASE_TTL_MS,
        );
        assert!(matches!(refused, Err(LeaseRefusal::Expired { .. })));
        let missing = leases.renew(
            &LeaseTarget::app("shop", "api"),
            &holder("holmes"),
            DEFAULT_LEASE_TTL_MS,
            NOW,
        );
        assert!(matches!(missing, Err(LeaseRefusal::NotFound { .. })));
    }

    #[test]
    fn only_the_holder_releases_and_a_release_moves_the_generation() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        assert!(matches!(
            leases.release(&web(), &holder("capacity"), NOW),
            Err(LeaseRefusal::Held { .. })
        ));
        let released = leases.release(&web(), &holder("holmes"), NOW).unwrap();
        assert_eq!(released.holder.name, "holmes");
        assert!(leases.leases.is_empty());
        assert_eq!(leases.generation, 2);
        assert!(matches!(
            leases.release(&web(), &holder("holmes"), NOW),
            Err(LeaseRefusal::NotFound { .. })
        ));
    }

    #[test]
    fn a_break_ends_anyones_live_lease_even_a_pinned_one() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        leases.pin(&web(), 17, NOW + 3_600_000, NOW).unwrap();
        let broken = leases.break_lease(&web(), NOW).unwrap();
        assert_eq!(broken.proposal, Some(17));
        assert!(leases.leases.is_empty());
        assert_eq!(leases.generation, 2);
        assert!(matches!(
            leases.break_lease(&web(), NOW),
            Err(LeaseRefusal::NotFound { .. })
        ));
    }

    #[test]
    fn expiry_removes_only_lapsed_leases_by_the_given_clock() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        let mut long = request(LeaseTarget::app("shop", "api"), "holmes");
        long.ttl_ms = MAX_LEASE_TTL_MS;
        leases.take(&long, NOW).unwrap();
        assert!(!leases.any_lapsed(NOW + DEFAULT_LEASE_TTL_MS - 1));
        assert!(leases.expire(NOW + DEFAULT_LEASE_TTL_MS - 1).is_empty());
        assert_eq!(leases.generation, 2, "nothing expired, nothing moved");
        assert!(leases.any_lapsed(NOW + DEFAULT_LEASE_TTL_MS));
        let expired = leases.expire(NOW + DEFAULT_LEASE_TTL_MS);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].target, web());
        assert_eq!(leases.generation, 3);
        assert_eq!(leases.leases.len(), 1);
    }

    #[test]
    fn a_lapsed_lease_no_longer_blocks_before_the_reaper_removes_it() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        let app = AppId::new("web", "shop");
        assert!(leases.blocker(&app, "token:alice", NOW).is_some());
        assert!(leases.blocker(&app, "token:holmes", NOW).is_none());
        assert!(
            leases
                .blocker(&app, "token:alice", NOW + DEFAULT_LEASE_TTL_MS)
                .is_none()
        );
    }

    #[test]
    fn a_namespace_lease_blocks_apps_created_while_it_is_held() {
        let mut leases = AgentLeases::default();
        leases
            .take(&request(LeaseTarget::namespace("shop"), "alice"), NOW)
            .unwrap();
        let new_app = AppId::new("brand-new", "shop");
        assert_eq!(
            leases
                .blocker(&new_app, "token:holmes", NOW)
                .map(|lease| lease.holder.name.as_str()),
            Some("alice")
        );
        assert!(
            leases
                .blocker(&AppId::new("web", "billing"), "token:holmes", NOW)
                .is_none()
        );
    }

    #[test]
    fn a_pinned_lease_follows_its_proposal_and_ends_with_it() {
        let mut leases = AgentLeases::default();
        leases.take(&request(web(), "holmes"), NOW).unwrap();
        leases.pin(&web(), 17, NOW + 3_600_000, NOW).unwrap();
        assert_eq!(
            leases.leases["app:shop/web"].expires_at_unix_ms,
            NOW + 3_600_000
        );
        assert_eq!(
            leases.release(&web(), &holder("holmes"), NOW),
            Err(LeaseRefusal::Pinned {
                target: "app:shop/web".to_string(),
                proposal: 17
            })
        );
        assert!(leases.end_pinned(18).is_empty());
        let ended = leases.end_pinned(17);
        assert_eq!(ended.len(), 1);
        assert!(leases.leases.is_empty());
        assert_eq!(leases.generation, 2);
        assert!(matches!(
            leases.pin(&web(), 17, NOW, NOW),
            Err(LeaseRefusal::NotFound { .. })
        ));
    }

    #[test]
    fn a_holder_may_hold_sixteen_leases_and_no_more() {
        let mut leases = AgentLeases::default();
        for index in 0..MAX_LEASES_PER_HOLDER {
            leases
                .take(
                    &request(LeaseTarget::app("shop", format!("app-{index}")), "holmes"),
                    NOW,
                )
                .unwrap();
        }
        let refused = leases.take(
            &request(LeaseTarget::app("shop", "one-more"), "holmes"),
            NOW,
        );
        assert_eq!(
            refused,
            Err(LeaseRefusal::HolderLimit {
                holder: "holmes".to_string(),
                limit: MAX_LEASES_PER_HOLDER
            })
        );
        // Renewing one it holds still works, and another holder isn't capped.
        leases
            .take(&request(LeaseTarget::app("shop", "app-0"), "holmes"), NOW)
            .unwrap();
        leases
            .take(&request(LeaseTarget::app("shop", "one-more"), "alice"), NOW)
            .unwrap();
    }

    #[test]
    fn the_cluster_holds_at_most_256_leases() {
        let mut leases = AgentLeases::default();
        for index in 0..MAX_LEASES {
            let who = format!("agent-{}", index / MAX_LEASES_PER_HOLDER);
            leases
                .take(
                    &request(LeaseTarget::app("shop", format!("app-{index}")), &who),
                    NOW,
                )
                .unwrap();
        }
        assert_eq!(
            leases.take(&request(LeaseTarget::app("shop", "extra"), "late"), NOW),
            Err(LeaseRefusal::ClusterLimit { limit: MAX_LEASES })
        );
    }

    #[test]
    fn a_lease_lasts_between_thirty_seconds_and_fifteen_minutes() {
        let mut leases = AgentLeases::default();
        for ttl_ms in [MIN_LEASE_TTL_MS - 1, MAX_LEASE_TTL_MS + 1, 0] {
            let mut short = request(web(), "holmes");
            short.ttl_ms = ttl_ms;
            assert!(matches!(
                leases.take(&short, NOW),
                Err(LeaseRefusal::Invalid { .. })
            ));
        }
        for ttl_ms in [MIN_LEASE_TTL_MS, MAX_LEASE_TTL_MS] {
            let mut edge = request(web(), "holmes");
            edge.ttl_ms = ttl_ms;
            leases.take(&edge, NOW).unwrap();
        }
        assert!(leases.leases.len() == 1 && leases.generation == 1);
    }

    #[test]
    fn a_target_with_a_separator_or_no_name_is_refused() {
        let mut leases = AgentLeases::default();
        for target in [
            LeaseTarget::app("shop", ""),
            LeaseTarget::app("", "web"),
            LeaseTarget::app("shop", "a/b"),
            LeaseTarget::app("shop", "a:b"),
            LeaseTarget::namespace("two words"),
            LeaseTarget::namespace("x".repeat(254)),
        ] {
            assert!(
                matches!(
                    leases.take(&request(target.clone(), "holmes"), NOW),
                    Err(LeaseRefusal::Invalid { .. })
                ),
                "{target:?}"
            );
        }
        assert!(leases.leases.is_empty());
    }

    #[test]
    fn a_raw_purpose_is_refused_and_a_cleaned_one_is_safe() {
        let mut leases = AgentLeases::default();
        let mut raw = request(web(), "holmes");
        raw.purpose = "line one\nline two".to_string();
        assert!(matches!(
            leases.take(&raw, NOW),
            Err(LeaseRefusal::Invalid { .. })
        ));
        raw.purpose = "x".repeat(MAX_PURPOSE_BYTES + 1);
        assert!(leases.take(&raw, NOW).is_err());

        assert_eq!(clean_purpose("  line one\nline two\t"), "line one line two");
        let long = clean_purpose(&"é".repeat(MAX_PURPOSE_BYTES));
        assert!(long.len() <= MAX_PURPOSE_BYTES);
        assert!(long.chars().all(|c| c == 'é'));
        let secret = clean_purpose("retry with token rbrg_0123456789abcdef0123456789abcdef");
        assert!(!secret.contains("0123456789abcdef"), "{secret}");
        raw.purpose = clean_purpose("line one\nline two");
        leases.take(&raw, NOW).unwrap();
    }

    #[test]
    fn a_target_is_keyed_and_shown_as_kind_namespace_and_app() {
        assert_eq!(web().key(), "app:shop/web");
        assert_eq!(LeaseTarget::namespace("shop").key(), "namespace:shop");
        assert_eq!(
            serde_json::to_value(web()).unwrap(),
            serde_json::json!({"kind": "app", "namespace": "shop", "app": "web"})
        );
        let mut leases = AgentLeases::default();
        let lease = leases.take(&request(web(), "holmes"), NOW).unwrap().lease;
        let view = lease.view(NOW + 30_500);
        assert_eq!(view.target, "app:shop/web");
        assert_eq!(view.kind, LeaseKind::App);
        assert_eq!(view.app.as_deref(), Some("web"));
        assert_eq!(view.holder, "holmes");
        assert_eq!(view.expires_in_seconds, 89);
    }

    #[test]
    fn an_outcome_round_trips_through_json() {
        let mut leases = AgentLeases::default();
        let taken = LeaseOutcome::from(leases.take(&request(web(), "holmes"), NOW));
        let refused = LeaseOutcome::from(leases.take(&request(web(), "alice"), NOW));
        for outcome in [taken, refused] {
            let json = serde_json::to_string(&outcome).unwrap();
            assert_eq!(
                serde_json::from_str::<LeaseOutcome>(&json).unwrap(),
                outcome
            );
        }
    }
}
