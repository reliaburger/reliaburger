//! Secrets and config-file cases.
//!
//! Config-file mounting needs a container rootfs, so these require
//! [`Capability::ContainerRuntime`]. The two encryption cases additionally
//! need the cluster age pubkey, which isn't exposed through the API, so they
//! skip until it is.
//!
//! [`Capability::ContainerRuntime`]: crate::bun::capabilities::Capability::ContainerRuntime

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::{TestCase, skip};
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A `[[config_file]]` is mounted into the workload with its contents intact.
async fn config_file_is_mounted_with_contents(ctx: TestContext) -> Result<(), String> {
    let app = "cfg-app";
    let spec = format!(
        "{}\n[[app.{app}.config_file]]\npath = \"/etc/app/config.yaml\"\ncontent = \"key: value\"\n",
        ctx.container_idle_spec(app),
    );
    ctx.apply(&spec).await?;
    ctx.wait_running_cluster(app, 1).await?;

    let contents = ctx
        .client
        .exec(
            app,
            &ctx.namespace,
            &["cat".to_string(), "/etc/app/config.yaml".to_string()],
        )
        .await
        .map_err(|error| format!("exec failed: {error}"))?;
    if !contents.contains("key: value") {
        return Err(format!("config file content not mounted: {contents:?}"));
    }
    Ok(())
}

/// Decrypting an `ENC[AGE:...]` env value needs a secret encrypted with the
/// cluster pubkey — which the harness can't fetch (no API endpoint exposes it).
async fn encrypted_env_value_is_decrypted_in_workload(_ctx: TestContext) -> Result<(), String> {
    skip("the cluster age pubkey is not exposed via the API, so a secret can't be encrypted here")
}

/// Same blocker: fetching the pubkey to encrypt a value round-trip needs an
/// API endpoint that doesn't exist yet.
async fn cluster_pubkey_encrypt_roundtrip(_ctx: TestContext) -> Result<(), String> {
    skip("the cluster age pubkey is not exposed via the API")
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "encrypted_env_value_is_decrypted_in_workload",
            group: TestGroup::SecretsConfig,
            requires: &[Capability::ContainerRuntime, Capability::Identity],
            run: testkit_case!(encrypted_env_value_is_decrypted_in_workload),
        },
        TestCase {
            name: "config_file_is_mounted_with_contents",
            group: TestGroup::SecretsConfig,
            requires: &[Capability::ContainerRuntime],
            run: testkit_case!(config_file_is_mounted_with_contents),
        },
        TestCase {
            name: "cluster_pubkey_encrypt_roundtrip",
            group: TestGroup::SecretsConfig,
            requires: &[Capability::ContainerRuntime, Capability::Identity],
            run: testkit_case!(cluster_pubkey_encrypt_roundtrip),
        },
    ]
}
