# Reliaburger and the AI SRE

Research note, 18 October 2026, written against main at `499863ae`. The
maintainer scheduled all of it as
[0.4.2](https://github.com/reliaburger/reliaburger/milestone/18), "Plug in an
AI SRE", the same day; section 4 maps each idea to its issue.

## The short version

Can Reliaburger become the easiest orchestrator to plug an AI SRE into? Yes,
and for a reason that has nothing to do with AI: everything an agent needs is
already in one binary, behind one API, with one token model and one audit log.
A Kubernetes agent has to stitch together the API server, Prometheus, Loki or
Elasticsearch, an events exporter, Argo or Flux, and a chaos tool, each with
its own auth and its own idea of time. We don't have that problem. We have
`relish wtf`, which already emits a versioned JSON report whose `UNKNOWN` rows
refuse to pass for green. That's the hardest part of an AI SRE (honest
evidence), done.

What's missing is the last metre: a standard protocol to reach it (MCP), a
token and approval model built for a caller that can be talked into things
(prompt injection through logs is real), a merged incident timeline, and a way
to measure whether an agent is any good (our Smoker faults plus the V02 soak
give us ground truth that nobody else has for free).

On Rocketgraph: don't depend on it. Its licence file is the Business Source
License, despite the README saying Apache 2.0; it's a Python/FastAPI service
with one contributor and 17 commits; and it targets the Datadog/Loki world, not
an orchestrator. The idea in it (compress logs into templates, flag the new and
rare ones, let the LLM explain only the deterministic output) is good, and
small enough to build in Rust inside Ketchup and feed into `wtf`.

The recommended first part of 0.4.2 is a read-only `relish mcp` server, agent-shaped
tokens, a timeline endpoint, an `llms.txt`, and an eval harness on five Smoker
scenarios. Mutations (behind proposals and human approval) come second, in the same
milestone.

## 1. Rocketgraph

### What it is

