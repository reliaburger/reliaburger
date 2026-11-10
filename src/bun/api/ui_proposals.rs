//! Brioche's Proposals pages (#1682): the list, one proposal, and the
//! forms a person approves, rejects and undoes with.
//!
//! The forms are the dashboard's first writes, so they're fenced more
//! tightly than anything else on it:
//!
//! - Only a request that came in on a browser session may post one; a
//!   bearer token uses `/v1/proposals`. The session cookie is
//!   `SameSite=Strict`, so a page on another site can't send it at all.
//! - Each form carries the session's anti-forgery token
//!   ([`crate::sesame::session::csrf_token_for`]), compared in constant
//!   time, and a browser that says the request is cross-site, or comes from
//!   inside a frame, is refused. Every page refuses to be framed at all
//!   (`ui::forbid_framing`).
//! - The session cookie stays read-only. Each form asks the person to paste
//!   their API token; it must be the very credential the session was opened
//!   with, still valid, and the decision acts as that token, through the
//!   same handler, checks and audit record as `relish proposal approve`.
//!   Someone holding only the cookie decides nothing, and an agent's token,
//!   the service token, or one since revoked decides nothing either.
//! - A node that isn't the council leader can't forward a session (the
//!   leader has never seen the cookie), so it says where to go instead.
//!
//! The page shows the forms only to a viewer the same checks would let
//! through; the server checks again when one is posted.

use super::*;

use super::ui::html_response;
use crate::brioche::proposals::{Actions, Notice, ProposalRow};
use crate::proposal::{Proposal, ProposalId};
use crate::sesame::session::BrowserSession;

/// `GET /ui/proposals`: every proposal the viewer's scope reaches.
pub(super) async fn ui_proposals_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) =
        crate::sesame::auth::authorize(auth, crate::sesame::types::ApiRole::ReadOnly)
    {
        return response;
    }
    let now = crate::testkit::lease::now_unix_millis();
    let rows = match &state.council {
        Some(council) => {
            council
                .read_desired(|desired| {
                    desired
                        .proposals
                        .proposals
                        .values()
                        .filter(|proposal| super::proposals::visible(auth, proposal))
                        .map(|proposal| row(desired, proposal, now))
                        .collect::<Vec<_>>()
                })
                .await
        }
        None => Vec::new(),
    };
    html_response(crate::brioche::proposals::render_proposals(&rows, now))
}

/// `?done=` on the detail page after a decision: which one, so the page
/// can say so. Only these words are read; nothing from the URL is shown.
#[derive(Debug, Default, Deserialize)]
pub(super) struct DetailQuery {
    #[serde(default)]
    done: Option<Decision>,
}

/// `GET /ui/proposal/{id}`: one proposal, with the forms the viewer may use.
pub(super) async fn ui_proposal_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    session: Option<axum::Extension<BrowserSession>>,
    State(state): State<ApiState>,
    Path(id): Path<u64>,
    Query(query): Query<DetailQuery>,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) =
        crate::sesame::auth::authorize(auth, crate::sesame::types::ApiRole::ReadOnly)
    {
        return response;
    }
    let notice = query
        .done
        .map(|decision| Notice::Done(decision.done().to_string()));
    render_detail(
        &state,
        auth,
        session.as_deref(),
        ProposalId(id),
        notice,
        StatusCode::OK,
    )
    .await
}

/// A decision a person makes on the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Decision {
    Approve,
    Reject,
    Undo,
}

impl Decision {
    fn path(self) -> &'static str {
        match self {
            Decision::Approve => "approve",
            Decision::Reject => "reject",
            Decision::Undo => "undo",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Decision::Approve => "Approved. The leader applied it in the same step.",
            Decision::Reject => "Rejected. Nothing changed.",
            Decision::Undo => "Undone. Every app it touched is back as it was.",
        }
    }
}

/// `POST /ui/proposal/{id}/approve`.
pub(super) async fn ui_proposal_approve_handler(
    session: Option<axum::Extension<BrowserSession>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: String,
) -> Response {
    let form = DecisionForm::parse(&body);
    let action = crate::config::PermissionAction::Deploy;
    let decision = (Decision::Approve, action);
    decide(
        &state,
        session.as_deref(),
        &headers,
        ProposalId(id),
        decision,
        form,
    )
    .await
}

/// `POST /ui/proposal/{id}/reject`.
pub(super) async fn ui_proposal_reject_handler(
    session: Option<axum::Extension<BrowserSession>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: String,
) -> Response {
    let form = DecisionForm::parse(&body);
    let action = crate::config::PermissionAction::Deploy;
    let decision = (Decision::Reject, action);
    decide(
        &state,
        session.as_deref(),
        &headers,
        ProposalId(id),
        decision,
        form,
    )
    .await
}

