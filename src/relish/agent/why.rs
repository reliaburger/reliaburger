//! `relish agent explain --restart | --placement | --refusal | --rules`:
//! why the cluster made a decision.
//!
//! A finding (`relish agent explain crashloop`) says what `wtf` thinks is
//! wrong. A decision is something the cluster did: it put `web-1` on
//! `node-2`, it restarted `web-0`, it refused a deploy. The node or leader
//! that decided recorded the rule and the evidence on the spot
//! ([`crate::bun::decisions`]); this module reads those records back from
//! the events ring and the audit log, through the same merge as the
//! timeline, and says what the rule means and where in the source it's
//! decided.
//!
//! A decision the records no longer hold is reported as not found, with
//! how far back the records reach. It's never reconstructed or guessed.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Serialize;

use super::budget::Rendered;
use super::diagnose::{
    ExplainData, ManualPointer, SourceSite, load_bundle, manual_pointer, show_step,
};
use super::envelope::{AgentError, BuildInfo, CommandName, ErrorCode, NextStep};
use super::source::{Answer, Bundle, ItemKind, enclosing_items, render_answer};
use super::timeline::{self, Scope, SourceState, TimelineEntry, Window};
use super::untrusted::Untrusted;
use crate::bun::decisions::{self, DecisionKind, RULES};
use crate::relish::client::BunClient;

/// The file that holds the rule catalogue. It names every rule, so it's
/// skipped when looking for where one is decided.
pub const CATALOGUE_FILE: &str = "src/bun/decisions.rs";

/// The most decisions one answer lists, newest first.
pub const MAX_DECISIONS: usize = 10;

/// The most related events a decision carries.
const MAX_RELATED: usize = 5;

/// How far before a decision a related event may be, in seconds.
const RELATED_SECONDS: u64 = 120;

/// `relish agent explain`'s flags: a finding id, or one decision question.
#[derive(Debug, Clone, clap::Args)]
pub struct ExplainArgs {
    /// A finding id, e.g. `crashloop`. Reads only this binary.
    #[arg(conflicts_with_all = ["restart", "placement", "refusal", "rules"])]
    pub id: Option<String>,
    /// Why this instance restarted: `web-0` or `default__web-0`.
    #[arg(long, value_name = "INSTANCE", conflicts_with_all = ["placement", "refusal", "rules"])]
    pub restart: Option<String>,
    /// Why this instance is on its node, or why it has none.
    #[arg(long, value_name = "INSTANCE", conflicts_with_all = ["refusal", "rules"])]
    pub placement: Option<String>,
    /// Why deploys of this app were refused.
    #[arg(long, value_name = "APP", conflicts_with = "rules")]
    pub refusal: Option<String>,
    /// Every decision rule: what it means, its evidence and where it's
    /// decided. Reads only this binary.
    #[arg(long)]
    pub rules: bool,
    /// The namespace of the instance or app.
    #[arg(long, default_value = "default")]
    pub namespace: String,
}

/// A decision question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Question {
    /// Why an instance restarted.
    Restart(Instance),
    /// Why an instance is where it is.
    Placement(Instance),
    /// Why an app's deploys were refused.
    Refusal { app: String, namespace: String },
}

/// An instance, by its full id and its app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// The full id, `namespace__app-ordinal`.
    pub id: String,
    /// Its app.
    pub app: String,
    /// Its namespace.
    pub namespace: String,
}

impl Instance {
    /// The instance `name` names in `namespace`: `web-0` or the full
    /// `default__web-0`, whose namespace wins.
    pub fn parse(name: &str, namespace: &str) -> Result<Self, AgentError> {
        let id = if name.contains("__") {
            name.to_string()
        } else {
            format!("{namespace}__{name}")
        };
        let identity = crate::grill::InstanceIdentity::parse(&id).ok_or_else(|| {
            AgentError::new(ErrorCode::Usage, format!("{name:?} isn't an instance name"))
                .with_hint("name it as the app and its ordinal, e.g. web-0 or default__web-0")
        })?;
        Ok(Instance {
            id,
            app: identity.app,
            namespace: identity.namespace,
        })
    }
}

