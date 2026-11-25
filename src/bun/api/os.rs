//! Appliance OS update routes (W6): a node staging an update, and the
//! cluster-wide rollout the leader runs (`crate::os::rollout`).

use super::*;
use crate::os::rollout::{OsDirective, OsRollout, OsRolloutPhase};

fn refuse(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// `POST /v1/os/stage`: stage an OS version on this node and reboot into
/// it (admin; the leader sends it as the system principal). 202 once
/// staging has started, 200 if this version is running already.
pub(super) async fn os_stage_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let directive: OsDirective = match serde_json::from_str(&body) {
        Ok(directive) => directive,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, format!("invalid directive: {e}")),
    };
    let Some(slot) = crate::os::slot::installed() else {
        return refuse(
            StatusCode::CONFLICT,
            "this node isn't an appliance, so it has no OS to update",
        );
    };
    match slot.begin(&directive).await {
        Ok(crate::os::slot::Begun::Running) => {
            Json(serde_json::json!({ "status": "already_running" })).into_response()
        }
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "status": "updating" })),
        )
            .into_response(),
        Err(reason) => refuse(StatusCode::CONFLICT, reason),
    }
}

/// Where this node's `netlog.conf` lives: the ESP on an appliance, `None`
/// on any other node. Tests mount the router with an
/// `Extension<NetlogPaths>` naming a temporary directory instead; bun
/// itself never adds one.
// `Response` is the reply to send as-is.
#[allow(clippy::result_large_err)]
fn netlog_paths(
    paths: Option<axum::Extension<crate::os::netlog::NetlogPaths>>,
) -> Result<crate::os::netlog::NetlogPaths, Response> {
    match paths {
        Some(axum::Extension(paths)) => Ok(paths),
        None if crate::os::slot::installed().is_some() => {
            Ok(crate::os::netlog::NetlogPaths::default())
        }
        None => Err(refuse(StatusCode::CONFLICT, NOT_AN_APPLIANCE)),
    }
}