/// `POST /ui/proposal/{id}/undo`.
pub(super) async fn ui_proposal_undo_handler(
    session: Option<axum::Extension<BrowserSession>>,
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<u64>,
    body: String,
) -> Response {
    let form = DecisionForm::parse(&body);
    let action = crate::config::PermissionAction::Deploy;
    let decision = (Decision::Undo, action);
    decide(
        &state,
        session.as_deref(),
        &headers,
        ProposalId(id),
        decision,
        form,
    )
    .await
}

/// The fields a decision form posts. No `Debug`, so the pasted token can't
/// be printed by accident.
#[derive(Default)]
struct DecisionForm {
    csrf: String,
    /// The person's API token, pasted for this one decision.
    token: String,
    reason: String,
    confirm: String,
}

/// The context of the API token pasted into a decision form, once it's
/// shown to be the very credential the session was opened with, still
/// valid. The cookie alone is a read credential: whoever holds only the
/// cookie can't produce the token.
#[allow(clippy::result_large_err)]
async fn pasted_token_context(
    state: &ApiState,
    session: &BrowserSession,
    token: &str,
) -> Result<crate::sesame::auth::AuthContext, Response> {
    if token.trim().is_empty() {
        return Err(refused(
            StatusCode::UNAUTHORIZED,
            "to decide, paste your API token into the form: the session cookie only reads",
        ));
    }
    let Some(store) = state.token_store.as_ref() else {
        return Err(refused(
            StatusCode::UNAUTHORIZED,
            "this node has no token store, so it can't check the token",
        ));
    };
    let tokens = store.read().await.clone();
    let Ok(context) = crate::sesame::auth::authenticate(token.trim(), &tokens) else {
        return Err(refused(
            StatusCode::UNAUTHORIZED,
            "that token isn't valid (unknown, expired or rotated); paste your API token again",
        ));
    };
    if !crate::sesame::auth::tokens_equal(&context.principal_id, &session.principal_id) {
        // A real token, but not the one this session was opened with.
        let mut response = refused(
            StatusCode::FORBIDDEN,
            "that isn't the token you logged in with; paste that one, or log in with this one",
        );
        super::audit::refused_as(&mut response, &context);
        return Err(response);
    }
    Ok(context)
}

impl DecisionForm {
    /// The form from an `application/x-www-form-urlencoded` body. Unknown
    /// fields are ignored, a repeated one keeps its last value.
    fn parse(body: &str) -> DecisionForm {
        let mut form = DecisionForm::default();
        for (key, value) in url::form_urlencoded::parse(body.as_bytes()) {
            match key.as_ref() {
                "csrf" => form.csrf = value.into_owned(),
                "token" => form.token = value.into_owned(),
                "reason" => form.reason = value.into_owned(),
                "confirm" => form.confirm = value.into_owned(),
                _ => {}
            }
        }
        form
    }
}

