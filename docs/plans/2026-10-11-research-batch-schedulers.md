# Research: the batch scheduler landscape

Status: research, 11 October 2026. Checked against `main` at `b2affe93`. The
maintainer decided the direction, order and repository layout on 11 October
(see [Decisions](#decisions)) and is reviewing the analysis before it becomes
milestones.

The question started narrow: what would Reliaburger need so that nobody picks
[Armada](https://github.com/armadaproject/armada) over it? Then it widened to
every job scheduler with a meaningful number of users: Slurm and the other HPC
schedulers, the Kubernetes batch add-ons, Mesos, Nomad and Ray, the cloud batch
services and the workflow engines people use as schedulers. For each one we
asked the same things. What does it do that we can't? Who uses it? What do
they complain about? What would it take for Reliaburger to be the obvious
choice instead?

Here's the short answer. Nobody owns **services and serious batch on one
small control plane**. The schedulers with the best batch features (Slurm,
Armada, Volcano) don't run services, or sit on top of Kubernetes, or both.
The ones that run services (Kubernetes, Nomad) are weak at batch until you
bolt on three more projects. Reliaburger already has the hard part of that
slot: services, compact task arrays and one capacity budget, in one binary.
What it lacks is the scheduling model batch users take for granted: GPUs,
queues with fair share, preemption, gangs, dependencies between arrays and a
Python client. Those fit in three releases.

What we can't do is beat each incumbent in its home. Slurm owns tightly
coupled supercomputing, Armada owns fleets of Kubernetes clusters, Ray owns
Python-native distributed computing, and the clouds own capacity on demand.
The last section says why we leave those alone.

## The landscape at a glance

GitHub numbers are from the GitHub API on 11 October 2026. "Activity" is
commits or merged pull requests over the last twelve months, whichever the
project's workflow makes meaningful.

| Scheduler | What it is | Stars | Activity, latest release | Owner or home | Known users |
|---|---|---|---|---|---|
| [Slurm](https://github.com/SchedMD/slurm) | HPC workload manager | 4.4k | Majors every 6 months; 26.05.4 (Sep 2026) | NVIDIA, which bought SchedMD in Dec 2025 | Most of the TOP500; AI neoclouds |
| [HTCondor](https://github.com/htcondor/htcondor) | High-throughput computing | 329 | Monthly; 25.14.1 (Sep 2026) | UW–Madison | CERN, OSG, LIGO, CMS |
| [OpenPBS](https://github.com/openpbs/openpbs) / PBS Pro | HPC workload manager | 805 | Open source last released Jun 2023 | Siemens (via Altair) | Government, weather, CAE |
| IBM Spectrum LSF | HPC workload manager | closed | | IBM | EDA, finance, pharma |
| [Flux](https://github.com/flux-framework/flux-core) | Hierarchical HPC scheduler | 213 | Monthly; v0.90.0 (Oct 2026) | LLNL | El Capitan |
| [Kueue](https://github.com/kubernetes-sigs/kueue) | Job admission and quota for Kubernetes | 3.0k | ~3,500 commits; v0.20.1 (Oct 2026) | Kubernetes SIG Scheduling | Netflix, Google Cloud, Red Hat, CoreWeave |
| [Volcano](https://github.com/volcano-sh/volcano) | Replacement batch scheduler for Kubernetes | 6.0k | ~930 commits; v1.15.3 (Sep 2026) | CNCF Incubating (Huawei-led) | Huawei Cloud, Tencent, Baidu, ING |
| [YuniKorn](https://github.com/apache/yunikorn-core) | YARN-style scheduler for Kubernetes | 1.0k | ~130 commits; v1.10.0 (Oct 2026) | Apache (Apple, Cloudera) | Apple, Cloudera, Pinterest |
| [KAI](https://github.com/kai-scheduler/KAI-Scheduler) | GPU scheduler for Kubernetes (open-sourced Run:ai) | 1.6k | ~700 commits; v0.18.3 (Oct 2026) | CNCF Sandbox (NVIDIA) | NVIDIA Run:ai, Lightning AI |
| [Armada](https://github.com/armadaproject/armada) | Batch meta-scheduler over many Kubernetes clusters | 635 | 437 merged PRs; v0.22.12 (Oct 2026) | CNCF Sandbox (G-Research) | G-Research only |
| [Nomad](https://github.com/hashicorp/nomad) | General orchestrator, one binary | 17.0k | ~830 commits; 2.0.7 (Sep 2026) | IBM, source-available since 2023 | Roblox, Cloudflare |
| [Mesos](https://github.com/apache/mesos) | Two-level cluster manager | 5.4k | Retired to the Apache Attic, 2025 | None | Twitter, Apple, Uber (all left or leaving) |
| [Ray](https://github.com/ray-project/ray) | Python distributed runtime | 44.0k | ~6,150 commits; 2.59.0 (Oct 2026) | PyTorch Foundation | Amazon, Uber, many ML teams |
| AWS, Google and Azure Batch | Managed batch queues | closed | | The clouds | Nextflow and Airflow users |
| [SkyPilot](https://github.com/skypilot-org/skypilot) | Launch ML jobs on any cloud | 10.7k | v0.14.0 (Oct 2026) | SkyPilot | ML teams chasing GPUs |
| [Argo Workflows](https://github.com/argoproj/argo-workflows) | DAGs of pods on Kubernetes | 17.0k | v4.1.5 (Oct 2026) | CNCF Graduated | 217 organisations listed |
| [Airflow](https://github.com/apache/airflow) | Python DAG orchestrator | 47.1k | | Apache | 5,800+ survey respondents |

Three ownership changes stand out. NVIDIA now owns Slurm
([heise](https://heise.de/-11115881)) and the Run:ai scheduler. IBM owns Nomad
([heise](https://heise.de/-10303743)). Siemens owns PBS Pro and Grid Engine
through Altair. A vendor-neutral, open scheduler is rarer than it was two years
ago, and users have noticed: "quite worried that the development and
maintenance of Slurm will be broken by the inevitable market volatility"
([HN](https://news.ycombinator.com/item?id=46277190)).

## HPC schedulers

### Slurm

Slurm is the scheduler to beat. It runs most of the world's supercomputers,
and SemiAnalysis's ClusterMAX 3.0 rating of 77 GPU clouds tests every provider
on Slurm as well as Kubernetes
([summary](https://newsletter.semianalysis.com/p/clustermax-30-the-industry-standard)),
so managed Slurm is table stakes for AI neoclouds. (Meta isn't a Slurm shop,
though: Llama 3 trained on Meta's own scheduler, MAST
([paper](https://arxiv.org/pdf/2407.21783)).)

What it has, and nobody else has all of:

- **Fair share over a tree of accounts**, with decayed usage and multifactor
  priority (age, fair share, job size, partition, QOS).
- **Backfill**, so small jobs fill the gaps in front of a big one without
  delaying it, and **advance reservations**.
- **Preemption** by requeue, cancel or suspend; **QOS** limits per user,
  account and association; accounting in `slurmdbd` for chargeback.
- **Heterogeneous jobs, MPI** through PMIx, **GPUs** through GRES, including
  MIG, and **topology-aware placement**.
- **Job arrays** of up to 4,000,001 tasks, with `%K` to throttle how many run
  at once ([job arrays](https://slurm.schedmd.com/job_array.html)).
- **Federation** across clusters, and cloud nodes through
  `ResumeProgram`/`SuspendProgram`.

SchedMD has validated 500 simple jobs per second sustained, after a page of
tuning: shorter `MinJobAge`, accounting plugins switched off, more MUNGE
threads ([high throughput](https://slurm.schedmd.com/high_throughput.html)).

Running it is the problem. You need `slurmctld`, `slurmd` on every node,
`slurmdbd` with MySQL or MariaDB, MUNGE for authentication, identical
`slurm.conf` and `gres.conf` everywhere, and usually a shared filesystem.
Upgrades must go in order (database daemon, then controller, then nodes), and
converting the accounting database can take hours
([ticket 13706](https://support.schedmd.com/show_bug.cgi?id=13706)). In
February 2026 a MUNGE overflow let local users forge any credential, root
included, and SchedMD told sites to "treat that security issue as if it were
local-root exploit" ([advisory](https://advisories.egi.eu/Advisory-EGI-SVG-2026-04)).

The complaints are about the model as much as the operations:

- "The problem with slurm is how it's typically used: ssh into a shared login
  node with a shared file system… authorization is tightly coupled to linux
  users" ([HN](https://news.ycombinator.com/item?id=25910178)).
- "Slurm should be the answer but it isn't" (same thread).
- "its API:s are awfully inconsistent and there's a lot of code churn between
  versions… but it's an invaluable and reliable tool"
  ([HN](https://news.ycombinator.com/item?id=46277190)).

Containers are a bolt-on (Pyxis and enroot, Apptainer, or `--container`
since 23.02). There are no services, no ingress and no service discovery. So
the industry runs two schedulers and bridges them:
[Slinky](https://github.com/SlinkyProject/slurm-operator) runs Slurm in
Kubernetes pods, Nebius's [Soperator](https://github.com/nebius/soperator)
does the same around a shared root filesystem, and CoreWeave's SUNK is a
proprietary variant. Three companies building the same bridge tells you the
gap is real.

### HTCondor

HTCondor is the closest thing in HPC to our task arrays: huge numbers of
independent jobs over machines that come and go, with no shared filesystem.
Jobs and machines both advertise requirements (ClassAds) and a matchmaker
pairs them. DAGMan runs workflows. The CMS global pool peaks around 350,000
cores across more than 70 sites
([CHEP 2021](https://www.epj-conferences.org/10.1051/epjconf/202125102055)),
and OSPool ran about 192 million jobs in a year. Its limits are per submit
node: a single `schedd` saturated around 50,000 running jobs in CMS tests
([arXiv](https://arxiv.org/pdf/2405.14631)), at roughly 1 MB of memory per
running job. The learning curve (ClassAds, many daemons) is its reputation.

### PBS, LSF, Grid Engine and Flux

- **OpenPBS / PBS Pro** has queues, fair share, backfill, preemption,
  reservations and arrays, and is strong in government and engineering
  simulation. Siemens bought Altair for about $10B in March 2025. The open
  source line hasn't released since June 2023.
- **LSF** is proprietary and priced per core. It has polished fair share and
  licence-aware scheduling for chip design tools. Cost is the recurring
  complaint: one university admin moved to Slurm "when LSF started increasing
  the costs for licensing" ([Beowulf](https://beowulf.org/pipermail/beowulf/2022-March/037071.html)).
- **Grid Engine** went closed under Oracle, then to Univa, then Altair. It
  survives at legacy life-science and chip design sites.
- **Flux** (LLNL) nests schedulers: any job can be a Flux instance with its
  own scheduler, which lifts the throughput ceiling for large ensembles. It
  runs El Capitan's early-access systems. It's pre-1.0 with 691 open issues.

## Kubernetes batch add-ons

Kubernetes doesn't do batch well out of the box, and everyone agrees why: the
default scheduler places one pod at a time, so gangs deadlock, and every unit
of work is a Pod object in etcd. Four projects fill the gap, and they stack.

- **[Kueue](https://github.com/kubernetes-sigs/kueue)** is admission control,
  not a scheduler. It suspends jobs, checks them against quota and lets
  kube-scheduler place the pods. It has cluster queues and local queues,
  cohorts that borrow and lend quota, fair sharing, preemption, topology-aware
  placement, autoscaler integration and **MultiKueue**, which dispatches jobs
  to worker clusters. It has the broadest community and the most momentum.
  Netflix replaced the queueing in its in-house batch platform with it
  ([InfoQ](https://infoq.com/news/2026/08/netflix-kueue-kubernetes-batch)). Its
  gangs are admission plus a timeout, not atomic placement, the API is still
  `v1beta2`, and release notes open with "No, really, you MUST read this
  before you upgrade" ([v0.20.1](https://github.com/kubernetes-sigs/kueue/releases/tag/v0.20.1)).
- **[Volcano](https://github.com/volcano-sh/volcano)** replaces kube-scheduler
  for its pods. It has hierarchical queues, gangs, DRF, preemption, backfill,
  network-topology placement, GPU sharing and the widest framework support
  (Spark, Flink, Ray, MPI, PyTorch). Running two schedulers side by side
  means two caches that disagree, and pods fail with `OutOfcpu` "when many
  jobs are created and completed in a short time"
  ([#2700](https://github.com/volcano-sh/volcano/issues/2700), still seen in
  2026 in [#4970](https://github.com/volcano-sh/volcano/issues/4970)).
- **[YuniKorn](https://github.com/apache/yunikorn-core)** brings YARN's
  hierarchical queues and user quotas to Spark on Kubernetes. It's a small
  Apple and Cloudera team, and the momentum has moved elsewhere.
- **[KAI](https://github.com/kai-scheduler/KAI-Scheduler)**, NVIDIA's
  open-sourced Run:ai scheduler, adds fractional GPUs and **time-based fair
  share** (historical usage counts, not just current allocation), with
  priority kept separate from preemptibility.

Underneath sit the job APIs: Indexed Job (Kubernetes' array, with per-index
retry budgets), JobSet, Kubeflow Trainer and LeaderWorkerSet. Upstream is
adding gang scheduling itself (KEP-4671, alpha in 1.35 and 1.36), which takes
one selling point away from Volcano and YuniKorn. Quota and fair share stay
out of tree.

The result is a typical stack of Trainer, then JobSet, then Jobs, then Pods,
with Kueue on top, a scheduler underneath and the GPU Operator beside it. Each
layer is a separate controller with its own CRDs, webhooks and versions.
SkyPilot puts it bluntly: these add-ons "do not reduce the operational burden
or steep learning curve… these solutions inherit the complexity of Kubernetes
itself" ([post](https://skypilot.ai/blog/slurm-vs-k8s/)).

## Armada

Armada deserves its own section because it's the closest thing to what we
built, and because the question started there.

G-Research started Armada in 2019 to replace HTCondor for its quant research
grid ([CNCF](https://www.cncf.io/blog/2021/01/25/armada-how-to-run-millions-of-batch-jobs-over-thousands-of-compute-nodes-using-kubernetes/)).
It keeps Condor's model, where you submit far more work than the hardware can
run and fair share decides who goes next, and puts it on top of many
Kubernetes clusters. The queue lives outside etcd because, as a maintainer put
it, Kubernetes Jobs put "a lot of load on etcd"
([discussion #1112](https://github.com/armadaproject/armada/discussions/1112)).

It's built around an event log. **Apache Pulsar** is the source of truth,
**PostgreSQL** holds the scheduler's and the UI's views, **Redis** backs the
event streams, and about **eight services** make up the control plane. An
**executor** in each worker cluster holds a gRPC stream to the scheduler,
leases runs and creates the pods. A job is exactly one pod, submitted to a
**queue** (the unit of fair share) and grouped in a **job set**.

Its scheduling is the most complete of the Kubernetes family:

- DRF across weighted queues, over configurable resources.
- Its own priority classes, with urgency preemption and preemption back to
  fair share. A protected fraction and a rate limit keep it stable, and one
  "evict everything, then reschedule" loop handles both kinds.
- Gangs with `gangNodeUniformityLabel`, which keeps a gang on one rack or one
  cluster.
- Pools with home and away scheduling, so jobs borrow idle capacity elsewhere
  and get preempted first.
- Floating resources (licences, fileserver connections), retry policies by
  failure category (new in 2026, off by default), node quarantine and a
  scheduler simulator.
- Experimental, undocumented **market scheduling** with spot prices and
  second-price bids. Nothing else open source has that.

Clients exist for Go, Python, .NET, Java, Rust and Scala, with an Airflow
operator and a young Spark backend.

The project is active (437 merged pull requests in a year, roughly weekly
releases) but narrow. **G-Research is the only organisation in
[ADOPTERS.md](https://github.com/armadaproject/armada/blob/master/ADOPTERS.md)**,
all 15 maintainers work there, Discussions have been silent since August 2023,
and the batch scheduler comparisons from
[InfraCloud](https://www.infracloud.io/blogs/batch-scheduling-on-kubernetes/)
and [Rafay](https://docs.rafay.co/blog/2024/10/11/compare-custom-schedulers-for-kubernetes/)
leave it out entirely. The complaints are in its issues:

- **Installing it is hard.** An AWS architect working through the quickstart
  hit `DeadlineExceeded ... connection reset by peer` and asked "how can I
  check the queue is ok with `armadactl`?"
  ([#3677](https://github.com/armadaproject/armada/issues/3677), open since
  2024). A third party built a guided installer with pre-flight checks for
  Postgres, Redis and Pulsar
  ([#4761](https://github.com/armadaproject/armada/issues/4761)).
- **The documentation lags.** A documentation epic has been open for years
  ([#2726](https://github.com/armadaproject/armada/issues/2726)), and the
  architecture page doesn't match the code.
- **Gangs start together but don't fail together**
  ([#2910](https://github.com/armadaproject/armada/issues/2910)), and members
  find each other out of band through the Kubernetes API.
- **No workflows**: DAGs go to Airflow or Flyte.

## General-purpose orchestrators

### Mesos: the cautionary tale

Mesos is where DRF went into production
([NSDI 2011](https://www.usenix.org/conference/nsdi11/dominant-resource-fairness-fair-allocation-multiple-resource-types)),
and it ran Twitter, Apple's Siri and Uber. It's gone now. A 2021 vote to
retire it was reversed after a burst of interest that never produced a
release, and in August 2025 the Apache board terminated the project; it moved
to the [Attic](https://attic.apache.org/projects/mesos.html) that October.
Marathon, Chronos, Aurora and Metronome are all archived. Uber finished
moving to Kubernetes in 2024 and called the Mesos base "outdated"
([Uber](https://www.uber.com/blog/ubers-journey-to-ray-on-kubernetes-ray-setup/)).

The lesson is about shape, not features. Mesos only offered resources; every
workload type needed its own framework, mostly JVM processes with their own
state in ZooKeeper. "You need to run ZooKeeper, the master, the slaves, and
then each framework"
([HN](https://news.ycombinator.com/item?id=9654174)). Kubernetes won with one
opinionated API. We keep DRF, roles, weights, quotas and reservations from
Mesos, and leave the two-level offer model where it is.

### Nomad: our closest peer

Nomad is one Go binary with Raft between servers and gossip for federation,
which makes it the system most like Reliaburger. Its batch features are
`batch` and `sysbatch` jobs, `periodic` (cron) and `parameterized` jobs you
`dispatch` with a payload, priorities, and preemption (open source since
0.12, but off by default for batch). HashiCorp's own benchmark placed two
million trivial containers in about 22 minutes, around 1,500 placements a
second ([HashiCorp](https://www.hashicorp.com/blog/hashicorp-nomad-meets-the-2-million-container-challenge)).

Its batch gaps are exactly where task arrays help:

- **No job arrays.** You either give a task group a `count`, or `dispatch` N
  times and get N child jobs, each with its own evaluations and allocations.
- **No gang scheduling**: "Add Gang Scheduling (Feedback Wanted)" has been
  open since October 2023
  ([#18773](https://github.com/hashicorp/nomad/issues/18773)).
- **No fair share.** A job that doesn't fit becomes a blocked evaluation and
  waits, with no ordering between tenants. Resource quotas and NUMA placement
  are Enterprise-only.
- **Throughput in practice is lower than the benchmark.** One user reported
  about 3 minutes for 1,000 trivial tasks and "For 10k jobs, it takes 30-40
  minutes" ([forum](https://discuss.hashicorp.com/t/batch-jobs-improve-performances-and-number-of-concurrent-executions/36256)).
  Evaluation storms and dead-job garbage collection need tuning, and one
  issue reports leader memory that "keeps increasing… We have to periodically
  restart the leader" ([#18113](https://github.com/hashicorp/nomad/issues/18113)).

Then there's the licence. Nomad moved to the source-available BUSL in August
2023, IBM closed its HashiCorp purchase in February 2025, and no credible open
fork exists. "Nomad is kind of dead and Apache Mesos is basically dead"
([HN, June 2026](https://news.ycombinator.com/item?id=48484694)) is unfair to
an active project, but it's what people say. Nomad's fans are loyal, though:
"Nomad was always much better than k8s, sad that it never got the same kind of
traction" ([HN](https://news.ycombinator.com/item?id=41362413)). Those are our
users.

### Ray: host it, don't replace it

Ray is a Python runtime for distributed tasks and actors, with placement
groups (its gang primitive), fractional GPUs and an autoscaler, and libraries
for data, training and serving on top. It's huge (44,000 stars, PyTorch
Foundation since October 2025) and Amazon moved exabyte-scale compaction from
Spark to it ([AWS](https://aws.amazon.com/blogs/opensource/amazons-exabyte-scale-migration-from-apache-spark-to-ray-on-amazon-ec2)).
In October 2026 Nscale is in the middle of acquiring Anyscale, Ray's company
([Nscale](https://www.nscale.com/press-releases/nscale-acquires-anyscale)).

Ray doesn't do multi-tenant quotas or priorities between jobs; on Kubernetes
that comes from Kueue or Volcano. Its head node is a single point of failure
unless you add a highly available Redis, which is "officially supported only
if you are using KubeRay for Ray Serve"
([docs](https://docs.ray.io/en/latest/ray-core/fault_tolerance/gcs.html)), and
200 submitted jobs drove one head to 99% memory, with the advice to "use
multiple clusters" ([#60159](https://github.com/ray-project/ray/issues/60159)).
Ray isn't a competitor to replace. It's a workload to host well: a gang that
reserves the head and its workers together, with quotas around it.

## Cloud batch services

The clouds set the baseline that everyone's expectations come from:

| | AWS Batch | Google Cloud Batch | Azure Batch |
|---|---|---|---|
| Array size | 10,000 | 100,000 per task group | via task dependencies |
| Submission rate | 50 calls/s per account | | |
| Dependencies | `N_TO_N` and `SEQUENTIAL` for arrays, 20 per job | Preview only | One-to-one, ranges, per-exit-code satisfy or block |
| Fair share | Weighted shares with decay and idle reservations | Priority 0–99, FIFO within | No |
| Gangs | Multi-node parallel jobs | MPI-style hosts file | Multi-instance tasks |

Sources: [AWS quotas](https://docs.aws.amazon.com/batch/latest/userguide/service_limits.html),
[AWS fair share](https://docs.aws.amazon.com/batch/latest/userguide/scheduling-policies.html),
[Google quotas](https://docs.cloud.google.com/batch/quotas),
[Azure task dependencies](https://learn.microsoft.com/en-us/azure/batch/batch-task-dependencies).

The complaints are about latency and opacity. AWS users report jobs waiting
in `RUNNABLE` for "3 to 6 hours"
([re:Post](https://repost.aws/questions/QU56MiZPbhR7W1Hor5CEHT-A/aws-batch-jobs-in-runnable-for-several-hours)),
and even the happy path spends about a minute between `SUBMITTED` and
`RUNNABLE`. Azure Batch isn't being retired, but it's shedding features
(low-priority VMs, custom images, older GPU series), and Microsoft is ending
HPC Pack support in August 2027, which leaves on-premises Windows HPC sites
looking for something new.

[SkyPilot](https://github.com/skypilot-org/skypilot) and Modal are the newer
"run it anywhere" crowd. SkyPilot launches jobs across clouds and chases
cheap GPUs, but it's a launcher with no queues or fair share, and its jobs
controller is fragile
([#10123](https://github.com/skypilot-org/skypilot/issues/10123)). Modal sets
the developer experience bar: `f.map(inputs)` from Python, starts in under a
second, no YAML.

## Workflow engines

Airflow, Argo Workflows, Flyte, Prefect, Dagster, Temporal and Nextflow all
end up being used as job schedulers, and none of them is good at it at volume:

- **Argo Workflows** keeps a workflow's status in one etcd object, which must
  stay under 1 MB. Large fan-outs fail with "etcdserver: request is too large"
  ([#1186](https://github.com/argoproj/argo-workflows/issues/1186)), and
  "You cannot horizontally scale the controller"
  ([scaling docs](https://argo-workflows.readthedocs.io/en/latest/scaling/)).
- **Airflow** caps dynamic task mapping at 1,024 by default, and its own
  improvement proposal admits that "with thousands of tasks… it causes
  starvation" ([AIP-100](https://cwiki.apache.org/confluence/spaces/AIRFLOW/pages/406618462/AIP-100+Eliminate+Scheduler+Queueing+Starvation+On+Concurrency+Limits)).
- **Flyte** has map tasks with `min_success_ratio`, so an array can succeed
  if, say, 95% of its tasks do.
- **Nextflow** submits arrays only to Slurm, PBS, LSF, Grid Engine, AWS Batch
  and Google Batch. Its bioinformatics users already think in arrays.

The pattern is clear. Batch users want simple dependencies *in* the scheduler
(this array after that one, element by element, or after all of it) and are
happy to get full DAG authoring, data passing, lineage and backfills from a
separate tool. No cloud batch service tries to be Airflow; they all ship an
Airflow operator and a Nextflow executor instead.

## What people complain about, everywhere

Across all these projects, the complaints group into five themes:

1. **Too many moving parts.** Pulsar, Postgres and Redis for Armada; MySQL,
   MUNGE and a shared filesystem for Slurm; ZooKeeper and JVM frameworks for
   Mesos; four CRD layers for Kubernetes batch; Redis for Ray's head.
2. **Two schedulers that disagree.** Volcano beside kube-scheduler, Slurm
   beside Kubernetes, Ray beside Kueue. Every bridge is a cache that can be
   wrong.
3. **One object per unit of work.** Pods in etcd, child jobs in Nomad, a 1 MB
   status in Argo, a megabyte per running job in HTCondor's `schedd`. That's
   what caps fan-out.
4. **Gangs that start together but don't fail together** (Armada, Kueue's
   timeouts), or no gangs at all (Nomad, Kubernetes until recently).
5. **Vendor risk.** NVIDIA owns Slurm and KAI, IBM owns Nomad and LSF,
   Siemens owns PBS and Grid Engine, and Armada is one company's project.

## Where Reliaburger stands

On `main`, 0.2.0 ("A million jobs") has its task-array foundation and common
job lifecycle merged but isn't released.

### What we already do better

- **One binary, no external services**, with its own registry, logs, metrics,
  dashboard, secrets and ingress. That answers complaint 1 outright.
- **One scheduler for services and batch**, sharing one execution budget per
  node (`src/bun/execution_budget.rs`), so nothing promises the same CPU
  twice. That answers complaint 2.
- **Compact task arrays.** An array is a template, a count and chunk grants
  in Raft (`src/meat/task_array.rs`), expanded on each node, up to 2^24
  (16,777,216) tasks. A million tasks is a few dozen Raft entries. That
  answers complaint 3, and beats AWS (10,000), Google (100,000) and Slurm
  (4,000,001) on paper.
- **Durable outcomes**: stable task indexes, results fenced against stale
  grants, no silent replay of unknown outcomes, and idempotency keys
  ([manual](../manual/14_batch-jobs.md)).
- **Process tasks**: a task can be a host binary, with no container start.
- **Cron and deployment hooks** on the same run pipeline as arrays.
- **Open source, with one maintainer.** That's no better than Armada for
  bus factor, and we should say so. But nobody can relicense it out from under
  its users.

### What we lack

Against the best of each family:

| Capability | Best in class | Reliaburger on `main` |
|---|---|---|
| GPUs | Slurm GRES with MIG; KAI fractions | Refused for jobs; the leader sets every node's GPUs to 0 (`src/cluster/orchestrate.rs`). F01, [#359](https://github.com/reliaburger/reliaburger/issues/359) |
| Queues with weights | Armada, Kueue, Slurm partitions | One chunk queue per array |
| Fair share | Slurm's account tree; Armada's DRF; KAI's time-based | FIFO per node. Namespace quotas for jobs are coming in 0.2.0 ([#679](https://github.com/reliaburger/reliaburger/issues/679)) |
| Backfill | Slurm | No |
| Priority and preemption | Slurm, Armada, Volcano | None |
| Gangs | Volcano, KAI, Ray placement groups | None |
| Node selectors for jobs | Everyone | Apps only (`src/meat/filter.rs`) |
| Retries by failure kind | Armada, AWS Batch | One attempt budget per array |
| Array dependencies | AWS `N_TO_N`, Azure ranges, Slurm `afterok` | None |
| Partial success | Flyte `min_success_ratio` | `max_failed_indexes` stops an array, but can't succeed one |
| Usage accounting | `sacct` | Aggregate counts in Brioche |
| Clients and integrations | Armada's six SDKs; AWS Batch in Airflow and Nextflow | HTTP API and `relish`; no OpenAPI spec |
| Multi-cluster | Armada, MultiKueue, Slurm federation | None; Franchise is v2 ([#711](https://github.com/reliaburger/reliaburger/issues/711)) |
| Proven throughput | Slurm 500 jobs/s validated; Nomad ~1,500/s claimed | Unqualified ([#640](https://github.com/reliaburger/reliaburger/issues/640), [#668](https://github.com/reliaburger/reliaburger/issues/668)) |
| Proven scale | Slurm and Armada at tens of thousands of nodes | Three-node clusters |
| Adopters | Everyone above | None yet |

The last three rows matter most and no milestone fixes them quickly. Our
real-container evidence is 1,064 tasks in 747 seconds on a 4-vCPU VM with a
debug build. That shows correctness, not throughput.

We also found a stale claim of our own. `docs/design/scheduler-meat.md` §9.1
has a "Measured (12-node bench)" column with 2,847 jobs/s. No qualification
record supports it, and 0.2.0's own docs say throughput is unqualified. The
whitepaper honesty pass
([#690](https://github.com/reliaburger/reliaburger/issues/690)) should remove
or relabel it.

## What we won't do

Some homes belong to their incumbents, and chasing them would make Reliaburger
worse at what it's for:

- **Fleets of Kubernetes clusters** (Armada, MultiKueue). Running on
  Kubernetes contradicts the point of Reliaburger. Scheduling across many
  Reliaburger clusters belongs with Franchise
  ([#711](https://github.com/reliaburger/reliaburger/issues/711)), which the
  maintainer decided not to schedule before 0.5.0. When Franchise is planned,
  its plan should say whether to add job leasing across clusters, building on
  the queues below and the pull model task arrays already use.
- **Tightly coupled supercomputing** (Slurm, Flux). MPI over InfiniBand on
  thousands of nodes, with a shared parallel filesystem and decades of site
  tuning, isn't a contest a 0.x project should enter. We'll run MPI and NCCL
  jobs as gangs, but we won't claim Slurm's ground.
- **Python-native distributed computing** (Ray, Dask, Spark). Host them as
  gangs; don't reimplement them.
- **Full workflow authoring** (Airflow, Argo, Flyte, Temporal). Simple array
  dependencies, yes. A DAG language, no; integrate with the engines people
  already use.
- **Capacity on demand** (the clouds, SkyPilot). A self-hosted cluster can't
  conjure VMs. Node autoscaling is a separate question for the fleet work.
- **Market scheduling** (Armada's experiment). It's novel, but it serves one
  firm's internal economy. Watch it.

## What it would take

Three releases, after the migration releases (0.4.0, 0.4.1 and 0.5.0), each
with one headline in the usual style: GPUs, then pipelines, then fair share.
Pipelines come before fair share because they bring users in through Airflow
and Nextflow; fair share is what keeps them once a second team shares the
cluster. The throughput qualification already in
0.2.0 ([#640](https://github.com/reliaburger/reliaburger/issues/640),
[#668](https://github.com/reliaburger/reliaburger/issues/668),
[#680](https://github.com/reliaburger/reliaburger/issues/680)) and namespace
quotas for jobs ([#679](https://github.com/reliaburger/reliaburger/issues/679))
come first; without numbers there's nothing to compare.

### 0.6.0: GPUs

Whole-device GPU placement for apps and jobs, which is F01
([#359](https://github.com/reliaburger/reliaburger/issues/359)) pulled forward
from "Later". Nodes report real device identities, type, memory, driver
version and health through gossip; the leader places by device, not by a
count; the runtime assigns and isolates the devices it was granted; and task
arrays stop refusing GPU requests. Fractions and MIG stay out until there's a
supported partitioning contract (the
[delegated jobs plan](2026-10-04-plan-delegated-jobs.md) says why).

> Exit test: on a cluster with GPU nodes, an app asking for one GPU and a
> task array asking for one GPU per task each get distinct physical devices,
> never share one, and see only their own in the container. Mark a GPU
> unhealthy and its work moves.

### 0.7.0: Pipelines

Arrays that depend on arrays, driven from Python:

1. **Array dependencies.** After all of another array; element by element
   (task *i* after task *i*, like AWS `N_TO_N`); and after a success ratio
   (like Flyte's `min_success_ratio`), with exit codes that satisfy or block
   (like Azure). No DAG language.
2. **Partial success**: an array can succeed with a stated fraction of its
   tasks.
3. **A Python client** and an **OpenAPI spec** for the HTTP API:
   `submit_array`, `wait`, `results`, and a `map` that feels like Modal's.
4. **Integrations**: an Airflow provider (operator and deferrable sensor) and
   a Nextflow executor. Both are thin layers over the Python client and the
   API, and both bring users who already think in arrays.

> Exit test: a Nextflow pipeline and an Airflow DAG each run a three-stage
> pipeline on Reliaburger (a 100,000-task preparation array, an element-wise
> array after it, and a reduce after 95% of that succeeds) with no
> Reliaburger-specific code beyond choosing the executor. Kill a node mid-run
> and both pipelines still finish, with each accepted result recorded once.

### 0.8.0: Fair share

The scheduling model batch users take for granted, inside one cluster:

1. **Queues.** Named, weighted queues in Raft; arrays are submitted into one.
   Namespace quotas stay as hard caps on top.
2. **Fair share.** The leader orders chunk grants by each queue's dominant
   share (DRF), with usage that decays over time, so a team that ran a lot
   yesterday yields to one that didn't (Slurm and KAI do this).
3. **Backfill.** Small chunks fill the gaps in front of a large one without
   delaying it, which also fixes FIFO stranding capacity behind a big request.
4. **Priority classes and preemption.** Urgency preemption and preemption
   back to fair share, with a protected fraction, a rate limit and a grace
   period (SIGTERM, then SIGKILL). A preempted attempt is requeued without
   spending one of the task's attempts. Apps sit above every batch class.
5. **Gangs.** All grants in one leader decision or none, with a node-label
   uniformity constraint, a rendezvous environment (rank, size, peer
   addresses) for every member, and group-wide failure: one member dies, the
   gang stops and retries as a unit. That fixes Armada's
   [#2910](https://github.com/armadaproject/armada/issues/2910) and Kueue's
   timeouts by design, and lets a Ray or PyTorch cluster start as one unit.
6. **Node selectors for task arrays**, reusing the app filter.
7. **Retries by failure kind**: out of memory, node lost, preempted and
   non-zero exit get separate budgets.
8. **Usage by queue and namespace**: CPU-, memory- and GPU-hours over time,
   in Mayo and `relish`, because decayed fair share needs the history anyway
   and teams want it for chargeback.

> Exit test: on one cluster with GPU nodes, team A floods its queue with a
> million CPU tasks. Team B, with equal weight, submits 10,000 tasks and gets
> within 5% of half the cluster inside a minute, through fair-share
> preemption. A high-priority eight-GPU gang, pinned to one rack label, then
> starts on all eight devices at once or not at all. Kill one gang member and
> the whole gang stops, requeues and restarts. Preempted tasks get their grace
> period, keep their attempt budget and still finish. No service on the
> cluster loses a replica, and the whole run is one binary with no external
> services.

### What we'd have then

With those three releases, Reliaburger is the obvious choice for a team that
runs services *and* batch on its own hardware, wants Slurm's or Armada's
scheduling model without their dependencies, and doesn't want Kubernetes plus
four add-ons. That's most of the people who look at Armada, Nomad, Volcano or
Kueue today, and the Slurm sites that are tired of bridging Slurm and
Kubernetes. It still isn't the choice for G-Research's grid, a national
supercomputer or a cloud-only team that wants capacity on demand, and we
should say that plainly on the website.

Floating resources, a scheduler simulator, topology beyond node labels, Spark
and Dagster or Prefect integrations are worth having but decide nobody's
choice. Leave them until a user asks.

## Decisions

The maintainer decided on 11 October 2026:

1. **The split.** Single-cluster scheduling becomes milestones; scheduling
   across clusters waits for Franchise.
2. **Order.** After the migration releases (0.4.0, 0.4.1, 0.5.0).
3. **GPUs first.** GPU placement ships before fair share, as its own release.
4. **A Python client before 1.0.** Yes.
5. **Pipelines before fair share**: 0.6.0 GPUs, 0.7.0 Pipelines, 0.8.0 Fair
   share.
6. **Integrations live in this repository until 1.0**: the Python client, the
   Airflow provider and the Nextflow executor (for example under `clients/`
   and `integrations/`), tested by the same CI against the same binary and
   published to PyPI and the Nextflow plugin registry from the same release.
   With no backwards compatibility before 1.0, an API change has to fix its
   clients in the same pull request. Reconsider splitting them out at 1.0.

Still open: the maintainer is reviewing this analysis before any milestones
or issues are created.

## Sources

Each project's repository, documentation, issues, adopter and maintainer
files, and release lists, read with the GitHub API on 11 October 2026; the
vendor, CNCF, conference and news pages linked inline. Reliaburger's own code
and documentation at `b2affe93`, with paths inline.

Not verified, so don't quote them as fact: Slurm's current TOP500 share (the
oft-quoted 60% dates from about 2021); G-Research's node and GPU counts; any
Armada adopter besides G-Research; LSF and Flux throughput; OpenPBS's future
under Siemens; whether CoreWeave's SUNK is open source; the general
availability of Google Batch dependencies; and the exact HPC Pack
end-of-support date (27 or 30 August 2027, from secondary sources).
