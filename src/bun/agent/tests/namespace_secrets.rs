//! Per-namespace secret keys at launch (F05 I4): which values a namespace
//! can open, and that a value it can't open fails the deploy closed.

use crate::config::app::AppSpec;
use crate::grill::mock::MockGrill;
use crate::meat::types::AppId;
use crate::sesame::secret::{
    encrypt_secret, generate_age_keypair, namespace_identities, reseal_namespace_values,
};
use crate::sesame::types::{AgeKeyScope, AgeKeypair, SecurityState};

use super::BunAgent;

const IKM: &[u8] = b"namespace-secret-test-ikm";

fn keypair(scope: AgeKeyScope) -> AgeKeypair {
    generate_age_keypair(scope, IKM, 0).unwrap().0
}

fn app_with_secret(value: &str) -> AppSpec {
    toml::from_str(&format!(
        "image = \"web:v1\"\n[env]\nDB_PASSWORD = \"{value}\"\nMODE = \"plain\"\n"
    ))
    .unwrap()
}

/// Build the launch spec for `spec` in `namespace`, with the identities a
/// node holding `state` would use there.
fn launch(
    state: &SecurityState,
    namespace: &str,
    spec: &AppSpec,
) -> Result<crate::grill::oci::OciSpec, crate::bun::BunError> {
    BunAgent::<MockGrill>::oci_spec_with_secrets(
        "web",
        namespace,
        spec,
        "web-0",
        None,
        "/sys/fs/cgroup/reliaburger/web-0",
        None,
        None,
        namespace_identities(state, namespace, IKM),
    )
}

fn env_entry(spec: &crate::grill::oci::OciSpec, name: &str) -> Option<String> {
    spec.process
        .env
        .iter()
        .find(|entry| entry.starts_with(&format!("{name}=")))
        .cloned()
}

#[test]
fn a_value_sealed_for_one_namespace_fails_closed_in_another() {
    let team_a = keypair(AgeKeyScope::Namespace("team-a".into()));
    let mut state = SecurityState {
        age_keypairs: vec![keypair(AgeKeyScope::ClusterWide), team_a.clone()],
        ..SecurityState::default()
    };
    let spec = app_with_secret(&encrypt_secret("team-a-only", &team_a.public_key).unwrap());

    let launched = launch(&state, "team-a", &spec).unwrap();
    assert_eq!(
        env_entry(&launched, "DB_PASSWORD").as_deref(),
        Some("DB_PASSWORD=team-a-only")
    );

    // team-b without a key of its own uses the cluster key, and team-b
    // with one uses that: neither opens team-a's value.
    for team_b_has_a_key in [false, true] {
        if team_b_has_a_key {
            state
                .age_keypairs
                .push(keypair(AgeKeyScope::Namespace("team-b".into())));
        }
        let error = launch(&state, "team-b", &spec).unwrap_err();
        assert!(
            matches!(error, crate::bun::BunError::DeployFailed { .. }),
            "team-b key: {team_b_has_a_key}: {error:?}"
        );
    }
}

#[test]
fn after_opting_in_a_cluster_sealed_value_fails_closed_in_that_namespace() {
    let cluster = keypair(AgeKeyScope::ClusterWide);
    let mut state = SecurityState {
        age_keypairs: vec![cluster.clone()],
        ..SecurityState::default()
    };
    let spec = app_with_secret(&encrypt_secret("shared", &cluster.public_key).unwrap());
    assert!(launch(&state, "team-a", &spec).is_ok(), "before opting in");

    state
        .age_keypairs
        .push(keypair(AgeKeyScope::Namespace("team-a".into())));

    let error = launch(&state, "team-a", &spec).unwrap_err();
    assert!(
        matches!(error, crate::bun::BunError::DeployFailed { .. }),
        "{error:?}"
    );
    assert!(
        launch(&state, "team-b", &spec).is_ok(),
        "a namespace that didn't opt in still uses the cluster key"
    );
}

/// Re-sealing changes the ciphertext, not the plaintext, so the launched
/// spec is the same and a retired runc intent's scrubbed copy, which keeps
/// only variable names (#512), still matches it.
#[test]
fn resealing_keeps_the_launched_spec_and_its_scrubbed_journal_copy() {
    let cluster = keypair(AgeKeyScope::ClusterWide);
    let team_a = keypair(AgeKeyScope::Namespace("team-a".into()));
    let before_state = SecurityState {
        age_keypairs: vec![cluster.clone()],
        ..SecurityState::default()
    };
    let app_id = AppId::new("web", "team-a");
    let mut spec = app_with_secret(&encrypt_secret("db-password", &cluster.public_key).unwrap());
    let before = launch(&before_state, "team-a", &spec).unwrap();

    let resealed = reseal_namespace_values(
        [(&app_id, &spec)],
        &namespace_identities(&before_state, "team-a", IKM),
        &team_a.public_key,
    )
    .unwrap();
    for entry in resealed {
        spec.env.insert(
            entry.env_key,
            crate::config::EnvValue::Encrypted(entry.sealed),
        );
    }
    let after_state = SecurityState {
        age_keypairs: vec![cluster, team_a],
        ..SecurityState::default()
    };
    let after = launch(&after_state, "team-a", &spec).unwrap();

    assert_eq!(after, before);
    let journal = before.without_environment_values();
    assert!(after.matches_journal(&journal));
    assert!(
        journal.process.env.iter().all(|entry| !entry.contains('=')),
        "the scrubbed copy keeps no value: {:?}",
        journal.process.env
    );
}