/// `GET /v1/os/boot-logs`: where this node's boot logs go (#2149), as its
/// next boot will read `reliaburger/netlog.conf` on the ESP.
pub(super) async fn os_boot_logs_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    paths: Option<axum::Extension<crate::os::netlog::NetlogPaths>>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return resp;
    }
    let paths = match netlog_paths(paths) {
        Ok(paths) => paths,
        Err(resp) => return resp,
    };
    match tokio::task::spawn_blocking(move || crate::os::netlog::read(&paths)).await {
        Ok(Ok(target)) => Json(crate::os::netlog::BootLogs { target }).into_response(),
        Ok(Err(reason)) => refuse(StatusCode::INTERNAL_SERVER_ERROR, reason),
        Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /v1/os/boot-logs {"target": "<address>:<port>" | null}`: send this
/// node's boot logs there from now on, or stop (cluster admin: the lines
/// carry the whole node's kernel log, in cleartext). `target` must be there,
/// as `null` for off, and nothing else may be. Answers the setting as it
/// now stands, with a `warning` if it only takes effect at the next boot.
/// The audit record names the target, or `off`.
pub(super) async fn os_boot_logs_set_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    paths: Option<axum::Extension<crate::os::netlog::NetlogPaths>>,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let paths = match netlog_paths(paths) {
        Ok(paths) => paths,
        Err(resp) => return resp,
    };
    let wanted: crate::os::netlog::BootLogs = match serde_json::from_str(&body) {
        Ok(wanted) => wanted,
        Err(e) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                format!("invalid boot log setting: {e}"),
            );
        }
    };
    let target = wanted.target;
    crate::bun::audit::annotate(crate::bun::audit::AuditNote {
        app: None,
        namespace: None,
        details: [(
            "target".to_string(),
            target.map_or_else(|| "off".to_string(), |target| target.to_string()),
        )]
        .into(),
    });
    let written = {
        let paths = paths.clone();
        tokio::task::spawn_blocking(move || crate::os::netlog::write(&paths, target)).await
    };
    match written {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, reason),
        Err(e) => return refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    let warning = crate::os::netlog::apply_now(&paths, target).await;
    if let Some(warning) = &warning {
        eprintln!("bun: boot logs: {warning}");
    }
    Json(serde_json::json!({ "target": target, "warning": warning })).into_response()
}

const NOT_AN_APPLIANCE: &str = "this node isn't an appliance, so it has no boot logs to send";

/// `POST /v1/os/rollout/start {"version", "channel_url", "allow_downgrade"}`:
/// roll an OS version across the cluster (admin, on the leader).
pub(super) async fn os_rollout_start_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    #[derive(serde::Deserialize)]
    struct StartRequest {
        version: String,
        channel_url: String,
        #[serde(default)]
        allow_downgrade: bool,
    }
    let Some(council) = &state.council else {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "OS rollouts need a council (cluster mode)",
        );
    };
    if let Some(forwarded) = super::upgrade::forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/os/rollout/start",
        &headers,
        &body,
    )
    .await
    {
        return forwarded;
    }
    let request: StartRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(e) => return refuse(StatusCode::BAD_REQUEST, format!("invalid request: {e}")),
    };
    let Ok(target) = request.version.parse::<crate::os::OsVersion>() else {
        return refuse(
            StatusCode::BAD_REQUEST,
            format!("{} isn't an OS version (YYYY.WW.N)", request.version),
        );
    };
    let desired = council.desired_state().await;
    if let Some(upgrade) = &desired.active_upgrade {
        return refuse(
            StatusCode::CONFLICT,
            format!(
                "bun upgrade {} is in progress; finish or abort it first",
                upgrade.upgrade_id
            ),
        );
    }
    if let Some(rollout) = &desired.os_rollout {
        return refuse(
            StatusCode::CONFLICT,
            format!(
                "OS rollout {} to {} is in progress; resume or abort it",
                rollout.rollout_id, rollout.target
            ),
        );
    }
    // Council members update one at a time, like a bun upgrade's. Two voters
    // never have the headroom to lose one, so the rollout would sit pending
    // for ever.
    if let Err(resp) = super::upgrade::check_council_can_roll(council) {
        return resp;
    }

    let view = match super::upgrade::build_authoritative_view(&state, council).await {
        Ok(view) => view,
        Err(resp) => return resp,
    };
    let control = crate::os::rollout::HttpOsControl::new(
        state.cluster_http.clone(),
        state.service_token.clone(),
    );
    let mut nodes = Vec::new();
    for (node_id, node) in &view {
        use crate::os::rollout::OsControl;
        let Some(address) = &node.address else {
            return refuse(
                StatusCode::CONFLICT,
                format!("{node_id} hasn't advertised its API address yet"),
            );
        };
        let Some(probe) = control.probe(address).await else {
            return refuse(StatusCode::CONFLICT, format!("{node_id} isn't answering"));
        };
        let Some(running) = probe.os_version else {
            return refuse(
                StatusCode::CONFLICT,
                format!("{node_id} isn't an appliance, so it has no OS to update"),
            );
        };
        let older = running
            .parse::<crate::os::OsVersion>()
            .is_ok_and(|running| target < running);
        if older && !request.allow_downgrade {
            return refuse(
                StatusCode::CONFLICT,
                format!(
                    "{node_id} runs {running}, newer than {target}; pass --allow-downgrade to go back"
                ),
            );
        }
        let council_member = !matches!(node.role, crate::upgrade::types::NodeRole::Worker);
        nodes.push((node_id.clone(), address.clone(), council_member, running));
    }
    let now = unix_now();
    let leader = state.node_name.clone().unwrap_or_default();
    let rollout = crate::os::rollout::plan(
        &format!("os-{target}-{now}"),
        &target.to_string(),
        &request.channel_url,
        &leader,
        nodes,
        now,
    );
    write_rollout(council, rollout, None).await
}

/// Start `rollout`, replacing the paused one `replaces` names (a resume).
/// The state machine refuses a start that would overlap another rollout or
/// a bun upgrade, reuse an id, or replace a rollout that has moved on, and
/// says why: that's a 409.
async fn write_rollout(
    council: &Arc<crate::council::CouncilNode>,
    rollout: OsRollout,
    replaces: Option<crate::council::types::ReplacesPaused>,
) -> Response {
    let written = rollout.clone();
    match council
        .write(crate::council::types::RaftRequest::OsRolloutStart {
            rollout: Box::new(rollout),
            replaces,
        })
        .await
    {
        Ok(crate::council::CouncilResponse::Refused { reason }) => {
            refuse(StatusCode::CONFLICT, reason)
        }
        Ok(_) => (StatusCode::ACCEPTED, Json(serde_json::json!(written))).into_response(),
        Err(e) => refuse(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    }
}

/// `GET /v1/os/rollout`: the rollout in progress and the finished ones
/// (replicated, so any node answers).
pub(super) async fn os_rollout_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return resp;
    }
    let Some(council) = &state.council else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "no council on this node");
    };
    let desired = council.desired_state().await;
    Json(serde_json::json!({
        "active": desired.os_rollout,
        "history": desired.os_rollout_history,
    }))
    .into_response()
}

/// `POST /v1/os/rollout/resume`: carry on with a paused rollout, retrying
/// the node that stopped it.
pub(super) async fn os_rollout_resume_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let Some(council) = &state.council else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "no council on this node");
    };
    if let Some(forwarded) = super::upgrade::forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/os/rollout/resume",
        &headers,
        &body,
    )
    .await
    {
        return forwarded;
    }
    match council.desired_state().await.os_rollout {
        Some(rollout) if matches!(rollout.phase, OsRolloutPhase::Paused { .. }) => {
            let replaces = crate::council::types::ReplacesPaused::Resume {
                id: rollout.rollout_id.clone(),
            };
            let resumed = crate::os::rollout::resume(rollout, unix_now());
            write_rollout(council, resumed, Some(replaces)).await
        }
        Some(rollout) => refuse(
            StatusCode::CONFLICT,
            format!("OS rollout {} isn't paused", rollout.rollout_id),
        ),
        None => refuse(StatusCode::NOT_FOUND, "no OS rollout in progress"),
    }
}

/// `POST /v1/os/rollout/abort`: stop the rollout. Nodes keep whatever
/// version they're on; one mid-update finishes its reboot.
pub(super) async fn os_rollout_abort_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let Some(council) = &state.council else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "no council on this node");
    };
    if let Some(forwarded) = super::upgrade::forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/os/rollout/abort",
        &headers,
        &body,
    )
    .await
    {
        return forwarded;
    }
    let Some(mut rollout) = council.desired_state().await.os_rollout else {
        return refuse(StatusCode::NOT_FOUND, "no OS rollout in progress");
    };
    rollout.phase = OsRolloutPhase::Aborted {
        reason: "aborted by the operator".to_string(),
    };
    let rollout_id = rollout.rollout_id.clone();
    for request in [
        crate::council::types::RaftRequest::OsRolloutUpdate {
            rollout: Box::new(rollout),
        },
        crate::council::types::RaftRequest::OsRolloutClear {
            rollout_id: rollout_id.clone(),
        },
    ] {
        match council.write(request).await {
            // It ended (or was resumed under a new id) since we read it.
            Ok(crate::council::CouncilResponse::Refused { reason }) => {
                return refuse(StatusCode::CONFLICT, reason);
            }
            Ok(_) => {}
            Err(e) => return refuse(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
        }
    }
    Json(serde_json::json!({ "aborted": rollout_id })).into_response()
}

/// `?local=true`: answer from this node's replica alone.
#[derive(Debug, Default, Deserialize)]
pub(super) struct SettingsQuery {
    #[serde(default)]
    local: bool,
}

/// How long a node with no Raft seat waits for the leader's settings.
const LEADER_SETTINGS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The cluster's `[appliance]` settings (#2151), from this node's replica,
/// or, on a node with no Raft seat, whose replica holds nothing, from the
/// leader's. A standalone node has no council, so every setting is off.
pub(super) async fn appliance_settings(
    state: &ApiState,
    directory: Option<&LeaderDirectory>,
) -> Result<crate::appliance::settings::ApplianceSettings, String> {
    use crate::appliance::settings::ApplianceSettings;
    let Some(council) = &state.council else {
        return Ok(ApplianceSettings::default());
    };
    if council.holds_security_state().await {
        return Ok(local_appliance_settings(council).await);
    }
    let LeaderDirectory(directory) =
        directory.ok_or("this node has no Raft seat and no gossip directory to find the leader")?;
    let leader = crate::cluster::directory::leader_api_address(Some(&council.metrics()), directory)
        .ok_or("no council leader is known yet")?;
    let url = state
        .cluster_http
        .url(&leader.to_string(), "/v1/os/settings?local=true");
    let mut request = state.cluster_http.client().get(url);
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    let fetch = async { request.send().await?.error_for_status()?.json().await };
    match tokio::time::timeout(LEADER_SETTINGS_TIMEOUT, fetch).await {
        Ok(Ok(settings)) => Ok(settings),
        Ok(Err(error)) => Err(format!("the leader's settings: {error}")),
        Err(_) => Err(format!(
            "the leader didn't answer within {} s",
            LEADER_SETTINGS_TIMEOUT.as_secs()
        )),
    }
}

async fn local_appliance_settings(
    council: &crate::council::CouncilNode,
) -> crate::appliance::settings::ApplianceSettings {
    council
        .read_desired(|desired| {
            crate::appliance::settings::ApplianceSettings::from_council(&desired.config)
        })
        .await
}

/// `GET /v1/os/settings`: the cluster's `[appliance]` settings (#2151).
/// Any node answers ([`appliance_settings`]); with `?local=true`, only from
/// its own replica, which is how a node with no Raft seat asks the leader.
pub(super) async fn os_settings_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    Query(query): Query<SettingsQuery>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::ReadOnly)
    {
        return resp;
    }
    let settings = match (&state.council, query.local) {
        (Some(council), true) => Ok(local_appliance_settings(council).await),
        _ => appliance_settings(&state, directory.as_deref()).await,
    };
    match settings {
        Ok(settings) => Json(settings).into_response(),
        Err(reason) => refuse(StatusCode::SERVICE_UNAVAILABLE, reason),
    }
}

/// `POST /v1/os/settings {"console_chaos": bool}`: replace the cluster's
/// `[appliance]` settings (cluster admin; audited). A field left out is
/// off. A follower passes the request, with the caller's own credential,
/// to the leader, which writes it to the council.
pub(super) async fn os_settings_set_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) = authorize_cluster_admin(&state, auth.as_deref()).await {
        return resp;
    }
    let settings: crate::appliance::settings::ApplianceSettings = match serde_json::from_str(&body)
    {
        Ok(settings) => settings,
        Err(e) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                format!("invalid appliance settings: {e}"),
            );
        }
    };
    let Some(council) = &state.council else {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "appliance settings live in the cluster's council; this node is standalone",
        );
    };
    if let Some(forwarded) = super::upgrade::forward_upgrade_to_leader(
        &state,
        council,
        directory.as_deref(),
        "/v1/os/settings",
        &headers,
        &body,
    )
    .await
    {
        return forwarded;
    }
    crate::bun::audit::annotate(crate::bun::audit::AuditNote {
        app: None,
        namespace: None,
        details: [(
            "console_chaos".to_string(),
            settings.console_chaos.to_string(),
        )]
        .into(),
    });
    for (key, value) in settings.council_entries() {
        if let Err(e) = council
            .write(crate::council::types::RaftRequest::ConfigSet { key, value })
            .await
        {
            return refuse(StatusCode::SERVICE_UNAVAILABLE, e.to_string());
        }
    }
    Json(settings).into_response()
}