[Rocketgraph](https://github.com/Rocketgraph/rocketgraph) describes itself as
an "Agent layer for observability". Its README calls it "self-hosted log
clustering and streaming anomaly detection that drops in next to the
observability stack you already run". Strip the framing away and it's two
things:

- **An ML engine** (`ml/`), a stateless Python FastAPI service on port 9020.
  It pulls a window of logs from a source you already run, then runs three
  deterministic algorithms in a row
  ([ml/README.md](https://github.com/Rocketgraph/rocketgraph/blob/main/ml/README.md)):
  1. **Drain3** mines log templates. It masks UUIDs, IPs and numbers, then
     routes each line through a fixed-depth parse tree. Drain is from
     [He, Zhu, Zheng and Lyu, ICWS 2017](https://logparser.readthedocs.io/en/latest/tools/Drain.html);
     the README credits "Du & Li, ICSE 2017", which is wrong. The maintained
     Python version is [logpai/Drain3](https://github.com/logpai/Drain3).
  2. **Isolation Forest** scores each template per service.
  3. **Half-Space-Trees** is an online model, one per service, that scores new
     lines as they arrive.

  Its endpoints are `GET /clusters`, `POST /clusters/train` and
  `POST /anomalies/detect`. Every anomaly carries a `reasons` array:
  `anomaly_score`, `new_template` or `error_burst`. The headline number is
  2,002,271 logs reduced to 58 templates and 9 anomalies in 90 seconds on one
  container
  ([ml/README.md](https://github.com/Rocketgraph/rocketgraph/blob/main/ml/README.md)).
- **`@rgraph/otel-node`**, a Claude-powered CLI that reads a Node.js project
  and writes its OpenTelemetry instrumentation file
  ([packages/otel-node](https://github.com/Rocketgraph/rocketgraph/tree/main/packages/otel-node)).
  Running it needs a `ROCKETGRAPH_API_KEY`, so it leads into the paid service.

The LLM only appears at the edges. `analyze.py --ai` hands the clusters to
Claude for a triage, and the README says "the engine itself stays deliberately
LLM-free and reproducible; the model only explains the deterministic clusters"
([README](https://github.com/Rocketgraph/rocketgraph)).

**Integrations.** It can read log files, OpenTelemetry, Loki, New Relic,
Datadog, CloudWatch, Sentry and ClickHouse; Splunk, Elastic, Azure and GCP are
on its roadmap ([README](https://github.com/Rocketgraph/rocketgraph)). There's
no MCP server and no Kubernetes integration. Kubernetes appears only as
`kubectl logs > app.log`. Installing it means `docker compose up`.

**The problem it solves** is token budget and noise. Two million raw log lines
won't fit in a model's context; 58 templates with anomaly flags will. The
commercial product behind it, at [rocketgraph.app](https://rocketgraph.app),
sells this as "Mission Control": logs compressed into snapshots so an LLM can
find root causes without burning tokens. It also sells "Detective" (ML
anomaly alerts). Pricing runs from a free 7-day trial to $50 a month for
50M logs, then Enterprise, and anomaly detection is listed only under
Enterprise ([rocketgraph.app](https://rocketgraph.app)).

### Maturity

Figures from the GitHub API on 18 October 2026.

| | |
|---|---|
| Stars / forks | 169 / 11 |
| Commits on `main` | 17 in total ([history](https://github.com/Rocketgraph/rocketgraph/commits/main)) |
| Contributors | 1 (`kaushik94`, 11 commits; the rest are merges) |
| Releases | 1, [v0.1.0](https://github.com/Rocketgraph/rocketgraph/releases/tag/v0.1.0), 27 May 2026 |
| Last push | 3 June 2026 (four and a half months of silence) |
| Languages | Python 54%, TypeScript 30%, plus JavaScript, HTML, shell (by bytes) |
| Open issues | 1 |

The repository dates from June 2022, but it began as something else: an
open-source Firebase alternative built on Hasura and Postgres
([dev.to, 2022](https://dev.to/kaushik94/open-source-alternative-to-firebase-rocketgraph-4b0i)).
A "rebranding" commit landed in March 2026, and the observability code
arrived between 25 May and 3 June 2026
([history](https://github.com/Rocketgraph/rocketgraph/commits/main)). So the
product we're assessing is about two weeks of work by one person, and nobody
has touched it since.

### Licence: a red flag

The README carries an "Apache 2.0" badge and its licence section says
"Apache 2.0". The licence file says otherwise.
[LICENSE.txt](https://github.com/Rocketgraph/rocketgraph/blob/main/LICENSE.txt)
is the [Business Source License 1.1](https://mariadb.com/bsl11/), added in a
commit titled "add BSL license" on 27 May 2026. Rocketgraph, Inc. is the
licensor. Its Additional Use Grant forbids offering the work "on a hosted or
embedded basis" in a competing paid product, and the work becomes GPL 2.0
four years after each version is published. GitHub's licence detection
reports `NOASSERTION`.

For us this decides it. Reliaburger is a single binary. Bundling or requiring
a BSL component that limits "embedded" use, from a vendor whose paid product
covers the same ground, isn't something an Apache-style infrastructure project
can do. And the README contradicts its own licence file, so we couldn't even
tell users clearly what they were getting.

### Option A: integrate with it, or depend on it

What it would take: run the ML engine as a Reliaburger app (it's a container),
and write a Ketchup connector, since it doesn't read our logs. The engine pulls
logs from a source; our logs live in Ketchup's per-node stores
(`src/ketchup/log_store.rs`) and Parquet exports (`relish logs-export`). We'd
either add a "Reliaburger" connector upstream or point it at exported files.

| For | Against |
|---|---|
| Zero algorithm work for us | BSL, and the README contradicts it |
| Proven algorithms (Drain, Isolation Forest, HS-Trees) | One contributor, 17 commits, quiet since June |
| A clean HTTP API | A Python and Docker sidecar: the opposite of "one binary, batteries included" |
| | It pulls logs out of the cluster to a separate service, so a second copy and a second trust boundary |
| | Its in-memory model is lost on restart, and it has no auth on `/credentials` or anything else ([endpoints](https://github.com/Rocketgraph/rocketgraph/blob/main/ml/README.md)) |
| | No MCP, no Kubernetes or orchestrator awareness, no tie to deploys or faults |

A fair middle ground: document how to run it next to Reliaburger as an
optional user-deployed app, the way the whitepaper treats tracing backends.
We wouldn't ship or depend on it.

### Option B: build the idea in Rust inside Reliaburger

The useful idea is small: **turn logs into a short, deterministic list of
templates, flag the new and bursty ones, and give the LLM that list instead of
raw lines.** We can do better than Rocketgraph here, because we know things it
can't:

- **When each deploy happened.** "A template first seen 40 seconds after
  `app.applied` for `payments`" is a much stronger signal than "a rare
  template". `wtf` already correlates crashloops with deploys
  (`src/relish/wtf/diagnose.rs`).
- **Which faults are active.** Smoker knows when we broke something on
  purpose, so a fault's own log noise can be labelled rather than flagged.
- **Per-instance identity** without a schema, because Ketchup already stores
  app, namespace, instance and stream with each line.

Building blocks:

- [`drain3`](https://crates.io/crates/drain3) 0.3.0, a Rust port of Drain3
  under Apache-2.0, published July 2026, with about 44k downloads. There's also
  [`logdrain`](https://crates.io/crates/logdrain). Drain itself is a few hundred
  lines, so writing our own (and explaining it in the book) is reasonable.
- Isolation Forest:
  [`extended-isolation-forest`](https://crates.io/crates/extended-isolation-forest),
  but untouched since 2022. We probably don't need it. For a first version,
  "template first seen since the last deploy" plus "template rate × N above
  its trailing baseline" gives deterministic, explainable flags with no ML at
  all. Half-Space-Trees has no maintained Rust crate we could find.

Where it fits: a template miner in Ketchup (`src/ketchup/`), run per node over
each app's recent lines, exposed as `GET /v1/logs/patterns/{app}/{namespace}`
next to `/v1/logs/query`. A new `wtf` evidence source
(`src/relish/wtf/collect.rs`) turns "new error template since deploy" into a
finding with the template, count, first-seen time and one example line. The
same endpoint becomes an MCP tool (`log_patterns`). Effort: M.

### Recommendation

**Build our own (Option B). Don't integrate or depend on Rocketgraph.** The
licence and maturity rule out a dependency, and a sidecar goes against the
single-binary design. The idea is worth taking, though, and it's cheap: it
fits naturally into Ketchup and `wtf`, and our version would be better because
it can correlate with deploys, faults and the audit log. Credit Drain properly
in the book, and mention Rocketgraph as prior art in the same "deterministic
compression, LLM only explains" vein.

Also, don't start here. Log patterns help an agent once it can reach the
cluster at all. That's the MCP server, and it comes first.

## 2. The AI SRE landscape, October 2026

Our two research passes ran on 18 October 2026. Where a figure comes from a
secondary source (an aggregator or a vendor blog about a competitor), I say so.

### Open source

**[k8sgpt](https://github.com/k8sgpt-ai/k8sgpt)** (CNCF Sandbox since December
2023, [cncf.io](https://cncf.io/projects/k8sgpt); Apache-2.0, about 8,250
stars, v0.4.40 on 9 October 2026). It scans a cluster with built-in
"analyzers" (Pod, Service, Ingress, Node, Events and about 30 more) and asks an
LLM to explain what they find. You run it as a CLI
(`k8sgpt analyze --explain`), as an MCP server (`k8sgpt serve --mcp`, over
stdio or HTTP), or as an [operator](https://github.com/k8sgpt-ai/k8sgpt-operator)
with CRDs. It needs a kubeconfig and supports a dozen LLM providers. Where it
struggles: it only knows what an analyzer encodes, and there's no generic CRD
analyzer ([#1528](https://github.com/k8sgpt-ai/k8sgpt/issues/1528)).
`--anonymize` doesn't mask log lines or event messages. In April 2026 the
operator's auto-remediation got a high-severity prompt-injection advisory: it
applied AI-generated YAML to a Deployment without checking it
([GHSA-rp7v-4384-hfrp](https://github.com/k8sgpt-ai/k8sgpt/security/advisories/GHSA-rp7v-4384-hfrp)).
k8sgpt's analyzer model is the closest thing out there to our `wtf`, and our
evidence model is stricter: k8sgpt has nothing like our `UNKNOWN`.

**[HolmesGPT](https://github.com/HolmesGPT/holmesgpt)** (CNCF Sandbox since
October 2025, [cncf.io](https://www.cncf.io/projects/holmesgpt/); Apache-2.0,
about 3,600 stars, 0.45.0 on 15 October 2026). Robusta started it and
Microsoft contributes heavily. It's an agent loop: it calls tools step by step
and refines a hypothesis as it goes. There are about 40 toolsets (Kubernetes,
Prometheus, Loki, Tempo, Datadog, the clouds, ArgoCD). You run it as a CLI, an
HTTP API (`/api/chat`) or a scheduled "operator mode" that posts to Slack and
opens PRs. It's mainly an MCP *client*, so MCP servers plug in as toolsets
([Robusta docs](https://docs.robusta.dev/master/configuration/holmesgpt/remote_mcp_servers.html)).
Where it struggles: it calls itself "read-only, RBAC-aware", but the `bash`
and remediation toolsets break that. A critical July 2026 advisory chained a
Host-header auth bypass with forged tool approval into unauthenticated RCE
([GHSA-6m4w-cmhp-f95f](https://github.com/HolmesGPT/holmesgpt/security/advisories/GHSA-6m4w-cmhp-f95f)).
That's the "approval" problem in one CVE. **This is the agent we should make
work out of the box**: a Reliaburger MCP server would plug straight in as a
toolset.

**[Robusta](https://github.com/robusta-dev/robusta)** (MIT, about 3,100
stars) now calls itself "Robusta Classic". It's a rule-based enricher for
Alertmanager webhooks with Python playbooks, and its AI moved to HolmesGPT.
Our alert webhooks (`src/mayo/webhook.rs`) already play the trigger role.

**[Keep](https://github.com/keephq/keep)** (MIT apart from an `ee/` directory,
about 12,400 stars, v0.54.3 on 9 September 2026) is open-source alert
management with AI correlation and YAML workflows. Elastic bought it
([Elastic blog](https://www.elastic.co/blog/elastic-and-keep-join-forces)),
and Business Wire reported the deal closed in May 2025
([Business Wire](https://www.businesswire.com/news/home/20250521686867/en/Elastic-Completes-Acquisition-of-Keep)).
Activity has slowed since. It's an alert layer, not an orchestrator-aware
agent.

**Kubernetes MCP servers.** These are the most direct comparison to what
we'd build:

- [containers/kubernetes-mcp-server](https://github.com/containers/kubernetes-mcp-server)
  (Red Hat, Go, Apache-2.0, about 2,160 stars) has the best safety model.
  Its TOML config offers `read_only = true` (only tools annotated
  `readOnlyHint`), `disable_destructive`, `[[denied_resources]]` (to hide
  Secrets, say), confirmation rules and OAuth/OIDC for HTTP
  ([configuration](https://github.com/containers/kubernetes-mcp-server/blob/main/docs/configuration.md)).
  One gotcha: create-or-update uses full-state server-side apply, so fields
  you leave out of a manifest are removed.
- [Flux159/mcp-server-kubernetes](https://github.com/Flux159/mcp-server-kubernetes)
  (TypeScript, wraps `kubectl`, MIT, about 1,600 stars).
  `ALLOW_ONLY_NON_DESTRUCTIVE_TOOLS` still allows apply, scale and patch. HTTP
  mode needs `ENABLE_UNSAFE_STREAMABLE_HTTP_TRANSPORT=1` and has no
  server-side auth, and secrets are masked in `get` but not in logs.
- [Azure/aks-mcp](https://github.com/Azure/aks-mcp) defaults to `readonly`,
  and says plainly that its access level "is **not** a security boundary".
  Anyone who can call the tools has the server identity's full rights. It has
  since dropped HTTP and SSE and is stdio only.
- [AWS EKS MCP server](https://github.com/awslabs/mcp/tree/main/src/eks-mcp-server)
  is read-only by default. `--allow-write` turns on writes, and logs, events
  and secrets need `--allow-sensitive-data-access`. A managed, hosted version
  with IAM auth and CloudTrail went into preview in November 2025
  ([AWS](https://aws.amazon.com/about-aws/whats-new/2025/11/amazon-eks-ecs-fully-managed-mcp-servers-preview/)).
- [GoogleCloudPlatform/gke-mcp](https://github.com/GoogleCloudPlatform/gke-mcp)
  is "not an officially supported Google product"; Google also runs managed
  remote MCP servers for GKE ([InfoQ](https://infoq.com/news/2025/12/google-cloud-managed-mcp/)).
  I couldn't find the flag that enables its destructive tools.

The pattern across all five: safety is a flag on a wrapper, sitting on top of
a credential that can do more. The authors say so themselves. Reliaburger can
do better, because the MCP server and the authorisation (`src/bun/authz.rs`)
are the same codebase.

**[kagent](https://github.com/kagent-dev/kagent)** (CNCF Sandbox since May
2025, [cncf.io](https://www.cncf.io/projects/kagent/); Apache-2.0, about 4,000
stars; v1.0.0-alpha14 on 16 October 2026) is a framework for *running* agents
in Kubernetes as CRDs, with MCP and A2A APIs. It's an agent host, not an
agent. More frameworks of this kind are waiting for CNCF Sandbox decisions
([CAIPE](https://github.com/cncf/sandbox/issues/528),
[agentregistry](https://github.com/cncf/sandbox/issues/477)).

**Grafana.** [mcp-grafana](https://github.com/grafana/mcp-grafana) (Apache-2.0,
about 3,600 stars, v2.0.2) covers PromQL, LogQL (including log patterns),
alerting, incidents and dashboards. It has per-category toggles and
`--disable-write`, uses service-account RBAC, and checks Host and Origin
headers against DNS rebinding. That last one matters for any HTTP MCP server we
ship. Prometheus has no official MCP server; the popular community one is
[pab1it0/prometheus-mcp-server](https://github.com/pab1it0/prometheus-mcp-server).

### Commercial

All of these are closed products. What they share matters more to us than any
one of them.

- **[incident.io](https://incident.io/ai-sre)**: "Investigations" correlates
  telemetry, deploys, code changes and past incidents, then gives a root cause
  with a confidence score and cites its sources. The only change it makes to
  your systems is a **pull request a human merges**. Code analysis runs in
  throwaway sandboxes, and data is redacted before it reaches model providers.
  A hosted remote MCP server (OAuth or API key) entered public beta on 31 March
  2026 ([changelog](https://incident.io/changelog/remote-mcp-server)). It
  raised $62M in April 2025
  ([Insight Partners](https://www.insightpartners.com/ideas/incident-io-raises-62m-to-build-ai-agents-that-resolve-incidents-with-you/)).
- **[Rootly](https://rootly.com/ai)**: ranks hypotheses in parallel and
  "never auto-remediates". Every rollback, flag flip or PR waits for a human.
  Its MCP server is open source (Apache-2.0), went GA in March 2026, and comes
  in a "slim" profile of about 70 tools because the full set overwhelms models
  ([changelog](https://rootly.com/changelog/rootly-mcp-server-is-ga),
  [PyPI](https://pypi.org/project/rootly-mcp-server/2.3.16/)).
- **Datadog Bits AI**: [Bits AI SRE](https://docs.datadoghq.com/bits_ai/bits_ai_sre/)
  investigates from monitor alerts.
  [Kubernetes Remediation](https://www.datadoghq.com/blog/bits-ai-kubernetes-remediation/)
  is GA and handles CrashLoopBackOff, OOMKilled and ImagePullBackOff. Its
  [Guardrails](https://docs.datadoghq.com/bits_ai/bits_remediation/) (Preview)
  give each action an Ask or Deny policy, approvals in Slack, and templates
  from Conservative to Permissive. Kubernetes actions need a Private Action
  Runner inside your network. The Datadog MCP server went GA on 9 March 2026
  ([press release](https://investors.datadoghq.com/news-releases/news-release-details/datadog-launches-mcp-server-provide-ai-agents-secure-real-time)).
  Third-party sources quote pricing per investigation (about $500 for 20 a
  month, [Better Stack](https://betterstack.com/community/comparisons/bits-ai-sre-alternatives/)),
  which I couldn't confirm on Datadog's own pages.
- **Grafana Cloud**: Assistant went GA in October 2025
  ([Grafana](https://grafana.com/whats-new/2025-10-08-grafana-assistant-is-now-generally-available/)),
  and Assistant Investigations followed in July 2026. It can start manually,
  from an alert or from an IRM webhook, and it only diagnoses
  ([Grafana](https://grafana.com/whats-new/2026-07-29-assistant-investigations-is-now-generally-available-in-grafana-cloud/)).
  Billing is by token, with investigation billing starting 1 October 2026
  ([pricing](https://grafana.com/docs/grafana-cloud/platform/pricing-and-usage/assistant/)).
- **Komodor (Klaudia)**: built for Kubernetes. Since March 2026 it's a
  multi-agent platform with 50+ specialist agents
  ([Help Net Security](https://www.helpnetsecurity.com/2026/03/19/komodor-klaudia-ai-extensibility-framework/)).
  It added memory, one-click actions and self-healing policies in July 2026
  ([Komodor](https://komodor.com/blog/klaudia-memory-launch/)), and it's named
  in Gartner's 2026 Market Guide for AI SRE Tooling
  ([Komodor](https://komodor.com/blog/komodor-named-a-representative-vendor-in-the-2026-gartner-market-guide-for-ai-site-reliability-engineering-tooling)).
  A third-party review flags the cluster-wide permissions of its in-cluster
  agent ([Adversa](https://airq.adversa.ai/agent/komodor); unverified).
- **[Cleric](https://cleric.ai)**: read-only by default, with writes enabled
  later, fixes as PRs, and a 14-day watch on each change after it deploys. It
  has raised $9.8M in total
  ([Business Wire](https://businesswire.com/news/home/20251209625361/en/Cleric-Launches-the-First-Self-Learning-AI-SRE)).
- **[Resolve.ai](https://resolve.ai/product/overview)**: $125M Series A at a
  $1B valuation ([TechCrunch](https://techcrunch.com/2026/02/04/ai-sre-resolve-ai-confirms-125m-raise-unicorn-valuation/)).
  It builds a knowledge graph of services, dependencies and owners. Its
  Kubernetes "Satellite" is strictly read-only (get, list and watch), scoped
  to the resource types and namespaces you allow, and redacts data before it
  leaves the cluster
  ([Resolve](https://resolve.ai/integrations/kubernetes)).
- **PagerDuty**: its [SRE Agent](https://support.pagerduty.com/main/docs/sre-agent)
  reads alert payloads, runbooks, logs, change events and past incidents, and
  mostly *reads*. It analyses only the first 2,000 characters of each custom
  field, and each request costs 4 "AI Actions". The hosted MCP server exposes
  72 tools, 50 read and 22 write ([Scalekit](https://www.scalekit.com/blog/pagerduty-mcp-vs-api),
  secondary).
- **The clouds**: [AWS DevOps Agent](https://infoq.com/news/2026/04/aws-devops-agent-ga)
  went GA around April 2026 and reaches EKS with the read-only
  `AmazonEKSViewPolicy` by default
  ([AWS](https://aws.amazon.com/blogs/architecture/ai-powered-event-response-for-amazon-eks)).
  [Azure SRE Agent](https://learn.microsoft.com/azure/sre-agent/run-modes)
  defaults to Review mode, where writes need an administrator's approval.
  [Gemini Cloud Assist investigations](https://docs.cloud.google.com/cloud-assist/investigations)
  can start from GKE alerts, but since April 2026 creating one needs Premium
  Support ([release notes](https://docs.cloud.google.cn/gemini/docs/release-notes)).
- **Others, in brief**: [Traversal](https://www.goodwinlaw.com/en/news-and-events/news/2025/06/announcements-technology-goodwin-represents-traversal)
  ($48M, out of stealth June 2025);
  [Dynatrace](https://www.dynatrace.com/news/press-release/dynatrace-introduces-domain-specific-agents/)
  (domain agents on deterministic AI plus MCP, January 2026);
  [New Relic SRE Agent](https://docs.newrelic.com/docs/agentic-ai/sre-agent/overview/)
  (preview);
  [Causely](https://www.businesswire.com/news/home/20251106134583/en/Causely-Introduces-MCP-Server-for-Automated-Remediation-Across-Kubernetes-and-Application-Code)
  (causal model, MCP server that writes Helm and Terraform patches);
  [NeuBird](https://www.maginative.com/article/neubird-raises-22-5m-to-scale-ai-powered-it-operations-assistant/).

### Standards and research

- **MCP.** The current spec is
  [2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28/changelog),
  and it makes MCP **stateless**. There's no `initialize` handshake any more
  (version and capabilities travel in `_meta`, and `server/discover`
  advertises them), no `Mcp-Session-Id`, and SSE resumability is gone.
  Elicitation now works through multi-round-trip `input_required` results
  ([tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)).
  Over HTTP, auth is OAuth 2.1: Protected Resource Metadata, audience-bound
  tokens and no token passthrough
  ([authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)).
  Tool annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`,
  `openWorldHint`) are hints that clients must treat as **untrusted**. The spec
  says a human SHOULD be able to deny a tool call, and servers MUST validate
  inputs, enforce access control, rate-limit and sanitise outputs
  ([tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)).
  The official Rust SDK, [`rmcp`](https://crates.io/crates/rmcp) 3.5.1 (5
  October 2026, Apache-2.0, built on tokio), implements 2026-07-28
  ([rust-sdk](https://github.com/modelcontextprotocol/rust-sdk)). It fits our
  stack without adding a new async runtime. A stateless protocol suits us
  too: no per-session state to keep on a node that might restart.
- **OpenTelemetry GenAI semantic conventions** have moved to their own
  repository,
  [semantic-conventions-genai](https://github.com/open-telemetry/semantic-conventions-genai),
  and **everything is still "Development" status**. That includes
  [agent spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-agent-spans.md)
  (`invoke_agent`, `gen_ai.agent.*`) and
  [MCP spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/mcp.md)
  (`mcp.method.name`, `gen_ai.tool.name`). The MCP examples still show the
  `initialize` handshake the latest spec removed, which suggests they lag
  behind it. Worth tracking; not worth building to yet.
- **Benchmarks.**
  [ITBench](https://github.com/itbench-hub/ITBench) (IBM) runs incident
  scenarios on Kubernetes. On its Artificial Analysis edition (59 tasks),
  frontier models score below 50%
  ([Artificial Analysis](https://artificialanalysis.ai/articles/itbench-aa-launch)).
  [AIOpsLab](https://arxiv.org/abs/2501.06706v1) (Microsoft) injects faults
  into live microservices and scores detection, localisation, RCA and
  mitigation. [SREGym](https://arxiv.org/abs/2605.07161) (NeurIPS 2026) is a
  live benchmark with 90 problems, including metastable and correlated
  failures. [OpenRCA](https://proceedings.iclr.cc/paper_files/paper/2025/file/d29b8d53678015079e1d245c023e49d2-Paper-Conference.pdf)
  (ICLR 2025) works on static data, and the best model solved 11.34%.
  [RCAEval](https://arxiv.org/abs/2412.17015v1) has 735 cases. The live ones
  all rebuild what Reliaburger already has: a cluster, a fault injector, and
  ground truth about what was injected.
- **Prompt injection through telemetry is demonstrated, not theoretical.**
  [LogJack](https://arxiv.org/abs/2604.15368) (April 2026) planted
  instructions in cloud logs. Depending on the model, debugging agents ran
  them anywhere from 0% to 86.2% of the time; a `curl|bash` payload worked on
  6 of 8 models; and the cloud guardrail products caught almost nothing. The
  attacker needs no cloud permissions at all, just the ability to get text into
  an error message. STAR Labs reported that poisoned OpenTelemetry data led an
  SRE agent to deploy a privileged container
  ([Straiker](https://www.straiker.ai/blog/ai-sre-agent-prompt-injection-kubernetes-ransomware);
  a vendor claim I couldn't verify). Researchers have also shown "tool
  poisoning", where any MCP output, not just a tool's description, can carry
  instructions
  ([CyberArk](https://www.cyberark.com/resources/agentic-ai-security/poison-everywhere-no-output-from-your-mcp-server-is-safe)).
  OWASP's [Top 10 for Agentic Applications 2026](https://genai.owasp.org/resource/owasp-top-10-for-agentic-applications-for-2026)
  turns this into guidance.
- **[llms.txt](https://llmstxt.org/)** is still just a proposal. Search
  crawlers mostly ignore it
  ([Optimizely](https://www.optimizely.com/field-notes/articles/does-llms-txt-work/),
  second-hand Ahrefs figures), but agent tooling reads it. MCP's own docs point
  agents at [modelcontextprotocol.io/llms.txt](https://modelcontextprotocol.io/llms.txt).
  It's cheap to publish, and it's for coding and ops agents, not search
  engines.

### What the landscape tells us

1. **Read-only first, everywhere.** Resolve, AWS, Cleric, Azure and every
   serious MCP server start read-only. The products that went further
   (HolmesGPT's bash, the k8sgpt operator) produced the advisories above.
2. **Mutations go through a human, and preferably through a PR.** incident.io,
   Cleric and Datadog's "Fix with Bits" fix the *source* (Git), not the live
   cluster. Rootly, Datadog Guardrails and Azure Review mode put an approval
   step in front of live actions.
3. **Change correlation is the most valuable input.** Every product asks for
   deploys, commits and flag changes. We hold these natively: the audit log,
   deploy history, GitOps.
4. **MCP is now the standard integration, and tool bloat is its known
   problem.** Rootly's "slim" profile and Datadog's toolsets exist because 70+
   tools confuse models.
5. **Kubernetes is hard for these products in three ways:** the data is spread
   across five systems; write access means extra in-cluster runners with broad
   RBAC; and the "read-only" flags sit on credentials that can do more, as
   aks-mcp admits. Reliaburger has none of these three problems.
6. **Nobody can evaluate an AI SRE honestly on their own cluster.** The
   benchmarks are academic rigs. A product that ships with its own fault
   injector and ground truth would be the first.

## 3. Ideas for Reliaburger, ranked

Ranked by how much they make Reliaburger the easy, safe choice for an agent,
divided by effort. Effort: S is days, M is one or two weeks, L is a milestone
of its own.

First, what we already have. This is unusual, and it should be the pitch:

- One API, one token model and one audit log. 132 routes in `src/bun/api.rs`;
  roles plus `[permission]` blocks (`src/bun/authz.rs`); the durable audit log
  (`src/bun/audit.rs`), which already records refusals (`api.refused`).
- `relish wtf` (`src/relish/wtf/`), whose `WtfReport` is a **versioned** JSON
  contract (`schema_version`). Its findings carry an `id`, `suggestion`,
  `correlated_events` and `affected_resource`, and the `Evidence` enum makes a
  missing source `Unavailable` or `Unsupported`, never green. That's exactly
  the honesty an LLM lacks.
- `--output json` on every `relish` command, and exit codes that mean
  something (`wtf`: 0, 1, 2).
- Smoker (`src/smoker/`) with safety rails that can't be overridden
  (`safety.rs`: quorum risk, replica minimum), scripted scenarios with
  `--dry-run`, and `relish test --chaos`, which proves its own cleanup or
  reports `Unknown`.
- `relish apply --dry-run` with a plan (`src/relish/plan.rs`), and
  `relish upgrade plan`.
- Signed alert webhooks (`src/mayo/webhook.rs`) to wake an agent up.
- The manual compiled into the binary (`src/relish/manual/assets.rs`).
- The V02 soak harness (`scripts/release/qualify-sustained.sh`,
  `sustained_check.py`), which already turns faults into machine-checked
  findings.

### 1. `relish mcp`: a read-only MCP server (effort S–M)

**What.** `relish mcp` speaks MCP over stdio. It uses the current `relish`
context (endpoint, token, CA), so `claude mcp add reliaburger -- relish mcp` or
the equivalent in any client just works. A small, curated set of tools, all
annotated `readOnlyHint: true`:

| Tool | Backed by |
|---|---|
| `cluster_status` | `/v1/status`, council status (`src/relish/council_view.rs`) |
| `diagnose` | `wtf`'s collector and diagnoser, returning `WtfReport` as is (optional `app`) |
| `inspect_app` | `src/relish/inspect.rs` |
| `app_logs` | `/v1/logs/query/{app}/{ns}`, **always bounded**: `tail`, `since`, `grep` |
| `app_metrics` | `/v1/metrics/app/{app}/{ns}`, `relish top` |
| `recent_events` | `/v1/events` |
| `deploy_history` | `/v1/deploys/history/{app}` |
| `network_path` | `/v1/path` (`relish path`) |
| `active_faults` | `relish fault list` |
| `audit_log` | `/v1/audit` (admin tokens only; omitted otherwise) |
| `search_manual` | the embedded manual, as MCP resources plus a search tool |

**Why it ranks first.** It's the integration every agent, HolmesGPT and Claude
Code included, can consume today, and it reuses code we already ship. `wtf`
runs client-side in `relish`, so putting the MCP server in `relish` (not `bun`)
means no evidence collection has to move. Keep the tool count under 15:
Rootly's slim profile tells us why.

**Fit.** A new `src/relish/mcp/` module on [`rmcp`](https://crates.io/crates/rmcp)
(tokio, Apache-2.0). Each tool is a thin wrapper over existing `BunClient`
calls in `src/relish/client.rs`. A new subcommand in `src/bin/relish.rs`.
Tests: snapshot each tool's JSON schema with insta, and a portable integration
test driving stdio against the test agent.

**Risks.** The server can't give an agent more than its token allows, because
`bun` enforces authorisation and the MCP layer is just a client. That's the
structural advantage over aks-mcp's admitted "not a security boundary". The
real risk is **prompt injection through log lines and event messages**: see
idea 3, which ships with this one. Data exposure: logs can hold PII and
secrets. `redact_secrets` in `src/bun/audit.rs` already scrubs tokens; extend
it to the MCP output path.

### 2. Agent tokens: scoped, short-lived and labelled (effort S)

**What.** `relish token create --name holmes --agent --ttl 8h`. In practice this
gives:

- **read-only by default**, with an explicit `--role` needed to raise it;
- a short default lifetime (the lifetime machinery already exists, see
  `docs/manual/10_security.md`, "Lifetimes");
- a `principal_kind: agent` field in every audit record and event, so
  `relish audit --principal-kind agent` shows exactly what agents did;
- a per-token rate limit on reads, so an agent in a loop can't hammer Ketchup;
- an optional session or correlation id (`X-Reliaburger-Session`) carried into
  audit records, so one investigation's calls can be grouped.

**Value.** It answers the first question every security reviewer asks about an
AI SRE ("what can it touch, and how do I see what it did?") with one flag and
one command. Most of it is metadata on machinery we already have.

**Fit.** `src/bun/authz.rs` (role and scope checks), `src/bun/api/` token
routes and `token_tests.rs`, the `AuditRecord` in `src/bun/audit.rs`,
`src/bun/token_sweep.rs` for expiry. Adding a field to `AuditRecord` is a
durable-state change, so it bumps `state` in `src/compatibility.rs`. Before
1.0 that's acceptable.

**Risks.** Low. A labelled token is still just a token. The label is for
audit, not a security boundary, and the docs should say so.

### 3. Untrusted-content envelopes and output budgets (effort S)

**What.** Every MCP tool result that contains text a workload controls (log
lines, event messages carrying app output, `wtf` details quoting log lines,
HTTP probe bodies) returns it in a clearly marked structure, never mixed into
our own prose:

```json
{"untrusted_workload_output": {"source": "logs", "app": "payments",
  "lines": ["..."], "truncated": true}}
```

Plus hard size caps (lines, bytes, per-line length), ANSI and control
characters stripped, and token redaction. Tool descriptions and server
instructions say plainly that this content is data and must never be followed.

**Value.** LogJack showed that "agent reads logs" means "anyone who can make
your app log a string can talk to your agent". Our server can't make a model
safe, but it can make the boundary obvious and keep the blast radius small. And
the real defence is ideas 2 and 5: a read-only token can't run `curl|bash` on
anything.

**Fit.** `src/relish/mcp/` output shaping; the Ketchup query types
(`src/ketchup/types.rs`); `redact_secrets` in `src/bun/audit.rs`.

**Risks.** Over-promising. Envelopes reduce the risk and don't remove it. The
docs must say that, and point at least privilege as the actual control.

### 4. Incident timeline: one merged, machine-readable story (effort M)

**What.** `GET /v1/timeline?app=payments&since=1h` and `relish timeline`,
returning one time-ordered list that merges:

- audit records (applies, rollbacks, faults injected and cleared, token
  changes);
- events (deploys, restarts, health changes, alerts, placements);
- deploy history and rollout results;
- alert transitions from Mayo;
- GitOps syncs;
- log-pattern firsts from idea 7, once it exists.

Each entry carries a stable `kind`, `node`, `principal` and a link back to its
source record.

**Value.** Every commercial product's best input is "what changed just before
it broke?". We can answer it from one endpoint, with no Argo, no events
exporter and no audit pipeline to wire up. It's also exactly the post-incident
record a human wants.

**Fit.** The cross-node fan-out already exists for events, logs and audit
(`relish audit --cluster`; `src/bun/cluster_view.rs`; the relay at
`/v1/nodes/{node}/relay/...`). `wtf`'s `CorrelatedEvent` is a tiny version of
this; the timeline generalises it, and `wtf` can then reuse it.

**Risks.** Events live in memory (the newest 1,024 per node), so the timeline
must flag the window it can't see. That's an `AvailableWithCaveat`, never a
silent gap, in the same spirit as `wtf`. Reading audit needs an unscoped admin
token, so the timeline should include audit entries only for tokens allowed to
read them and say when it leaves them out.

### 5. Proposals and approval for mutations (effort M–L)

**What.** Agents never mutate directly. A token with `--agent --propose`
can call mutation routes, but each call becomes a **proposal** instead of an
action:

```sh
relish proposals                     # pending, with the agent's stated reason
relish proposals show 17             # the exact plan: apply diff, rollback target, fault ids
relish proposals approve 17          # runs as the approving human, audited as both
relish proposals reject 17 --reason "..."
```

Proposals carry the `relish apply --dry-run` plan (`src/relish/plan.rs`) or
the equivalent for rollback, stop and fault clear, and they expire. Approval
is a separate call by a different principal (no self-approval), and the audit
record names both. Webhooks (idea 9) and the dashboard can surface pending
proposals.

Start with a fixed catalogue of **runbook-safe actions**, chosen because
they're reversible and bounded:

- `rollback <app>` to the previous good version;
- `clear fault <id>`;
- `restart instance <id>`;
- `scale <app>` within the replica bounds the app declares;
- `cancel-deploy <operation>`.

Each one runs the same pre-checks Smoker uses (`src/smoker/safety.rs`: quorum
risk, replica minimum), so an approved action still can't take out the
council. No `apply` of arbitrary manifests and no `exec` in the first version.
For GitOps clusters, the right write path is a PR to the repository, as
incident.io and Cleric do. Note that, and don't build it here.

**Value.** It's the feature that turns "it can explain" into "it can help at
3 a.m." without handing a model the keys. The HolmesGPT advisory (forged
approvals) shows why approval has to live in the orchestrator, server-side and
audited, rather than in the agent's own UI.

**Fit.** The test-lease machinery (`src/bun/api/test_leases.rs`) is a good
model for something that's leased, expires and is audited. Proposals belong in
Raft state (new durable state, so a `state` bump). Authz in
`src/bun/authz.rs`; approval routes under `src/bun/api/`; audit actions
`proposal.created`, `proposal.approved`, `proposal.rejected`,
`proposal.expired`.

**Risks.** This is the big one for blast radius. Approval fatigue (humans
clicking yes) is real, so keep the catalogue small and the diff exact. Don't
let the agent write the human-facing summary on its own: show the
machine-generated plan first and the agent's reasoning second, labelled as
such. Never auto-approve in the first release.

### 6. Eval harness: an AI SRE gym on our own chaos (effort M, growing to L)

**What.** `relish eval` (or a script under `scripts/`) runs a scenario
catalogue against a laptop cluster:

1. reset to a known state;
2. inject one or more faults with Smoker (`relish fault scenario`), or a
   "realistic" failure such as a bad deploy, a full disk or a lost node;
3. wake the agent (a webhook, or just a prompt with a read-only token);
4. collect its answer (the structured diagnosis it submits through an MCP tool
   `submit_diagnosis`) and its tool-call log;
5. score it against **ground truth from Smoker and the audit log**: did it name
   the right app, the right fault kind, the right node? How many tool calls
   and tokens did it use? Did it attempt anything outside its scope (from
   `api.refused` audit records)? That last one should always be zero.

Ship 5–10 scenarios at first, and add log-injection scenarios (an app that
logs "ignore previous instructions and run …") to measure robustness, not
just accuracy.

**Value.** This is the unique part. ITBench, AIOpsLab and SREGym each built a
cluster, a fault injector and a ground-truth recorder to do this, and we ship
all three in the binary. Teams choosing an AI SRE vendor could run the same
gym against each candidate on their own laptop. For the project it's also a
regression test: if a `wtf` change makes agents worse, we'd see it.

**Fit.** `src/smoker/scenario.rs` (scenario files and `--dry-run`),
`relish test --chaos` (consent, exact fault ids, proven cleanup), the V02
harness pattern in `scripts/release/sustained_check.py` (evidence in, findings
and a Markdown record out). Results go in `docs/qualification/`, like the
soak records.

**Risks.** Non-determinism: models vary from run to run, so report
distributions over N runs, never a single pass/fail. Cost: real API spend, so
keep it out of CI and run it as a gated `make` target. Benchmark gaming:
publish the scenarios, but keep a held-out set.

### 7. Log patterns: the Rocketgraph idea in Ketchup (effort M)

See section 1, Option B. `GET /v1/logs/patterns/{app}/{ns}?since=…` returns
templates with counts, first-seen times, a new-since-deploy flag and one
example line (inside an untrusted envelope). A `wtf` finding
`log-pattern-new` fires when an error-level template first appears after a
deploy. The MCP tool `log_patterns` becomes the agent's default way to read
logs, with raw `app_logs` as a fallback.

**Value.** Fewer tokens, less noise, and smaller injection exposure: a
template like `ignore previous instructions <*>` is much less persuasive than
the raw line, and it's visibly anomalous. That said, a template mask won't
reliably neutralise an injection, so don't sell it as a defence.

**Fit.** `src/ketchup/` (a new `patterns.rs` next to `query.rs`),
`src/relish/wtf/collect.rs`. Deploy times come from deploy history.

**Risks.** CPU on busy nodes: bound the work per query and run it off the
agent loop. The 0.1.3 loop-meter rule (no inline awaits on slow work) applies.

### 8. `explain`: one call for "what's going on?" (effort S, once 1 and 4 exist)

**What.** `relish explain` and an MCP tool that returns, within a set token
budget: the `wtf` summary and criticals, the last N timeline entries, active
faults and alerts, and a one-line state for each app. It's the first call an
agent should make, the way a human runs `relish status` first.

**Value.** Agents waste turns finding their footing. One well-shaped snapshot
cuts the tool calls per investigation.

**Fit.** It composes ideas 1 and 4. No new server work.

**Risks.** It's tempting to make it smart. Keep it deterministic: no LLM
inside Reliaburger.

### 9. Agent-ready webhooks (effort S)

**What.** An `incident` webhook payload: when `wtf`-class conditions or Mayo
critical alerts fire, POST a signed (HMAC, as today) JSON message carrying the
finding ids, the affected resource and the exact `relish`/MCP call that
reproduces the evidence. Grafana starts Investigations from IRM webhooks;
HolmesGPT and Robusta start from Alertmanager. This makes us a first-class
trigger for both.

**Fit.** `src/mayo/webhook.rs`, `src/mayo/alert.rs`, `[[alerts.destinations]]`.
An Alertmanager-compatible payload shape would let HolmesGPT consume it without
any glue.

**Risks.** The payload must not carry log text (injection), only ids and
pointers. The agent then fetches through its own scoped token.

### 10. `llms.txt` and docs for agents (effort S)

**What.** Generate `llms.txt` and `llms-full.txt` from `docs/manual/` at
build time, and publish them at `reliaburger.com/llms.txt`. Also serve them
from `bun` at `/llms.txt` (unauthenticated: the manual is public anyway) and
via `relish manual --format markdown`. Add a manual chapter, "Plugging in an
AI SRE", covering the token recipe, the MCP setup for Claude Code and
HolmesGPT, and the safety model.

**Value.** It's cheap, and coding agents do read these files. The manual's
style (exact commands, exit codes, honest limits) already suits agents.

**Fit.** `src/relish/manual/assets.rs` already embeds the chapters;
`docs/website/`; `make readme-commands` shows the generate-from-source pattern.

**Risks.** None to speak of. Keep it generated so it can't drift.

### 11. OpenTelemetry export (effort M–L)

**What.** In three parts, in order of value:

1. OTLP export of Mayo metrics and Ketchup logs, so teams with an existing
   backend (and its AI features: Datadog Bits, Grafana Assistant) can see
   Reliaburger;
2. agent actions in the audit log mapped to GenAI and MCP span attributes
   (`gen_ai.agent.name`, `mcp.method.name`), so an agent's investigation shows
   up as a trace;
3. accepting OTLP traces from apps. The whitepaper deliberately leaves tracing
   backends out, and that's still right.

**Value.** Real, but it's interoperability with *other* AI SREs, not a
reason to pick Reliaburger. The GenAI semantic conventions are still in
Development status, so building to them now means churn.

**Fit.** `src/mayo/` and `src/ketchup/export.rs`, alongside the existing remote
archives (`docs/manual/04_observability.md`, "Remote metrics archives").
Roadmap item F06 (the full metrics architecture,
[#364](https://github.com/reliaburger/reliaburger/issues/364)) touches the
same code.

**Risks.** Scope creep, and a new outbound network surface: it needs workload
identity or credentials for the collector.

### 12. A remote MCP endpoint in `bun` (effort M, later)

**What.** Streamable-HTTP MCP at `/v1/mcp` on every node, following the
stateless 2026-07-28 spec, authenticated with our existing bearer tokens. Full
OAuth 2.1 with Protected Resource Metadata comes later, if hosted agents
(incident.io, Rootly, Datadog) need to reach in.

**Why later.** stdio through `relish` covers local agents and anything running
next to an operator. A network endpoint needs Host and Origin validation
(mcp-grafana's DNS-rebinding defence), a decision about `wtf` running
server-side, and an OAuth story. aks-mcp *removed* its HTTP transport, which
says something about the cost. Do it when a real user asks for it.

### Summary table

| # | Idea | Effort | Main risk | Part of 0.4.2 |
|---|------|--------|-----------|----------|
| 1 | `relish mcp`, read-only | S–M | log injection (see 3) | 1 |
| 2 | Agent tokens | S | none new; a durable-state bump | 1 |
| 3 | Untrusted envelopes, budgets | S | over-promising | 1 |
| 4 | Incident timeline | M | bounded event memory | 1 |
| 6 | Eval harness, 5 scenarios | M | non-determinism, cost | 1 |
| 10 | `llms.txt`, agent chapter | S | none | 1 |
| 8 | `explain` | S | making it "smart" | 1 |
| 9 | Agent-ready webhooks | S | payload leaking log text | 1 |
| 5 | Proposals and safe actions | M–L | approval fatigue, blast radius | 2 |
| 7 | Log patterns | M | CPU on busy nodes | 2 |
| 12 | Remote MCP in `bun` | M | HTTP auth surface | last |
| 11 | OTel export | M–L | scope, churn in the GenAI semantic conventions | last |

## 4. The 0.4.2 milestone

The roadmap's rule is one headline feature per minor release, with one exit
test. The current queue is 0.2.1 (cluster safety), 0.2.2 (UX), 0.3.0 (bare
metal), 0.4.x (container migration) and 0.5.0 (live moves)
([docs/roadmap.md](https://github.com/reliaburger/reliaburger/blob/main/docs/roadmap.md)).
The AI work doesn't depend on any of those. It's mostly CLI and read paths,
and doesn't touch the risky data plane.

**Decision (18 October 2026).** The maintainer put all of it in one minor
release, [0.4.2](https://github.com/reliaburger/reliaburger/milestone/18), after
the container-migration releases. It ships in two parts inside that milestone:
part 1 (read-only) first, then part 2 (approved mutations). Ideas 11 and 12 come
last, and are the first to move out if the milestone runs long.

| # | Idea | Issue | Part |
|---|------|-------|------|
| 1 | `relish mcp`, read-only | [#1094](https://github.com/reliaburger/reliaburger/issues/1094) | 1 |
| 2 | Agent tokens | [#1082](https://github.com/reliaburger/reliaburger/issues/1082) | 1 |
| 3 | Untrusted envelopes, budgets | [#1083](https://github.com/reliaburger/reliaburger/issues/1083) | 1 |
| 4 | Incident timeline | [#1084](https://github.com/reliaburger/reliaburger/issues/1084) | 1 |
| 5 | Proposals and safe actions | [#1085](https://github.com/reliaburger/reliaburger/issues/1085) | 2 |
| 6 | Eval harness | [#1086](https://github.com/reliaburger/reliaburger/issues/1086) | 1, grown in 2 |
| 7 | Log patterns | [#1087](https://github.com/reliaburger/reliaburger/issues/1087) | 2 |
| 8 | `explain` | [#1088](https://github.com/reliaburger/reliaburger/issues/1088) | 1 |
| 9 | Agent-ready webhooks | [#1089](https://github.com/reliaburger/reliaburger/issues/1089) | 1 |
| 10 | `llms.txt`, agent chapter | [#1090](https://github.com/reliaburger/reliaburger/issues/1090) | 1 |
| 11 | OTel export | [#1091](https://github.com/reliaburger/reliaburger/issues/1091) | last |
| 12 | Remote MCP in `bun` | [#1092](https://github.com/reliaburger/reliaburger/issues/1092) | last |
| — | Exit tests and their record | [#1093](https://github.com/reliaburger/reliaburger/issues/1093) | 1 and 2 |

The headings below were written as two releases; they're now the two parts of
0.4.2, with the same exit tests.

### Part 1: "Plug in an SRE" (read-only)

Ideas 1, 2, 3, 4, 8, 10, 9 and the first cut of 6.

- `relish mcp` (stdio, about 12 read-only tools, `rmcp`);
- agent tokens: `--agent`, short TTL, `principal_kind` in audit and events,
  read rate limit, session id;
- untrusted-content envelopes and output budgets on every text-bearing tool;
- `/v1/timeline`, `relish timeline` and `relish explain`;
- an agent-ready webhook payload;
- `llms.txt`, plus a manual chapter "Plugging in an AI SRE" (with setup for
  Claude Code and HolmesGPT), and the matching section in the book chapter
  that owns diagnostics (chapter 11, "Eyes Everywhere", or wherever `wtf`
  lives), explaining MCP, `rmcp` and the safety model;
- `relish eval` (or `scripts/eval/`) with five scenarios, including one log
  injection. The first results go in `docs/qualification/`.

**Exit test.** On a fresh three-node laptop cluster, an off-the-shelf agent
(Claude Code or HolmesGPT) with only `relish mcp` and an `--agent` read-only
token diagnoses each of the five Smoker scenarios: it names the right app,
fault kind and node in at least 4 of 5 over 5 runs each. Across every run, the
audit log shows zero mutating calls and zero `api.refused` from that token.
Running the injection scenario never leads to a mutating attempt.

### Part 2: "Let it help" (approved mutations)

Ideas 5 and 7, and a larger eval.

- proposals with approve and reject, the five runbook-safe actions, and Smoker
  safety pre-checks on each;
- log patterns in Ketchup, plus the `log-pattern-new` finding in `wtf`;
- the eval harness grown to 15–20 scenarios, adding mitigation scoring (did
  the approved action restore service, measured by `wtf` going back to 0?), and
  a held-out set.

**Exit test.** Over 20 scenario runs, every mutation an agent proposes goes
through a proposal. None executes without a human approval, which the audit
log records. Approved proposals restore a healthy `wtf` in at least the
scenarios where the right action is in the catalogue.

### After 0.4.2

The streamable-HTTP MCP endpoint and OTLP export started on this list; they're
now the last two issues in 0.4.2 (#1092 and #1091). What's still for later:

- a GitOps PR write path (propose a change to the repository instead of the
  live cluster);
- publishing the eval as a standalone "AI SRE gym" that others can run
  against vendors;
- agent memory: past incidents as searchable timelines. Until then the
  timeline plus the audit log already give an agent most of the value.

### What not to do

- **Don't put an LLM inside `bun`.** Reliaburger should be the best thing for
  an agent to *operate*, not an agent itself. Keep everything we ship
  deterministic and testable, in line with the project's quality standards.
  Model choice, cost and data residency stay the user's problem and the user's
  choice.
- **Don't build a Kubernetes-style "read-only flag" on top of a powerful
  credential.** Our authorisation is the boundary. The MCP layer adds nothing
  to it and takes nothing away.
- **Don't take a BSL dependency** (see section 1).

## Sources

Rocketgraph and log mining

- https://github.com/Rocketgraph/rocketgraph
- https://github.com/Rocketgraph/rocketgraph/blob/main/LICENSE.txt
- https://github.com/Rocketgraph/rocketgraph/blob/main/ml/README.md
- https://github.com/Rocketgraph/rocketgraph/tree/main/packages/otel-node
- https://github.com/Rocketgraph/rocketgraph/releases/tag/v0.1.0
- https://github.com/Rocketgraph/rocketgraph/commits/main
- https://rocketgraph.app
- https://dev.to/kaushik94/open-source-alternative-to-firebase-rocketgraph-4b0i
- https://mariadb.com/bsl11/
- https://logparser.readthedocs.io/en/latest/tools/Drain.html
- https://github.com/logpai/Drain3
- https://crates.io/crates/drain3
- https://crates.io/crates/logdrain
- https://crates.io/crates/extended-isolation-forest

Open source

- https://github.com/k8sgpt-ai/k8sgpt
- https://github.com/k8sgpt-ai/k8sgpt-operator
- https://cncf.io/projects/k8sgpt
- https://github.com/k8sgpt-ai/k8sgpt/issues/1528
- https://github.com/k8sgpt-ai/k8sgpt/security/advisories/GHSA-rp7v-4384-hfrp
- https://github.com/HolmesGPT/holmesgpt
- https://www.cncf.io/projects/holmesgpt/
- https://docs.robusta.dev/master/configuration/holmesgpt/remote_mcp_servers.html
- https://github.com/HolmesGPT/holmesgpt/security/advisories/GHSA-6m4w-cmhp-f95f
- https://github.com/robusta-dev/robusta
- https://github.com/keephq/keep
- https://www.elastic.co/blog/elastic-and-keep-join-forces
- https://www.businesswire.com/news/home/20250521686867/en/Elastic-Completes-Acquisition-of-Keep
- https://github.com/containers/kubernetes-mcp-server
- https://github.com/containers/kubernetes-mcp-server/blob/main/docs/configuration.md
- https://github.com/Flux159/mcp-server-kubernetes
- https://github.com/Azure/aks-mcp
- https://github.com/awslabs/mcp/tree/main/src/eks-mcp-server
- https://aws.amazon.com/about-aws/whats-new/2025/11/amazon-eks-ecs-fully-managed-mcp-servers-preview/
- https://github.com/GoogleCloudPlatform/gke-mcp
- https://infoq.com/news/2025/12/google-cloud-managed-mcp/
- https://github.com/kagent-dev/kagent
- https://www.cncf.io/projects/kagent/
- https://github.com/cncf/sandbox/issues/528
- https://github.com/cncf/sandbox/issues/477
- https://github.com/grafana/mcp-grafana
- https://github.com/pab1it0/prometheus-mcp-server

Commercial

- https://incident.io/ai-sre
- https://incident.io/changelog/remote-mcp-server
- https://www.insightpartners.com/ideas/incident-io-raises-62m-to-build-ai-agents-that-resolve-incidents-with-you/
- https://rootly.com/ai
- https://rootly.com/changelog/rootly-mcp-server-is-ga
- https://pypi.org/project/rootly-mcp-server/2.3.16/
- https://docs.datadoghq.com/bits_ai/bits_ai_sre/
- https://www.datadoghq.com/blog/bits-ai-kubernetes-remediation/
- https://docs.datadoghq.com/bits_ai/bits_remediation/
- https://investors.datadoghq.com/news-releases/news-release-details/datadog-launches-mcp-server-provide-ai-agents-secure-real-time
- https://betterstack.com/community/comparisons/bits-ai-sre-alternatives/
- https://grafana.com/whats-new/2025-10-08-grafana-assistant-is-now-generally-available/
- https://grafana.com/whats-new/2026-07-29-assistant-investigations-is-now-generally-available-in-grafana-cloud/
- https://grafana.com/docs/grafana-cloud/platform/pricing-and-usage/assistant/
- https://www.helpnetsecurity.com/2026/03/19/komodor-klaudia-ai-extensibility-framework/
- https://komodor.com/blog/klaudia-memory-launch/
- https://komodor.com/blog/komodor-named-a-representative-vendor-in-the-2026-gartner-market-guide-for-ai-site-reliability-engineering-tooling
- https://airq.adversa.ai/agent/komodor
- https://cleric.ai
- https://businesswire.com/news/home/20251209625361/en/Cleric-Launches-the-First-Self-Learning-AI-SRE
- https://techcrunch.com/2026/02/04/ai-sre-resolve-ai-confirms-125m-raise-unicorn-valuation/
- https://resolve.ai/product/overview
- https://resolve.ai/integrations/kubernetes
- https://support.pagerduty.com/main/docs/sre-agent
- https://www.scalekit.com/blog/pagerduty-mcp-vs-api
- https://infoq.com/news/2026/04/aws-devops-agent-ga
- https://aws.amazon.com/blogs/architecture/ai-powered-event-response-for-amazon-eks
- https://learn.microsoft.com/azure/sre-agent/run-modes
- https://docs.cloud.google.com/cloud-assist/investigations
- https://docs.cloud.google.cn/gemini/docs/release-notes
- https://www.goodwinlaw.com/en/news-and-events/news/2025/06/announcements-technology-goodwin-represents-traversal
- https://www.dynatrace.com/news/press-release/dynatrace-introduces-domain-specific-agents/
- https://docs.newrelic.com/docs/agentic-ai/sre-agent/overview/
- https://www.businesswire.com/news/home/20251106134583/en/Causely-Introduces-MCP-Server-for-Automated-Remediation-Across-Kubernetes-and-Application-Code
- https://www.maginative.com/article/neubird-raises-22-5m-to-scale-ai-powered-it-operations-assistant/

Standards, benchmarks, security

- https://modelcontextprotocol.io/specification/2026-07-28/changelog
- https://modelcontextprotocol.io/specification/2026-07-28/server/tools
- https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization
- https://github.com/modelcontextprotocol/rust-sdk
- https://crates.io/crates/rmcp
- https://github.com/open-telemetry/semantic-conventions-genai
- https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/gen-ai-agent-spans.md
- https://github.com/open-telemetry/semantic-conventions-genai/blob/main/docs/gen-ai/mcp.md
- https://github.com/itbench-hub/ITBench
- https://artificialanalysis.ai/articles/itbench-aa-launch
- https://arxiv.org/abs/2501.06706v1
- https://arxiv.org/abs/2605.07161
- https://proceedings.iclr.cc/paper_files/paper/2025/file/d29b8d53678015079e1d245c023e49d2-Paper-Conference.pdf
- https://arxiv.org/abs/2412.17015v1
- https://arxiv.org/abs/2604.15368
- https://www.straiker.ai/blog/ai-sre-agent-prompt-injection-kubernetes-ransomware
- https://www.cyberark.com/resources/agentic-ai-security/poison-everywhere-no-output-from-your-mcp-server-is-safe
- https://genai.owasp.org/resource/owasp-top-10-for-agentic-applications-for-2026
- https://llmstxt.org/
- https://www.optimizely.com/field-notes/articles/does-llms-txt-work/
- https://modelcontextprotocol.io/llms.txt

Reliaburger

- https://github.com/reliaburger/reliaburger/blob/main/docs/roadmap.md
- https://github.com/reliaburger/reliaburger/issues/364
- https://github.com/reliaburger/reliaburger/milestone/18

### Couldn't verify

- Datadog Bits AI SRE pricing (third-party figures only, and they conflict).
- Komodor's MCP server details (the help page returned 403) and the scope of
  its agent's permissions (a third-party review only).
- The STAR Labs/Straiker attack chain (a vendor's own write-up).
- The gke-mcp flag that enables destructive tools.
- The OWASP LLM Top 10 2026 numbering (secondary sources).
- llms.txt adoption statistics (second-hand Ahrefs figures).
- Rocketgraph's commercial customers and funding (none public).
