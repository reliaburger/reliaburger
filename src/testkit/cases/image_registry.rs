//! Image-registry cases: push, list, and deploy from the Pickle registry.
//!
//! These stage synthetic and pinned runnable OCI fixtures under their server-owned
//! repository leases, and speak the raw `/v2`
//! protocol to the registry origin declared by the node or managed host
//! context. Missing reachability is an explicit error. Gated on
//! [`Capability::Registry`].
//!
//! [`Capability::Registry`]: crate::bun::capabilities::Capability::Registry

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::oci;
use crate::testkit::registry::TestCase;
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A pushed image can be pulled back with a matching manifest digest.
async fn push_and_pull_image_roundtrip(ctx: TestContext) -> Result<(), String> {
    let base = ctx.registry_base()?;
    let client = ctx.client.registry_http_client(&base)?;
    let repo = format!("{}/roundtrip", ctx.namespace);
    let lease = ctx
        .lease_id
        .as_deref()
        .ok_or("registry fixture requires its server-issued lease")?;
    let image = oci::build_synthetic_image("roundtrip");

    oci::push_leased_image(&client, &base, &repo, "v1", &image, lease).await?;

    let pulled = oci::fetch_manifest(&client, &base, &repo, "v1").await?;
    let pulled_digest = oci::sha256_digest(&pulled);
    if pulled_digest != image.manifest_digest {
        return Err(format!(
            "pulled manifest digest {pulled_digest} != pushed {}",
            image.manifest_digest
        ));
    }
    Ok(())
}

/// A pushed image appears in the manifest catalogue.
async fn manifest_catalog_lists_pushed_image(ctx: TestContext) -> Result<(), String> {
    let base = ctx.registry_base()?;
    let client = ctx.client.registry_http_client(&base)?;
    let repo = format!("{}/listed", ctx.namespace);
    let lease = ctx
        .lease_id
        .as_deref()
        .ok_or("registry fixture requires its server-issued lease")?;
    let image = oci::build_synthetic_image("listed");
    oci::push_leased_image(&client, &base, &repo, "v1", &image, lease).await?;

    let images = ctx
        .client
        .images()
        .await
        .map_err(|error| format!("could not list images: {error}"))?;
    let listed = images["images"]
        .as_array()
        .map(|entries| {
            entries.iter().any(|entry| {
                entry["repository"].as_str() == Some(repo.as_str())
                    && entry["digest"].as_str() == Some(image.manifest_digest.as_str())
            })
        })
        .unwrap_or(false);
    if !listed {
        return Err(format!("pushed image {repo} not in /v1/images: {images}"));
    }
    Ok(())
}

/// Stage the pinned Linux workload in this lease's Pickle repository, deploy the
/// exact child digest and inspect a real HTTP response in the owning container.
async fn deploy_from_cluster_registry(ctx: TestContext) -> Result<(), String> {
    use crate::testkit::context::PINNED_TEST_WORKLOAD_IMAGE;
    let base = ctx.registry_base()?;
    let client = ctx.client.registry_http_client(&base)?;
    let repo = format!("{}/runnable", ctx.namespace);
    let lease = ctx
        .lease_id
        .as_deref()
        .ok_or("registry fixture requires its server-issued lease")?;
    // Stage from the node's own mirrors, falling back to the upstream, so an
    // air-gapped or rate-limited cluster qualifies the same exact bytes.
    let upstream = crate::pickle::upstream::OciUpstream::new(Default::default())
        .with_mirrors(ctx.capabilities.image_mirrors.clone())
        .with_linux_architecture(&ctx.capabilities.runtime.architecture)
        .map_err(|error| error.to_string())?;
    let image = crate::grill::image::ImageReference::parse(PINNED_TEST_WORKLOAD_IMAGE)
        .map_err(|error| error.to_string())?;
    let digest = ctx
        .deadline
        .run(
            "stage pinned runnable image",
            oci::stage_upstream_image(&client, &base, &repo, lease, &upstream, &image),
        )
        .await
        .map_err(|error| error.to_string())??;
    let app = "registry-web";
    let spec = ctx
        .container_http_spec(app, 1)
        .replace(PINNED_TEST_WORKLOAD_IMAGE, &format!("{repo}@{digest}"));
    ctx.apply(&spec).await?;
    ctx.wait_running_cluster(app, 1).await?;
    let output = ctx
        .exec_in_workload(
            app,
            &[
                "/bin/busybox".into(),
                "wget".into(),
                "-q".into(),
                "-O-".into(),
                "-T".into(),
                "3".into(),
                format!("http://127.0.0.1:{}/hostname", ctx.container_port(app)),
            ],
        )
        .await?;
    if output.trim() != "reliaburger-test" {
        return Err(format!(
            "pushed image returned unexpected HTTP content: {output:?}"
        ));
    }
    Ok(())
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "push_and_pull_image_roundtrip",
            group: TestGroup::ImageRegistry,
            requires: &[Capability::Registry],
            run: testkit_case!(push_and_pull_image_roundtrip),
        },
        TestCase {
            name: "manifest_catalog_lists_pushed_image",
            group: TestGroup::ImageRegistry,
            requires: &[Capability::Registry],
            run: testkit_case!(manifest_catalog_lists_pushed_image),
        },
        TestCase {
            name: "deploy_from_cluster_registry",
            group: TestGroup::ImageRegistry,
            requires: &[Capability::Registry, Capability::ContainerRuntime],
            run: testkit_case!(deploy_from_cluster_registry),
        },
    ]
}
