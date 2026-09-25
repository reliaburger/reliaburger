//! Automatic node leaf renewal and observable worker health.

use super::{
    credentials::LiveNodeIdentity,
    mtls::{self, CrlHandle, MtlsError},
};
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::sync::{RwLock, watch};
use tokio_util::sync::CancellationToken;

/// Current progress of the node's automatic renewal owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewalState {
    /// The owner has not checked the installed identity yet.
    Starting,
    /// The owner is running and renewal is not yet due.
    Valid,
    /// A renewal request or durable replacement is in progress.
    Renewing,
    /// The last attempt failed; the current identity remains installed.
    Retrying,
    /// The identity expired and requires authorised re-enrolment.
    Expired,
    /// The owner exited or was cancelled.
    Stopped,
}

impl RenewalState {
    /// Stable public diagnostic state, containing no credential material.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Valid => "valid",
            Self::Renewing => "renewing",
            Self::Retrying => "retrying",
            Self::Expired => "expired",
            Self::Stopped => "stopped",
        }
    }
}

/// Read-only health evidence. A dropped or panicked owner cannot leave a stale
/// healthy value behind: closing its watch channel reports `Stopped`.
#[derive(Debug, Clone)]
pub struct RenewalMonitor(watch::Receiver<RenewalState>);

impl RenewalMonitor {
    /// Read the live worker state, or `Stopped` after its owner disappears.
    pub fn state(&self) -> RenewalState {
        if self.0.has_changed().is_err() {
            RenewalState::Stopped
        } else {
            *self.0.borrow()
        }
    }
}

/// Pause after a failed renewal attempt before the next one.
pub const RETRY_DELAY: Duration = Duration::from_secs(5);

/// One renewal owner for one durable identity. The HTTP client refuses redirects
/// and uses a fresh TLS connection for each attempt, with the current live leaf.
pub struct NodeRenewalWorker {
    identity: LiveNodeIdentity,
    http: reqwest::Client,
    state: watch::Sender<RenewalState>,
    /// Pause after a failed attempt before the next one.
    retry_delay: Duration,
    /// This node's `leaf_lifetime_override_secs`, when set. A leaf signed for
    /// longer is renewed at the midpoint of this ceiling instead.
    lifetime_ceiling: Option<Duration>,
}

