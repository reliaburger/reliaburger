//! What a test case is handed.
//!
//! The context is the only way a case touches the cluster: a `BunClient`
//! pointed at a node, and a namespace of its own. Everything a case creates
//! carries that namespace, and teardown stops that namespace and nothing
//! else — which is what makes it safe to point `relish test` at a cluster
//! that has real work on it.

use std::time::{Duration, Instant};

use crate::bun::agent::InstanceStatus;
use crate::bun::capabilities::ClusterCapabilities;
use crate::relish::client::BunClient;

/// Prefix for every namespace the test runner creates.
///
/// The safety net for running against a real cluster: teardown only ever
/// touches namespaces it made, and they are recognisable at a glance in
/// `relish status` if something does leak.
pub const TEST_NAMESPACE_PREFIX: &str = "rbtest";

/// Where `bun` is installed on a cluster node.
///
/// `relish dev` installs the binary here (`/usr/local/bin`), and `testapp` is
/// a `bun` subcommand, so this is how the harness launches the test workload
/// on a process-runtime cluster without shipping a separate binary or image.
pub const BUN_BINARY_PATH: &str = "/usr/local/bin/bun";

/// The stock public image container-workload cases deploy. Small, ubiquitous,
/// and carries `sh`/`httpd`/`wget` — enough to exercise volumes, ingress and
/// firewall without building an image.
pub const BUSYBOX_IMAGE: &str = "busybox:latest";

/// One test case's handle on the cluster.
#[derive(Clone)]
pub struct TestContext {
    pub client: BunClient,
    /// This case's own namespace, e.g. `rbtest-4f2a91-03`.
    pub namespace: String,
    pub capabilities: ClusterCapabilities,
    /// Per-case budget, from `--timeout`. Poll loops measure against it so a
    /// case fails with a useful message rather than being killed from
    /// outside with none.
    pub timeout: Duration,
}

impl TestContext {
    /// Build a namespace name for case number `seq` of run `run_id`.
    pub fn namespace_for(run_id: &str, seq: usize) -> String {
        format!("{TEST_NAMESPACE_PREFIX}-{run_id}-{seq:02}")
    }

    /// Whether `namespace` was created by the test runner.
    ///
    /// Teardown consults this before stopping anything. A bug that pointed
    /// teardown at an operator's namespace would be the worst thing this
    /// tool could do, so the check is explicit rather than implied by how
    /// the name was built.
    pub fn is_test_namespace(namespace: &str) -> bool {
        namespace
            .strip_prefix(TEST_NAMESPACE_PREFIX)
            .is_some_and(|rest| rest.starts_with('-'))
    }

    /// Apply a TOML app config into this test's cluster.
    ///
    /// The case is responsible for putting its work in [`Self::namespace`] —
    /// [`Self::testapp_spec`] does this for you; a hand-written spec must set
    /// `namespace` itself, or teardown won't find it.
    pub async fn apply(&self, toml: &str) -> Result<(), String> {
        let config = crate::config::Config::parse(toml)
            .map_err(|error| format!("config does not parse: {error}"))?;
        self.client
            .apply(&config)
            .await
            .map(|_| ())
            .map_err(|error| format!("apply failed: {error}"))
    }

    /// Wait until `app` has at least `replicas` instances in the `running`
    /// state *on this node*, or fail at the context deadline.
    ///
    /// This is the node-local view. For an app whose replicas spread across a
    /// cluster, use [`Self::wait_running_cluster`] — `/v1/status` only ever
    /// reports the instances the queried node runs.
    pub async fn wait_running(&self, app: &str, replicas: u32) -> Result<(), String> {
        self.wait_for(
            app,
            &format!("{replicas} running replica(s)"),
            |instances| {
                instances.iter().filter(|i| i.state == "running").count() as u32 >= replicas
            },
        )
        .await
    }