/// Check that a browser session posted `decision` on purpose, then make it
/// through the API's own handler, as the session's token. `action` is the
/// `[permission]` action every touched app needs, as on `/v1/proposals`.
async fn decide(
    state: &ApiState,
    session: Option<&BrowserSession>,
    headers: &HeaderMap,
    id: ProposalId,
    (decision, action): (Decision, crate::config::PermissionAction),
    form: DecisionForm,
) -> Response {
    let Some(session) = session else {
        return refused(
            StatusCode::FORBIDDEN,
            "these forms are for the dashboard; with a token, use relish proposal or \
             /v1/proposals",
        );
    };
    if cross_site(headers) {
        return refused(StatusCode::FORBIDDEN, "refused: a cross-site request");
    }
    if !crate::sesame::auth::tokens_equal(&form.csrf, &session.csrf_token) {
        return refused(
            StatusCode::FORBIDDEN,
            "refused: the form's anti-forgery token doesn't match this session; reload the page",
        );
    }
    let context = match pasted_token_context(state, session, &form.token).await {
        Ok(context) => context,
        Err(response) => return response,
    };
    // The same checks the API makes, here before anything else, so a
    // viewer the page offered no form gets the API's refusal.
    let checked = match decision {
        Decision::Approve | Decision::Reject => {
            super::proposals::authorise_decider(state, Some(&context), id, action).await
        }
        Decision::Undo => {
            super::proposals::authorise_undoer(state, Some(&context), id, action).await
        }
    };
    let proposal = match checked {
        Ok(proposal) => proposal,
        Err(response) => return answer_refusal(state, session, &context, id, response).await,
    };
    if !named_leader(state) {
        let notice = Notice::Refused(NOT_LEADER.to_string());
        let auth = Some(&context);
        return render_detail(
            state,
            auth,
            Some(session),
            id,
            Some(notice),
            StatusCode::CONFLICT,
        )
        .await;
    }
    let view = proposal.view(crate::testkit::lease::now_unix_millis());
    let typed = view.reversibility != "exact" && decision != Decision::Reject;
    let expected = crate::brioche::proposals::first_app_name(&view);
    if typed && form.confirm.trim() != expected {
        let notice = Notice::Refused(format!(
            "not confirmed: type the app's name ({expected}) to {}; proposal {id} is unchanged",
            decision.path()
        ));
        return render_detail(
            state,
            Some(&context),
            Some(session),
            id,
            Some(notice),
            StatusCode::UNPROCESSABLE_ENTITY,
        )
        .await;
    }

    // No headers: the decision runs here, on the leader, and a session
    // cookie forwarded anywhere else would mean nothing there.
    let extension = Some(axum::Extension(context.clone()));
    let path = Path(id.0);
    let response = match decision {
        Decision::Approve => {
            super::proposals::proposal_approve_handler(
                extension,
                State(state.clone()),
                None,
                path,
                HeaderMap::new(),
            )
            .await
        }
        Decision::Reject => {
            super::proposals::proposal_reject_handler(
                extension,
                State(state.clone()),
                None,
                path,
                HeaderMap::new(),
                Json(crate::proposal::view::RejectRequest {
                    reason: form.reason.trim().to_string(),
                }),
            )
            .await
        }
        Decision::Undo => {
            super::proposals::proposal_undo_handler(
                extension,
                State(state.clone()),
                None,
                path,
                HeaderMap::new(),
            )
            .await
        }
    };
    if response.status().is_success() {
        return axum::response::Redirect::to(&format!(
            "/ui/proposal/{id}?done={}",
            decision.path()
        ))
        .into_response();
    }
    answer_refusal(state, session, &context, id, response).await
}

const NOT_LEADER: &str = "this node isn't the council leader, so it can't apply a decision \
                          from the dashboard; open the dashboard on the leader, or use the \
                          relish command below";

