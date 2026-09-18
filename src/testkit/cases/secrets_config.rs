//! Secrets and config-file cases.
//!
//! Config-file mounting needs a container rootfs, so these require
//! [`Capability::ContainerRuntime`]. Encryption cases fetch the public recipient
//! over the authenticated API and inspect the decrypted environment inside
//! the workload. No test reads a cluster private key.
//!
//! [`Capability::ContainerRuntime`]: crate::bun::capabilities::Capability::ContainerRuntime

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::TestCase;
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

    let contents = exec_in_workload(
        &ctx,
        app,
        &[
            "/bin/busybox".into(),
            "cat".into(),
            "/etc/app/config.yaml".into(),
        ],
    )
    .await?;
    if !contents.contains("key: value") {
        return Err(format!("config file content not mounted: {contents:?}"));
    }
    Ok(())
}

/// Inspect the actual owning node, since the entry node may not run this app.
async fn exec_in_workload(
    ctx: &TestContext,
    app: &str,
    command: &[String],
) -> Result<String, String> {
    ctx.deadline
        .run("inspect workload contents", async {
            for (node, client) in ctx.node_clients().await? {
                let instances = client
                    .status()
                    .await
                    .map_err(|error| format!("could not inspect node {node}: {error}"))?;
                if instances.iter().any(|instance| {
                    instance.app_name == app
                        && instance.namespace == ctx.namespace
                        && instance.state == "running"
                }) {
                    return client
                        .exec(app, &ctx.namespace, command)
                        .await
                        .map_err(|error| format!("workload inspection failed on {node}: {error}"));
                }
            }
            Err(format!(
                "no running instance of {}/{app} found",
                ctx.namespace
            ))
        })
        .await
        .map_err(|error| error.to_string())?
}

/// Decrypt a sealed variable without changing an adjacent plaintext value.
async fn encrypted_env_value_is_decrypted_in_workload(ctx: TestContext) -> Result<(), String> {
    encrypted_environment_roundtrip(&ctx, "secret-env", "catalogue-value").await
}

/// The fetched recipient seals multiple randomised ciphertexts whose Unicode
/// and newline bytes survive the complete API-to-container round trip.
async fn cluster_pubkey_encrypt_roundtrip(ctx: TestContext) -> Result<(), String> {
    encrypted_environment_roundtrip(&ctx, "secret-roundtrip", "line one\nclé=burger").await
}

async fn encrypted_environment_roundtrip(
    ctx: &TestContext,
    app: &str,
    plaintext: &str,
) -> Result<(), String> {
    let key = ctx
        .deadline
        .run(
            "fetch public encryption key",
            ctx.client.secret_public_key(),
        )
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("could not fetch public encryption key: {error}"))?;
    let first = crate::sesame::secret::encrypt_secret(plaintext, &key.public_key)
        .map_err(|error| error.to_string())?;
    let second = crate::sesame::secret::encrypt_secret(plaintext, &key.public_key)
        .map_err(|error| error.to_string())?;
    if first == second || first == plaintext {
        return Err("age encryption did not produce distinct sealed values".into());
    }
    let spec = format!(
        "{}\n[app.{app}.env]\nFIRST = {first:?}\nSECOND = {second:?}\nPLAIN = 'unchanged'\n",
        ctx.container_idle_spec(app),
    );
    ctx.apply(&spec).await?;
    ctx.wait_running_cluster(app, 1).await?;
    for (name, expected) in [
        ("FIRST", plaintext),
        ("SECOND", plaintext),
        ("PLAIN", "unchanged"),
    ] {
        let output = exec_in_workload(
            ctx,
            app,
            &["/bin/busybox".into(), "printenv".into(), name.into()],
        )
        .await?;
        if output.strip_suffix('\n') != Some(expected) {
            return Err(format!(
                "{name} did not preserve its expected value in the workload"
            ));
        }
    }
    Ok(())
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "encrypted_env_value_is_decrypted_in_workload",
            group: TestGroup::SecretsConfig,
            requires: &[
                Capability::ContainerRuntime,
                Capability::Identity,
                Capability::Council,
            ],
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
            requires: &[
                Capability::ContainerRuntime,
                Capability::Identity,
                Capability::Council,
            ],
            run: testkit_case!(cluster_pubkey_encrypt_roundtrip),
        },
    ]
}