    /// A TOML app spec that runs the `testapp` workload in this test's
    /// namespace.
    ///
    /// The workload is the `testapp` server embedded in `bun` itself, launched
    /// as a process workload — every node already has `bun` at
    /// [`BUN_BINARY_PATH`], so no container image is needed. Cases built on
    /// this must therefore require
    /// [`Capability::ProcessRuntime`](crate::bun::capabilities::Capability::ProcessRuntime);
    /// on a runc/apple cluster the spec would be handed to the container
    /// runtime and fail.
    ///
    /// `mode` is a `testapp` mode string (`"healthy"`, `"unhealthy-after"`,
    /// `"hang"`, `"exit-after"`, `"slow"`, `"alloc"`). The port is derived
    /// from the app name so two apps in one case don't fight over a socket —
    /// but `testapp` binds a *fixed* port, so more than one replica only
    /// coexists on distinct nodes; multi-replica cases must also require
    /// `MultiNode`.
    pub fn testapp_spec(&self, app: &str, mode: &str, replicas: u32) -> String {
        self.testapp_spec_args(app, mode, replicas, &[])
    }

    /// [`Self::testapp_spec`] with extra `testapp` flags, e.g.
    /// `&["--count", "3"]` for `unhealthy-after` or `&["--delay", "500"]` for
    /// `slow`.
    pub fn testapp_spec_args(
        &self,
        app: &str,
        mode: &str,
        replicas: u32,
        extra: &[&str],
    ) -> String {
        let port = testapp_port(app);
        let mut argv: Vec<String> = vec![
            BUN_BINARY_PATH.to_string(),
            "testapp".to_string(),
            "--mode".to_string(),
            mode.to_string(),
            "--port".to_string(),
            port.to_string(),
        ];
        argv.extend(extra.iter().map(|arg| arg.to_string()));
        let command = argv
            .iter()
            .map(|arg| format!("\"{arg}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "[app.{app}]\n\
             image = \"proc-grill:image-ignored\"\n\
             command = [{command}]\n\
             port = {port}\n\
             replicas = {replicas}\n\
             namespace = \"{ns}\"\n\
             \n\
             [app.{app}.health]\n\
             path = \"/\"\n\
             interval = 1\n\
             timeout = 2\n\
             threshold_unhealthy = 3\n\
             threshold_healthy = 1\n",
            ns = self.namespace,
        )
    }

    /// TOML for a run-to-completion job in this test's namespace, running
    /// `command` as a process workload (no image). Needs the `process`
    /// runtime, same as [`Self::testapp_spec`].
    ///
    /// `JobSpec` has no `retries` field, so retry-count is not something a case
    /// can configure — the agent's default backoff applies.
    pub fn process_job_spec(&self, job: &str, command: &[&str]) -> String {
        let argv = command
            .iter()
            .map(|arg| format!("\"{arg}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "[job.{job}]\n\
             image = \"proc-grill:image-ignored\"\n\
             command = [{argv}]\n\
             namespace = \"{ns}\"\n",
            ns = self.namespace,
        )
    }

    /// A TOML spec for an idle container workload in this test's namespace.
    ///
    /// Runs a stock `busybox` image (pulled from Docker Hub by the runc
    /// runtime) doing nothing but staying up, so a case can `exec` into it —
    /// the workload for volume and firewall-source cases. No health check, so
    /// it reaches Running as soon as the container starts. Needs
    /// [`Capability::ContainerRuntime`](crate::bun::capabilities::Capability::ContainerRuntime).
    pub fn container_idle_spec(&self, app: &str) -> String {
        format!(
            "[app.{app}]\n\
             image = \"{BUSYBOX_IMAGE}\"\n\
             command = [\"sleep\", \"infinity\"]\n\
             namespace = \"{ns}\"\n",
            ns = self.namespace,
        )
    }

    /// A TOML spec for an HTTP container workload in this test's namespace.
    ///
    /// Runs `busybox httpd` serving `/etc`, health-checked on `/hostname`
    /// (which maps to `/etc/hostname`, always present). The workload for
    /// ingress and firewall-target cases. Port is derived from the app name.
    pub fn container_http_spec(&self, app: &str, replicas: u32) -> String {
        let port = testapp_port(app);
        format!(
            "[app.{app}]\n\
             image = \"{BUSYBOX_IMAGE}\"\n\
             command = [\"httpd\", \"-f\", \"-p\", \"{port}\", \"-h\", \"/etc\"]\n\
             port = {port}\n\
             replicas = {replicas}\n\
             namespace = \"{ns}\"\n\
             \n\
             [app.{app}.health]\n\
             path = \"/hostname\"\n\
             interval = 2\n\
             timeout = 2\n\
             threshold_unhealthy = 3\n\
             threshold_healthy = 1\n",
            ns = self.namespace,
        )
    }

    /// The port [`container_http_spec`](Self::container_http_spec) derives for
    /// `app` — so a case can build a URL to it.
    pub fn container_port(&self, app: &str) -> u16 {
        testapp_port(app)
    }

    /// A `BunClient` for every node in the cluster, paired with its node id.
    ///
    /// `/v1/status` is node-local, so a case that reasons about cluster-wide
    /// placement fans out with this. Each node's API address is its gossip IP
    /// with the entry node's API port and scheme — every `bun` serves its API
    /// on the same port, so the entry client's port is the right one to reuse.
    pub async fn node_clients(&self) -> Result<Vec<(String, BunClient)>, String> {
        let nodes = self
            .client
            .nodes()
            .await
            .map_err(|error| format!("could not list nodes: {error}"))?;
        if nodes.is_empty() {
            return Ok(vec![("local".to_string(), self.client.clone())]);
        }
        let scheme = self.client.scheme();
        let port = self.api_port();
        let clients = nodes
            .into_iter()
            .filter_map(|node| {
                let ip = node.address.rsplit_once(':').map(|(ip, _)| ip)?;
                Some((
                    node.node_id,
                    BunClient::new(&format!("{scheme}://{ip}:{port}")),
                ))
            })
            .collect();
        Ok(clients)
    }

    /// Every instance of `app` in this namespace, gathered across all nodes.
    pub async fn cluster_instances(&self, app: &str) -> Result<Vec<InstanceStatus>, String> {
        let mut all = Vec::new();
        for (_node, client) in self.node_clients().await? {
            if let Ok(statuses) = client.status().await {
                all.extend(statuses.into_iter().filter(|instance| {
                    instance.app_name == app && instance.namespace == self.namespace
                }));
            }
        }
        Ok(all)
    }

    /// Wait until `app` has at least `replicas` running instances *anywhere in
    /// the cluster*, or fail at the deadline.
    pub async fn wait_running_cluster(&self, app: &str, replicas: u32) -> Result<(), String> {
        self.wait_for_cluster(
            app,
            &format!("{replicas} running replica(s)"),
            |instances| {
                instances.iter().filter(|i| i.state == "running").count() as u32 >= replicas
            },
        )
        .await
    }

    /// Like [`Self::wait_for`] but over the whole cluster's instances of
    /// `app`, for cases whose one replica may land on any node.
    pub async fn wait_for_cluster<F>(
        &self,
        app: &str,
        what: &str,
        predicate: F,
    ) -> Result<(), String>
    where
        F: Fn(&[InstanceStatus]) -> bool,
    {
        let deadline = Instant::now() + self.timeout;
        let mut last: Vec<InstanceStatus> = Vec::new();
        loop {
            if let Ok(instances) = self.cluster_instances(app).await {
                last = instances;
                if predicate(&last) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                let seen: Vec<&str> = last.iter().map(|i| i.state.as_str()).collect();
                return Err(format!(
                    "timed out after {:?} waiting for {app} to reach {what} cluster-wide; \
                     last saw {} instance(s): {seen:?}",
                    self.timeout,
                    last.len()
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// The Pickle registry base URL on the entry node.
    ///
    /// The dev registry is bound to loopback (`127.0.0.1:5050`), reachable only
    /// from *on* the node — so the registry cases run only when the harness
    /// itself runs on a cluster node. The host is taken from the client so a
    /// loopback client yields a loopback registry.
    pub fn registry_base(&self) -> String {
        let scheme = self.client.scheme();
        let host = self
            .client
            .base_url()
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split(':')
            .next()
            .unwrap_or("127.0.0.1");
        format!("{scheme}://{host}:5050")
    }

    /// The API port the entry node serves on, reused for every node.
    fn api_port(&self) -> u16 {
        self.client
            .base_url()
            .rsplit(':')
            .next()
            .map(|tail| tail.trim_end_matches('/'))
            .and_then(|port| port.parse().ok())
            .unwrap_or(9117)
    }

    /// Poll this app's instances until `predicate` holds, or fail at the
    /// context deadline with a diagnostic naming what we were waiting for and
    /// what we last saw.
    ///
    /// `what` is a human phrase completing "waiting for {app} to reach …". A
    /// transient status error or a slow node is not a verdict — the loop keeps
    /// polling until the deadline decides, so a blip doesn't fail a case.
    pub async fn wait_for<F>(&self, app: &str, what: &str, predicate: F) -> Result<(), String>
    where
        F: Fn(&[InstanceStatus]) -> bool,
    {
        let deadline = Instant::now() + self.timeout;
        let poll = Duration::from_millis(500);
        let mut last: Vec<InstanceStatus> = Vec::new();
        loop {
            if let Ok(Ok(all)) =
                tokio::time::timeout(Duration::from_secs(10), self.client.status()).await
            {
                last = all
                    .into_iter()
                    .filter(|instance| {
                        instance.app_name == app && instance.namespace == self.namespace
                    })
                    .collect();
                if predicate(&last) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                let seen: Vec<(&str, &str)> = last
                    .iter()
                    .map(|instance| (instance.app_name.as_str(), instance.state.as_str()))
                    .collect();
                return Err(format!(
                    "timed out after {:?} waiting for {app} to reach {what}; \
                     last saw {} instance(s): {seen:?}",
                    self.timeout,
                    last.len()
                ));
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Stop every app this case created. Best-effort: it never errors and
    /// never returns anything the runner has to check.
    ///
    /// The runner calls this after *every* case — pass, fail or timeout —
    /// because the case that failed halfway is exactly the one that left a
    /// workload running. The [`is_test_namespace`](Self::is_test_namespace)
    /// guard is a second lock on top of the name match: even a bug in
    /// namespace construction cannot make teardown stop an operator's app.
    pub async fn teardown(&self) {
        if !Self::is_test_namespace(&self.namespace) {
            return;
        }
        let Ok(instances) = self.client.status().await else {
            // A cluster we can't reach has nothing we can tear down. Not an
            // error the caller can act on — swallow it.
            return;
        };
        let mut apps: Vec<&str> = instances
            .iter()
            .filter(|instance| instance.namespace == self.namespace)
            .map(|instance| instance.app_name.as_str())
            .collect();
        apps.sort_unstable();
        apps.dedup();
        for app in apps {
            let _ = self.client.stop(app, &self.namespace).await;
        }
    }
}

/// A stable, per-app port in the ephemeral range, so two apps in one case
/// don't collide. Deterministic (an FNV-1a hash of the name) so a case's
/// spec is reproducible between runs.
fn testapp_port(app: &str) -> u16 {
    let mut hash: u32 = 2_166_136_261;
    for byte in app.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    // 40000–59999: above the usual service range, below the OS ephemeral
    // range that would clash with outbound connections.
    40_000 + (hash % 20_000) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn context(namespace: &str) -> TestContext {
        TestContext {
            client: BunClient::new_with_token("http://127.0.0.1:9117", None),
            namespace: namespace.to_string(),
            capabilities: ClusterCapabilities::default(),
            timeout: Duration::from_millis(200),
        }
    }

    #[test]
    fn a_testapp_spec_parses_and_lands_in_the_test_namespace() {
        let ctx = context("rbtest-abc-00");
        let toml = ctx.testapp_spec("web", "healthy", 1);
        let config = Config::parse(&toml).expect("testapp_spec must be valid TOML");
        let app = config.app.get("web").expect("app named web");
        assert_eq!(app.namespace.as_deref(), Some("rbtest-abc-00"));
        assert_eq!(app.replicas, crate::config::types::Replicas::Fixed(1));
        // The workload is launched from the node's installed bun, as a process.
        assert_eq!(
            app.command.first().map(String::as_str),
            Some(BUN_BINARY_PATH)
        );
        assert_eq!(app.command.get(1).map(String::as_str), Some("testapp"));
        assert!(
            app.command.iter().any(|arg| arg == "healthy"),
            "mode should appear in argv: {:?}",
            app.command
        );
        assert!(app.health.is_some(), "testapp_spec carries a health check");
    }

    #[test]
    fn container_specs_parse_and_land_in_the_test_namespace() {
        let ctx = context("rbtest-abc-00");

        let http = Config::parse(&ctx.container_http_spec("web", 1)).unwrap();
        let web = http.app.get("web").expect("app web");
        assert_eq!(web.namespace.as_deref(), Some("rbtest-abc-00"));
        assert_eq!(web.image.as_deref(), Some(BUSYBOX_IMAGE));
        assert!(web.health.is_some());
        assert_eq!(web.port, Some(ctx.container_port("web")));

        let idle = Config::parse(&ctx.container_idle_spec("box")).unwrap();
        let box_app = idle.app.get("box").expect("app box");
        assert_eq!(box_app.namespace.as_deref(), Some("rbtest-abc-00"));
        assert!(box_app.command.iter().any(|a| a == "sleep"));
        assert!(box_app.health.is_none());
    }

    #[test]
    fn testapp_spec_args_appends_extra_flags() {
        let ctx = context("rbtest-abc-00");
        let toml = ctx.testapp_spec_args("web", "unhealthy-after", 1, &["--count", "3"]);
        let config = Config::parse(&toml).expect("valid TOML");
        let command = &config.app.get("web").unwrap().command;
        assert!(
            command.windows(2).any(|w| w == ["--count", "3"]),
            "{command:?}"
        );
    }

    #[test]
    fn two_apps_get_distinct_testapp_ports() {
        assert_ne!(testapp_port("web"), testapp_port("api"));
        // Deterministic between calls.
        assert_eq!(testapp_port("web"), testapp_port("web"));
        for name in ["web", "api", "redis", "worker"] {
            let port = testapp_port(name);
            assert!((40_000..60_000).contains(&port), "{name} -> {port}");
        }
    }

    #[test]
    fn namespaces_are_unique_per_case_and_run() {
        let a = TestContext::namespace_for("4f2a91", 3);
        let b = TestContext::namespace_for("4f2a91", 4);
        let c = TestContext::namespace_for("99bb00", 3);
        assert_eq!(a, "rbtest-4f2a91-03");
        assert_ne!(a, b, "two cases in one run must not share a namespace");
        assert_ne!(a, c, "two runs must not share a namespace");
    }

    /// Teardown keys off this. A false positive here would let the runner
    /// stop an operator's apps, which is the worst thing this tool could do.
    #[test]
    fn only_runner_created_namespaces_are_recognised() {
        assert!(TestContext::is_test_namespace("rbtest-4f2a91-03"));
        assert!(TestContext::is_test_namespace("rbtest-anything"));

        assert!(!TestContext::is_test_namespace("default"));
        assert!(!TestContext::is_test_namespace("production"));
        // A namespace that merely starts with the letters must not match —
        // `rbtestingground` is somebody's real namespace.
        assert!(!TestContext::is_test_namespace("rbtestingground"));
        assert!(!TestContext::is_test_namespace("rbtest"));
        assert!(!TestContext::is_test_namespace("my-rbtest-01"));
    }
}