impl Question {
    /// The question `args` ask, or `None` when they ask about a finding.
    pub fn from_args(args: &ExplainArgs) -> Result<Option<Self>, AgentError> {
        if let Some(name) = &args.restart {
            return Ok(Some(Question::Restart(Instance::parse(
                name,
                &args.namespace,
            )?)));
        }
        if let Some(name) = &args.placement {
            return Ok(Some(Question::Placement(Instance::parse(
                name,
                &args.namespace,
            )?)));
        }
        Ok(args.refusal.as_ref().map(|app| Question::Refusal {
            app: app.clone(),
            namespace: args.namespace.clone(),
        }))
    }

    fn app(&self) -> (&str, &str) {
        match self {
            Question::Restart(instance) | Question::Placement(instance) => {
                (&instance.app, &instance.namespace)
            }
            Question::Refusal { app, namespace } => (app, namespace),
        }
    }

    fn kind(&self) -> DecisionKind {
        match self {
            Question::Restart(_) => DecisionKind::Restart,
            Question::Placement(_) => DecisionKind::Placement,
            Question::Refusal { .. } => DecisionKind::Refusal,
        }
    }

    /// The rule of `entry`'s decision, when it's one this question is about.
    fn asks_about(&self, entry: &TimelineEntry) -> Option<&'static decisions::DecisionRule> {
        let rule = entry.rule.as_deref().and_then(decisions::rule)?;
        if rule.kind != self.kind() {
            return None;
        }
        let about = match self {
            Question::Restart(instance) => entry.details.get("instance") == Some(&instance.id),
            // A placement rule with no per-instance evidence is about the
            // whole app: why an instance has no node yet.
            Question::Placement(instance) => {
                entry.details.contains_key(&instance.id)
                    || !rule.evidence.iter().any(|(key, _)| *key == "<instance>")
            }
            Question::Refusal { .. } => true,
        };
        about.then_some(rule)
    }

    fn describe(&self) -> String {
        match self {
            Question::Restart(instance) => format!("restart of {}", instance.id),
            Question::Placement(instance) => format!("placement of {}", instance.id),
            Question::Refusal { app, namespace } => {
                format!("refused deploy of {app} in {namespace}")
            }
        }
    }
}

/// One decision the cluster made, with the rule and the evidence.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DecisionData {
    /// What it was about.
    pub kind: DecisionKind,
    /// The rule's stable id, e.g. `restart.health_failed`.
    pub rule: String,
    /// What the rule says.
    pub meaning: String,
    /// When, in Unix seconds.
    pub timestamp: u64,
    /// The app.
    pub app: Option<String>,
    /// The app's namespace.
    pub namespace: Option<String>,
    /// The node that decided: the leader for a placement, the instance's
    /// node for a restart, the API node for a refusal.
    pub node: Option<String>,
    /// Who asked, for a refused deploy.
    pub principal: Option<String>,
    /// The evidence the rule compared, one value per key; the rule's
    /// `evidence` in `--rules` says what each key means.
    pub evidence: BTreeMap<String, String>,
    /// The record's own message, which can quote an app.
    pub message: Untrusted,
    /// Where the rule is decided, when the source bundle could be read.
    pub decided_in: Option<SourceSite>,
    /// The record, as the timeline names it.
    pub reference: String,
    /// What else happened to the same app on the same node in the two
    /// minutes before, newest first.
    pub related: Vec<RelatedEvent>,
}

/// An event shortly before a decision.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RelatedEvent {
    /// When, in Unix seconds.
    pub timestamp: u64,
    /// Its kind, e.g. `health`.
    pub kind: String,
    /// What it says.
    pub message: Untrusted,
}

/// One decision rule.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuleData {
    /// The rule's stable id.
    pub id: String,
    /// What it's about.
    pub kind: DecisionKind,
    /// What it says.
    pub meaning: String,
    /// The evidence it records.
    pub evidence: Vec<EvidenceKey>,
    /// Where it's decided, when the source bundle could be read.
    pub decided_in: Option<SourceSite>,
    /// The manual section that covers it.
    pub manual: Option<ManualPointer>,
}

/// One evidence key a rule records.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EvidenceKey {
    /// The key; `<instance>` stands for an instance id.
    pub key: String,
    /// What its value says.
    pub meaning: String,
}

