//! `relish audit`: read the cluster's durable audit log (#685).
//!
//! Each node keeps its own bounded log of who did what. Without `--cluster`
//! this shows the log of the node relish talks to; with it, the node asks
//! every live member and merges their answers by time.

use std::fmt;

use serde::Serialize;

use super::RelishError;
use super::client::BunClient;
use super::output::{OutputFormat, format_output};
use crate::bun::audit::{AuditFilter, AuditListing, AuditRecord};

/// The filters `relish audit` takes, as typed.
#[derive(Debug, Clone, Default)]
pub struct AuditFlags {
    /// Epoch seconds, or a duration before now (`30m`, `2h`, `7d`).
    pub since: Option<String>,
    /// Epoch seconds, or a duration before now.
    pub until: Option<String>,
    /// An action, or a family such as `fault.*`.
    pub action: Option<String>,
    /// A principal id or token name.
    pub principal: Option<String>,
    /// A namespace.
    pub namespace: Option<String>,
    /// How many of the newest records to show.
    pub limit: Option<usize>,
    /// Ask every live node, not just this one.
    pub cluster: bool,
}

impl AuditFlags {
    /// The server-side filter these flags describe, with relative times
    /// resolved against `now_epoch`.
    pub fn filter(&self, now_epoch: u64) -> Result<AuditFilter, RelishError> {
        let time = |flag: &str, value: &Option<String>| -> Result<Option<u64>, RelishError> {
            value
                .as_deref()
                .map(|value| super::commands::parse_time(flag, value, now_epoch))
                .transpose()
        };
        Ok(AuditFilter {
            since: time("since", &self.since)?,
            until: time("until", &self.until)?,
            action: self.action.clone(),
            principal: self.principal.clone(),
            namespace: self.namespace.clone(),
            limit: self.limit,
        })
    }
}

/// `relish audit`.
pub async fn audit(flags: AuditFlags, output: OutputFormat) -> Result<(), RelishError> {
    audit_with_client(flags, output, &BunClient::default_local()).await
}

async fn audit_with_client(
    flags: AuditFlags,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filter = flags.filter(now)?;
    let listing = client.audit(&filter, flags.cluster).await?;
    for warning in &listing.warnings {
        eprintln!("warning: {warning}");
    }
    print!("{}", format_output(&AuditReport(listing), output)?);
    if output == OutputFormat::Human {
        println!();
    }
    Ok(())
}

/// An audit listing, rendered as a table for people.
#[derive(Debug, Serialize)]
#[serde(transparent)]
struct AuditReport(AuditListing);

impl fmt::Display for AuditReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.records.is_empty() {
            return write!(f, "no audit records match");
        }
        write!(
            f,
            "{:<20}  {:<12}  {:<16}  {:<26}  {:<24}  SOURCE",
            "TIME (UTC)", "NODE", "WHO", "ACTION", "TARGET"
        )?;
        for record in &self.0.records {
            write!(
                f,
                "\n{:<20}  {:<12}  {:<16}  {:<26}  {:<24}  {}",
                format_time(record.timestamp),
                record.node.as_deref().unwrap_or("-"),
                who(record),
                record.action,
                target(record),
                record.source_address.as_deref().unwrap_or("-"),
            )?;
        }
        Ok(())
    }
}

/// The token name a record was made under, or its principal.
fn who(record: &AuditRecord) -> &str {
    record
        .details
        .get("token_name")
        .map(String::as_str)
        .unwrap_or(&record.principal)
}

/// `app@namespace`, whichever of them the record has.
fn target(record: &AuditRecord) -> String {
    match (&record.app, &record.namespace) {
        (Some(app), Some(namespace)) => format!("{app}@{namespace}"),
        (Some(app), None) => app.clone(),
        (None, Some(namespace)) => format!("@{namespace}"),
        (None, None) => "-".to_string(),
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC.
fn format_time(timestamp: u64) -> String {
    let Ok(seconds) = i64::try_from(timestamp) else {
        return timestamp.to_string();
    };
    match time::OffsetDateTime::from_unix_timestamp(seconds) {
        Ok(at) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            at.year(),
            u8::from(at.month()),
            at.day(),
            at.hour(),
            at.minute(),
            at.second()
        ),
        Err(_) => timestamp.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::events::{EventKind, EventSeverity};

    fn record() -> AuditRecord {
        AuditRecord {
            sequence: 3,
            timestamp: 1_791_720_000,
            kind: EventKind::Operation,
            severity: EventSeverity::Info,
            action: "app.deleted".to_string(),
            principal: "token-7f3a".to_string(),
            app: Some("web".to_string()),
            namespace: Some("team-a".to_string()),
            node: Some("node-2".to_string()),
            source_address: Some("10.0.0.9:51000".to_string()),
            details: [("token_name".to_string(), "alice".to_string())].into(),
            message: "app.deleted web in team-a by alice".to_string(),
        }
    }

    #[test]
    fn flags_turn_relative_times_into_epoch_seconds() {
        let flags = AuditFlags {
            since: Some("2h".to_string()),
            until: Some("1000".to_string()),
            action: Some("fault.*".to_string()),
            ..AuditFlags::default()
        };
        let filter = flags.filter(10_000).unwrap();
        assert_eq!(filter.since, Some(10_000 - 7200));
        assert_eq!(filter.until, Some(1000));
        assert_eq!(filter.action.as_deref(), Some("fault.*"));
        let bad = AuditFlags {
            since: Some("yesterday".to_string()),
            ..AuditFlags::default()
        };
        assert!(bad.filter(10_000).is_err());
    }

    #[test]
    fn the_table_names_who_did_what_where_and_from_where() {
        let table = AuditReport(AuditListing {
            records: vec![record()],
            warnings: Vec::new(),
        })
        .to_string();
        insta::assert_snapshot!(table);
    }

    #[test]
    fn json_output_is_the_api_listing() {
        let listing = AuditListing {
            records: vec![record()],
            warnings: vec!["node node-3 timed out".to_string()],
        };
        let json = format_output(&AuditReport(listing), OutputFormat::Json).unwrap();
        let parsed: AuditListing = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.records, vec![record()]);
        assert_eq!(parsed.warnings, ["node node-3 timed out"]);
    }
}