impl NodeRenewalWorker {
    /// Prepare an authenticated renewal owner and its public health monitor.
    pub fn new(
        identity: LiveNodeIdentity,
        crl: CrlHandle,
        service_token: &str,
    ) -> Result<(Self, RenewalMonitor), MtlsError> {
        let tls = (*mtls::build_live_mtls_client_config(&identity, crl, None)?).clone();
        let mut headers = reqwest::header::HeaderMap::new();
        let mut bearer = reqwest::header::HeaderValue::from_str(&format!("Bearer {service_token}"))
            .map_err(|error| MtlsError::ConfigFailed(format!("renewal service token: {error}")))?;
        bearer.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, bearer);
        let http = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|error| MtlsError::ConfigFailed(format!("renewal HTTP client: {error}")))?;
        let (state, monitor) = watch::channel(RenewalState::Starting);
        Ok((
            Self {
                identity,
                http,
                state,
                retry_delay: RETRY_DELAY,
                lifetime_ceiling: None,
            },
            RenewalMonitor(monitor),
        ))
    }

    /// Replace the production [`RETRY_DELAY`] between failed attempts. Tests
    /// that drive several failures use a short delay instead of waiting it out.
    pub fn with_retry_delay(mut self, retry_delay: Duration) -> Self {
        self.retry_delay = retry_delay;
        self
    }

    /// Renew no later than half of `ceiling` (this node's
    /// `[security] leaf_lifetime_override_secs`) after issue, even when the
    /// installed leaf was signed for longer. A node that gains the override
    /// while holding a one-year leaf then renews within half the ceiling
    /// instead of in six months. If the leader signs without the override, the
    /// node still renews only once per half-ceiling, never on every tick.
    pub fn with_leaf_lifetime_ceiling(mut self, ceiling: Duration) -> Self {
        self.lifetime_ceiling = Some(ceiling);
        self
    }

    /// Renew at the signed lifetime midpoint (or the ceiling's, when that is
    /// sooner), retrying failed attempts after five seconds. Resolve the
    /// current leader on every attempt; never proxy a CSR through another node
    /// or guess a remote API port. Shutdown cancels waiting or network I/O; an
    /// identity save already in progress owns its completion.
    pub async fn run(
        self,
        council: Arc<crate::council::CouncilNode>,
        membership: Arc<RwLock<Vec<crate::bun::api::NodeMembershipInfo>>>,
        local_api: SocketAddr,
        shutdown: CancellationToken,
    ) {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut retry_at = tokio::time::Instant::now();
        let mut last_error = None;
        loop {
            tokio::select! { biased;
                _ = shutdown.cancelled() => return,
                _ = ticker.tick() => {}
            }
            let current = self.identity.snapshot();
            let now = SystemTime::now();
            if now >= current.not_after {
                self.state.send_replace(RenewalState::Expired);
                continue;
            }
            if now < current.not_before {
                self.state.send_replace(RenewalState::Retrying);
                continue;
            }
            let signed = current
                .not_after
                .duration_since(current.not_before)
                .unwrap_or_default();
            // Issuance backdates every leaf, so a leaf signed with the ceiling
            // spans the ceiling plus the backdate; only a longer one is capped.
            let lifetime = self.lifetime_ceiling.map_or(signed, |ceiling| {
                signed.min(ceiling.saturating_add(super::ca::CLOCK_SKEW_BACKDATE))
            });
            // Both endpoints are validated SystemTimes; the midpoint cannot lie
            // beyond them. Checked arithmetic also handles platform range limits.
            let due = current
                .not_before
                .checked_add(lifetime / 2)
                .unwrap_or(current.not_before);
            if now < due {
                self.state.send_replace(RenewalState::Valid);
                continue;
            }
            if tokio::time::Instant::now() < retry_at {
                continue;
            }
            self.state.send_replace(RenewalState::Renewing);
            let result = tokio::select! { biased;
                _ = shutdown.cancelled() => return,
                result = tokio::time::timeout(Duration::from_secs(15),
                    self.renew(&council, &membership, local_api)) =>
                    result.unwrap_or_else(|_| Err("node renewal attempt timed out".into())),
            };
            match result {
                Ok(()) => {
                    self.state.send_replace(RenewalState::Valid);
                    last_error = None;
                }
                Err(error) => {
                    self.state.send_replace(RenewalState::Retrying);
                    if last_error.as_ref() != Some(&error) {
                        eprintln!("node identity renewal failed; retrying: {error}");
                        last_error = Some(error);
                    }
                    retry_at = tokio::time::Instant::now() + self.retry_delay;
                }
            }
        }
    }

    async fn renew(
        &self,
        council: &crate::council::CouncilNode,
        membership: &RwLock<Vec<crate::bun::api::NodeMembershipInfo>>,
        local_api: SocketAddr,
    ) -> Result<(), String> {
        use base64::Engine as _;
        let current = self.identity.snapshot();
        let leader_name = {
            let metrics = council.metrics();
            let metrics = metrics.borrow();
            let leader = metrics.current_leader.ok_or("no current council leader")?;
            metrics
                .membership_config
                .membership()
                .get_node(&leader)
                .ok_or("current leader is absent from council membership")?
                .name
                .clone()
        };
        let endpoint = if leader_name == current.node_id {
            local_api
        } else {
            membership
                .read()
                .await
                .iter()
                .find(|node| node.node_id.0 == leader_name)
                .map(|node| node.address)
                .ok_or("current leader has no advertised API endpoint")?
        };
        let node_id = current.node_id.clone();
        let (csr, key) = tokio::task::spawn_blocking(move || super::ca::create_node_csr(&node_id))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let request = super::renewal::RenewalRequest {
            compatibility: crate::compatibility::CURRENT,
            csr_b64: base64::engine::general_purpose::STANDARD.encode(csr),
        };
        let mut response = self
            .http
            .post(format!("https://{endpoint}/v1/cluster/renew"))
            .json(&request)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("renewal endpoint returned {}", response.status()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            if body.len().saturating_add(chunk.len()) > 64 * 1024 {
                return Err("renewal response exceeds 64 KiB".into());
            }
            body.extend_from_slice(&chunk);
        }
        let bundle: super::join::JoinBundle = serde_json::from_slice(&body)
            .map_err(|error| format!("invalid renewal response: {error}"))?;
        let replacement = bundle
            .into_identity(key)
            .map_err(|error| error.to_string())?;
        self.identity
            .replace(replacement)
            .await
            .map_err(|error| error.to_string())
    }
}
