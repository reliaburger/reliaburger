//! The policies we publish judge the way their comments say (#1126, #1311).
//!
//! `examples/policies/platform.toml` holds the four policies platform
//! teams ask for most; the 0.2.4 exit test applies it to a real cluster.
//! The V02 soak workload carries a deny, a warn and a default, and none of
//! them may refuse a soak workload, or the soak would fail at its first
//! apply. Both are checked here offline, with the pure function the leader
//! runs, the way `relish policy test` checks them.

use reliaburger::config::Config;
use reliaburger::policy::{Caller, Evaluation, PolicySet, Source, evaluate};
use reliaburger::sesame::types::ApiRole;

const PLATFORM: &str = include_str!("../../examples/policies/platform.toml");
const SOAK_WORKLOADS: &str = include_str!("../../scripts/release/sustained/soak-workloads.toml");
const SOAK_JOBS: &str = include_str!("../../scripts/release/sustained/soak-jobs.toml");

/// Judge `workloads` under the policies in `policies`, as an admin's apply.
fn judge(policies: &str, workloads: &str) -> Evaluation {
    let policies = Config::parse(policies).unwrap();
    policies.validate().unwrap();
    let set = PolicySet::compile(&policies.policy).unwrap();
    let workloads = Config::parse(workloads).unwrap();
    evaluate(
        &set,
        &workloads,
        &Caller::token("operator", ApiRole::Admin),
        Source::Apply,
    )
}

fn denied_by(evaluation: &Evaluation) -> Vec<String> {
    evaluation
        .verdict
        .denials
        .iter()
        .map(|finding| finding.policy.clone())
        .collect()
}

#[test]
fn the_platform_policies_commit_as_written() {
    let config = Config::parse(PLATFORM).unwrap();
    config.validate().unwrap();
    assert_eq!(
        config.policy.keys().collect::<Vec<_>>(),
        vec![
            "no-latest",
            "prod-needs-memory-limits",
            "team-env",
            "trusted-registries"
        ]
    );
}

#[test]
fn the_platform_policies_decide_each_case_as_their_comments_say() {
    // (case, workload, the policies that refuse it)
    let rows: [(&str, &str, &[&str]); 14] = [
        (
            "a prod app with a limit and a pinned, trusted image",
            "[app.web]\nnamespace = 'prod'\nimage = 'registry.example.com/web:1.4'\nmemory = '256Mi-1Gi'\n",
            &[],
        ),
        (
            "a prod app without a memory limit",
            "[app.web]\nnamespace = 'prod'\nimage = 'registry.example.com/web:1.4'\n",
            &["prod-needs-memory-limits"],
        ),
        (
            "an app outside prod needs no limit",
            "[app.web]\nimage = 'registry.example.com/web:1.4'\n",
            &[],
        ),
        (
            ":latest",
            "[app.web]\nimage = 'registry.example.com/web:latest'\n",
            &["no-latest"],
        ),
        (
            "no tag at all",
            "[app.web]\nimage = 'registry.example.com/web'\n",
            &["no-latest"],
        ),
        (
            ":latest pinned to a digest is still :latest",
            "[app.web]\nimage = 'registry.example.com/web:latest@sha256:1234'\n",
            &["no-latest"],
        ),
        (
            "a digest is a pin",
            "[app.web]\nimage = 'ghcr.io/example/web@sha256:9532d8c39891ca2ecde4d30d7710e01f'\n",
            &[],
        ),
        (
            "an untrusted registry",
            "[app.web]\nimage = 'docker.io/library/nginx:1.27'\n",
            &["trusted-registries"],
        ),
        (
            "a look-alike host",
            "[app.web]\nimage = 'registry.example.com.evil.net/web:1'\n",
            &["trusted-registries"],
        ),
        (
            "a registry port is not a tag",
            "[app.web]\nimage = 'registry.example.com:5000/web'\n",
            &["no-latest", "trusted-registries"],
        ),
        (
            "an app that names its team keeps it",
            "[app.web]\nimage = 'registry.example.com/web:1'\nenv = { TEAM = 'payments' }\n",
            &[],
        ),
        (
            "an empty team",
            "[app.web]\nimage = 'registry.example.com/web:1'\nenv = { TEAM = '' }\n",
            &["team-env"],
        ),
        (
            "a host script has no image to judge",
            "[app.tick]\nscript = 'exec sleep 60'\n",
            &[],
        ),
        (
            "a job on :latest",
            "[job.report]\nimage = 'registry.example.com/report:latest'\ncommand = ['report']\n",
            &["no-latest"],
        ),
    ];
    for (case, workload, expected) in rows {
        let evaluation = judge(PLATFORM, workload);
        assert_eq!(denied_by(&evaluation), expected.to_vec(), "{case}");
    }
}

#[test]
fn the_team_default_fills_only_an_absent_team() {
    let evaluation = judge(
        PLATFORM,
        "[app.web]\nimage = 'registry.example.com/web:1'\n\
         [app.pay]\nimage = 'registry.example.com/pay:1'\nenv = { TEAM = 'payments' }\n",
    );
    assert!(evaluation.verdict.is_allowed(), "{:?}", evaluation.verdict);
    let filled: Vec<String> = evaluation
        .filled
        .iter()
        .map(|fill| format!("{} {} {}", fill.app.name, fill.field, fill.value))
        .collect();
    assert_eq!(filled, vec!["web env.TEAM \"platform\""]);
    let pay = serde_json::to_value(&evaluation.apps["pay"]).unwrap();
    assert_eq!(pay["env"]["TEAM"], "payments");
}

/// The workload file exactly as the harness writes it when the job load is
/// on: the apps, then the job fixtures appended.
fn soak_workload_file() -> String {
    format!("{SOAK_WORKLOADS}\n{SOAK_JOBS}")
}

#[test]
fn the_soak_policies_admit_every_soak_workload() {
    let file = soak_workload_file();
    let config = Config::parse(&file).unwrap();
    config.validate().unwrap();
    let evaluation = judge(&file, &file);
    assert!(
        evaluation.verdict.is_allowed(),
        "a soak policy refuses a soak workload: {:?}",
        evaluation.verdict.denials
    );
    // The warn and the default both fire on every soak app, so every apply
    // the soak makes exercises them, not just the deny.
    for name in config.app.keys() {
        assert!(
            evaluation
                .verdict
                .warnings
                .iter()
                .any(|finding| finding.policy == "soak-memory-limits"
                    && &finding.workload.name == name),
            "no warning for {name}"
        );
        assert!(
            evaluation
                .filled
                .iter()
                .any(|fill| fill.field == "env.SOAK_TEAM" && &fill.app.name == name),
            "nothing filled for {name}"
        );
    }
    // The generation bump the harness makes keeps its value.
    let identity = serde_json::to_value(&evaluation.apps["soak-identity"]).unwrap();
    assert_eq!(identity["env"]["SOAK_GENERATION"], "0");
}

#[test]
fn the_soak_policies_judge_only_soak_workloads() {
    // An unpinned soak app is refused; the catalogue pulse's apps and the
    // podinfo demo, which aren't named soak-*, pass untouched.
    let evaluation = judge(
        SOAK_WORKLOADS,
        "[app.soak-extra]\nimage = 'busybox:1.36'\n\
         [app.catalogue-web]\nimage = 'nginx:1.27'\n",
    );
    assert_eq!(denied_by(&evaluation), vec!["soak-pinned-images"]);
    assert_eq!(evaluation.verdict.warnings.len(), 1);
    assert!(
        evaluation
            .filled
            .iter()
            .all(|fill| fill.app.name == "soak-extra")
    );
}