/// The detail page again, saying why the API refused, with its status.
async fn answer_refusal(
    state: &ApiState,
    session: &BrowserSession,
    context: &crate::sesame::auth::AuthContext,
    id: ProposalId,
    response: Response,
) -> Response {
    let status = response.status();
    // Why the API refused, for the audit record, which this new page
    // would otherwise lose with the API's response.
    let reason = response
        .extensions()
        .get::<crate::sesame::auth::RefusalReason>()
        .copied();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap_or_default();
    let message = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|body| body["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
    let notice = Notice::Refused(format!("refused ({}): {message}", status.as_u16()));
    let mut page = render_detail(
        state,
        Some(context),
        Some(session),
        id,
        Some(notice),
        status,
    )
    .await;
    // Audited as the token that asked, not the session it rode in on.
    super::audit::refused_as(&mut page, context);
    if let Some(reason) = reason {
        page.extensions_mut().insert(reason);
    }
    page
}

/// A plain refusal page, for a post that never got as far as a proposal.
fn refused(status: StatusCode, message: &str) -> Response {
    let html = format!(
        "{}{}<p class=\"notice notice-error\">{}</p>\n</body>\n</html>\n",
        crate::brioche::app_detail::render_head("Refused"),
        crate::brioche::app_detail::render_nav(),
        crate::brioche::dashboard::escape_html(message)
    );
    let mut response = html_response(html);
    *response.status_mut() = status;
    response
}

/// Whether the browser says this request came from another site or from
/// inside a frame: a `Sec-Fetch-Site` other than `same-origin` (or `none`,
/// a typed navigation), as `relish dashboard`'s proxy asks, an `Origin`
/// whose host isn't the one the request was sent to, or a `Sec-Fetch-Dest`
/// other than `document`. A client that sends none of these (an old
/// browser, a script) still needs the form's anti-forgery token and the
/// person's own API token.
fn cross_site(headers: &HeaderMap) -> bool {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let fetch_site =
        header("sec-fetch-site").is_some_and(|site| !matches!(site, "same-origin" | "none"));
    let origin = header("origin").is_some_and(|origin| {
        let authority = origin.split_once("://").map_or(origin, |(_, rest)| rest);
        Some(authority) != header("host")
    });
    // A form submitted from inside a frame is a clickjack even when it's
    // same-origin: the browser marks it `iframe` (or `frame`, `embed`).
    let framed = header("sec-fetch-dest").is_some_and(|dest| dest != "document");
    fetch_site || origin || framed
}

/// What the session's token may do now, from the store.
async fn write_context(
    state: &ApiState,
    session: &BrowserSession,
) -> Option<crate::sesame::auth::AuthContext> {
    let store = state.token_store.as_ref()?;
    let tokens = store.read().await.clone();
    crate::sesame::auth::session_write_context(&session.principal_id, &tokens)
}

/// Whether Raft names this node the leader. The decision itself still
/// confirms leadership before it writes.
fn named_leader(state: &ApiState) -> bool {
    state.council.as_ref().is_some_and(|council| {
        let metrics = council.metrics();
        let (leader, id) = {
            let metrics = metrics.borrow();
            (metrics.current_leader, metrics.id)
        };
        leader == Some(id)
    })
}

fn row(
    desired: &crate::council::types::DesiredState,
    proposal: &Proposal,
    now_unix_ms: u64,
) -> ProposalRow {
    ProposalRow {
        view: proposal.view(now_unix_ms),
        stale: if proposal.state.is_pending() {
            crate::bun::summary::staleness(desired, proposal, now_unix_ms)
        } else {
            None
        },
    }
}

/// The detail page for `id` as `auth` sees it, with `notice` on top.
async fn render_detail(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    session: Option<&BrowserSession>,
    id: ProposalId,
    notice: Option<Notice>,
    status: StatusCode,
) -> Response {
    let now = crate::testkit::lease::now_unix_millis();
    let Some(council) = &state.council else {
        return refused(
            StatusCode::NOT_FOUND,
            "proposals live in the cluster's council; this node is standalone",
        );
    };
    let found = council
        .read_desired(|desired| {
            desired
                .proposals
                .get(id)
                .filter(|proposal| super::proposals::visible(auth, proposal))
                .map(|proposal| row(desired, proposal, now))
        })
        .await;
    let Some(row) = found else {
        return refused(
            StatusCode::NOT_FOUND,
            &format!("no proposal {id} that your token can see"),
        );
    };
    let actions = actions_for(state, session, &row).await;
    let mut response = html_response(crate::brioche::proposals::render_proposal(
        &row,
        now,
        &actions,
        notice.as_ref(),
    ));
    *response.status_mut() = status;
    response
}

/// Which forms the viewer gets: the ones the API would let its token use.
async fn actions_for(
    state: &ApiState,
    session: Option<&BrowserSession>,
    row: &ProposalRow,
) -> Actions {
    let Some(session) = session else {
        return Actions::default();
    };
    let mut actions = Actions {
        csrf_token: Some(session.csrf_token.clone()),
        ..Actions::default()
    };
    let Some(context) = write_context(state, session).await else {
        return actions;
    };
    let id = ProposalId(row.view.id);
    let context = Some(&context);
    let action = crate::config::PermissionAction::Deploy;
    actions.decide = row.view.state == "pending"
        && super::proposals::authorise_decider(state, context, id, action)
            .await
            .is_ok();
    actions.undo = row.view.state == "applied"
        && row.view.undo.command.is_some()
        && super::proposals::authorise_undoer(state, context, id, action)
            .await
            .is_ok();
    actions.not_leader = (actions.decide || actions.undo) && !named_leader(state);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_body_parses_and_ignores_the_unknown() {
        let form =
            DecisionForm::parse("csrf=abc&token=rbrg_x&reason=too+risky%21&confirm=web&extra=1");
        assert_eq!(form.csrf, "abc");
        assert_eq!(form.token, "rbrg_x");
        assert_eq!(form.reason, "too risky!");
        assert_eq!(form.confirm, "web");
        assert_eq!(DecisionForm::parse("").csrf, "");
    }

    #[test]
    fn only_a_same_origin_or_typed_navigation_is_not_cross_site() {
        let with = |pairs: &[(&'static str, &str)]| {
            let mut headers = HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(*name, value.parse().unwrap());
            }
            headers
        };
        assert!(!cross_site(&HeaderMap::new()));
        assert!(!cross_site(&with(&[("sec-fetch-site", "same-origin")])));
        assert!(!cross_site(&with(&[("sec-fetch-site", "none")])));
        assert!(cross_site(&with(&[("sec-fetch-site", "cross-site")])));
        assert!(cross_site(&with(&[("sec-fetch-site", "same-site")])));
        let host = ("host", "node-a:9117");
        assert!(!cross_site(&with(&[
            host,
            ("origin", "https://node-a:9117")
        ])));
        assert!(cross_site(&with(&[
            host,
            ("origin", "https://evil.example")
        ])));
        assert!(cross_site(&with(&[("origin", "https://node-a:9117")])));
        assert!(!cross_site(&with(&[("sec-fetch-dest", "document")])));
        assert!(cross_site(&with(&[("sec-fetch-dest", "iframe")])));
        assert!(cross_site(&with(&[("sec-fetch-dest", "empty")])));
    }
}
