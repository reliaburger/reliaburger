//! What a test case is handed.
//!
//! The context is the only way a case touches the cluster: a `BunClient`
//! pointed at a node, and a namespace of its own. Production runners use
//! server-owned leases for apps and namespaces and exact receipts for chaos
//! faults. Cleanup is attempted after every case and reported separately as
//! confirmed, failed or unknown. Other resource kinds still need explicit
//! ownership support; a namespace prefix alone does not provide that support.

use std::time::Duration;

use crate::bun::agent::InstanceStatus;
use crate::bun::capabilities::ClusterCapabilities;
use crate::relish::client::BunClient;
use crate::testkit::deadline::Deadline;
use crate::testkit::report::CleanupOutcome;

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

/// Immutable multi-architecture OCI workload for container-profile cases.
///
/// This is the official BusyBox 1.37.0 OCI index, resolved on 28 July 2026.
/// The index contains both `linux/amd64` and `linux/arm64`; pinning the index
/// rather than a tag makes runc and Apple Container execute identical content
/// on repeated acceptance runs.
pub const PINNED_TEST_WORKLOAD_IMAGE: &str = "public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028";

/// Shell prefix that makes a container fixture's PID 1 exit on SIGTERM.
///
/// The kernel drops any signal PID 1 has no handler for, and neither
/// `busybox sleep` nor `busybox httpd` installs one. Run bare, each fixture
/// sat out Bun's full stop grace (ten seconds) on every lease cleanup.
pub(crate) const SIGTERM_TRAP: &str = "trap 'kill $! 2>/dev/null; exit 0' TERM; ";

/// Wrap `script` for `/bin/sh -c` so a SIGTERM stops it at once.
///
/// The script's last command runs in the background under a waiting shell,
/// because a trapped signal interrupts `wait` but not a foreground child.
/// The result has no `"` or `\`, so it embeds in a TOML basic string as is.
fn exit_on_sigterm(script: &str) -> String {
    format!("{SIGTERM_TRAP}{script} & wait")
}

/// How the runner reaches a peer node's own API.
///
/// Some reads are node-local (`/v1/status` lists only that node's
/// instances), so cases fan out to every node. From inside the cluster each
/// node's advertised API address works. From a laptop behind a quickstart's
/// port forwards it doesn't: the host reaches node 1's forwarded port and
/// nothing else, so peers go through the entry node's relay instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PeerRoute {
    /// Each node at its advertised API endpoint.
    #[default]
    Direct,
    /// Each peer through the entry node's `/v1/nodes/{node}/relay`.
    Relay,
}

impl PeerRoute {
    /// Pick the route once per run, by asking each peer's `/v1/health` both
    /// ways.
    ///
    /// Direct unless some peer fails directly but answers through the relay.
    /// A peer that answers neither way is down, which says nothing about the
    /// route. A cluster the caller can reach stays direct: one hop, and it
    /// still reaches a node that gossip has forgotten.
    pub async fn detect(client: &BunClient, entry_node: &str) -> Self {
        let Ok(nodes) = client.nodes().await else {
            return Self::Direct;
        };
        for node in nodes.iter().filter(|node| node.node_id != entry_node) {
            if answers_health(client.for_node(node)).await {
                continue;
            }
            if answers_health(client.via_node(&node.node_id)).await {
                return Self::Relay;
            }
        }
        Self::Direct
    }
}

async fn answers_health(client: Result<BunClient, crate::relish::RelishError>) -> bool {
    match client {
        Ok(client) => client.health().await.is_ok(),
        Err(_) => false,
    }
}

/// One test case's handle on the cluster.
#[derive(Clone)]
pub struct TestContext {
    pub client: BunClient,
    /// This case's own namespace, e.g. `rbtest-4f2a91-03`.
    pub namespace: String,
    /// Server-owned app/resource lease. Production runners always set this;
    /// `None` remains only for focused unit tests of the runner itself.
    pub(crate) lease_id: Option<String>,
    /// Exact chaos faults owned by this case. The runner retains a clone so a
    /// timed-out or panicking body cannot lose reversal ownership.
    pub(crate) chaos_guard: crate::testkit::chaos::ChaosGuard,
    pub capabilities: ClusterCapabilities,
    /// Per-case budget, from `--timeout`. Poll loops measure against it so a
    /// case fails with a useful message rather than being killed from
    /// outside with none.
    pub timeout: Duration,
    /// One absolute case deadline. Poll helpers must not start fresh budgets.
    pub deadline: Deadline,
    /// How [`Self::node_clients`] reaches peer nodes.
    pub peer_route: PeerRoute,
    /// What the case's current wait is waiting for, for a timeout report.
    pub(crate) wait_note: WaitNote,
}

/// What a case's poll helper is waiting for and what it last saw.
///
/// A helper gives up on the same deadline the runner enforces, so the runner
/// usually cancels the body before the helper's own message comes back. The
/// helper writes that message here on every poll instead, and the runner adds
/// it to the timeout. Clones share one note.
#[derive(Clone, Default)]
pub(crate) struct WaitNote(std::sync::Arc<tokio::sync::Mutex<Option<String>>>);

impl WaitNote {
    /// Replace the note with the current wait's state.
    pub(crate) async fn record(&self, note: String) {
        *self.0.lock().await = Some(note);
    }

    /// Forget the note once its wait is over.
    pub(crate) async fn clear(&self) {
        *self.0.lock().await = None;
    }

    /// The note left by a wait that never finished, if any.
    pub(crate) async fn take(&self) -> Option<String> {
        self.0.lock().await.take()
    }
}

