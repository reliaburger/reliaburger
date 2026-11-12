//! `relish lease`: the people's side of leases between agents (#1686).
//!
//! The same leases `relish agent lease` takes, in text by default and JSON
//! with `-o json`. A person takes one to keep agents out while they work
//! (`relish lease take namespace shop --for 15m`), lists who holds what, and
//! breaks a lease that's in their way. A break is audited with who broke
//! whose lease.

use std::time::Duration;

use crate::proposal::lease::{
    LeaseAnswer, LeaseListing, LeaseTargetRequest, LeaseView, RenewLeaseRequest, TakeLeaseRequest,
};

use super::RelishError;
use super::agent::lease::TargetArgs;
use super::client::BunClient;
use super::output::{OutputFormat, print_output};

/// `relish lease list`: every live lease the caller's scope reaches.
pub async fn lease_list(output: OutputFormat) -> Result<(), RelishError> {
    let listing = BunClient::default_local().leases().await?;
    print_output(&Listing(listing), output)
}

/// `relish lease take`: take a lease, or renew one you hold.
pub async fn lease_take(
    target: &TargetArgs,
    purpose: &str,
    ttl: Option<Duration>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let request = TakeLeaseRequest {
        target: target_of(target)?,
        purpose: purpose.to_string(),
        ttl_seconds: ttl.map(|ttl| ttl.as_secs()),
    };
    let answer = BunClient::default_local()
        .lease_take(&request)
        .await
        .map_err(plain_error)?;
    print_output(&Changed(answer), output)
}

/// `relish lease renew`: extend a lease you hold.
pub async fn lease_renew(
    target: &TargetArgs,
    ttl: Option<Duration>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let request = RenewLeaseRequest {
        target: target_of(target)?,
        ttl_seconds: ttl.map(|ttl| ttl.as_secs()),
    };
    let answer = BunClient::default_local()
        .lease_renew(&request)
        .await
        .map_err(plain_error)?;
    print_output(&Changed(answer), output)
}

/// `relish lease release`: give up a lease you hold.
pub async fn lease_release(target: &TargetArgs, output: OutputFormat) -> Result<(), RelishError> {
    let request = LeaseTargetRequest {
        target: target_of(target)?,
    };
    let answer = BunClient::default_local()
        .lease_release(&request)
        .await
        .map_err(plain_error)?;
    print_output(&Changed(answer), output)
}

/// `relish lease break`: end anyone's lease on the target. Audited.
pub async fn lease_break(target: &TargetArgs, output: OutputFormat) -> Result<(), RelishError> {
    let request = LeaseTargetRequest {
        target: target_of(target)?,
    };
    let answer = BunClient::default_local()
        .lease_break(&request)
        .await
        .map_err(plain_error)?;
    print_output(&Changed(answer), output)
}

fn target_of(target: &TargetArgs) -> Result<crate::proposal::lease::LeaseTarget, RelishError> {
    target.target().map_err(|error| RelishError::InvalidFlag {
        flag: "--namespace".to_string(),
        reason: error.message,
    })
}

/// The API's own sentence for a refused lease write, not its JSON.
fn plain_error(error: RelishError) -> RelishError {
    let RelishError::ApiError { status, body } = &error else {
        return error;
    };
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(answer) => match answer["error"].as_str() {
            Some(message) => RelishError::ApiError {
                status: *status,
                body: message.to_string(),
            },
            None => error,
        },
        Err(_) => error,
    }
}

/// What a write did, printed as one sentence.
#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
struct Changed(LeaseAnswer);

impl std::fmt::Display for Changed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lease = &self.0.lease;
        match self.0.outcome.as_str() {
            "released" => writeln!(f, "released the lease on {}", lease.target),
            "broken" => writeln!(f, "broke {}'s lease on {}", lease.holder, lease.target),
            outcome => writeln!(
                f,
                "{outcome} the lease on {} for {}s",
                lease.target, lease.expires_in_seconds
            ),
        }
    }
}

/// The table `relish lease list` prints.
#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
struct Listing(LeaseListing);

impl std::fmt::Display for Listing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", render_lease_list(&self.0.leases))
    }
}

/// The human view of `relish lease list`.
pub fn render_lease_list(leases: &[LeaseView]) -> String {
    if leases.is_empty() {
        return "No leases.\n".to_string();
    }
    let target_width = leases
        .iter()
        .map(|lease| lease.target.len())
        .max()
        .unwrap_or(0)
        .max("TARGET".len());
    let holder_width = leases
        .iter()
        .map(|lease| lease.holder.len())
        .max()
        .unwrap_or(0)
        .max("HOLDER".len());
    let mut out = format!(
        "{:target_width$}  {:holder_width$}  LEFT   PURPOSE\n",
        "TARGET", "HOLDER"
    );
    for lease in leases {
        let mut purpose = lease.purpose.clone();
        if let Some(proposal) = lease.proposal {
            purpose = format!("(proposal {proposal}) {purpose}");
        }
        out.push_str(&format!(
            "{:target_width$}  {:holder_width$}  {:>4}s  {purpose}\n",
            lease.target, lease.holder, lease.expires_in_seconds
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proposal::lease::LeaseKind;

    fn view(target: &str, holder: &str, left: u64, proposal: Option<u64>) -> LeaseView {
        LeaseView {
            target: target.to_string(),
            kind: LeaseKind::App,
            namespace: "shop".to_string(),
            app: Some("web".to_string()),
            holder: holder.to_string(),
            purpose: "investigating".to_string(),
            proposal,
            acquired_at_unix_ms: 0,
            expires_at_unix_ms: 0,
            expires_in_seconds: left,
        }
    }

    #[test]
    fn the_list_shows_target_holder_time_left_and_purpose() {
        assert_eq!(render_lease_list(&[]), "No leases.\n");
        insta::assert_snapshot!(render_lease_list(&[
            view("app:shop/web", "holmes", 87, None),
            view("namespace:billing", "alice", 840, Some(17)),
        ]));
    }

    #[test]
    fn a_refusal_prints_the_apis_sentence() {
        let error = plain_error(RelishError::ApiError {
            status: 409,
            body: r#"{"error":"app:shop/web is held by holmes","code":"lease_held"}"#.to_string(),
        });
        assert_eq!(
            error.to_string(),
            "API error (status 409): app:shop/web is held by holmes"
        );
    }
}
