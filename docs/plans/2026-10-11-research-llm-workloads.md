# Research: LLM inference and training

Status: research, 11 October 2026. Checked against `main` at `b2affe93`. A
follow-up to [the batch scheduler landscape](2026-10-11-research-batch-schedulers.md),
which this page assumes you've read. The maintainer decided the release order
on 11 October (see [Decisions](#decisions)); three questions remain before it
becomes milestones.

The question: what would make Reliaburger the obvious choice for running LLM
inference *and* training on your own GPUs? Which projects would we have to
beat, which should we host instead of fighting, and what can we learn from
all of them?

Here's the short answer. The market splits by size. Frontier labs with
thousands of GPUs run Slurm and their own tooling, and that won't change.
Teams with **8 to 256 GPUs** are badly served. On Kubernetes, the reference
stack for serving one model is a dozen or more separately versioned
components. On Slurm, there are no services at all. The single-node tools
(Ollama, GPUStack, dstack) each cover one slice. Nobody gives that team one
system that serves models, fine-tunes them, runs the product around them and
shows them what their GPUs are doing.

Reliaburger is shaped for that slot. It runs services and batch on one
control plane, it's the node agent rather than a broker, and it ships the
registry, ingress, metrics, logs and secrets that the Kubernetes stack bolts
on. But today it can't run a single GPU workload. The scheduler sees zero
GPUs on every node, no device reaches the container, the ingress times out
after 30 seconds of silence, and the registry can't move a 2 GiB layer
between peers. The GPU release we already planned has to grow, and we need
one new release for serving models before the pipelines and fair-share
releases.

## Who we'd be up against

### Inference

| Project | What it is | Stars | Backers | Notes |
|---|---|---|---|---|
| [vLLM](https://github.com/vllm-project/vllm) | Inference engine | 93.5k | Community, many vendors | The default engine; inside almost every layer below |
| [SGLang](https://github.com/sgl-project/sglang) | Inference engine | 37.0k | Community | Strong number two, with its own Rust router |
| [llm-d](https://github.com/llm-d/llm-d) | Distributed serving on Kubernetes | 4.8k | Red Hat, Google, IBM, NVIDIA, CoreWeave | CNCF Sandbox since March 2026; the one to beat on Kubernetes |
| [NVIDIA Dynamo](https://github.com/ai-dynamo/dynamo) | Distributed serving framework | 8.3k | NVIDIA | KV-aware router, disaggregation, planner |
| [AIBrix](https://github.com/vllm-project/aibrix) | Serving control plane on Kubernetes | 5.1k | ByteDance | Routing, LoRA, KV cache, autoscalers |
| [KServe](https://github.com/kserve/kserve) | Model serving on Kubernetes | 6.1k | Community, Red Hat | Wraps vLLM and llm-d; ships in OpenShift AI |
| [GPUStack](https://github.com/gpustack/gpustack) | GPU cluster manager for serving | 5.8k | GPUStack (Shenzhen) | Picks and tunes the engine; broad Chinese accelerator support |
| [Ollama](https://github.com/ollama/ollama) | Local model runner | 183k | Ollama | Where everyone starts |
| [TGI](https://github.com/huggingface/text-generation-inference) | Inference engine | 10.9k | Hugging Face | **Archived**, in maintenance mode; drop it as a target |

### Training

| Project | What it is | Stars | Notes |
|---|---|---|---|
| [Slurm](https://github.com/SchedMD/slurm) | HPC workload manager | 4.4k | Still the default for pre-training; NVIDIA-owned since December 2025 |
| [Kubeflow Trainer](https://github.com/kubeflow/trainer) | Training jobs on Kubernetes | 2.2k | v2 builds on JobSet; topology through Kueue or Volcano |
| [Ray](https://github.com/ray-project/ray) | Python distributed runtime | 44.0k | Underneath most RL post-training frameworks |
| [dstack](https://github.com/dstackai/dstack) | AI orchestration over clouds, Kubernetes and SSH hosts | 2.3k | Closest to our pitch; tasks, services, dev environments |
| [SkyPilot](https://github.com/skypilot-org/skypilot) | Launch jobs and services across clouds, Kubernetes and Slurm | 10.7k | $20M seed in July 2026 ([Fortune](https://fortune.com/2026/07/21/skypilot-from-databricks-cofounder-raises-20m-to-be-the-switzerland-of-ai-compute/)) |
| [verl](https://github.com/volcengine/verl), [OpenRLHF](https://github.com/OpenRLHF/OpenRLHF), [slime](https://github.com/THUDM/slime) | RL post-training | 23.8k, 10.1k, 8.6k | Mix inference servers and trainers in one job |
| Unsloth, TRL, Axolotl | Fine-tuning frameworks | 77.7k, 19.5k, 12.6k | What people actually launch |

Two former contenders have dropped out: [Determined AI](https://github.com/determined-ai/determined)
hasn't committed since March 2025, and torchtune's README says it's "no
longer actively maintained" ([#2883](https://github.com/meta-pytorch/torchtune/issues/2883)). Managed fine-tuning APIs (Thinking Machines'
[Tinker](https://siliconangle.com/2025/12/12/thinking-machines-makes-tinker-ai-fine-tuning-service-generally-available/),
Together, Fireworks) take a slice of fine-tuning away from schedulers altogether.

### Commercial platforms that set the bar

NVIDIA now owns Run:ai (fractional GPUs, quotas), the KAI scheduler, Slurm
and Lepton (managed endpoints, dev pods and batch jobs, with "bring your own
compute" node groups). Its AI Factory designs ship NVIDIA AI Enterprise and
Mission Control on top of Kubernetes or Slurm. Modal sets the bar for cold
starts, Baseten for dedicated serving, and Together and Fireworks for
"fine-tune, then serve" in one click. Every one of them assumes either their
cloud or a Kubernetes or Slurm cluster someone else runs.

## What we can learn

### 1. The Kubernetes inference stack is a dozen projects

Count what a team installs to serve one model well on Kubernetes in late
2026 ([llm-d](https://llm-d.ai), [OpenShift AI](https://docs.redhat.com/en/documentation/red_hat_openshift_ai_self-managed/3.4/html/deploy_models_using_distributed_inference_with_llm-d/deploying-models-using-distributed-inference_distributed-inference)):

1. Kubernetes.
2. The NVIDIA GPU Operator: driver, container toolkit, device plugin or DRA
   driver, DCGM exporter, node feature discovery.
3. A GPU-aware batch scheduler (KAI or Volcano).
4. LeaderWorkerSet for multi-node models.
5. Gateway API CRDs.
6. A gateway (Istio, kgateway, Envoy Gateway).
7. The Gateway API Inference Extension and its endpoint picker (llm-d's
   router).
8. vLLM or SGLang.
9. Optionally KServe, plus Knative for scale to zero.
10. KEDA or llm-d's autoscaler.
11. Prometheus and Grafana.
12. A model cache (volumes, Dragonfly or a model streamer).
13. cert-manager or a mesh for mTLS.
14. Optionally LMCache or NIXL for KV offload and disaggregation.
15. An API gateway for keys and quotas (LiteLLM, Envoy AI Gateway).

Even the maintainers know it's too much. The Inference Extension's own issue
says "many users are not there yet and find it operationally unjustified to
start with the full fledged solution"
([#1838](https://github.com/kubernetes-sigs/gateway-api-inference-extension/issues/1838)).
A first-time AIBrix user with eight L40S cards wrote "I am very confused"
([#1690](https://github.com/vllm-project/aibrix/issues/1690)). And from
Hacker News: "companies buy a ton of DGX boxes and then are surprised that
Nvidia does not have any Kubernetes native platform for training and
inferencing across all the DGX machines"
([HN](https://news.ycombinator.com/item?id=45588189)).

Reliaburger already covers items 1, 5, 6, 11 and 13, and parts of 12 and 15.
The rest is what this page is about.

### 2. Routing is the cheap win; disaggregation is a niche

The most rigorous published study, by Google engineers on llm-d's blog,
found that routing requests by prefix-cache affinity and load ("sticky until
saturated") gave **2.9×** input throughput on prefill-heavy code generation
and **2.0×** on a SaaS workload, but roughly nothing on decode-bound
reasoning ([llm-d](https://llm-d.ai/blog/sticky-until-saturated-token-aware-routing)).
Single runs, no confidence intervals, but the direction matches every other
claim. Splitting prefill from decode onto different GPUs is different: llm-d
reports anything from 10% to 70%, NVIDIA's "up to 30×" for Dynamo is a
[projection](https://developer.nvidia.com/blog/introducing-nvidia-dynamo-a-low-latency-distributed-inference-framework-for-scaling-reasoning-ai-models) for DeepSeek-R1 on GB200 NVL72, and an ICML 2026 paper found that
KV transfers can saturate the network and "no single fixed routing strategy
meets all SLOs" ([arXiv](https://arxiv.org/abs/2603.13358)).

So inference-aware routing belongs in our ingress. Disaggregation belongs in
the engines, with us providing the plumbing.

### 3. CPU is the wrong autoscaling signal

Red Hat's own documentation says "CPU utilization does not reflect the actual
load on the inference server", and GPU utilisation sits near 100% whenever
the engine is batching, whatever the load
([llm-d autoscaling](https://llm-d.ai/docs/guides/workload-autoscaling)). The
signals that matter are the ones every engine already exports: queue depth
(`vllm:num_requests_waiting`), running requests, KV-cache use, and the
time-to-first-token and inter-token latency histograms. Our autoscaler
accepts only CPU and memory (`src/meat/autoscaler.rs`).

### 4. Cold start is weight movement

A 70B model in 16-bit is about 140 GB, and a CUDA serving image is 5 to 20
GB. AWS measured 80 to 460 seconds just to move 60 to 200 GiB of weights from
S3 to the GPU ([AWS](https://aws.amazon.com/blogs/containers/fast-model-loading-for-ai-inference-on-amazon-eks)).
KServe's issue on the subject does the sums: an eight-node cluster caching a
405B model downloads "6,480 GB total… 8 identical copies"
([#5838](https://github.com/kserve/kserve/issues/5838)), and it's proposing
Dragonfly to fix it. As [Modular's handbook](https://handbook.modular.com/infrastructure-and-operations/fast-scaling/)
puts it, no autoscaler can outrun a multi-minute cold start.

The emerging answers are weights packaged as OCI artifacts (the CNCF
[ModelPack](https://github.com/modelpack/model-spec) spec, Docker Model
Runner, KServe modelcars), peer-to-peer distribution (Dragonfly v2.5 speaks
`hf://` natively and [claims](https://www.cncf.io/blog/2026/04/06/peer-to-peer-acceleration-for-ai-model-distribution-with-dragonfly/)
origin traffic fell from 26 TB to about 130 GB across 200 nodes), streaming loaders, and placing replicas where the weights
already are. Kubernetes users assemble that from four or five projects. We
have a registry with P2P distribution built in, and an image-locality term in
the scheduler that's waiting for data. **This is our clearest differentiator**
and it's mostly built.

### 5. Failure is the normal state of a training run

Meta's Llama 3 run on 16,000 H100s saw 419 unexpected interruptions in 54
days, 78% of them hardware, with faulty GPUs and HBM alone about half
([paper](https://arxiv.org/html/2407.21783), §3.3.4). Meta's study of its
research cluster puts the mean time to failure at **7.9 hours for a
1,024-GPU job, against 47.7 days for an 8-GPU one**
([arXiv](https://arxiv.org/html/2410.21680v2)). OPT-175B needed "at least 35
manual restarts and the cycling of over 100 hosts" in two months; the loop
each time was diagnose, cordon, resume from checkpoint
([paper](https://arxiv.org/abs/2205.01068)). BLOOM kept four spare nodes for
48 working ones ([paper](https://arxiv.org/pdf/2211.05100)). ByteDance's MegaScale recovered automatically more than 100
times in a few weeks ([paper](https://arxiv.org/abs/2402.15627)).

SemiAnalysis now grades GPU clouds on exactly this: a bad node detected
within two minutes of an injected fault, taken out of scheduling and
remediated, ideally by swapping in a hot spare
([ClusterMAX 3.0 summary](https://newsletter.semianalysis.com/p/clustermax-30-the-industry-standard)).
Providers lose marks for "a lack of health checks or dcgmi integration, and
no monitoring dashboard" ([ClusterMAX review](https://clustermax.semianalysis.com/cloudreview/primeintellect)). NVIDIA's [NVSentinel](https://github.com/NVIDIA/NVSentinel)
does detection and remediation on Kubernetes, and stops cordoning at half the
cluster as a circuit breaker.

The lesson splits by size. At 8 GPUs, failure is rare and ease of use wins.
At 1,024, the scheduler's main job is to notice, cordon and restart without a
human. Our gossip layer and reporting tree are a natural carrier for health
events; we just don't have any GPU health events yet.

### 6. RL post-training is services and batch in one job

The fastest-growing training workload mixes the two things every other
scheduler keeps apart. verl puts the trainer, the rollout engine (vLLM or
SGLang) and the reference model on the same GPUs and reshards weights between
them every step ([docs](https://verl.readthedocs.io/en/latest/blog/v0.7.html)).
slime runs Megatron training beside SGLang servers behind a router. NeMo Gym
attaches CPU sandboxes for environments. Modal found weight sync was the
bottleneck: 95 seconds over TCP against 1.5 over RDMA for Kimi K2.6
([Modal](https://modal.com/blog/reinforcement-learning-infrastructure-problem)).

Today Ray fills the gap and the scheduler underneath knows nothing about the
roles. A first-class group that contains long-running servers with health
checks and service names *and* batch ranks is exactly Reliaburger's thesis.
The way to win isn't replacing Ray; it's starting the Ray cluster these
frameworks expect as one group, unchanged.

### 7. The UX bar is "one command to an endpoint"

The features users cite when they choose a platform
([dstack HN thread](https://news.ycombinator.com/item?id=42053180), GPUStack
and Ollama issues):

- One command from a model name to an OpenAI-compatible URL (Ollama,
  GPUStack, Baseten).
- Idle models unloaded so their memory goes back to the pool, then reloaded
  on the next request (Ollama's keep-alive, the top request on GPUStack:
  [#2824](https://github.com/gpustack/gpustack/issues/2824)).
- Scale to zero with a cold start measured in seconds (Modal).
- GPU dev environments you SSH or VS Code into, on the same cluster (dstack,
  Lepton, GPUStack).
- Fine-tune, then serve, in one flow (Together, Fireworks).
- "if I don't have to think beyond a docker-compose.yml that would be the
  right level of simple" (same HN thread).

### 8. GPUs sit idle, and nobody can see it

"Datacenters run at roughly 30% to 40% effective utilisation… from 122k
jobs, 59% of the compute was wasted", and the cause is that "under-requesting
kills your job mid-run… So everyone over-requests by two to three times"
([Launch HN](https://news.ycombinator.com/item?id=48356312)). Per-container
GPU metrics, on by default, would make that visible on day one. On
Kubernetes it takes the [DCGM exporter](https://github.com/NVIDIA/dcgm-exporter), the pod-resources API and Grafana.

### 9. Don't build drivers, engines or Ray

Every successful platform here leaves the driver to the OS, the engine to
vLLM or SGLang, KV transfer to NIXL or LMCache, health probes to DCGM, and
distributed Python to Ray. The ones that try to own those layers (Dynamo for
NVIDIA's own stack, Run:ai's memory swapping) are vendors protecting
hardware. Our job is placement, plumbing, weights, routing and recovery.

### 10. Vendor neutrality is a selling point again

NVIDIA owns Run:ai, KAI, Slurm and Lepton. Sovereign and regulated buyers
keep asking for air-gapped, vendor-neutral installs. A single open binary
that runs from an internal mirror is an easy story to tell them.

## What's missing in Reliaburger today

An inventory of `main` turned up more blockers than "no GPUs":

| Area | Today | Why it matters |
|---|---|---|
| GPU placement | The leader sets every node's GPUs to 0 (`src/cluster/orchestrate.rs`), so a clustered `gpu = 1` app never places | Nothing runs |
| GPU in the container | No `/dev/nvidia*`, no CDI, no device assignment in `src/grill` | Nothing runs |
| GPU model | A count (`gpu: Option<u32>`), not devices with identities | Can't pick connected GPUs or say which one failed |
| Shared memory | `/dev` is a 64 MiB tmpfs; no `/dev/shm` size, no memlock limits (`src/grill/oci.rs`) | NCCL and multi-GPU engines fail; vLLM's examples use 10 GiB |
| Ingress timeouts | 30 s idle read timeout, not configurable (`src/wrapper/proxy.rs`) | A slow first token or a pause between tokens kills the stream |
| Ingress bodies | Buffered whole, 10 MiB cap | Long contexts and images in one request hit 413 |
| Ingress routing | Host and path, round-robin | No model-aware or cache-aware routing |
| Peer blob pulls | 2 GiB cap and a 30 s deadline for the whole blob (`src/pickle/pull.rs`, `src/pickle/p2p.rs`) | A weights layer can't move between nodes |
| Pull resume | None; a failed pull starts again | Hundred-gigabyte pulls never finish on a flaky link |
| Pull-through cache | Upstream layers buffered in memory (`src/pickle/upstream.rs`) | A 10 GB layer is 10 GB of RAM |
| OCI artifacts | Only image manifests and indexes accepted | No ModelPack or Docker Model Runner weights |
| Image locality | Scored (weight 15) but always empty: "Nothing reports cached images yet" | Replicas don't land where weights are |
| Health checks | One probe; startup fails after 60 misses (about 10 minutes at the default interval) | Workable for weights loading, but no separate readiness and liveness |
| Autoscaling | CPU or memory only; `min` must be at least 1 | No queue-depth scaling, no scale to zero |
| Groups | None; distributed training is design text | No multi-node models, no multi-node training |
| Storage | Node-local volumes; object storage only for exports and backups | Checkpoints and datasets live on host-mounted shared filesystems, which works but isn't modelled |

Resident model workers ([#641](https://github.com/reliaburger/reliaburger/issues/641)),
an engine adapter, whole-device GPUs (F01,
[#359](https://github.com/reliaburger/reliaburger/issues/359)) and
distributed training groups are all designed in the
[delegated jobs plan](2026-10-04-plan-delegated-jobs.md) but not built.

## What to build, what to host, what to leave

### Build

These are Reliaburger's job and nobody can do them for us:

- **Devices**: GPUs as typed devices with identities and attributes (UUID,
  model, memory, MIG profile, NUMA node, NVLink peers) in Raft, assigned by
  device, not count.
- **The container plumbing**: device injection through CDI, `/dev/shm` size,
  memlock and IPC settings, RDMA devices for training.
- **Health**: watch the driver's XID errors, map them to NVIDIA's documented
  actions (restart the app, reset the GPU, drain and reboot), run DCGM's
  quick diagnostic as a readiness gate, and cordon through our existing drain
  path, with a circuit breaker.
- **Weights**: model artifacts in Pickle, distributed peer to peer, pre-warmed
  on the nodes the scheduler picks, mounted read-only, with placement that
  prefers nodes already holding them.
- **Routing**: OpenAI-aware, prefix- and load-aware routing in Wrapper, with
  long-lived streams and load shedding.
- **Autoscaling** on engine metrics, and scale to zero with the ingress
  holding requests while a replica starts.
- **Groups**: all-or-nothing placement of N members with ranks, a leader
  address and group restart, roles within a group, topology domains, and hot
  spares.
- **Observability**: GPU metrics per container and inference metrics per
  model, on by default.

### Host as ordinary workloads

vLLM, SGLang and llama.cpp as apps. torchrun, Axolotl, TRL and Unsloth as
jobs and groups. Ray as a group (head plus workers) for verl, OpenRLHF and
anyone else who needs it. NIXL and LMCache inside the engines.

### Integrate through standard protocols

- [CDI](https://github.com/cncf-tags/container-device-interface) for devices:
  NVIDIA's toolkit already writes the spec, there's an official Rust crate,
  and Nomad still has CDI as an open issue
  ([#24990](https://github.com/hashicorp/nomad/issues/24990)), so we'd get
  there first. AMD's toolkit writes CDI too, so AMD comes cheaply after
  NVIDIA.
- DCGM for health and GPU telemetry.
- The Gateway API Inference Extension's
  [endpoint picker protocol](https://github.com/kubernetes-sigs/gateway-api-inference-extension/blob/main/docs/proposals/004-endpoint-picker-protocol/README.md),
  which has a standalone mode without Kubernetes in progress. If Wrapper can
  call an external picker, llm-d's router and Dynamo-style pickers plug in,
  and we get precise KV-event routing and disaggregated routing without
  building them.
- ModelPack and Docker Model Runner artifacts for weights.
- An OpenAI-compatible batch API (`/v1/batches`) on top of task arrays.

### Leave alone

- **Drivers, Fabric Manager and IMEX installation.** Check they're there and
  report readiness; the OS installs them.
- **Fractional GPUs through CUDA interception** (HAMi's `LD_PRELOAD`).
  Fragile, and Kubernetes' own device allocation is overtaking it. MIG
  instances, exposed as devices, are the honest version.
- **Frontier pre-training.** 1,000 to 100,000 GPUs, MPI tightly bound to the
  fabric, RMA contracts, power-aware throttling. Slurm's ground. Offer a
  documented split instead: Slurm runs the pre-training partition,
  Reliaburger runs fine-tuning, evaluation, RL and serving.
- **Engines, KV transfer and distributed Python runtimes.**
- **Capacity on demand** from clouds.
- **Intel Gaudi**, until someone asks.

## A revised release plan

The landscape page's plan (decided on 11 October) was 0.6.0 GPUs, 0.7.0
Pipelines and 0.8.0 Fair share. For LLM workloads, three things change:

1. **The GPU release grows.** Placing GPUs by count isn't enough: without
   devices, CDI, shared memory, health and topology inside a node, vLLM with
   tensor parallelism doesn't run.
2. **A new "Models" release for serving**, before pipelines. Single-node
   serving (a 70B model on four to eight GPUs) is what most 8–256 GPU teams
   want first, and it's where our registry and ingress pay off.
3. **Groups become their own release, straight after Models.** Gangs were
   part of the fair-share release; for LLMs they have to carry ranks and a
   leader address, roles, topology domains and hot spares, because the same
   group runs multi-node inference, multi-node training and RL. That's too
   important to wait behind pipelines and fair share.

So the order becomes 0.6.0 GPUs, 0.7.0 Models, 0.8.0 Groups, 0.9.0
Pipelines and 0.10.0 Fair share. The maintainer decided the Models release
and moving groups earlier on 11 October. Pipelines still comes before fair
share, as decided on the landscape page. Each release below says what it contains, what using
it would look like, how we'd build it, the demo we'd put on the landing page,
and the exit test that proves it.

The configuration and commands are sketches to make the shape concrete, not
settled syntax; each release's plan decides the real names. The demos follow
the pattern the homepage already uses: an asciinema recording, plus a step in
the executable tour wherever a laptop can run it. Most GPU demos can't run on
a laptop, so they'd be recordings on real hardware with a small laptop
variant beside them.

### 0.6.0: GPUs

F01 ([#359](https://github.com/reliaburger/reliaburger/issues/359)) plus:

1. Typed devices in Raft; per-device assignment for apps and task arrays.
2. CDI injection into the runc spec, reading the spec NVIDIA's toolkit
   generates; cgroup device rules.
3. Topology inside a node: choose GPUs that share NVLink or a PCIe switch,
   with CPUs on the same NUMA node.
4. Container settings for GPU work: `/dev/shm` size, memlock, IPC.
5. Driver and CUDA version gossiped per node; refuse or redirect an image
   whose CUDA needs exceed a node's driver.
6. Health: XID watch, NVIDIA's action table as an enum, DCGM quick
   diagnostic as a readiness gate, cordon and drain with a circuit breaker.
7. GPU metrics per container in Mayo (utilisation, memory, power, tensor
   activity, XIDs), and GPU columns in `relish top`.
8. Existing MIG instances exposed as devices.

**What you'd write.** `gpu` stays a number for the common case and grows a
table when you care which GPUs:

```toml
[app.chat]
image = "vllm/vllm-openai:v0.31.0"
args = ["--model", "/models/llama-3.3-70b", "--tensor-parallel-size", "4"]
port = 8000
gpu = 4                  # four whole devices, connected by NVLink if possible
shm = "16Gi"

[job.finetune]
image = "axolotl:0.9"
command = ["axolotl", "train", "/config/qlora.yaml"]
gpu = { count = 2, model = "H100*", min_memory = "80Gi" }
```

And one new view, because "which GPU is doing what" is the question every
team with GPUs asks first:

```text
$ relish gpus
NODE    GPU  MODEL      DRIVER  HEALTH    USED BY          UTIL  MEMORY
gpu-1   0-3  H100 80GB  580.82  healthy   chat-0 (nvlink)  91%   74/80Gi
gpu-1   4-5  H100 80GB  580.82  healthy   finetune         64%   61/80Gi
gpu-1   6    H100 80GB  580.82  XID 79    cordoned
gpu-1   7    H100 80GB  580.82  healthy   idle             0%    0/80Gi
```

**How we'd build it.**

- *Discovery.* Replace the `nvidia-smi` parsing in `src/bun/gpu.rs` with
  NVML (the [`nvml-wrapper`](https://github.com/Cldfire/nvml-wrapper) crate)
  for each device's UUID, model, memory, NUMA node, NVLink peers and the
  driver and CUDA versions. Each node reports its inventory through gossip
  (`src/mustard`), and the leader stores real allocatable devices instead of
  the zero in `src/cluster/orchestrate.rs`.
- *Scheduling.* `Resources` (`src/meat/types.rs`) gains device requests. The
  filter (`src/meat/filter.rs`) checks model, memory and CUDA compatibility;
  a small topology search on each candidate node picks a concrete device set
  (prefer one NVLink clique, then one PCIe switch, then one NUMA node). The
  scheduling decision records the device UUIDs, so the node can't pick
  differently. Task-array admission (`src/meat/admission.rs`) stops
  hard-coding zero GPUs, and the node's `ExecutionBudget` counts devices
  beside CPU and memory.
- *Injection.* The official
  [`container-device-interface`](https://github.com/cncf-tags/container-device-interface-rs)
  crate reads the spec that NVIDIA's `nvidia-cdi-refresh`
([docs](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/1.19.1/cdi-support.html)) keeps current, and
  `src/grill/oci.rs` applies the edits for the granted UUIDs, adds the cgroup
  device rules, a sized `/dev/shm` mount and a memlock limit. Process
  workloads get `CUDA_VISIBLE_DEVICES` only, documented as unisolated.
- *Health.* A node task subscribes to NVML's XID events, maps each to
  NVIDIA's documented action
([XID catalog](https://docs.nvidia.com/deploy/xid-errors/analyzing-xid-catalog.html)) through an enum (restart the app, reset the GPU,
  drain and reboot), marks the device unhealthy in gossip and calls the
  existing drain path. The leader refuses to cordon past a configured share
  of the cluster. If `dcgmi` is installed, its quick
[diagnostic](https://docs.nvidia.com/datacenter/dcgm/latest/reference/command-line-reference/dcgmi/dcgmi-diag.html) runs when a
  node joins and after a failure.
- *Metrics.* Mayo samples NVML per device on its scrape tick and labels each
  series with the app and instance from the allocation table, which is the
  join Kubernetes needs the DCGM exporter and the pod-resources API for.
- *Testing.* A fake NVML and CDI backend lets the portable suite test
  allocation, topology choice and XID handling. Real devices need a new gated
  target, `make test-gpu`, and a GPU machine to run it on.

**The demo: "Your GPU died at 3 a.m."** A recording on an eight-GPU machine.
Apply the chat app; `relish gpus` shows four NVLink-connected GPUs taken. Ask
the model a question with `curl`. Then inject a fault with a new Smoker
command (`relish fault gpu gpu-1/2 --xid 79 --acknowledge`, built on DCGM's
error injection): `relish events` shows the device marked failed and the node
cordoned within seconds, the replica rescheduled onto healthy GPUs, and
`relish wtf` naming the exact device. Same `curl`, same answer. It fits the
chaos story the homepage already tells, and nobody else demos it because
nobody else can do it in one command.

> Exit test: on a node with eight GPUs, vLLM serves a 70B model with tensor
> parallelism over four NVLink-connected GPUs, a fine-tuning job uses two
> more, and each sees only its own devices. Inject an XID 79 on one of the
> serving GPUs: the node is cordoned within two minutes, the job moves, and
> `relish` says which device failed.

### 0.7.0: Models

Serving LLMs on one node per replica, from a model name to an endpoint:

1. **Weights in Pickle.** ModelPack and Docker Model Runner artifacts; peer
   pulls with no 2 GiB cap, chunked and resumable, streamed to disk rather
   than memory; pre-warming on the chosen nodes; read-only mounts; image and
   model locality reported, so placement prefers nodes that hold them.
2. **Health and rollouts for slow starters.** A startup gate measured in
   minutes, readiness through a real generate call, no liveness kills while
   loading, and surge rollouts that never drop serving capacity.
3. **Wrapper for inference.** Long-lived streams with configurable timeouts,
   streamed request bodies, routing by the OpenAI `model` field and LoRA
   adapter, prefix- and load-aware scoring from the engines' standard
   metrics, 429 load shedding, and a hook for an external endpoint picker.
4. **Autoscaling on engine metrics**: queue depth, KV-cache use, running
   requests, and latency targets; scale to zero with the ingress holding
   requests during a cold start.
5. **API keys, token rate limits and usage per namespace and model.**
6. **Inference metrics** per model and replica, beside the GPU ones.
7. **`relish model run`**: from a model name to an OpenAI-compatible URL,
   choosing the engine and tensor parallelism from the detected GPUs, with
   idle unload.

**What you'd write.** The one-liner, for the first five minutes:

```sh
relish model pull hf://Qwen/Qwen3-32B      # into Pickle, as a ModelPack artifact
relish model run qwen3-32b
# serving at https://qwen3-32b.models.example.com/v1 (OpenAI-compatible)
```

And the TOML it writes, for everything after:

```toml
[model.qwen3]
source = "models/qwen3-32b:2026-09"   # in Pickle; hf:// and s3:// pull through
engine = "vllm"                       # or "sglang", "llama.cpp"; chosen from the GPUs if left out
gpu = 2
replicas = { min = 0, max = 6 }
scale_on = { queue_depth = 4 }        # waiting requests per replica
idle_unload = "15m"

[model.qwen3.ingress]
host = "qwen3.models.example.com"
routing = "prefix-affinity"           # or "least-loaded", or picker = "http://llm-d-router:9002"
api_keys = true
token_rate_limit = "200k/min"
```

`[model.*]` is shorthand, not a new workload type: it expands to an ordinary
`[app.*]` with the engine image, a read-only weights mount, a generate-based
readiness check, the engine's metrics endpoint and the autoscaling policy.
`relish apply --dry-run` shows the expansion, and you can write the app by
hand if you want something the shorthand doesn't cover.

**How we'd build it.**

- *Pickle as a model store.* Accept artifact manifests (`artifactType` and
  the ModelPack media types) in `src/pickle/api.rs`. Lift the 2 GiB peer cap,
  replace the 30-second whole-blob deadline with an idle timeout that only
  fires when bytes stop arriving, resume with HTTP `Range`, and stream
  pull-through layers to disk instead of memory (`src/pickle/upstream.rs`).
  Extend the rarest-first planner in `src/pickle/p2p.rs` from whole layers
  to byte ranges, so a 140 GB model comes from every peer that has it at
  once.
- *Mounting.* Each node unpacks a model once into a content-addressed,
  read-only cache, and `src/grill/oci.rs` bind-mounts it into the engine.
  Reference counts and least-recently-used eviction keep the cache within a
  disk budget.
- *Placement.* Nodes report the images and models they hold, which finally
  feeds the locality score in `src/meat/score.rs` that "Nothing reports
  cached images yet" leaves inert; models get a much heavier weight than
  images. When the scheduler commits a placement, the target node starts
  pulling at once, and a surge rollout keeps the old replica serving until
  the new one is ready.
- *Health.* The health block (`src/bun/health.rs`) gains a startup timeout,
  a readiness check that asks for a one-token completion, and liveness that
  stays quiet until startup has passed.
- *Wrapper.* Per-route read timeouts and streamed request bodies
  (`src/wrapper/proxy.rs`). Read the `model` field from the start of the
  JSON body to pick the route. For scoring, Mayo already scrapes each
  engine's metrics; feed queue depth and KV-cache use into the routing table
  (`src/wrapper/routing.rs`) and hash the first few kilobytes of the prompt
  for prefix affinity, following the published "sticky until saturated"
  design. Answer 429 when every replica is saturated. For anything smarter,
  call an external picker over the endpoint picker protocol.
- *Autoscaling.* `src/meat/autoscaler.rs` gains engine-metric signals read
  from Mayo and allows `min = 0` for models. To scale from zero, Wrapper
  holds requests in a bounded queue per route, tells the leader, and
  releases them when the first replica is ready.
- *Keys and limits.* API keys are Sesame tokens scoped to a model route;
  token counts come from each response's `usage` field and land in Mayo per
  namespace and key, which also feeds the rate limiter.
- *The CLI.* `relish model run` reads `relish gpus`, picks the engine and
  tensor parallelism, writes the TOML and applies it.
- *A laptop path.* With `engine = "llama.cpp"` and a small model, all of this
  runs on a laptop quickstart cluster without GPUs.

**The demo: "From zero to a 70B endpoint, and back to zero."** A recording
on two GPU nodes. `relish model run llama-3.3-70b`: the first replica pulls
from Hugging Face with a progress bar. Scale to two: the second replica's
weights come from the first node's peers, and `relish model status` shows
the transfer rate and the time saved. Start a load generator with a long
shared system prompt and switch routing from round-robin to prefix affinity;
Brioche's time-to-first-token graph drops while you watch. Stop the load:
the model scales to zero and `relish gpus` shows the memory free. Send one
more request: it's held, answered, and the cold start is printed. A laptop
step in the executable tour runs `relish model run qwen3-0.6b --engine
llama.cpp` and a chat completion with `curl`.

> Exit test: `relish model run` serves a 70B model on a fresh three-node
> cluster. The second and third replicas start from peer-held weights, at
> least five times faster than the first. A load test on shared-prefix
> traffic gets at least twice the throughput of round-robin at the same
> latency target. Traffic stops and the model scales to zero; the next
> request is held, answered, and the cold start is reported. All without
> leaving the binary.

### 0.8.0: Groups

Multi-node work as one unit, which multi-node inference, multi-node training
and RL all need. Groups were part of the fair-share release; the maintainer
brought them forward on 11 October, so they come straight after Models:

1. All-or-nothing placement with ranks, size and leader address injected (the
   [torchrun](https://docs.pytorch.org/docs/main/elastic/run.html) and
   [vLLM multi-node](https://docs.vllm.ai/en/stable/serving/parallelism_scaling/)
   contracts), a stable name for rank 0, and group restart with a retry
   budget.
2. Roles within a group (trainer ranks, inference servers with health checks
   and service names, CPU sandboxes), so a Ray cluster, an RL job or a
   prefill and decode pair starts as one unit.
3. Topology domains from node labels (NVLink domain, rack, switch), required
   or preferred.
4. RDMA for training: InfiniBand or RoCE devices, memlock, `/sys` and host
   networking, the way Slurm sites run NCCL today.
5. Checkpoint-aware lifecycle: SIGTERM with a configurable grace period on
   drain and group restart, and a `$CHECKPOINT_DIR` convention for resume.
   The fair-share release reuses the same grace period for preemption.
6. Hot spares: a pool of reserved nodes, and when a member's node fails, the
   group restarts on a spare from the latest checkpoint without a human.
7. Health as prolog and epilog: DCGM diagnostics before a group starts and
   after it fails, with "lemon" nodes taken out.

**What you'd write.** A multi-node fine-tune is a group whose members get
their rank from the environment:

```toml
[group.finetune]
members = 2                       # nodes
gpu = 8                           # per member
image = "trl:0.20"
command = ["torchrun", "--nnodes=${RB_GROUP_SIZE}", "--node-rank=${RB_RANK}",
           "--rdzv-endpoint=${RB_LEADER}:29500", "train.py"]
topology = { same = "rack" }
network = "rdma"
checkpoint = { dir = "/ckpt", grace = "120s" }
restart = { attempts = 5, use_spares = true }
```

An RL job is a group with roles, so verl's Ray cluster starts as one unit:

```toml
[[group.rl.role]]
name = "head"
image = "verl:0.7"
command = ["ray", "start", "--head", "--block"]
port = 6379

[[group.rl.role]]
name = "worker"
count = 4
gpu = 8
image = "verl:0.7"
command = ["ray", "start", "--address=${RB_ROLE_HEAD}:6379", "--block"]
```

And a model too big for one node is a `[model.*]` from 0.7.0 with
`members = 2`: the shorthand expands to a group running vLLM's multi-node
mode, with tensor parallelism inside each node and pipeline parallelism
across them.

**How we'd build it.**

- *Placement.* A group run in Raft holds one slot per member. The leader
  places all members in one decision and one Raft entry, or none, so there
  are no partial reservations to deadlock on. If a group can't fit, it waits
  without holding anything; reserving capacity for a large waiting group is
  the fair-share release's backfill.
- *Identity.* Each member gets `RB_RANK`, `RB_GROUP_SIZE` and `RB_LEADER`,
  each role's address is in `RB_ROLE_<NAME>`, and rank 0 gets a stable name
  through Onion's DNS.
- *Failure.* Any member failing bumps the group's generation, which fences
  every member, the same way stale task-array grants are fenced today. The
  leader sends every member SIGTERM, waits for the grace period, then places
  the group again, spares first. Roles can opt out: a crashed rollout server
  restarts in place instead of taking the trainers down with it.
- *Topology.* Nodes label themselves with their NVLink domain from NVML, and
  operators add rack and switch labels. `same = "rack"` is a filter that all
  members share one value of a label.
- *RDMA.* `network = "rdma"` means the host network namespace, InfiniBand
  devices through CDI, memlock and `/sys`. Host networking weakens
  isolation, so it needs an explicit namespace permission, in line with the
  namespace isolation work in
  [#677](https://github.com/reliaburger/reliaburger/issues/677).
- *Spares.* Nodes labelled as spares run only task-array work, which is
  requeued when a group needs the node. Graceful preemption of that work
  arrives with fair share.
- *Prolog and epilog.* The DCGM quick diagnostic runs on every member's node
  before a group starts, and a longer one on the failed member's node after
  a failure. The leader counts failures per node over a window and cordons
  repeat offenders.

**The demo: "Kill a node mid-training."** A recording of a two-node
fine-tune with its loss curve in Brioche. `relish fault kill-node gpu-2
--acknowledge`: the group stops, a spare joins, training resumes from the
last checkpoint, and the loss curve picks up where it left off, with the
lost minutes printed and no human in the loop. Second beat: a verl RL run
starts as one group next to the served model from 0.7.0, and its rollout
servers show up in `relish status` like any other service.

> Exit test: a 16-GPU fine-tuning job across two nodes and a verl RL job
> (Ray, with vLLM rollouts) share a cluster with a served model. Kill a node
> under the fine-tuning job: it restarts on a hot spare from its latest
> checkpoint within five minutes, with no human. Kill one rollout server: it
> restarts in place and the RL job carries on. The served model never drops
> below its minimum replicas.

### 0.9.0: Pipelines

As decided on the landscape page: array dependencies, partial success, a
Python client and OpenAPI spec, and an Airflow provider and Nextflow
executor in this repository. Two additions for LLM work:

- **An OpenAI-compatible batch API** (`/v1/batches`) over task arrays, so
  offline inference, embedding and evaluation fill idle GPUs at low priority.
- **The resident model worker contract**
  ([#641](https://github.com/reliaburger/reliaburger/issues/641)), so batch
  tasks call a warm engine instead of starting a process per task. The
  batch API is its natural front door.

**What you'd write.** Dependencies are one key on a job:

```toml
[job.shard]
image = "prep:v3"
command = ["/prep", "--shard", "{index}"]
count = 100000

[job.embed]
image = "embed:v1"
command = ["/embed", "--shard", "{index}"]
count = 100000
gpu = 1
after = { job = "shard", each = true }              # task i after shard task i

[job.index]
image = "index:v1"
after = { job = "embed", min_success = "95%" }      # after the whole array, partial success allowed
```

From Python, for the people who'd otherwise reach for Modal:

```python
import reliaburger as rb

cluster = rb.connect()
run = cluster.map("embed:v1", inputs=documents, gpu=1)
for result in run.results():
    store(result)
```

From existing tools, with no Reliaburger code beyond choosing it:
`process.executor = 'reliaburger'` in Nextflow, and a
`ReliaburgerArrayOperator` in Airflow that defers while the array runs.

**How we'd build it.**

- *Dependencies stay compact.* An `after` condition is stored on the job
  definition (`src/meat/job.rs`), not as per-task edges. The leader's
  planning loop (`src/bun/task_array_leader.rs`) treats a dependent array as
  ungrantable until its condition holds. For `each`, `plan_grants`
  (`src/meat/task_array_state.rs`) intersects the downstream array's queued
  chunks with the upstream array's accepted successes, which are already
  held as compact index sets, so a 16-million-task pipeline costs no more
  Raft state than two arrays.
- *Partial success.* A run-level `min_success` turns a run whose failures
  stay under the threshold into a success that says it was partial.
- *The API description.* Generate the OpenAPI spec from the axum routes, and
  check it in CI so the spec and the server can't drift.
- *Clients.* The Python client in `clients/python/`, the Airflow provider and
  Nextflow plugin in `integrations/`, each with tests that run against a
  development cluster in CI and are published from the same release.
- *The batch API.* A JSONL file of requests becomes a task array whose tasks
  call a model's route at low priority through the resident worker contract.

**The demo: "Your GPUs serve by day and embed by night."** Brioche shows a
served model and a three-stage pipeline (shard, embed, index) on the same
GPUs. As chat traffic drops, the embedding array fills the idle GPUs; when
traffic returns, the model's latency graph stays flat and the batch yields.
Kill a node mid-run: the dashboard shows chunks granted elsewhere, and the
final count is exact. Beside it, a ten-line Airflow DAG and a three-line
Nextflow config, both green.

> Exit test: a Nextflow pipeline and an Airflow DAG each run a three-stage
> pipeline on Reliaburger (a 100,000-task preparation array, an element-wise
> array after it, and a reduce after 95% of that succeeds) with no
> Reliaburger-specific code beyond choosing the executor. Kill a node mid-run
> and both pipelines still finish, with each accepted result recorded once.

### 0.10.0: Fair share

The fair-share scope from the landscape page, now that groups exist to share:
queues, decayed DRF, backfill, priority classes and preemption, node
selectors for task arrays, retries by failure kind, and usage by queue and
namespace. Groups and task arrays are both scheduled through the queues.

**What you'd write.** Queues are a few lines, and any job, array or group
names one:

```toml
[queue.research]
weight = 2

[queue.product]
weight = 1
guaranteed = { gpu = 8 }

[group.finetune]
queue = "research"
priority = "preemptible"
# ...the rest as in 0.8.0
```

**How we'd build it.**

- *Queues and fair share.* Queue definitions live in Raft. `plan_grants`
  orders candidate chunks and groups by each queue's dominant share over
  CPU, memory and GPUs, using usage counters that decay with a configured
  half-life. The leader keeps them in memory and rebuilds them from Mayo's
  usage history after a failover, so fair share doesn't add Raft writes per
  grant.
- *Backfill.* The head of the queue that doesn't fit, often a large group,
  gets a reservation on specific nodes; smaller chunks may use those nodes
  if their task timeout ends before the reservation's expected start. The
  timeout plays the role of Slurm's time limit.
- *Preemption.* The leader revokes a victim's grant, and the existing fencing
  for stale grants handles the race with a late completion. The node sends
  SIGTERM, waits for the grace period that groups introduced, then SIGKILL,
  and records a `Preempted` outcome that doesn't spend an attempt. The
  protected fraction and the rate limit live in the planning step. Apps sit
  above every batch class.
- *Usage.* CPU-, memory- and GPU-hours per queue and namespace go into Mayo,
  which feeds both the decay and `relish usage` for chargeback.

**The demo: "Two teams, one cluster, no arguments."** Team A floods its
queue with a million CPU tasks and a big fine-tune. Team B submits its own
group. Brioche shows team A's fine-tune checkpointing in its grace period
and yielding, team B's group starting, and the two queues' shares settling
at their 2:1 weights. The served model's latency graph doesn't move.

> Exit test: on one cluster with GPU nodes, team A floods its queue with a
> million CPU tasks. Team B, with equal weight, submits 10,000 tasks and gets
> within 5% of half the cluster inside a minute, through fair-share
> preemption. A high-priority eight-GPU group pinned to one rack label then
> starts on all eight devices at once or not at all. Preempted work gets its
> grace period, keeps its attempt budget and still finishes. No service on
> the cluster loses a replica.

### A landing page for AI work

Put together, the five demos tell one story in the order a team meets it.
Get a GPU working and survive its failure (0.6.0). Serve a model from one
command and watch it scale to zero (0.7.0). Survive a dead node
mid-training (0.8.0). Fill the idle GPUs with batch work overnight (0.9.0).
Share the cluster between teams without arguments (0.10.0). Each is a
short recording with a laptop-sized step in the executable tour where one
exists. Together they'd be a second
tour on the homepage, beside the existing one, aimed at people with GPUs.

### Testing on real GPUs

The maintainer's GPU hardware is one NVIDIA RTX 5080: a consumer Blackwell
card with 16 GB of memory, no NVLink and no MIG. That's enough to prove
everything that happens on one GPU, and not enough to prove anything that
needs two. So the GPU releases are tested in three tiers:

1. **The portable suite, everywhere.** A fake NVML and CDI backend lets CI
   test device inventory, allocation, topology choice, XID handling, group
   placement and failure without a GPU.
2. **`make test-gpu`, on the RTX 5080.** A new gated target for everything
   one real GPU can prove: CDI injection and cgroup isolation (a container
   without a grant sees no GPU), NVML metrics, the health path, and vLLM
   serving a model that fits in 16 GB (an 8B model in 8-bit, for example).
   It also covers model artifacts in Pickle, generate-based readiness, scale
   to zero and cold-start timing.
3. **Rented multi-GPU machines, a few hours per release.** For what one card
   can't show: tensor parallelism over NVLink, topology choice, a second
   replica loading weights from a peer GPU node, multi-node groups, RDMA and
   hot spares. The demo recordings for those beats happen in the same
   session.

What the 5080 can and can't prove, release by release:

| Release | On the RTX 5080 | Needs rented hardware |
|---|---|---|
| 0.6.0 GPUs | Device inventory, CDI, isolation, metrics, `relish gpus`, a synthetic XID leading to a cordon | Several GPUs per job, NVLink topology, moving work to a healthy GPU |
| 0.7.0 Models | `relish model run`, weights in Pickle, readiness, scale to zero, cold start, API keys and token limits | Peer-loaded replicas on a second GPU node, prefix routing across replicas |
| 0.8.0 Groups | Group mechanics with CPU members (torchrun on its CPU backend across nodes) and one GPU member | Multi-node GPU training, RDMA, spares, verl at scale |
| 0.9.0 Pipelines | GPU task arrays and the batch API on one card | Throughput across many GPUs |
| 0.10.0 Fair share | Queues, preemption and grace periods on one card | Shares settling across many GPUs |

Two caveats to check early. DCGM's diagnostics and error injection are
built for data-centre GPUs, and we can't count on them on a GeForce card, so
on the 5080 the health path gets a synthetic XID through the node's event
watcher (ClusterMAX's graders inject synthetic XIDs the same way). And
Blackwell consumer cards need a recent driver and CUDA 12.8 or later, which
is a useful first case for the driver and CUDA compatibility check.

The exit tests above describe the multi-GPU target. Each release's plan
splits its exit test into the part the 5080 proves on every run and the part
the rented session proves once before the release.

### Later

GPU dev environments (SSH and VS Code with an idle timeout), disaggregated
prefill and decode through the picker hook, LoRA adapters pulled from Pickle,
fast restore of warm engines (CRIU with cuda-checkpoint, once NVIDIA's own
version stops being slower than a cold start on large models), AMD through
CDI, GB200 NVL72 compute domains, and a Slurm-style submission shim.

## Where that leaves us

After 0.6.0 and 0.7.0, Reliaburger serves models on a team's own GPUs with
llm-d's baseline (prefix-aware routing, metric-driven autoscaling, weights
distributed peer to peer) from one binary, with the API, the database and
the UI running beside the model. That's the pitch nobody else makes:
dstack orchestrates GPU jobs, GPUStack serves models, SkyPilot needs a
Kubernetes or Slurm to sit on. Reliaburger would be the cluster, and run the
whole product.

After 0.8.0, it fine-tunes, runs RL and trains across nodes with automatic
recovery, which covers most teams below a few hundred GPUs. It still isn't
the choice for a frontier pre-training run, and we should say that plainly.

## Decisions

The maintainer decided on 11 October 2026:

1. **A Models release.** 0.7.0 Models comes before pipelines.
2. **Groups earlier.** Groups become their own release, 0.8.0, straight after
   Models; Pipelines moves to 0.9.0 and Fair share to 0.10.0.
3. **The GPU release carries all eight items**, health and metrics included.
4. **Models are `[model.*]`**, shorthand that expands to an `[app.*]`.
5. **Hardware.** One RTX 5080 runs `make test-gpu`; multi-GPU tests and
   recordings use rented machines (see [Testing on real GPUs](#testing-on-real-gpus)).

Still open:

6. Where do GPU dev environments go? dstack, Lepton and GPUStack all win
   researchers with them.
7. Do we commit to the endpoint picker protocol as Wrapper's extension point,
   so llm-d's router plugs in, or build our own routing only?
8. Is renting multi-GPU machines for a few hours per release acceptable, and
   from which provider?

## Sources

GitHub figures (stars, activity, releases, issue counts) come from the
[GitHub REST API](https://docs.github.com/en/rest) on 11 October 2026.
Reliaburger's own code and documentation are at
[`b2affe93`](https://github.com/reliaburger/reliaburger/tree/b2affe93), with paths inline.
Every other source is linked where it's used, and listed here by section.
The landscape sources are in
[the batch scheduler landscape](2026-10-11-research-batch-schedulers.md#sources).

**Who we'd be up against**

- [vllm-project/vllm](https://github.com/vllm-project/vllm)
- [sgl-project/sglang](https://github.com/sgl-project/sglang)
- [llm-d/llm-d](https://github.com/llm-d/llm-d)
- [ai-dynamo/dynamo](https://github.com/ai-dynamo/dynamo)
- [vllm-project/aibrix](https://github.com/vllm-project/aibrix)
- [kserve/kserve](https://github.com/kserve/kserve)
- [gpustack/gpustack](https://github.com/gpustack/gpustack)
- [ollama/ollama](https://github.com/ollama/ollama)
- [huggingface/text-generation-inference](https://github.com/huggingface/text-generation-inference)
- [SchedMD/slurm](https://github.com/SchedMD/slurm)
- [kubeflow/trainer](https://github.com/kubeflow/trainer)
- [ray-project/ray](https://github.com/ray-project/ray)
- [dstackai/dstack](https://github.com/dstackai/dstack)
- [skypilot-org/skypilot](https://github.com/skypilot-org/skypilot)
- [fortune.com: skypilot-from-databricks-cofounder-raises-20m-to-be-the-switzerland-of-ai-compute](https://fortune.com/2026/07/21/skypilot-from-databricks-cofounder-raises-20m-to-be-the-switzerland-of-ai-compute/)
- [volcengine/verl](https://github.com/volcengine/verl)
- [OpenRLHF/OpenRLHF](https://github.com/OpenRLHF/OpenRLHF)
- [THUDM/slime](https://github.com/THUDM/slime)
- [determined-ai/determined](https://github.com/determined-ai/determined)
- [meta-pytorch/torchtune issue #2883](https://github.com/meta-pytorch/torchtune/issues/2883)
- [siliconangle.com: thinking-machines-makes-tinker-ai-fine-tuning-service-generally-available](https://siliconangle.com/2025/12/12/thinking-machines-makes-tinker-ai-fine-tuning-service-generally-available/)

**What we can learn**

- [llm-d.ai](https://llm-d.ai)
- [docs.redhat.com: deploying-models-using-distributed-inference_distributed-inference](https://docs.redhat.com/en/documentation/red_hat_openshift_ai_self-managed/3.4/html/deploy_models_using_distributed_inference_with_llm-d/deploying-models-using-distributed-inference_distributed-inference)
- [kubernetes-sigs/gateway-api-inference-extension issue #1838](https://github.com/kubernetes-sigs/gateway-api-inference-extension/issues/1838)
- [vllm-project/aibrix issue #1690](https://github.com/vllm-project/aibrix/issues/1690)
- [Hacker News item 45588189](https://news.ycombinator.com/item?id=45588189)
- [llm-d.ai: sticky-until-saturated-token-aware-routing](https://llm-d.ai/blog/sticky-until-saturated-token-aware-routing)
- [developer.nvidia.com: introducing-nvidia-dynamo-a-low-latency-distributed-inference-framework-for-scaling-reasoning-ai-models](https://developer.nvidia.com/blog/introducing-nvidia-dynamo-a-low-latency-distributed-inference-framework-for-scaling-reasoning-ai-models)
- [arXiv 2603.13358](https://arxiv.org/abs/2603.13358)
- [llm-d.ai: workload-autoscaling](https://llm-d.ai/docs/guides/workload-autoscaling)
- [aws.amazon.com: fast-model-loading-for-ai-inference-on-amazon-eks](https://aws.amazon.com/blogs/containers/fast-model-loading-for-ai-inference-on-amazon-eks)
- [kserve/kserve issue #5838](https://github.com/kserve/kserve/issues/5838)
- [handbook.modular.com: fast-scaling](https://handbook.modular.com/infrastructure-and-operations/fast-scaling/)
- [modelpack/model-spec](https://github.com/modelpack/model-spec)
- [cncf.io: peer-to-peer-acceleration-for-ai-model-distribution-with-dragonfly](https://www.cncf.io/blog/2026/04/06/peer-to-peer-acceleration-for-ai-model-distribution-with-dragonfly/)
- [arXiv 2407.21783](https://arxiv.org/html/2407.21783)
- [arXiv 2410.21680v2](https://arxiv.org/html/2410.21680v2)
- [arXiv 2205.01068](https://arxiv.org/abs/2205.01068)
- [arXiv 2211.05100](https://arxiv.org/pdf/2211.05100)
- [arXiv 2402.15627](https://arxiv.org/abs/2402.15627)
- [newsletter.semianalysis.com: clustermax-30-the-industry-standard](https://newsletter.semianalysis.com/p/clustermax-30-the-industry-standard)
- [clustermax.semianalysis.com: primeintellect](https://clustermax.semianalysis.com/cloudreview/primeintellect)
- [NVIDIA/NVSentinel](https://github.com/NVIDIA/NVSentinel)
- [verl.readthedocs.io: v0.7](https://verl.readthedocs.io/en/latest/blog/v0.7.html)
- [modal.com: reinforcement-learning-infrastructure-problem](https://modal.com/blog/reinforcement-learning-infrastructure-problem)
- [Hacker News item 42053180](https://news.ycombinator.com/item?id=42053180)
- [gpustack/gpustack issue #2824](https://github.com/gpustack/gpustack/issues/2824)
- [Hacker News item 48356312](https://news.ycombinator.com/item?id=48356312)
- [NVIDIA/dcgm-exporter](https://github.com/NVIDIA/dcgm-exporter)

**What's missing in Reliaburger today**

- [reliaburger/reliaburger issue #641](https://github.com/reliaburger/reliaburger/issues/641)
- [reliaburger/reliaburger issue #359](https://github.com/reliaburger/reliaburger/issues/359)

**What to build, what to host, what to leave**

- [cncf-tags/container-device-interface](https://github.com/cncf-tags/container-device-interface)
- [hashicorp/nomad issue #24990](https://github.com/hashicorp/nomad/issues/24990)
- [kubernetes-sigs/gateway-api-inference-extension: README.md](https://github.com/kubernetes-sigs/gateway-api-inference-extension/blob/main/docs/proposals/004-endpoint-picker-protocol/README.md)

**A revised release plan**

- [Cldfire/nvml-wrapper](https://github.com/Cldfire/nvml-wrapper)
- [cncf-tags/container-device-interface-rs](https://github.com/cncf-tags/container-device-interface-rs)
- [docs.nvidia.com: cdi-support](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/1.19.1/cdi-support.html)
- [docs.nvidia.com: analyzing-xid-catalog](https://docs.nvidia.com/deploy/xid-errors/analyzing-xid-catalog.html)
- [docs.nvidia.com: dcgmi-diag](https://docs.nvidia.com/datacenter/dcgm/latest/reference/command-line-reference/dcgmi/dcgmi-diag.html)
- [docs.pytorch.org: run](https://docs.pytorch.org/docs/main/elastic/run.html)
- [docs.vllm.ai: parallelism_scaling](https://docs.vllm.ai/en/stable/serving/parallelism_scaling/)
- [reliaburger/reliaburger issue #677](https://github.com/reliaburger/reliaburger/issues/677)

Not verified, so don't quote them as fact: the count of components in the
Kubernetes inference stack (our own tally, not a published one); vendor
benchmark claims (llm-d's routing and disaggregation figures, NVIDIA's
Dynamo projection), which are each project's own measurements; Dragonfly's
origin-traffic figure, from a CNCF post by its maintainers; the ClusterMAX
3.0 criteria, read from a summary because the primary page wasn't reachable;
and the size of the 8–256 GPU segment, for which we found no credible
figure.