/// Every line under `src/`, outside tests and the catalogue, that names the
/// rule `id` as a string literal, with the function each sits in.
pub fn rule_sites(bundle: &Bundle, id: &str) -> Vec<SourceSite> {
    let needle = format!("\"{id}\"");
    bundle
        .texts()
        .filter(|(path, _)| is_production_source(path))
        .flat_map(|(path, text)| {
            let lines = production_lines(text);
            lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.contains(&needle))
                .map(|(index, _)| {
                    let line = index + 1;
                    let function = enclosing_items(&lines, line)
                        .into_iter()
                        .rev()
                        .find(|item| item.kind == ItemKind::Fn);
                    SourceSite {
                        path: path.to_string(),
                        line,
                        function_line: function.as_ref().map(|item| item.line),
                        function: function.map(|item| item.name),
                        permalink: bundle.permalink(path, &format!("L{line}")),
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The lines of `text` before its test module. Unlike the finding search,
/// a `#[cfg(test)]` on a single helper doesn't end the file: only one on a
/// `mod`.
fn production_lines(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    let end = lines
        .windows(2)
        .position(|pair| {
            pair[0].trim() == "#[cfg(test)]" && pair[1].trim_start().starts_with("mod ")
        })
        .unwrap_or(lines.len());
    lines[..end].to_vec()
}

/// Whether `path` is Rust that ships: under `src/`, not the catalogue and
/// not a file of tests.
fn is_production_source(path: &str) -> bool {
    path.starts_with("src/")
        && path.ends_with(".rs")
        && path != CATALOGUE_FILE
        && !path.ends_with("tests.rs")
        && !path.contains("/tests/")
}

/// Where `id` is decided: the first site that names it.
pub fn decided_in(bundle: &Bundle, id: &str) -> Option<SourceSite> {
    rule_sites(bundle, id).into_iter().next()
}

/// Every rule, as `--rules` lists it.
pub fn rules(bundle: Option<&Bundle>) -> Answer<ExplainData> {
    let rules = RULES
        .iter()
        .map(|rule| RuleData {
            id: rule.id.to_string(),
            kind: rule.kind,
            meaning: rule.meaning.to_string(),
            evidence: rule
                .evidence
                .iter()
                .map(|(key, meaning)| EvidenceKey {
                    key: key.to_string(),
                    meaning: meaning.to_string(),
                })
                .collect(),
            decided_in: bundle.and_then(|bundle| decided_in(bundle, rule.id)),
            manual: bundle.and_then(|bundle| manual_pointer(bundle, rule.id)),
        })
        .collect();
    Answer {
        data: ExplainData {
            rules,
            ..ExplainData::default()
        },
        next: Vec::new(),
        warnings: Vec::new(),
    }
}

/// Answer `question` from what the cluster recorded. Pure, so the tests
/// run it over fixtures.
pub fn decisions(
    question: &Question,
    inputs: timeline::Inputs,
    bundle: Option<&Bundle>,
) -> Answer<ExplainData> {
    let (app, namespace) = question.app();
    let scope = Scope {
        window: Window {
            since: 0,
            until: u64::MAX,
        },
        app: Some(app),
        namespace: Some(namespace),
        limit: usize::MAX,
    };
    let merged = timeline::timeline(inputs, &scope).data;
    let entries = merged.entries;
    let found: Vec<DecisionData> = entries
        .iter()
        .filter_map(|entry| Some((entry, question.asks_about(entry)?)))
        .take(MAX_DECISIONS)
        .map(|(entry, rule)| decision(entry, rule, &entries, bundle))
        .collect();
    let sources: Vec<SourceState> = merged.sources;
    let not_found = found.is_empty().then(|| not_found(question, &sources));
    let mut next = Vec::new();
    if let Some(newest) = found.first() {
        next.push(NextStep {
            run: format!(
                "relish agent timeline --app {} --namespace {} --since {} --until {}",
                super::shell_quote(app),
                super::shell_quote(namespace),
                newest.timestamp.saturating_sub(600),
                newest.timestamp
            ),
            why: "what else changed in the ten minutes before it".to_string(),
            read_only: true,
        });
        if let Some(site) = &newest.decided_in {
            next.push(show_step(
                &site.path,
                site.function_line.unwrap_or(site.line),
                super::diagnose::CODE_STEP_WHY,
            ));
        }
    } else {
        next.push(NextStep {
            run: format!(
                "relish agent timeline --app {} --namespace {}",
                super::shell_quote(app),
                super::shell_quote(namespace)
            ),
            why: format!("everything recorded about {app} in the last hour"),
            read_only: true,
        });
    }
    Answer {
        data: ExplainData {
            decisions: found,
            sources,
            not_found,
            ..ExplainData::default()
        },
        next,
        warnings: Vec::new(),
    }
}

/// Why there's no answer: what was read, and how far back it reaches.
fn not_found(question: &Question, sources: &[SourceState]) -> String {
    let reach = sources
        .iter()
        .filter_map(|source| source.oldest)
        .min()
        .map_or_else(
            || "no records of this app were found at all".to_string(),
            |oldest| format!("the records read reach back to {oldest}"),
        );
    format!(
        "no {} is recorded; {reach}. Each node keeps its newest {} events in memory and \
         forgets them on restart, so an older decision may have left the records: unknown, \
         not \"didn't happen\"",
        question.describe(),
        crate::bun::events::EVENT_BUFFER_CAP
    )
}

fn decision(
    entry: &TimelineEntry,
    rule: &decisions::DecisionRule,
    entries: &[TimelineEntry],
    bundle: Option<&Bundle>,
) -> DecisionData {
    let mut evidence = entry.details.clone();
    evidence.remove(decisions::RULE_KEY);
    let related = entries
        .iter()
        .filter(|other| {
            other.reference != entry.reference
                && other.node == entry.node
                && other.timestamp <= entry.timestamp
                && entry.timestamp - other.timestamp <= RELATED_SECONDS
        })
        .take(MAX_RELATED)
        .map(|other| RelatedEvent {
            timestamp: other.timestamp,
            kind: other.kind.clone(),
            message: other.message.clone(),
        })
        .collect();
    DecisionData {
        kind: rule.kind,
        meaning: rule.meaning.to_string(),
        decided_in: bundle.and_then(|bundle| decided_in(bundle, rule.id)),
        rule: rule.id.to_string(),
        timestamp: entry.timestamp,
        app: entry.app.clone(),
        namespace: entry.namespace.clone(),
        node: entry.node.clone(),
        principal: entry.principal.clone(),
        evidence,
        message: entry.message.clone(),
        reference: entry.reference.clone(),
        related,
    }
}

/// Run `relish agent explain` with its flags.
pub async fn run(args: &ExplainArgs, max_bytes: usize, rerun: &str) -> Rendered {
    if args.rules {
        let (bundle, warnings) = load_bundle();
        let build = bundle
            .as_ref()
            .map_or_else(BuildInfo::current, Bundle::build_info);
        return render_answer(
            CommandName::Explain,
            build,
            Ok(rules(bundle.as_ref())),
            &warnings,
            max_bytes,
            rerun,
        );
    }
    let question = match Question::from_args(args) {
        Ok(Some(question)) => question,
        Ok(None) => return super::diagnose::run_explain(args.id.as_deref(), max_bytes, rerun),
        Err(error) => {
            return render_answer::<()>(
                CommandName::Explain,
                BuildInfo::current(),
                Err(error),
                &[],
                max_bytes,
                rerun,
            );
        }
    };
    let (app, namespace) = question.app();
    let scope = Scope {
        window: Window {
            since: 0,
            until: timeline::now(),
        },
        app: Some(app),
        namespace: Some(namespace),
        limit: usize::MAX,
    };
    let inputs = timeline::collect(&BunClient::default_local(), &scope).await;
    let inputs = timeline::Inputs {
        deploys: None,
        ..inputs
    };
    if let (Err(error), Err(_)) = (&inputs.events, &inputs.audit) {
        return render_answer::<()>(
            CommandName::Explain,
            BuildInfo::current(),
            Err(AgentError::from(error)),
            &[],
            max_bytes,
            rerun,
        );
    }
    let (bundle, warnings) = load_bundle();
    let build = bundle
        .as_ref()
        .map_or_else(BuildInfo::current, Bundle::build_info);
    render_answer(
        CommandName::Explain,
        build,
        Ok(decisions(&question, inputs, bundle.as_ref())),
        &warnings,
        max_bytes,
        rerun,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::audit::{AuditListing, AuditRecord};
    use crate::bun::cluster_view::ClusterEvents;
    use crate::bun::events::{ClusterEvent, EventKind, EventSeverity};
    use crate::relish::RelishError;
    use crate::relish::agent::DEFAULT_MAX_BYTES;
    use serde_json::Value;

    const NOW: u64 = 2_000_000;

    fn event(
        sequence: u64,
        timestamp: u64,
        kind: EventKind,
        message: &str,
        details: BTreeMap<String, String>,
    ) -> ClusterEvent {
        ClusterEvent {
            sequence,
            timestamp,
            kind,
            severity: EventSeverity::Warning,
            app: Some("web".to_string()),
            namespace: Some("default".to_string()),
            node: Some("node-1".to_string()),
            action: None,
            principal: None,
            source_address: None,
            details,
            message: message.to_string(),
        }
    }

    fn health_restart(sequence: u64, timestamp: u64, instance: &str) -> ClusterEvent {
        let decision = crate::bun::restart::restart_decision(
            "restart.health_failed",
            instance,
            2,
            &crate::bun::restart::RestartPolicy::default(),
        )
        .with("consecutive_failures", 3)
        .with("threshold", 3)
        .with("probe", "http port 8080 path /healthz")
        .with("interval_seconds", 10);
        event(
            sequence,
            timestamp,
            EventKind::Restart,
            &format!("instance {instance} restarted (attempt 2)"),
            decision.into_details(),
        )
    }

    fn inputs(events: Vec<ClusterEvent>, records: Vec<AuditRecord>) -> timeline::Inputs {
        timeline::Inputs {
            events: Ok(ClusterEvents {
                events,
                warnings: Vec::new(),
            }),
            audit: Ok(AuditListing {
                records,
                warnings: Vec::new(),
            }),
            deploys: None,
        }
    }

    fn restart_question() -> Question {
        Question::Restart(Instance::parse("web-0", "default").unwrap())
    }

    fn render(answer: Answer<ExplainData>) -> Value {
        let rendered = render_answer(
            CommandName::Explain,
            BuildInfo {
                version: "0.2.8".to_string(),
                commit: None,
                dirty: None,
                source_digest: None,
            },
            Ok(answer),
            &[],
            DEFAULT_MAX_BYTES,
            "relish agent explain",
        );
        serde_json::from_str(&rendered.json).unwrap()
    }

    fn fixture_bundle() -> Bundle {
        super::super::diagnose::fixture::bundle_with(&[(
            "src/bun/agent/health_checks.rs",
            "fn restart_published_unhealthy() {\n    let rule = \"restart.health_failed\";\n}\n",
        )])
    }

    #[test]
    fn instances_are_named_short_or_in_full() {
        let short = Instance::parse("web-0", "payments").unwrap();
        assert_eq!(short.id, "payments__web-0");
        assert_eq!(
            (short.app.as_str(), short.namespace.as_str()),
            ("web", "payments")
        );
        let full = Instance::parse("default__api-12", "payments").unwrap();
        assert_eq!(full.id, "default__api-12");
        assert_eq!(full.namespace, "default");
        let error = Instance::parse("web", "default").unwrap_err();
        assert_eq!(error.code, ErrorCode::Usage);
    }

    #[test]
    fn snapshot_of_a_restart_decision() {
        let unhealthy = event(
            1,
            NOW - 30,
            EventKind::Health,
            "instance default__web-0 became unhealthy",
            BTreeMap::new(),
        );
        let bundle = fixture_bundle();
        let answer = decisions(
            &restart_question(),
            inputs(
                vec![unhealthy, health_restart(2, NOW, "default__web-0")],
                Vec::new(),
            ),
            Some(&bundle),
        );
        insta::assert_json_snapshot!(render(answer));
    }

    #[test]
    fn a_restart_answer_gives_the_rule_the_evidence_and_what_came_before() {
        let unhealthy = event(
            1,
            NOW - 30,
            EventKind::Health,
            "instance default__web-0 became unhealthy",
            BTreeMap::new(),
        );
        let other = health_restart(3, NOW - 5, "default__web-1");
        let bundle = fixture_bundle();
        let answer = decisions(
            &restart_question(),
            inputs(
                vec![unhealthy, other, health_restart(2, NOW, "default__web-0")],
                Vec::new(),
            ),
            Some(&bundle),
        );
        let data = &answer.data;
        assert_eq!(data.decisions.len(), 1, "web-1's restart isn't web-0's");
        let decision = &data.decisions[0];
        assert_eq!(decision.rule, "restart.health_failed");
        assert_eq!(decision.kind, DecisionKind::Restart);
        assert!(decision.meaning.contains("health check"));
        assert_eq!(decision.evidence["threshold"], "3");
        assert_eq!(decision.evidence["consecutive_failures"], "3");
        assert!(!decision.evidence.contains_key("rule"));
        let site = decision.decided_in.as_ref().unwrap();
        assert_eq!(site.path, "src/bun/agent/health_checks.rs");
        assert_eq!(
            site.function.as_deref(),
            Some("restart_published_unhealthy")
        );
        let related: Vec<&str> = decision.related.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(related, ["restart", "health"]);
        assert!(data.not_found.is_none());
        assert!(
            answer.next[0]
                .run
                .starts_with("relish agent timeline --app web")
        );
        // The decision already names the rule: the code is for checking it.
        let code = answer.next.last().unwrap();
        assert!(code.run.starts_with("relish agent source show"));
        assert!(code.why.starts_with("optional"), "{}", code.why);
    }

    #[test]
    fn a_restart_the_records_dont_hold_is_unknown_not_denied() {
        let answer = decisions(
            &restart_question(),
            inputs(
                vec![event(
                    1,
                    NOW - 900,
                    EventKind::Deploy,
                    "deployed",
                    BTreeMap::new(),
                )],
                Vec::new(),
            ),
            None,
        );
        assert!(answer.data.decisions.is_empty());
        let note = answer.data.not_found.unwrap();
        assert!(
            note.starts_with("no restart of default__web-0 is recorded"),
            "{note}"
        );
        assert!(
            note.contains(&format!("reach back to {}", NOW - 900)),
            "{note}"
        );
        assert!(note.contains("unknown, not \"didn't happen\""), "{note}");
    }

    #[test]
    fn a_placement_answer_names_the_instances_node_and_score() {
        let placed = crate::bun::decisions::Decision::new("placement.scored")
            .with("wanted", 3)
            .with("kept", 2)
            .with(
                "default__web-1",
                "node-2: 0 of its replicas there, score 72 (bin-pack 60, labels 100, image 0, \
                 uptime 100); 3 nodes passed the filter; runner-up node-3 (1 of its replicas \
                 there, score 70)",
            );
        let mut placement = event(
            4,
            NOW - 60,
            EventKind::Placement,
            "placed default/web: default__web-1 on node-2",
            placed.into_details(),
        );
        placement.node = Some("node-leader".to_string());
        let question = Question::Placement(Instance::parse("web-1", "default").unwrap());
        let answer = decisions(&question, inputs(vec![placement], Vec::new()), None);
        let decision = &answer.data.decisions[0];
        assert_eq!(decision.rule, "placement.scored");
        assert!(decision.evidence["default__web-1"].starts_with("node-2: "));
        assert_eq!(decision.node.as_deref(), Some("node-leader"));
    }

    #[test]
    fn a_refusal_answer_reads_the_audit_log() {
        let details = crate::bun::decisions::Decision::new("deploy.refused.policy")
            .with("policies", "no-latest")
            .with("workloads", "web@default")
            .into_details();
        let record = AuditRecord {
            sequence: 9,
            timestamp: NOW - 120,
            kind: EventKind::Operation,
            severity: EventSeverity::Warning,
            action: "policy.denied".to_string(),
            principal: "token:ci".to_string(),
            principal_kind: crate::sesame::types::PrincipalKind::Human,
            app: Some("web".to_string()),
            namespace: Some("default".to_string()),
            node: Some("node-1".to_string()),
            source_address: None,
            details,
            message: "apply refused by policy: policy no-latest denies app default/web".to_string(),
        };
        let question = Question::Refusal {
            app: "web".to_string(),
            namespace: "default".to_string(),
        };
        let answer = decisions(&question, inputs(Vec::new(), vec![record]), None);
        let decision = &answer.data.decisions[0];
        assert_eq!(decision.rule, "deploy.refused.policy");
        assert_eq!(decision.principal.as_deref(), Some("token:ci"));
        assert_eq!(decision.evidence["policies"], "no-latest");
        assert_eq!(decision.reference, "audit node-1#9");
    }

    #[test]
    fn an_unreadable_audit_log_is_named_in_the_sources() {
        let mut inputs = inputs(vec![health_restart(1, NOW, "default__web-0")], Vec::new());
        inputs.audit = Err(RelishError::RequestTimeout);
        let answer = decisions(&restart_question(), inputs, None);
        assert_eq!(answer.data.decisions.len(), 1);
        assert_eq!(
            answer.data.sources[1].state,
            super::super::context::SectionState::Unknown
        );
    }

    #[test]
    fn decisions_are_newest_first_and_bounded() {
        let events = (0..30)
            .map(|index| health_restart(index, NOW - 1000 + index, "default__web-0"))
            .collect();
        let answer = decisions(&restart_question(), inputs(events, Vec::new()), None);
        assert_eq!(answer.data.decisions.len(), MAX_DECISIONS);
        assert_eq!(answer.data.decisions[0].timestamp, NOW - 1000 + 29);
    }

    #[test]
    fn a_finding_id_and_a_decision_flag_dont_mix() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: ExplainArgs,
        }
        assert!(Cli::try_parse_from(["explain", "crashloop", "--restart", "web-0"]).is_err());
        assert!(Cli::try_parse_from(["explain", "--restart", "web-0", "--rules"]).is_err());
        let cli = Cli::try_parse_from(["explain", "--refusal", "web"]).unwrap();
        assert_eq!(
            Question::from_args(&cli.args).unwrap(),
            Some(Question::Refusal {
                app: "web".to_string(),
                namespace: "default".to_string()
            })
        );
    }

    // The next tests read the real source bundle: they keep the catalogue,
    // the code that records each rule and the manual in step.

    fn real_bundle() -> Bundle {
        Bundle::load().expect("the source bundle")
    }

    #[test]
    fn every_rule_is_decided_in_one_function() {
        let bundle = real_bundle();
        for rule in RULES {
            let sites = rule_sites(&bundle, rule.id);
            assert!(!sites.is_empty(), "nothing records {}", rule.id);
            let functions: std::collections::BTreeSet<(&str, Option<&str>)> = sites
                .iter()
                .map(|site| (site.path.as_str(), site.function.as_deref()))
                .collect();
            assert_eq!(functions.len(), 1, "{} is decided in {sites:?}", rule.id);
        }
    }

    #[test]
    fn every_rule_the_code_records_is_in_the_catalogue() {
        let bundle = real_bundle();
        let mut recorded = std::collections::BTreeSet::new();
        for (path, text) in bundle.texts() {
            if !is_production_source(path) {
                continue;
            }
            for line in production_lines(text) {
                for literal in line.split('"').skip(1).step_by(2) {
                    let rule_like = ["placement.", "restart.", "deploy.refused."]
                        .iter()
                        .any(|prefix| literal.starts_with(prefix))
                        && literal
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_');
                    if rule_like {
                        recorded.insert(literal.to_string());
                    }
                }
            }
        }
        assert!(recorded.len() >= RULES.len(), "the scan found {recorded:?}");
        for id in recorded {
            assert!(
                decisions::rule(&id).is_some(),
                "{id} isn't in the catalogue"
            );
        }
    }

    #[test]
    fn every_rule_is_in_the_manual() {
        let bundle = real_bundle();
        for rule in RULES {
            assert!(
                manual_pointer(&bundle, rule.id).is_some(),
                "docs/manual doesn't mention `{}`",
                rule.id
            );
        }
    }

    #[test]
    fn rules_lists_every_rule_with_its_evidence() {
        let answer = rules(None);
        let ids: Vec<&str> = answer
            .data
            .rules
            .iter()
            .map(|rule| rule.id.as_str())
            .collect();
        let all: Vec<&str> = RULES.iter().map(|rule| rule.id).collect();
        assert_eq!(ids, all);
        assert!(
            answer
                .data
                .rules
                .iter()
                .all(|rule| !rule.evidence.is_empty())
        );
    }
}