impl TestContext {
    /// Inspect the actual owning node, since the entry node may not run this app.
    pub async fn exec_in_workload(&self, app: &str, command: &[String]) -> Result<String, String> {
        self.deadline
            .run("inspect workload contents", async {
                for (node, client) in self.node_clients().await? {
                    let instances = client
                        .status()
                        .await
                        .map_err(|error| format!("could not inspect node {node}: {error}"))?;
                    if instances.iter().any(|instance| {
                        instance.app_name == app
                            && instance.namespace == self.namespace
                            && instance.state == "running"
                    }) {
                        return client
                            .exec(app, &self.namespace, command)
                            .await
                            .map_err(|error| {
                                format!("workload inspection failed on {node}: {error}")
                            });
                    }
                }
                Err(format!(
                    "no running instance of {}/{app} found",
                    self.namespace
                ))
            })
            .await
            .map_err(|error| error.to_string())?
    }

    /// Build a bounded HTTP client for workloads, without cluster credentials.
    ///
    /// Workload requests must never use the authenticated Bun API client.
    /// Redirects and ambient proxies are disabled to keep probes on their target.
    pub fn workload_http_client(&self) -> Result<reqwest::Client, String> {
        self.client
            .workload_http_builder()?
            .build()
            .map_err(|error| format!("could not create workload HTTP client: {error}"))
    }

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
        let node_jobs = self
            .lease_id
            .as_deref()
            .is_some_and(crate::testkit::lease::is_node_job_lease);
        let lease_compatible = config.permission.is_empty()
            && config.build.is_empty()
            && if node_jobs {
                !config.job.is_empty() && config.app.is_empty() && config.namespace.is_empty()
            } else {
                config.job.is_empty() && (!config.app.is_empty() || !config.namespace.is_empty())
            };
        let result = match &self.lease_id {
            Some(lease_id) if lease_compatible => {
                self.client.apply_with_lease(&config, lease_id).await
            }
            Some(_) => {
                return Err(
                    "test lease refuses unsupported or empty manifests for its resource scope"
                        .to_string(),
                );
            }
            None => self.client.apply(&config).await,
        };
        result
            .map(|_| ())
            .map_err(|error| format!("apply failed: {error}"))
    }

    /// Fault owner shared with the runner's unconditional teardown path.
    pub fn chaos(&self) -> &crate::testkit::chaos::ChaosGuard {
        &self.chaos_guard
    }

    /// The client that injects and later reverses a node fault aimed at the
    /// node `node_client` reaches.
    ///
    /// Directly, that is the target's own API. Through the relay it is the
    /// entry node: the relay forwards no fault requests, but Bun routes a
    /// node fault, and its reversal, to the `target_node` it names.
    pub fn fault_owner(&self, node_client: BunClient) -> BunClient {
        match self.peer_route {
            PeerRoute::Direct => node_client,
            PeerRoute::Relay => self.client.clone(),
        }
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
    /// Runs the digest-pinned multi-architecture test workload doing nothing
    /// but staying up, so a case can `exec` into it. This is the workload for
    /// volume and firewall-source cases. No health check, so it reaches
    /// Running as soon as the container starts. Needs
    /// [`Capability::ContainerRuntime`](crate::bun::capabilities::Capability::ContainerRuntime).
    pub fn container_idle_spec(&self, app: &str) -> String {
        format!(
            "[app.{app}]\n\
             image = \"{PINNED_TEST_WORKLOAD_IMAGE}\"\n\
             command = [\"/bin/sh\", \"-c\", \"{script}\"]\n\
             namespace = \"{ns}\"\n",
            script = exit_on_sigterm("/bin/busybox sleep infinity"),
            ns = self.namespace,
        )
    }

    /// A TOML spec for an HTTP container workload in this test's namespace.
    ///
    /// Creates a known response file and serves it with the pinned workload's
    /// HTTP server. Neither the executable nor its content depends on image
    /// defaults. This is the ingress/firewall fixture; its port derives from
    /// the app name.
    pub fn container_http_spec(&self, app: &str, replicas: u32) -> String {
        let port = testapp_port(app);
        format!(
            "[app.{app}]\n\
             image = \"{PINNED_TEST_WORKLOAD_IMAGE}\"\n\
             command = [\"/bin/sh\", \"-c\", \"{script}\"]\n\
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
            script = container_http_script(port, 0),
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
    /// placement fans out with this. The entry node is the connection the run
    /// already has. Peers go by [`Self::peer_route`]: directly, where each must
    /// supply its own resolved API endpoint, or through the entry node's relay.
    /// Missing evidence fails collection; it never guesses a port or silently
    /// omits a node. The entry client's credentials and CA are reused.
    pub async fn node_clients(&self) -> Result<Vec<(String, BunClient)>, String> {
        if self
            .lease_id
            .as_deref()
            .is_some_and(crate::testkit::lease::is_node_job_lease)
        {
            return Ok(vec![("local".to_string(), self.client.clone())]);
        }
        let nodes = self
            .client
            .nodes()
            .await
            .map_err(|error| format!("could not list nodes: {error}"))?;
        if nodes.is_empty() {
            return Ok(vec![("local".to_string(), self.client.clone())]);
        }
        nodes
            .into_iter()
            .map(|node| {
                let client = if node.node_id == self.capabilities.node_id {
                    Ok(self.client.clone())
                } else {
                    match self.peer_route {
                        PeerRoute::Direct => self.client.for_node(&node),
                        PeerRoute::Relay => self.client.via_node(&node.node_id),
                    }
                }
                .map_err(|error| error.to_string())?;
                Ok((node.node_id, client))
            })
            .collect()
    }

    /// Every instance of `app` in this namespace, gathered across all nodes.
    pub async fn cluster_instances(&self, app: &str) -> Result<Vec<InstanceStatus>, String> {
        self.deadline
            .run("cluster instance collection", async {
                let mut all = Vec::new();
                for (node, client) in self.node_clients().await? {
                    let statuses = client
                        .status()
                        .await
                        .map_err(|error| format!("could not inspect node {node}: {error}"))?;
                    all.extend(statuses.into_iter().filter(|instance| {
                        instance.app_name == app && instance.namespace == self.namespace
                    }));
                }
                Ok(all)
            })
            .await
            .map_err(|error| error.to_string())?
    }

    async fn namespace_instances(&self) -> Result<Vec<InstanceStatus>, String> {
        let mut all = Vec::new();
        for (node, client) in self.node_clients().await? {
            let statuses = client
                .status()
                .await
                .map_err(|error| format!("could not inspect cleanup on node {node}: {error}"))?;
            all.extend(
                statuses
                    .into_iter()
                    .filter(|instance| instance.namespace == self.namespace),
            );
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
        let mut last: Vec<InstanceStatus> = Vec::new();
        loop {
            let last_error = match self.cluster_instances(app).await {
                Ok(instances) => {
                    last = instances;
                    if predicate(&last) {
                        self.wait_note.clear().await;
                        return Ok(());
                    }
                    None
                }
                Err(error) => Some(error),
            };
            let seen: Vec<&str> = last.iter().map(|i| i.state.as_str()).collect();
            let waiting = format!(
                "waiting for {app} to reach {what} cluster-wide; \
                 last saw {} instance(s): {seen:?}; last query error: {last_error:?}",
                last.len()
            );
            if self.deadline.remaining().is_zero() {
                return Err(format!("timed out after {:?} {waiting}", self.timeout));
            }
            self.wait_note.record(waiting).await;
            tokio::time::sleep(Duration::from_millis(500).min(self.deadline.remaining())).await;
        }
    }

    /// The declared Pickle origin, including its actual scheme and bound port.
    pub fn registry_base(&self) -> Result<String, String> {
        self.service_endpoint(
            self.capabilities.service_endpoints.registry.as_deref(),
            "registry",
        )
        .map(|url| url.as_str().trim_end_matches('/').to_string())
    }

    /// Select an explicitly declared ingress listener, preferring HTTP when
    /// both protocols are available. No native port is inferred from the API.
    pub fn ingress_endpoint(&self) -> Result<url::Url, String> {
        let endpoints = &self.capabilities.service_endpoints;
        self.service_endpoint(
            endpoints
                .ingress_http
                .as_deref()
                .or(endpoints.ingress_https.as_deref()),
            "ingress",
        )
    }

    fn service_endpoint(&self, declared: Option<&str>, service: &str) -> Result<url::Url, String> {
        let declared = declared.ok_or_else(|| {
            format!(
                "{service} endpoint is not declared; managed clusters need an explicit host forward"
            )
        })?;
        let mut endpoint = url::Url::parse(declared)
            .map_err(|error| format!("invalid {service} endpoint: {error}"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.path() != "/"
            || endpoint.port_or_known_default() == Some(0)
        {
            return Err(format!("invalid {service} origin"));
        }
        let api = url::Url::parse(self.client.base_url()).map_err(|error| error.to_string())?;
        let address = match endpoint.host() {
            Some(url::Host::Ipv4(address)) => Some(std::net::IpAddr::V4(address)),
            Some(url::Host::Ipv6(address)) => Some(std::net::IpAddr::V6(address)),
            _ => None,
        };
        if address.is_some_and(|address| address.is_unspecified()) {
            endpoint
                .set_host(api.host_str())
                .map_err(|error| error.to_string())?;
        } else if address.is_some_and(|address| address.is_loopback()) {
            let api_loopback = match api.host() {
                Some(url::Host::Ipv4(address)) => address.is_loopback(),
                Some(url::Host::Ipv6(address)) => address.is_loopback(),
                _ => false,
            };
            if !api_loopback {
                return Err(format!(
                    "{service} listener is node-local; configure an explicit reachable forward"
                ));
            }
        }
        Ok(endpoint)
    }

    /// Keep the application's Host and TLS SNI while connecting to the declared
    /// listener or host forward. Resolution and HTTP have independent bounds.
    pub async fn ingress_probe(
        &self,
        hostname: &str,
    ) -> Result<(url::Url, reqwest::Client), String> {
        let mut endpoint = self.ingress_endpoint()?;
        let port = endpoint
            .port_or_known_default()
            .ok_or("ingress port is missing")?;
        let host = match endpoint.host().ok_or("ingress host is missing")? {
            url::Host::Domain(host) => host.to_string(),
            url::Host::Ipv4(host) => host.to_string(),
            url::Host::Ipv6(host) => host.to_string(),
        };
        let addresses: Vec<_> = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::lookup_host((host.as_str(), port)),
        )
        .await
        .map_err(|_| "ingress endpoint resolution timed out")?
        .map_err(|error| error.to_string())?
        .collect();
        if addresses.is_empty() {
            return Err("ingress endpoint resolved to no addresses".into());
        }
        endpoint
            .set_host(Some(hostname))
            .map_err(|error| error.to_string())?;
        let client = self
            .client
            .workload_http_builder()?
            .resolve_to_addrs(hostname, &addresses)
            .build()
            .map_err(|error| error.to_string())?;
        Ok((endpoint, client))
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
        let poll = Duration::from_millis(500);
        let mut last: Vec<InstanceStatus> = Vec::new();
        loop {
            if let Ok(Ok(all)) = self.deadline.run("status poll", self.client.status()).await {
                last = all
                    .into_iter()
                    .filter(|instance| {
                        instance.app_name == app && instance.namespace == self.namespace
                    })
                    .collect();
                if predicate(&last) {
                    self.wait_note.clear().await;
                    return Ok(());
                }
            }
            let seen: Vec<(&str, &str)> = last
                .iter()
                .map(|instance| (instance.app_name.as_str(), instance.state.as_str()))
                .collect();
            let waiting = format!(
                "waiting for {app} to reach {what}; last saw {} instance(s): {seen:?}",
                last.len()
            );
            if self.deadline.remaining().is_zero() {
                return Err(format!("timed out after {:?} {waiting}", self.timeout));
            }
            self.wait_note.record(waiting).await;
            tokio::time::sleep(poll.min(self.deadline.remaining())).await;
        }
    }

    /// Attempt reversal of owned faults and leased resources, then report evidence.
    ///
    /// The runner calls this after pass, fail, panic or timeout. Production
    /// cleanup uses the server's lease ownership record and checks runtime
    /// absence independently. An unreachable owner or expired cleanup deadline
    /// returns unknown, not a guarantee that resources are gone. The
    /// lease-free unit-test path also checks [`is_test_namespace`](Self::is_test_namespace).
    pub async fn teardown(&self, deadline: Deadline) -> CleanupOutcome {
        let faults = self.chaos_guard.cleanup(deadline).await;
        let resources = self.teardown_resources(deadline).await;
        merge_cleanup(faults, resources)
    }

    async fn teardown_resources(&self, deadline: Deadline) -> CleanupOutcome {
        if let Some(lease_id) = &self.lease_id {
            match deadline
                .run("lease release", self.client.release_test_lease(lease_id))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(crate::relish::RelishError::ApiError { status: 404, .. })) => {
                    // The expiry reaper may have completed first. Absence is
                    // durable ownership evidence, but runtime absence still
                    // needs the independent check below.
                }
                Ok(Err(crate::relish::RelishError::AgentUnreachable)) => {
                    return CleanupOutcome::Unknown {
                        reason: "could not reach the lease owner to confirm cleanup".to_string(),
                    };
                }
                // Reached it, but the lease was still held when the release's
                // own 30 s budget ran out: not the same as unreachable.
                Ok(Err(crate::relish::RelishError::RequestTimeout)) => {
                    return CleanupOutcome::Unknown {
                        reason: "the lease owner did not confirm cleanup within 30 s".to_string(),
                    };
                }
                Ok(Err(error)) => {
                    return CleanupOutcome::Failed {
                        reason: format!("server-owned lease cleanup failed: {error}"),
                    };
                }
                Err(error) => {
                    return CleanupOutcome::Unknown {
                        reason: error.to_string(),
                    };
                }
            }
            let mut last_error = None;
            loop {
                match deadline
                    .run("cleanup confirmation", self.namespace_instances())
                    .await
                {
                    Ok(Ok(instances)) if instances.is_empty() => {
                        return CleanupOutcome::Confirmed;
                    }
                    Ok(Ok(_)) => last_error = None,
                    Ok(Err(error)) => last_error = Some(error),
                    Err(error) => {
                        return CleanupOutcome::Unknown {
                            reason: match last_error {
                                Some(last) => format!("{error}; last observation: {last}"),
                                None => error.to_string(),
                            },
                        };
                    }
                }
                tokio::time::sleep(Duration::from_millis(100).min(deadline.remaining())).await;
            }
        }
        if !Self::is_test_namespace(&self.namespace) {
            return CleanupOutcome::Failed {
                reason: format!("refused to clean unsafe namespace {}", self.namespace),
            };
        }
        let instances = match deadline.run("cleanup status", self.client.status()).await {
            Ok(Ok(instances)) => instances,
            Ok(Err(error)) => {
                return CleanupOutcome::Unknown {
                    reason: format!("could not inspect owned resources: {error}"),
                };
            }
            Err(error) => {
                return CleanupOutcome::Unknown {
                    reason: error.to_string(),
                };
            }
        };
        let mut apps: Vec<String> = instances
            .iter()
            .filter(|instance| instance.namespace == self.namespace)
            .map(|instance| instance.app_name.clone())
            .collect();
        apps.sort_unstable();
        apps.dedup();
        if apps.is_empty() {
            return CleanupOutcome::NotRequired;
        }
        for app in &apps {
            match deadline
                .run("cleanup stop", self.client.stop(app, &self.namespace))
                .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    return CleanupOutcome::Failed {
                        reason: format!("failed to stop {app}: {error}"),
                    };
                }
                Err(error) => {
                    return CleanupOutcome::Unknown {
                        reason: error.to_string(),
                    };
                }
            }
        }

        loop {
            match deadline
                .run("cleanup confirmation", self.client.status())
                .await
            {
                Ok(Ok(instances))
                    if !instances.iter().any(|instance| {
                        instance.namespace == self.namespace && apps.contains(&instance.app_name)
                    }) =>
                {
                    return CleanupOutcome::Confirmed;
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    return CleanupOutcome::Unknown {
                        reason: format!("could not confirm cleanup: {error}"),
                    };
                }
                Err(error) => {
                    return CleanupOutcome::Unknown {
                        reason: error.to_string(),
                    };
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn merge_cleanup(left: CleanupOutcome, right: CleanupOutcome) -> CleanupOutcome {
    use CleanupOutcome::{Confirmed, Failed, NotRequired, Unknown};

    match (left, right) {
        (Failed { reason: left }, Failed { reason: right }) => Failed {
            reason: format!("{left}; {right}"),
        },
        (Failed { reason }, _) | (_, Failed { reason }) => Failed { reason },
        (Unknown { reason: left }, Unknown { reason: right }) => Unknown {
            reason: format!("{left}; {right}"),
        },
        (Unknown { reason }, _) | (_, Unknown { reason }) => Unknown { reason },
        (NotRequired, NotRequired) => NotRequired,
        (Confirmed | NotRequired, Confirmed | NotRequired) => Confirmed,
    }
}

/// A stable, per-app port in the ephemeral range, so two apps in one case
/// don't collide. Deterministic (an FNV-1a hash of the name) so a case's
/// spec is reproducible between runs.
/// The shell script behind every HTTP container fixture, for `/bin/sh -c`.
///
/// It writes the `/hostname` file its health check asks for and serves only
/// that directory, naming BusyBox by absolute path. The pinned image has no
/// `PATH` and no `/etc/hostname`, so a fixture that leans on either never turns
/// healthy on a real container runtime. `startup_delay_secs` holds the server
/// back, for cases that need a deploy to stay in flight for a while.
pub(crate) fn container_http_script(port: u16, startup_delay_secs: u32) -> String {
    let delay = if startup_delay_secs == 0 {
        String::new()
    } else {
        format!("/bin/busybox sleep {startup_delay_secs}; ")
    };
    // No `exec`: httpd runs under the trapping shell, so PID 1 exits on
    // SIGTERM instead of sitting out the stop grace.
    exit_on_sigterm(&format!(
        "{delay}/bin/busybox mkdir -p {HTTP_ROOT}; \
         printf 'reliaburger-test' > {HTTP_ROOT}/hostname; \
         /bin/busybox httpd -f -p {port} -h {HTTP_ROOT}"
    ))
}

/// The directory [`container_http_script`] creates and serves.
const HTTP_ROOT: &str = "/tmp/reliaburger-test-http";

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
            lease_id: None,
            chaos_guard: crate::testkit::chaos::ChaosGuard::default(),
            capabilities: ClusterCapabilities::default(),
            timeout: Duration::from_millis(200),
            deadline: Deadline::after(Duration::from_millis(200)).unwrap(),
            peer_route: PeerRoute::Direct,
            wait_note: Default::default(),
        }
    }

    #[tokio::test]
    async fn node_clients_use_each_advertised_api_endpoint() {
        let router = axum::Router::new().route("/v1/cluster/nodes", axum::routing::get(|| async {
            axum::Json(serde_json::json!([
                {"node_id":"one", "address":"127.0.0.1:7946", "api_address":"127.0.0.1:19117",
                 "state":"alive", "incarnation":1, "is_council":true, "is_leader":true, "labels":{}},
                {"node_id":"two", "address":"[::1]:7947", "api_address":"[::1]:29117",
                 "state":"alive", "incarnation":1, "is_council":true, "is_leader":false, "labels":{}}
            ]))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut context = context("rbtest-endpoints");
        context.client = BunClient::new(&format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let clients = context.node_clients().await.unwrap();
        server.abort();
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].1.base_url(), "http://127.0.0.1:19117");
        assert_eq!(clients[1].1.base_url(), "http://[::1]:29117");
    }

    /// Two nodes as the entry node lists them. `one` is the entry node; both
    /// advertise API addresses the caller may or may not reach.
    fn two_nodes(entry_api: &str, peer_api: &str) -> serde_json::Value {
        serde_json::json!([
            {"node_id":"one", "address":"10.0.0.1:7946", "api_address":entry_api,
             "state":"alive", "incarnation":1, "is_council":true, "is_leader":true, "labels":{}},
            {"node_id":"two", "address":"10.0.0.2:7946", "api_address":peer_api,
             "state":"alive", "incarnation":1, "is_council":true, "is_leader":false, "labels":{}}
        ])
    }

    /// A loopback port nothing listens on.
    async fn closed_port() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    }

    /// Serve `router` on a fresh loopback port.
    async fn serve(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, server)
    }

    /// An entry node that lists `nodes` and relays `health` and `status` for
    /// node `two` only, as the real relay would for a reachable peer.
    fn entry_router(nodes: serde_json::Value, relay_answers: bool) -> axum::Router {
        use axum::{extract::Path, http::StatusCode, response::IntoResponse, routing::get};
        axum::Router::new()
            .route(
                "/v1/cluster/nodes",
                get(move || {
                    let nodes = nodes.clone();
                    async move { axum::Json(nodes) }
                }),
            )
            .route(
                "/v1/nodes/{node}/relay/{*path}",
                get(
                    move |Path((node, path)): Path<(String, String)>| async move {
                        if !relay_answers || node != "two" {
                            return StatusCode::BAD_GATEWAY.into_response();
                        }
                        match path.as_str() {
                            "v1/health" => {
                                axum::Json(serde_json::json!({"status": "ok"})).into_response()
                            }
                            "v1/status" => axum::Json(serde_json::json!([])).into_response(),
                            _ => StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                ),
            )
    }

    #[tokio::test]
    async fn relayed_peers_go_through_the_entry_node_and_the_entry_node_uses_its_own_connection() {
        // Neither advertised address is reachable, as from a laptop: the
        // guests' own API ports mean nothing on the host.
        let unreachable = closed_port().await.to_string();
        let (base, server) = serve(entry_router(two_nodes(&unreachable, &unreachable), true)).await;
        let mut context = context("rbtest-relay");
        context.client = BunClient::new(&base);
        context.capabilities.node_id = "one".to_string();
        context.peer_route = PeerRoute::Relay;

        let clients = context.node_clients().await.unwrap();
        assert_eq!(clients[0].0, "one");
        assert_eq!(clients[0].1.base_url(), base);
        assert_eq!(clients[1].0, "two");
        assert_eq!(
            clients[1].1.base_url(),
            format!("{base}/v1/nodes/two/relay")
        );
        // The per-node read the cleanup check makes arrives through the relay.
        assert!(clients[1].1.status().await.unwrap().is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn direct_route_still_uses_the_entry_connection_for_the_entry_node() {
        let unreachable = closed_port().await.to_string();
        let (base, server) = serve(entry_router(
            two_nodes(&unreachable, "127.0.0.1:29117"),
            true,
        ))
        .await;
        let mut context = context("rbtest-direct");
        context.client = BunClient::new(&base);
        context.capabilities.node_id = "one".to_string();

        let clients = context.node_clients().await.unwrap();
        server.abort();
        assert_eq!(clients[0].1.base_url(), base);
        assert_eq!(clients[1].1.base_url(), "http://127.0.0.1:29117");
    }

    #[tokio::test]
    async fn peer_route_is_relay_when_a_peer_answers_only_through_the_entry_node() {
        let unreachable = closed_port().await.to_string();
        let (base, server) = serve(entry_router(two_nodes(&unreachable, &unreachable), true)).await;
        let route = PeerRoute::detect(&BunClient::new(&base), "one").await;
        server.abort();
        assert_eq!(route, PeerRoute::Relay);
    }

    #[tokio::test]
    async fn peer_route_stays_direct_when_every_peer_answers_directly() {
        let (peer, peer_server) = serve(axum::Router::new().route(
            "/v1/health",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        ))
        .await;
        let peer = peer.trim_start_matches("http://").to_string();
        let unreachable = closed_port().await.to_string();
        let (base, server) = serve(entry_router(two_nodes(&unreachable, &peer), true)).await;
        let route = PeerRoute::detect(&BunClient::new(&base), "one").await;
        server.abort();
        peer_server.abort();
        assert_eq!(route, PeerRoute::Direct);
    }

    #[tokio::test]
    async fn a_peer_down_both_ways_says_nothing_about_the_route() {
        let unreachable = closed_port().await.to_string();
        let (base, server) =
            serve(entry_router(two_nodes(&unreachable, &unreachable), false)).await;
        let route = PeerRoute::detect(&BunClient::new(&base), "one").await;
        server.abort();
        assert_eq!(route, PeerRoute::Direct);
    }

    #[tokio::test]
    async fn node_clients_refuse_missing_or_unusable_peer_endpoints() {
        for endpoint in [
            serde_json::Value::Null,
            serde_json::json!("0.0.0.0:9117"),
            serde_json::json!("127.0.0.1:0"),
            serde_json::json!("not-an-address"),
        ] {
            let router = axum::Router::new().route("/v1/cluster/nodes", axum::routing::get(move || {
                let endpoint = endpoint.clone();
                async move { axum::Json(serde_json::json!([
                    {"node_id":"broken", "address":"127.0.0.1:7946", "api_address":endpoint,
                     "state":"alive", "incarnation":1, "is_council":true, "is_leader":true, "labels":{}}
                ])) }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut context = context("rbtest-endpoints");
            context.client = BunClient::new(&format!("http://{}", listener.local_addr().unwrap()));
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let result = context.node_clients().await;
            server.abort();
            assert!(
                result.is_err(),
                "missing peer evidence must not yield a guessed or empty inventory"
            );
        }
    }

    #[tokio::test]
    async fn accepted_lease_cleanup_waits_for_durable_absence() {
        use axum::{
            http::StatusCode,
            routing::{delete, get},
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        for disappears in [false, true] {
            let polls = Arc::new(AtomicUsize::new(0));
            let observed = polls.clone();
            let router = axum::Router::new()
                .route(
                    "/v1/test/leases/owned",
                    delete(|| async { StatusCode::ACCEPTED }).get(move || {
                        let count = observed.fetch_add(1, Ordering::SeqCst);
                        async move {
                            if disappears && count > 0 {
                                StatusCode::NOT_FOUND
                            } else {
                                StatusCode::OK
                            }
                        }
                    }),
                )
                .route(
                    "/v1/cluster/nodes",
                    get(|| async { axum::Json(Vec::<crate::bun::agent::NodeStatus>::new()) }),
                )
                .route(
                    "/v1/status",
                    get(|| async { axum::Json(Vec::<crate::bun::agent::InstanceStatus>::new()) }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut ctx = context("rbtest-cleanup");
            ctx.client = BunClient::new_with_token(
                &format!("http://{}", listener.local_addr().unwrap()),
                None,
            );
            ctx.lease_id = Some("owned".into());
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let outcome = ctx
                .teardown(Deadline::after(Duration::from_millis(450)).unwrap())
                .await;
            server.abort();
            if disappears {
                assert_eq!(outcome, CleanupOutcome::Confirmed);
                assert!(polls.load(Ordering::SeqCst) >= 2);
            } else {
                assert!(
                    matches!(outcome, CleanupOutcome::Unknown { .. }),
                    "{outcome:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn lease_cleanup_keeps_unreachable_runtime_evidence_unknown_at_the_deadline() {
        let (client, server) = status_server(false).await;
        let mut ctx = context("rbtest-unreachable");
        ctx.client = client;
        ctx.lease_id = Some("already-released".into());
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            ctx.teardown(Deadline::after(Duration::from_millis(200)).unwrap()),
        )
        .await
        .unwrap();
        server.abort();
        assert!(
            matches!(result, CleanupOutcome::Unknown { .. }),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn lease_cleanup_retries_busy_status_until_absence_is_observed() {
        use axum::{
            response::IntoResponse,
            routing::{delete, get},
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let router = axum::Router::new()
            .route(
                "/v1/test/leases/owned",
                delete(|| async { axum::http::StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/cluster/nodes",
                get(|| async { axum::Json(Vec::<crate::bun::agent::NodeStatus>::new()) }),
            )
            .route(
                "/v1/status",
                get(move || {
                    let number = observed.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if number == 0 {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                        } else {
                            axum::Json(Vec::<crate::bun::agent::InstanceStatus>::new())
                                .into_response()
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut ctx = context("rbtest-cleanup");
        ctx.client = BunClient::new_with_token(&format!("http://{address}"), None);
        ctx.lease_id = Some("owned".into());
        let result = ctx
            .teardown(Deadline::after(Duration::from_secs(2)).unwrap())
            .await;
        server.abort();
        assert!(matches!(result, CleanupOutcome::Confirmed), "{result:?}");
        assert!(polls.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn leased_apply_never_falls_back_to_unowned_mutations() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let router = axum::Router::new().route(
            "/v1/apply",
            axum::routing::post(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                async { axum::http::StatusCode::SERVICE_UNAVAILABLE }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut ctx = context("rbtest-admission");
        ctx.client = BunClient::new_with_token(&format!("http://{address}"), None);
        ctx.lease_id = Some("lease".into());
        for manifest in [
            "[job.batch]\nimage = \"busybox:latest\"\nnamespace = \"rbtest-admission\"\n",
            "[permission.operator]\nactions = [\"deploy\"]\n",
            "[build.image]\ncontext = \".\"\ndestination = \"pickle://image:v1\"\n",
        ] {
            assert!(ctx.apply(manifest).await.is_err());
        }
        server.abort();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "unsupported resources escaped their lease before refusal"
        );
    }

    async fn status_server(stalled: bool) -> (BunClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route(
                "/v1/cluster/nodes",
                axum::routing::get(|| async {
                    axum::Json(Vec::<crate::bun::agent::NodeStatus>::new())
                }),
            )
            .route(
                "/v1/status",
                axum::routing::get(move || async move {
                    if stalled {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (
            BunClient::new_with_token(&format!("http://{address}"), None),
            server,
        )
    }

    #[tokio::test]
    async fn cluster_collection_reports_a_failed_node_instead_of_empty_success() {
        let (client, server) = status_server(false).await;
        let mut ctx = context("rbtest-errors");
        ctx.client = client;
        let result = ctx.cluster_instances("web").await;
        server.abort();
        assert!(result.unwrap_err().contains("local"));
    }

    #[tokio::test]
    async fn cluster_wait_cannot_outlive_its_shared_deadline() {
        let (client, server) = status_server(true).await;
        let mut ctx = context("rbtest-deadline");
        ctx.client = client;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            ctx.wait_for_cluster("web", "no instances", |instances| instances.is_empty()),
        )
        .await;
        server.abort();
        assert!(
            result
                .expect("cluster collection exceeded the case deadline")
                .is_err()
        );
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

    #[tokio::test]
    async fn workload_requests_do_not_receive_the_cluster_bearer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                axum::Json(headers.contains_key(axum::http::header::AUTHORIZATION))
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut ctx = context("rbtest-http-00");
        ctx.client =
            BunClient::new_with_token(&format!("http://{address}"), Some("private-cluster-token"));
        let leaked = ctx
            .workload_http_client()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap()
            .json::<bool>()
            .await
            .unwrap();
        server.abort();
        assert!(!leaked, "workload requests must never carry the API bearer");
    }

    #[test]
    fn registry_uses_declared_ipv6_scheme_and_port() {
        let mut ctx = context("rbtest-endpoints");
        ctx.client = BunClient::new_with_token("http://[::1]:19117", None);
        let mut report = serde_json::to_value(&ctx.capabilities).unwrap();
        report["service_endpoints"] = serde_json::json!({
            "registry": "https://[::1]:15051"
        });
        ctx.capabilities = serde_json::from_value(report).unwrap();
        assert_eq!(ctx.registry_base().unwrap(), "https://[::1]:15051");
    }

    #[tokio::test]
    async fn registry_reaches_a_declared_ipv6_listener_with_explicit_authentication() {
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new().route(
            "/v2/",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                assert_eq!(
                    headers.get("authorization").unwrap(),
                    "Bearer registry-token"
                );
                "registry"
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut ctx = context("rbtest-ipv6");
        ctx.client = BunClient::new_with_token("http://[::1]:19117", Some("registry-token"));
        ctx.capabilities.service_endpoints.registry = Some(format!("http://{address}"));
        let origin = ctx.registry_base().unwrap();
        let client = ctx.client.registry_http_client(&origin).unwrap();
        assert_eq!(
            client
                .get(format!("{origin}/v2/"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "registry"
        );
        server.abort();
    }

    #[test]
    fn endpoints_refuse_guesses_and_use_the_api_host_only_for_wildcard_binds() {
        let mut ctx = context("rbtest-endpoints");
        assert!(ctx.registry_base().unwrap_err().contains("not declared"));
        assert!(ctx.ingress_endpoint().unwrap_err().contains("not declared"));
        ctx.client = BunClient::new_with_token("https://[2001:db8::1]:19117", None);
        ctx.capabilities.service_endpoints.registry = Some("https://[::]:15051".into());
        assert_eq!(ctx.registry_base().unwrap(), "https://[2001:db8::1]:15051");
        for invalid in [
            "http://127.0.0.1:5050",
            "https://user:secret@example.com",
            "ftp://example.com",
            "https://example.com/path",
            "https://example.com?token=secret",
        ] {
            ctx.capabilities.service_endpoints.registry = Some(invalid.into());
            assert!(ctx.registry_base().is_err(), "accepted {invalid}");
        }
    }

    #[tokio::test]
    async fn https_ingress_uses_a_forward_with_workload_sni_and_no_api_credentials() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let certificate =
            rcgen::generate_simple_self_signed(vec!["workload.example".into()]).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.key_pair.serialize_der())
                .into(),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
        let guest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let guest_address = guest.local_addr().unwrap();
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = guest.accept().await.unwrap();
            let mut tls = acceptor.accept(socket).await.unwrap();
            let sni = tls.get_ref().1.server_name().map(str::to_string);
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 1024];
                let count = tls.read(&mut chunk).await.unwrap();
                assert!(count > 0 && bytes.len() < 8192);
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            observed_tx
                .send((sni, String::from_utf8(bytes).unwrap()))
                .unwrap();
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            tls.shutdown().await.unwrap();
        });
        let forward = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = forward.local_addr().unwrap();
        let forwarding = tokio::spawn(async move {
            let (mut host, _) = forward.accept().await.unwrap();
            let mut guest = tokio::net::TcpStream::connect(guest_address).await.unwrap();
            tokio::io::copy_bidirectional(&mut host, &mut guest)
                .await
                .ok();
        });
        let mut ctx = context("rbtest-tls");
        ctx.client = BunClient::new_with_ca(
            "https://127.0.0.1:19117",
            Some("private-api-token"),
            certificate.cert.pem().as_bytes(),
        )
        .unwrap();
        ctx.capabilities.service_endpoints.ingress_https = Some(format!("https://{address}"));
        let (url, client) = ctx.ingress_probe("workload.example").await.unwrap();
        assert_eq!(url.port(), Some(address.port()));
        assert_eq!(
            client.get(url).send().await.unwrap().text().await.unwrap(),
            "ok"
        );
        let (sni, headers) = observed_rx.await.unwrap();
        assert_eq!(sni.as_deref(), Some("workload.example"));
        assert!(headers.to_lowercase().contains("host: workload.example:"));
        assert!(!headers.to_lowercase().contains("authorization:"));
        assert!(!headers.contains("private-api-token"));
        server.await.unwrap();
        forwarding.await.unwrap();
    }

    #[test]
    fn container_specs_parse_and_land_in_the_test_namespace() {
        let ctx = context("rbtest-abc-00");

        let http = Config::parse(&ctx.container_http_spec("web", 1)).unwrap();
        let web = http.app.get("web").expect("app web");
        assert_eq!(web.namespace.as_deref(), Some("rbtest-abc-00"));
        assert_eq!(web.image.as_deref(), Some(PINNED_TEST_WORKLOAD_IMAGE));
        let (_, digest) = PINNED_TEST_WORKLOAD_IMAGE
            .split_once("@sha256:")
            .expect("test workload must use a digest-pinned reference");
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(web.health.is_some());
        assert_eq!(web.port, Some(ctx.container_port("web")));
        assert!(
            std::path::Path::new(&web.command[0]).is_absolute(),
            "container fixtures must not depend on an image-provided PATH"
        );

        let idle = Config::parse(&ctx.container_idle_spec("box")).unwrap();
        let box_app = idle.app.get("box").expect("app box");
        assert_eq!(box_app.namespace.as_deref(), Some("rbtest-abc-00"));
        assert!(box_app.command[2].contains("/bin/busybox sleep infinity"));
        assert!(box_app.health.is_none());
        assert!(std::path::Path::new(&box_app.command[0]).is_absolute());
    }

    /// A container's PID 1 ignores any signal it has no handler for, so a bare
    /// `busybox sleep` or `httpd` sat out Bun's whole stop grace on every
    /// lease cleanup, holding the node's agent loop for ten seconds each.
    #[test]
    fn container_fixtures_trap_sigterm_as_pid_one() {
        let ctx = context("rbtest-abc-00");
        let specs = [
            ("web", ctx.container_http_spec("web", 1)),
            ("box", ctx.container_idle_spec("box")),
        ];
        for (app, spec) in specs {
            let config = Config::parse(&spec).unwrap();
            let command = &config.app[app].command;
            assert_eq!(command[..2], ["/bin/sh", "-c"], "{app}: {command:?}");
            assert!(command[2].starts_with(SIGTERM_TRAP), "{app}: {command:?}");
            assert!(command[2].ends_with("& wait"), "{app}: {command:?}");
        }
    }

    /// The trap wrapper, run by the host's `sh`: TERM ends it at once with
    /// status 0 while its long-running child is still going.
    #[tokio::test]
    async fn sigterm_wrapper_exits_promptly_while_its_child_runs() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let script = exit_on_sigterm(&format!("touch {}; sleep 30", ready.display()));
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        while !ready.exists() {
            assert!(started.elapsed() < Duration::from_secs(20), "never ready");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid = child.id().unwrap().to_string();
        let killed = tokio::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .await
            .unwrap();
        assert!(killed.success());

        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("the wrapper ignored SIGTERM")
            .unwrap();
        assert_eq!(status.code(), Some(0));
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
