//! Image-registry cases: push, list, and deploy from the Pickle registry.
//!
//! These build a synthetic OCI image in the harness and speak the raw `/v2`
//! protocol to the node's registry, which is loopback-bound on a dev cluster —
//! so they run only when the harness itself runs on a node. Gated on
//! [`Capability::Registry`].
//!
//! [`Capability::Registry`]: crate::bun::capabilities::Capability::Registry

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::oci;
use crate::testkit::registry::{TestCase, skip};
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A pushed image can be pulled back with a matching manifest digest.
async fn push_and_pull_image_roundtrip(ctx: TestContext) -> Result<(), String> {
    let base = ctx.registry_base();
    let repo = "rbtest-roundtrip";
    let image = oci::build_synthetic_image("roundtrip");

    oci::push_image(ctx.client.http(), &base, repo, "v1", &image).await?;

    let pulled = oci::fetch_manifest(ctx.client.http(), &base, repo, "v1").await?;
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
    let base = ctx.registry_base();
    let repo = "rbtest-listed";
    let image = oci::build_synthetic_image("listed");
    oci::push_image(ctx.client.http(), &base, repo, "v1", &image).await?;

    let images = ctx
        .client
        .images()
        .await
        .map_err(|error| format!("could not list images: {error}"))?;
    let listed = images["images"]
        .as_array()
        .map(|entries| {
            entries.iter().any(|entry| {
                entry["repository"].as_str() == Some(repo) || format!("{entry:?}").contains(repo)
            })
        })
        .unwrap_or(false);
    if !listed {
        return Err(format!("pushed image {repo} not in /v1/images: {images}"));
    }
    Ok(())
}

/// Deploying an app whose image lives in the cluster registry needs a
/// *runnable* image there — which the synthetic fixture is not (it carries one
/// marker file, no binary). Skipped until the harness can stage a runnable
/// image in Pickle.
async fn deploy_from_cluster_registry(_ctx: TestContext) -> Result<(), String> {
    skip("needs a runnable image in the cluster registry; the synthetic fixture isn't runnable")
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
