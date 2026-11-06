# Agent-friendly onboarding: what others do, and what Reliaburger could add

Research scan, 4 November 2026. Input for the 0.2.7 "built for humans and their agents" release.

Scope: ideas **not** already in the 0.2.7 list (`relish agent …`, JSON everywhere, read-only `relish mcp`, the generated guide / `llms.txt` / SKILL.md / plugin marketplace / `AGENTS.md`, the planted-bug eval, the website `/agents` page, the two install tabs, the second recording, the improvement loop). The earlier research in `docs/plans/2026-10-18-research-ai-sre.md` already covers scoped agent tokens, untrusted-content envelopes, mutation proposals and a remote MCP endpoint, so this scan doesn't repeat those.

Every claim about another project links to a page I opened. Where I only saw a search snippet or couldn't confirm something, I say so.

---

## 1. The landscape in short

**Now standard (most serious developer platforms have these):**

- **A hosted or local MCP server, with a narrow default.** Supabase, Vercel, Neon, Stripe and Cloudflare all run remote MCP servers with OAuth ([Supabase](https://supabase.com/docs/guides/getting-started/mcp), [Vercel](https://vercel.com/docs/mcp/vercel-mcp), [Neon](https://neon.com/docs/ai/neon-mcp-server), [Stripe](https://docs.stripe.com/building-with-llms), [Cloudflare](https://developers.cloudflare.com/agents/model-context-protocol/mcp-servers-for-cloudflare/)). The careful ones add read-only modes, scoping to one project, and switches that turn tool groups on or off. Supabase has `read_only=true`, `project_ref` and `features`. Neon has `readonly=true`, `projectId` and `category`. Grafana has `--disable-write` and per-category flags ([mcp-grafana](https://github.com/grafana/mcp-grafana)).
- **Markdown copies of the docs.** You add `.md` to a URL and get the page as Markdown. Stripe and Temporal both do this ([Stripe](https://docs.stripe.com/building-with-llms), [Temporal](https://docs.temporal.io/with-ai.md)). Pages also carry "Copy page" and "Open in Claude / ChatGPT" buttons. Mintlify ships these buttons as a config option, which is why they turn up everywhere ([Mintlify contextual menu](https://www.mintlify.com/docs/ai/contextual-menu)). The [llms.txt proposal](https://llmstxt.org/) now specifies the `.md` sibling and the `rel="alternate" type="text/markdown"` link as well.
- **Skills and plugins alongside MCP.** Stripe, Neon and Temporal each ship one command that detects which agents you use and installs MCP config plus skills: `stripe agent setup`, `npx neon@latest init`, and Temporal's plugin bundle ([Stripe skills](https://docs.stripe.com/skills.md), [Neon](https://neon.com/docs/ai/neon-mcp-server), [Temporal](https://docs.temporal.io/with-ai.md)). Vercel ships `npx add-mcp https://mcp.vercel.com`, which configures every detected client ([Vercel](https://vercel.com/docs/mcp/vercel-mcp)).
- **One-click install links.** Cursor (`cursor://…/mcp/install?name=…&config=<base64>`), VS Code (`vscode:mcp/install?…`) and Goose (`goose://extension?…`) all accept install links. Each one asks the user to confirm ([Cursor install links](https://cursor.com/docs/context/mcp/install-links), examples on [Vercel's page](https://vercel.com/docs/mcp/vercel-mcp) and in [kubernetes-mcp-server](https://github.com/containers/kubernetes-mcp-server)).
- **AGENTS.md.** It is now stewarded by the Linux Foundation's Agentic AI Foundation. The site claims 60,000+ repositories and 20+ supporting tools ([agents.md](https://agents.md/)).
- **Written AI contribution policies.** RedMonk tracks dozens of them ([RedMonk](https://redmonk.com/kholterhoff/2026/02/26/generative-ai-policy-landscape-in-open-source/)). The common shape: disclose AI use, and a human stays accountable. The Linux kernel ([coding-assistants](https://docs.kernel.org/next/process/coding-assistants.html)), Kubernetes ([blog, June 2026](https://kubernetes.io/blog/2026/06/26/open-source-maintainership-in-the-age-of-ai/)) and Ghostty ([AI_POLICY.md](https://github.com/ghostty-org/ghostty/blob/main/AI_POLICY.md)) all follow it.

**Emerging (worth doing early, cheap to change later):**

- **A discovery index for skills** at `/.well-known/agent-skills/index.json`, with a sha256 digest per skill. It comes from a Cloudflare RFC, and the `npx skills` installer reads it ([Evil Martians write-up](https://evilmartians.com/chronicles/publishing-agent-skills-discovery-index), [Stripe's index](https://docs.stripe.com/skills.md)). Cloudflare's "Agent Readiness" score checks for it, along with Markdown content negotiation, an MCP Server Card and OAuth discovery ([Cloudflare blog](https://blog.cloudflare.com/agent-readiness/)).
- **Feedback tools inside the MCP server.** Vercel MCP has a `submit_feedback` tool. It only sends when the user asks. Every Vercel docs page also ends with a line telling agents where to report wrong information ([changelog](https://vercel.com/changelog/send-feedback-from-vercel-mcp), [Vercel MCP page](https://vercel.com/docs/mcp/vercel-mcp)).
- **The official MCP Registry.** It holds metadata only and verifies who owns a namespace through GitHub or DNS. It is still in preview ([quickstart](https://modelcontextprotocol.io/registry/quickstart)).
- **GitHub tools against PR floods.** You can disable PRs or restrict them to collaborators (Feb 2026), cap open PRs from users without write access and keep a bypass list (Jun 2026), and exempt contributors by account age or merged-PR history (Oct 2026) ([Feb](https://github.blog/changelog/2026-02-13-new-repository-settings-for-configuring-pull-request-access/), [Jun](https://github.blog/changelog/2026-06-17-limit-open-pull-requests-for-users-without-write-access/), [Oct](https://github.blog/changelog/2026-10-28-pull-request-limits-exempt-contributors-by-age-or-history)).
- **Agent-run triage of issues,** heavily sandboxed. Anthropic's own `claude-code` repo runs a dedupe workflow. It uses an egress firewall, no web or edit tools, and a cap of one comment per run ([workflow](https://github.com/anthropics/claude-code/blob/main/.github/workflows/claude-dedupe-issues.yml)).

**The backlash, which shapes everything above.** curl ended its bug bounty on 31 January 2026. Its confirmed-vulnerability rate fell from above 15% to below 5% from 2025, and the maintainers blamed AI slop and the cash incentive ([Daniel Stenberg](https://daniel.haxx.se/blog/2026/01/26/the-end-of-the-curl-bug-bounty/)). Ghostty keeps a public denouncement list for slop contributors ([AI_POLICY.md](https://github.com/ghostty-org/ghostty/blob/main/AI_POLICY.md)). GitHub itself considered a pull-request "kill switch" ([The Register](https://www.theregister.com/2026/02/03/github_kill_switch_pull_requests_ai/)). The lesson for Reliaburger's improvement loop: make the *verified* report the easy path, and make the unverified one hard to send.

**A caution on llms.txt.** It is widely published, but whether AI systems actually read it is unproven. Cloudflare's readiness score doesn't check it by default ([Cloudflare blog](https://blog.cloudflare.com/agent-readiness/)). I saw several third-party adoption studies only as search snippets and didn't verify them. Treat llms.txt as cheap hygiene, not a growth lever.

---

## 2. Ranked suggestions

Effort: S = a day or two, M = about a week, L = several weeks.

### 1. An "agent report contract": issue forms that demand evidence (S)

**Idea.** Add `.github/ISSUE_TEMPLATE/*.yml` issue forms. The bug form requires: version (`relish --version` output), the redacted `relish agent` draft ID, steps, a reproducer, and what was *not* tested. It also has an AI-disclosure dropdown. Add a separate "agent-drafted bug" form whose fields match the JSON of the planned issue draft one-to-one.

**Who does it.** The kernel's assistant workflow makes the reproducer, fix, build and test mandatory. It says to "say so explicitly" when something couldn't be built or reproduced, and the assistant "must never send anything itself" ([kernel](https://docs.kernel.org/next/process/coding-assistants.html)). GitHub issue forms support required fields, dropdowns, checkboxes and `render: shell` for logs ([GitHub docs](https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests/syntax-for-issue-forms)).

**Why it fits.** The repo has no `.github/ISSUE_TEMPLATE/` today; `.github/` holds only `dependabot.yml` and `workflows`. The planned redacted issue draft can then produce exactly the form's fields, and the form becomes the schema the draft is tested against.

**Risk.** Low. Forms add friction for a human with a one-line bug, so keep a short "question" form and point to Discussions.

### 2. A written AI-assistance policy in CONTRIBUTING.md, mirrored in AGENTS.md (S)

**Idea.** One section covering:

- Disclose the tool and the extent of AI help in the PR body.
- A human must be able to explain every line.
- Agents never open issues or PRs on their own.
- Every bug-fix PR carries a failing-then-passing test.
- Reports with no reproducer can be closed without discussion.

**Who does it.** Kubernetes: disclose in the PR, the human is fully accountable, AI must not answer review comments, and PRs whose author can't explain them may be closed ([Kubernetes blog](https://kubernetes.io/blog/2026/06/26/open-source-maintainership-in-the-age-of-ai/)). Ghostty: disclose all AI use, the contributor must understand the change, and only text and code may be AI output ([Ghostty](https://github.com/ghostty-org/ghostty/blob/main/AI_POLICY.md)).

**Why it fits.** `CONTRIBUTING.md` today only says that small fixes can go straight to a PR. The "tests first" rule in `CLAUDE.md` is already the strongest quality gate an agent PR can face, so state it as the entry ticket.

**Decision needed.** Kubernetes *prohibits* AI co-author and `assisted-by` trailers. The kernel forbids AI `Signed-off-by` lines. Reliaburger's own commits carry a `Co-Authored-By: Claude …` trailer. Pick one rule and apply it to both maintainers and contributors. Ghostty's maintainer exemption is the kind of double standard outsiders notice.

**Risk.** Reputational if the policy reads as anti-AI in a release billed "for humans and their agents". Frame it as pro-evidence.

### 3. Turn on GitHub's inflow controls and private vulnerability reporting before launch (S)

**Idea.**

- Cap open PRs from users without write access (for example 2).
- Exempt accounts with merged PRs.
- Count drafts towards the cap.
- Enable "Report a vulnerability".
- Write a `SECURITY.md` that says there is no bounty.

**Who does it.** GitHub's PR limits, bypass list and exemptions ([Jun](https://github.blog/changelog/2026-06-17-limit-open-pull-requests-for-users-without-write-access/), [Oct](https://github.blog/changelog/2026-10-28-pull-request-limits-exempt-contributors-by-age-or-history)). Private vulnerability reporting ([GitHub docs](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/configuring-private-vulnerability-reporting-for-a-repository)). curl now routes security reports through GitHub private reporting or email, with no money at any severity ([curl](https://daniel.haxx.se/blog/2026/01/26/the-end-of-the-curl-bug-bounty/)).

**Why it fits.** The 0.2.7 marketing deliberately invites agent traffic. Set the valves first. This is repo settings only; no code.

**Risk.** It may deter a genuine first-time contributor. Mitigate with a clear message in `CONTRIBUTING.md` about how to get onto the bypass list.

### 4. `relish agent setup`: one command that wires every detected agent (S–M)

**Idea.** Detect Claude Code, Codex, Cursor, VS Code, Goose and Gemini CLI. For each, write the `relish mcp` stdio config and install the skill or plugin, behind a single yes/no prompt. Add `--print` to show the config instead of writing it.

**Who does it.** `stripe agent setup` "automatically detects which agents you use" ([Stripe](https://docs.stripe.com/skills.md)). `npx neon@latest init` writes MCP config, skills and a linked project ([Neon](https://neon.com/docs/ai/neon-mcp-server)). `npx add-mcp` configures all detected clients ([Vercel](https://vercel.com/docs/mcp/vercel-mcp)).

**Why it fits.** The pasted "With your agent" prompt then shrinks to "install, run `relish agent setup`, run the tour". A human gets the same thing without an agent.

**Risk.** Writing into other tools' config files is invasive. Show a diff, ask once, and make it reversible (`relish agent setup --remove`).

### 5. Install buttons on `/agents` and in the README (S)

**Idea.** "Add to Cursor", "Add to VS Code" and "Add to Goose" badges that install `relish mcp` (stdio, command `relish`, args `["mcp"]`). Add copy-paste lines for `claude mcp add` and `codex mcp add`.

**Who does it.** Cursor's documented link format ([Cursor](https://cursor.com/docs/context/mcp/install-links)). Vercel shows Cursor, VS Code and Goose buttons on one page ([Vercel](https://vercel.com/docs/mcp/vercel-mcp)). kubernetes-mcp-server ships VS Code and Cursor buttons ([repo](https://github.com/containers/kubernetes-mcp-server)).

**Why it fits.** The buttons are static links that can go straight into `docs/website/` (served by GitHub Pages via `static.yml`).

**Risk.** A button that installs a command that isn't on PATH yet fails silently. Pair each button with "install Relish first".

### 6. `.md` siblings, `rel="alternate"` links and "Copy as Markdown / Open in Claude" on every manual and website page (S)

**Idea.** Generate `page.md` next to every page from the same source that builds the manual and website. Add `<link rel="alternate" type="text/markdown">` and a `rel="describedby"` link to `llms.txt`. Add a small per-page menu: copy Markdown, open in Claude, open in ChatGPT.

**Who does it.** The [llms.txt spec](https://llmstxt.org/) describes `.md` siblings and both link relations. [Stripe](https://docs.stripe.com/building-with-llms) and [Temporal](https://docs.temporal.io/with-ai.md) serve `.md` for every page; Temporal has "Copy for LLM / Open in ChatGPT/Claude". The [Mintlify menu](https://www.mintlify.com/docs/ai/contextual-menu) is the de facto pattern.

**Why it fits.** The manual is already Markdown compiled into `relish`, so the `.md` copies are nearly free.

**Constraint.** Cloudflare's `Accept: text/markdown` negotiation ([docs](https://developers.cloudflare.com/fundamentals/reference/markdown-for-agents/)) needs an edge in front. That is my inference: GitHub Pages can't vary responses on the `Accept` header. So rely on `.md` siblings.

**Risk.** None worth noting.

### 7. Publish a skills discovery index and list `relish mcp` in the MCP Registry (S)

**Idea.**

- Serve `https://reliaburger.com/.well-known/agent-skills/index.json`, with the generated SKILL.md and its sha256 digest, rebuilt per release.
- Publish a `server.json` under `io.github.reliaburger/relish` (or DNS-verified `com.reliaburger/…`) to the official MCP Registry.

**Who does it.** Stripe's index ([Stripe](https://docs.stripe.com/skills.md)). The RFC format and the rule that installers must verify the digest ([Evil Martians](https://evilmartians.com/chronicles/publishing-agent-skills-discovery-index)). Cloudflare scores sites on it ([Cloudflare](https://blog.cloudflare.com/agent-readiness/)). The MCP Registry flow with `mcp-publisher` and GitHub or DNS auth ([MCP Registry](https://modelcontextprotocol.io/registry/quickstart)).

**Why it fits.** The digest matches the project's checksum-pinning habit (`install.sh` already pins CLI checksums).

**Unverified.** The registry quickstart covers npm packages. I didn't open its package-types page, so I can't say whether a plain GitHub-release binary qualifies or whether you'd need an OCI image or MCPB bundle.

**Risk.** The registry is in preview and "data resets may occur".

### 8. `relish bugreport`: a marker-based, local, reviewable diagnostic bundle (M)

**Idea.** `relish bugreport` writes a redacted bundle and prints an ID like `RB-<cluster-hash>-<timestamp>-<rand>`. The bundle holds version, compatibility versions, node list, recent events, `relish wtf` output and a log excerpt. `--record` drops a start marker, waits while you reproduce, then drops an end marker. The planned agent issue draft attaches the ID and a summary, never raw logs.

**Who does it.** `tailscale bugreport` produces exactly this kind of identifier, with a `--record` before/after window, records the state into local logs you can inspect, and publishes the code ([Tailscale](https://tailscale.com/kb/1227/bug-report)).

**Why it fits.** `relish wtf` already diagnoses. This adds the before/after window and a stable fingerprint, which is the raw material for deduplication (section 3).

**Risk.** Leaking secrets or hostnames. Reuse the planned redaction, show the bundle before anything leaves the machine, and keep it local by default. Reliaburger hosts nothing, so there is no upload.

### 9. Read-only by default, with toolset switches and namespace scoping on `relish mcp` (S)

**Idea.** The planned MCP is read-only. Go one step further:

- `--toolsets diagnose,logs,manual,source` so agents load fewer tools.
- `--namespace` / `--app` scoping.
- `--no-logs` for users who don't want workload logs in an LLM context.

**Who does it.** [Grafana](https://github.com/grafana/mcp-grafana) (`--enabled-tools`, `--disable-<category>`, some categories off by default). [Supabase](https://supabase.com/docs/guides/getting-started/mcp) (`features`, `project_ref`; Storage off by default). [Neon](https://neon.com/docs/ai/neon-mcp-server) (`category`, `projectId`). [kubernetes-mcp-server](https://github.com/containers/kubernetes-mcp-server) (toolsets, `denied_resources` such as Secrets).

**Why it fits.** Workload logs are the untrusted content in the "lethal trifecta" ([Simon Willison](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/)). Letting a user drop them shrinks the attack surface.

**Risk.** More flags to test. Keep the defaults narrow and snapshot-test the tool list per toolset (insta).

### 10. A "draft_report" MCP tool and a docs footer for agents (S)

**Idea.**

- Expose the planned redacted issue draft as an MCP tool that *returns* a draft and a pre-filled GitHub issue-form URL. It never posts.
- End every manual page and `relish agent guide` with one line: "If this page is wrong, run `relish agent report --docs <page>`."

**Who does it.** Vercel MCP `submit_feedback`, sent only when the user asks ([changelog](https://vercel.com/changelog/send-feedback-from-vercel-mcp)). Vercel's docs footer pointing agents to `feedback.md` ([Vercel MCP page](https://vercel.com/docs/mcp/vercel-mcp)).

**Why it fits.** It closes the loop from inside the agent's own tool surface without giving the MCP server a write path.

**Risk.** Vercel posts server-side. Reliaburger shouldn't, because that would make `relish mcp` a write tool with internet egress, which completes the trifecta.

### 11. Release provenance a stranger can check in one command (M)

**Idea.** Add GitHub artifact attestations (`actions/attest`, verified with `gh attestation verify <file> -R reliaburger/reliaburger`). Optionally add cosign keyless signatures for the release tarballs. Print both verification commands on the install page and in `install.sh --help`. Finish macOS Developer ID signing and notarisation.

**Who does it.** GitHub artifact attestations ([GitHub docs](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations/using-artifact-attestations-to-establish-provenance-for-builds)). cosign `verify-blob` with `--certificate-identity` and `--certificate-oidc-issuer` ([Sigstore](https://docs.sigstore.dev/cosign/verifying/verify/)).

**Why it fits.**

- Releases are already Ed25519-signed with a project key (`docs/releasing.md`, "Signing identity"), and the installer pins checksums.
- What's missing is verification *by a third party who doesn't trust the website*.
- `docs/releasing.md` says macOS builds "are not yet Developer ID signed or notarised".
- An agent asked to install software benefits most: it can run one verification command and report the result.
- Keyless cosign was already planned for images (`docs/plans/2026-10-05-plan-f03c-keyless-cosign.md`).

**Risk.** Apple developer account cost and secret handling, both maintainer-owned.

### 12. Homebrew tap now; Nix flake next; apt and rpm later (S–M)

**Idea.** A `reliaburger/homebrew-tap` with a formula for `relish` (and `bun` on Linux), updated by the release workflow. Then a `flake.nix`.

**Who does it.** Homebrew's tap guide: `brew install user/repo/formula` taps the repo and trusts only that formula, and bottles are built by Actions ([Homebrew](https://docs.brew.sh/How-to-Create-and-Maintain-a-Tap)). k8sgpt ships brew, RPM, DEB and APK ([k8sgpt](https://github.com/k8sgpt-ai/k8sgpt)). kubectl-ai ships an install script, krew and Nix ([kubectl-ai](https://github.com/GoogleCloudPlatform/kubectl-ai)).

**Why it fits.** Agents and humans who won't pipe `curl | sh` reach for brew first. It also gives you `brew upgrade` for free alongside `relish upgrade`.

**Risk.** Two upgrade paths can fight. The formula must defer cluster upgrades to `relish upgrade` and only manage the CLI binary.

### 13. A browser playground for "Kick the tyres" (M)

**Idea.** A Killercoda scenario that runs `install.sh --laptop`, or the Linux equivalent, plus the tour, with `verify` scripts per step. Later, an iximiuz Labs playground on the multi-VM "MiniLAN" for a three-node cluster.

**Who does it.** Killercoda builds scenarios from a GitHub repo (JSON, Markdown and Bash; every push updates them). It offers an Ubuntu 24.04 image with Docker and Podman in 2 or 4 GB, with a 1-hour limit on the free tier ([Killercoda](https://killercoda.com/creators)). iximiuz Labs offers K3s, kubeadm, multi-VM "MiniLAN" and custom FlexBox playgrounds ([iximiuz playgrounds](https://labs.iximiuz.com/playgrounds)). I didn't verify iximiuz's authoring terms; a search snippet suggested authoring needs a paid plan.

**Why it fits.** It is the only way to reach "no install at all". It also gives agents a disposable, public environment to point a human at.

**Unverified.** Whether these VMs allow cgroup v2 delegation, eBPF and runc as Reliaburger needs. Test with ProcessGrill (`--runtime process`) first.

**Risk.** A third-party dependency; public URLs mean no secrets in the scenario.

### 14. A cloud-init snippet as the "any cloud" path (S)

**Idea.** Publish `reliaburger.com/cloud-init.yaml`. It uses `write_files` for a node config, `runcmd` to run the pinned, checksum-verified installer and join, and `ssh_authorized_keys`. Users or their agents paste it into any provider's "user data" box.

**Who does it.** cloud-init supports `write_files`, `runcmd`, `packages`, `users` and `ssh_authorized_keys` ([cloud-init examples](https://docs.cloud-init.io/en/latest/reference/examples.html)). Hetzner builds its own one-click apps with Packer plus cloud-init, but currently accepts no third-party app suggestions ([hetznercloud/apps](https://github.com/hetznercloud/apps)). A Hetzner marketplace listing isn't available, so cloud-init is the portable route.

**Why it fits.** It gives the agent install tab a provider-neutral cloud path with no provider SDK in Reliaburger. The "explicit yes for anything paid" rule still applies to creating the VM.

**Risk.** Join tokens in user data are readable from the metadata service. Use short-lived join tokens and say so.

### 15. A devcontainer and `copilot-setup-steps.yml` so cloud agents can build and test patches (S–M)

**Idea.**

- `.devcontainer/devcontainer.json` with the pinned Rust toolchain, nextest, a C compiler and static libc.
- `.github/workflows/copilot-setup-steps.yml` with a single `copilot-setup-steps` job that warms the cargo cache.
- Document that only the portable suite (`make ci`) is expected to pass there.

**Who does it.** Dev Container spec ([containers.dev](https://containers.dev/)). Copilot's cloud agent reads `copilot-setup-steps.yml`, the job must have that exact name, and it is capped at 59 minutes ([GitHub docs](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/coding-agent/customize-the-agent-environment)).

**Why it fits.** It complements the planned "patch build/test/try on a throwaway cluster". A contributor's agent in a cloud sandbox gets a working `make ci` without guessing toolchain details from `CONTRIBUTING.md`.

**Risk.** Gated suites (Linux, eBPF, Apple) won't run there. Say so, or agents will claim "all tests pass".

### 16. Steer the third-party indexes that agents already read (S)

**Idea.** Add `.devin/wiki.json` so DeepWiki builds pages around the book chapters and design docs instead of guessing clusters. Check and claim the Context7 entry, adding `context7.json` if one is needed.

**Who does it.** DeepWiki's steering file, with `repo_notes` and `pages` ([Devin docs](https://docs.devin.ai/work-with-devin/deepwiki)). Context7 serves "up-to-date, version-specific documentation" to agents via CLI or MCP and keeps a `context7.json` in its own repo ([Context7](https://github.com/upstash/context7)). I didn't open Context7's "Adding Libraries" guide.

**Why it fits.** Agents asked "what is Reliaburger?" may hit these indexes before reliaburger.com. The book (`docs/book/`) is a ready-made table of contents.

**Risk.** A stale third-party summary. Low cost to check each release.

### 17. Self-description and stable exit-code classes for `relish` (S–M)

**Idea.** Add `relish commands --json`, a list of every command with flags and a JSON Schema of its output. Add documented exit-code classes (usage, auth, not found, conflict, retryable). Make sure every mutating command takes `--dry-run`, and that `--yes` is the only way past a prompt.

**Who does it.** Anthropic's tool-design guidance: consolidate operations, namespace them, return high-signal context, write actionable errors, and refine with evals ([Anthropic engineering](https://www.anthropic.com/engineering/writing-tools-for-agents)). Several community "agent-native CLI" checklists recommend schema introspection and classified exit codes. I saw these only as search snippets, so treat them as opinion.

**Why it fits.** "JSON for every command" is planned. A schema per command lets the eval and `relish mcp` be generated from one source and snapshot-tested.

**Risk.** Schema churn before 1.0 is fine under the no-compatibility rule, but say so in the schema.

### 18. Publish the agent eval as a public scorecard others can run (S, once the eval exists)

**Idea.** Publish the eval results per release (task, agent, model, pass/fail, tokens) on `/agents`. Keep part of the tasks held out. Let users run `make agent-eval AGENT=…` against their own agent.

**Who does it.** Anthropic recommends realistic, verifiable multi-step tasks and held-out sets to avoid overfitting ([Anthropic engineering](https://www.anthropic.com/engineering/writing-tools-for-agents)). HolmesGPT ships a benchmark runner in its repo ([HolmesGPT](https://github.com/robusta-dev/holmesgpt)), though its README doesn't describe the benchmark in detail.

**Why it fits.** It turns "agent-friendly" from a claim into a number that moves between releases. It also fits the qualification-record habit (`docs/qualification/`).

**Risk.** Model results drift. Date every run.

### 19. Mutation testing as a source of agent-sized test tasks (M)

**Idea.** Run `cargo mutants --in-diff` on PRs, which reports only mutants in changed code. Also run a periodic full run. Turn surviving mutants into "missed-mutant" issues: each one is a precise, verifiable "write a test that kills this" task.

**Who does it.** cargo-mutants finds places "where bugs could be inserted without causing any tests to fail" ([mutants.rs](https://mutants.rs/)). `--in-diff` limits a run to the changed regions ([in-diff](https://mutants.rs/in-diff.html)).

**Why it fits.** Tests-first is non-negotiable here, and a missed mutant is the ideal first contribution for an agent: the success check is mechanical. See section 3.

**Risk.** CI time. Keep the full run nightly or weekly, not per PR.

### 20. Opt-in, local-first usage counters, if ever (L; defer)

**Idea.** If the project ever wants usage data, copy Go:

- Modes: `local` (the default) collects on disk and never uploads; `on` uploads; `off` collects nothing.
- Only counters named in a public config are uploaded.
- New counters go through public proposals.
- Aggregates are published.

**Who does it.** Go toolchain telemetry ([go.dev](https://go.dev/doc/telemetry)). Homebrew is the opt-*out* contrast: anonymous, no user ID or IP field, a first-run notice, 365-day retention, public aggregates ([Homebrew analytics](https://docs.brew.sh/Analytics)).

**Why it fits.** It would answer "which commands fail most" without reading issues. It is ranked last because the planned improvement loop already gives qualitative data. An orchestrator phoning home also draws suspicion from the self-hosting crowd Reliaburger courts.

**Risk.** High reputational cost if it is ever on by default.

---

## 3. Getting feedback from users and agents without drowning

The goal: more *verified* reports, fewer total reports.

**Quality gates, cheapest first.**

1. **Make evidence the default output.** The agent issue draft should fail closed. If there is no reproducer, it says "unverified" in the title and the form's "what was not tested" field. This is the kernel's rule ([kernel](https://docs.kernel.org/next/process/coding-assistants.html)).
2. **A test is the entry ticket for a bug-fix PR.** CI checks that a PR labelled `bug` adds or changes a test that fails on `main`. That is easy to state and hard to fake, and it matches `CLAUDE.md`.
3. **No money, no leaderboard.** curl's data shows the incentive drives slop ([curl](https://daniel.haxx.se/blog/2026/01/26/the-end-of-the-curl-bug-bounty/)). Credit people by name in release notes instead.
4. **Volume valves.** PR caps with exemptions (suggestion 3). Use Discussions for ideas and keep issues for bugs with evidence.

**Templates.** One issue form per kind: bug, agent-drafted bug, docs error, test request, idea → Discussions. The agent-drafted form mirrors the draft's JSON fields exactly, so `relish agent` can produce a pre-filled form URL. Issue forms support required fields and `render: shell` blocks for logs ([GitHub docs](https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests/syntax-for-issue-forms)).

**Dedupe before posting, not after.**

- Give each failure a fingerprint: Reliaburger version, error enum variant, and a hash of the top frames or the `relish wtf` finding ID. Go's stack counters are the model: function names and line numbers only, no user data ([go.dev](https://go.dev/doc/telemetry)).
- Put the fingerprint in the issue title or a hidden field.
- Before drafting, `relish agent` searches existing issues for it, a read-only call the user's own agent can make with `gh`. If there is a match, it suggests "add your evidence to #N" rather than a new issue.
- The draft states whether the fix is already on `main` or released, using the planned upgrade-check.

**Triage, if you automate it.** Copy the claude-code dedupe workflow's constraints ([workflow](https://github.com/anthropics/claude-code/blob/main/.github/workflows/claude-dedupe-issues.yml)):

- an egress-filtered runner
- no web or edit tools
- at most one comment per run
- OIDC-minted short-lived credentials
- read-only repo contents

Issue bodies are attacker-controlled. Invariant Labs showed a malicious issue hijacking an agent through the GitHub MCP server to leak private-repo data ([Invariant Labs](https://invariantlabs.ai/blog/mcp-github-vulnerability)). The triage bot must have no access to anything private and no way to send data out except the one labelled comment.

**Labels that route work.** `agent-drafted`, `has-reproducer`, `patch-tested`, `unverified`, `missed-mutant`, `flake`, `docs-wrong`. The maintainer reads `has-reproducer` and `patch-tested` first. `unverified` gets a canned reply after a week.

**Agents suggesting better tests.**

- **Missed mutants** (suggestion 19). Each surviving mutant becomes a self-checking task.
- **A "test request" issue form.** It says what behaviour is untested, where, and the test name as a behaviour sentence (the `CLAUDE.md` naming rule). It is accepted only with the failing test attached, or a clear statement that the agent couldn't write one.
- **Flake register entries.** `docs/flakes.md` already lists known flakes. An agent can be asked to reproduce one under `--retries 0` loops, which is a well-bounded task with a clear success signal.
- **Proptest regressions.** `proptest-regressions/` files are concrete failing seeds. Ask agents to turn a found seed into a named unit test.
- **Coverage gaps below the floor.** Hand agents the uncovered lines in a changed file. The coverage floor in the `Makefile` is the check.
- **A "test-only PR" fast lane.** PRs that touch only tests, pass CI and kill a mutant or cover new lines get a lighter review. The kernel and Kubernetes both put the burden of proof on the submitter; tests are the cheapest proof to review.

---

## 4. Things to avoid

- **A cash bug bounty.** curl's confirmed rate fell below 5%, and it ended the programme to "remove the incentive" ([curl](https://daniel.haxx.se/blog/2026/01/26/the-end-of-the-curl-bug-bounty/)).
- **Agents posting issues or PRs themselves.** The kernel says the assistant "must never send anything itself" ([kernel](https://docs.kernel.org/next/process/coding-assistants.html)). Keep the human-posts rule from the plan, including in the MCP server (suggestion 10).
- **Combining private data, untrusted content and outbound writes in one tool surface.** That is the lethal trifecta ([Willison](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/)). `relish mcp` reads workload logs (untrusted) and cluster state (private), so it must have no write or egress tools. Supabase says the same about prompt injection via support tickets and recommends read-only for unattended use ([Supabase](https://supabase.com/docs/guides/getting-started/mcp)).
- **Recommending "Always allow" for tool calls anywhere in the docs.** The Invariant exploit ran with "Always Allow" set ([Invariant](https://invariantlabs.ai/blog/mcp-github-vulnerability)). Vercel's docs say to enable human confirmation ([Vercel](https://vercel.com/docs/mcp/vercel-mcp)).
- **An unauthenticated HTTP MCP listener.** kubectl-ai warns its `streamable-http` endpoint "is unauthenticated by default" ([kubectl-ai](https://github.com/GoogleCloudPlatform/kubectl-ai)). Keep `relish mcp` on stdio until auth exists.
- **Public shaming lists.** Ghostty's denouncement list works for Ghostty ([Ghostty](https://github.com/ghostty-org/ghostty/blob/main/AI_POLICY.md)), but it clashes with a release that invites agents. Close and lock quietly instead.
- **Telemetry that is on by default.** See suggestion 20. Go's local-first default is the only model that fits.
- **Selling llms.txt as discovery.** Cloudflare's readiness score doesn't check it by default ([Cloudflare](https://blog.cloudflare.com/agent-readiness/)). Ship it, but don't claim it brings users.
- **Telling agents that "tests pass" when only the portable suite ran.** Gated suites need Linux, eBPF or Apple hardware. Every agent-facing doc (AGENTS.md, the skill, the devcontainer) should say which suite ran.
- **Relying on a gated or invite-only gateway.** Docker's MCP Gateway, as part of Docker AI Governance, is invite-only ([Docker](https://docs.docker.com/ai/mcp-catalog-and-toolkit/)). Don't make it the install path.
- **Cloud marketplace images and a Terraform provider before there is demand.** Hetzner won't take new third-party apps ([hetznercloud/apps](https://github.com/hetznercloud/apps)). Images also need patching every release. cloud-init (suggestion 14) covers most of the need for a fraction of the upkeep.
- **Long `llms-full.txt` dumps of the whole book.** Temporal notes its own `llms-full.txt` is large and that the index or single pages are usually better ([Temporal](https://docs.temporal.io/with-ai.md)). Prefer the index plus `.md` siblings.

---

## 5. Sources (pages opened)

- llms.txt proposal — https://llmstxt.org/
- AGENTS.md — https://agents.md/
- Cursor MCP install links — https://cursor.com/docs/context/mcp/install-links
- curl: the end of the bug bounty — https://daniel.haxx.se/blog/2026/01/26/the-end-of-the-curl-bug-bounty/
- Ghostty AI policy — https://github.com/ghostty-org/ghostty/blob/main/AI_POLICY.md
- Claude Code plugin marketplaces — https://code.claude.com/docs/en/plugin-marketplaces
- Agent Skills — https://agentskills.io/
- Supabase MCP — https://supabase.com/docs/guides/getting-started/mcp
- Vercel MCP — https://vercel.com/docs/mcp/vercel-mcp
- Vercel MCP feedback tool — https://vercel.com/changelog/send-feedback-from-vercel-mcp
- Stripe: building with LLMs — https://docs.stripe.com/building-with-llms
- Stripe agent skills — https://docs.stripe.com/skills.md
- Cloudflare MCP servers — https://developers.cloudflare.com/agents/model-context-protocol/mcp-servers-for-cloudflare/
- Cloudflare Agent Readiness score — https://blog.cloudflare.com/agent-readiness/
- Cloudflare Markdown for Agents — https://developers.cloudflare.com/fundamentals/reference/markdown-for-agents/
- Neon MCP server — https://neon.com/docs/ai/neon-mcp-server
- Fly.io `fly mcp` — https://docs.fly.io/flyctl/mcp
- Docker MCP Catalog and Toolkit — https://docs.docker.com/ai/mcp-catalog-and-toolkit/
- HashiCorp Terraform MCP server — https://developer.hashicorp.com/terraform/mcp-server
- Grafana MCP server — https://github.com/grafana/mcp-grafana
- Temporal: Develop with AI — https://docs.temporal.io/with-ai.md
- kubectl-ai — https://github.com/GoogleCloudPlatform/kubectl-ai
- HolmesGPT — https://github.com/robusta-dev/holmesgpt
- kubernetes-mcp-server — https://github.com/containers/kubernetes-mcp-server
- k8sgpt — https://github.com/k8sgpt-ai/k8sgpt
- Linux kernel: AI coding assistants — https://docs.kernel.org/next/process/coding-assistants.html
- Kubernetes: open source maintainership in the age of AI — https://kubernetes.io/blog/2026/06/26/open-source-maintainership-in-the-age-of-ai/
- RedMonk: generative AI policy landscape — https://redmonk.com/kholterhoff/2026/02/26/generative-ai-policy-landscape-in-open-source/
- The Register: GitHub PR kill switch — https://www.theregister.com/2026/02/03/github_kill_switch_pull_requests_ai/
- GitHub changelog, PR access settings (Feb 2026) — https://github.blog/changelog/2026-02-13-new-repository-settings-for-configuring-pull-request-access/
- GitHub changelog, PR limits (Jun 2026) — https://github.blog/changelog/2026-06-17-limit-open-pull-requests-for-users-without-write-access/
- GitHub changelog, PR limit exemptions (Oct 2026) — https://github.blog/changelog/2026-10-28-pull-request-limits-exempt-contributors-by-age-or-history
- GitHub issue forms syntax — https://docs.github.com/en/communities/using-templates-to-encourage-useful-issues-and-pull-requests/syntax-for-issue-forms
- GitHub private vulnerability reporting — https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/configuring-private-vulnerability-reporting-for-a-repository
- GitHub artifact attestations — https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations/using-artifact-attestations-to-establish-provenance-for-builds
- Copilot cloud agent environment — https://docs.github.com/en/copilot/how-tos/use-copilot-agents/coding-agent/customize-the-agent-environment
- claude-code workflows — https://github.com/anthropics/claude-code/tree/main/.github/workflows
- claude-code dedupe workflow — https://github.com/anthropics/claude-code/blob/main/.github/workflows/claude-dedupe-issues.yml
- Mintlify contextual menu — https://www.mintlify.com/docs/ai/contextual-menu
- Anthropic: writing tools for agents — https://www.anthropic.com/engineering/writing-tools-for-agents
- MCP Registry — https://registry.modelcontextprotocol.io/
- MCP Registry quickstart — https://modelcontextprotocol.io/registry/quickstart
- Evil Martians: agent skills discovery index — https://evilmartians.com/chronicles/publishing-agent-skills-discovery-index
- iximiuz Labs playgrounds — https://labs.iximiuz.com/playgrounds
- Killercoda creators — https://killercoda.com/creators
- Go telemetry — https://go.dev/doc/telemetry
- Homebrew analytics — https://docs.brew.sh/Analytics
- Homebrew taps — https://docs.brew.sh/How-to-Create-and-Maintain-a-Tap
- Dev Containers — https://containers.dev/
- Sigstore cosign verify — https://docs.sigstore.dev/cosign/verifying/verify/
- E2B docs — https://docs.e2b.dev/
- Railway templates — https://docs.railway.com/reference/templates
- Hetzner Cloud apps — https://github.com/hetznercloud/apps
- cloud-init examples — https://docs.cloud-init.io/en/latest/reference/examples.html
- Tailscale bug report — https://tailscale.com/kb/1227/bug-report
- Context7 — https://github.com/upstash/context7
- DeepWiki (Devin docs) — https://docs.devin.ai/work-with-devin/deepwiki
- Invariant Labs: GitHub MCP toxic agent flow — https://invariantlabs.ai/blog/mcp-github-vulnerability
- Simon Willison: the lethal trifecta — https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/
- cargo-mutants — https://mutants.rs/
- cargo-mutants `--in-diff` — https://mutants.rs/in-diff.html

**Opened but not used for claims.** E2B's docs page didn't describe its VM technology or pricing. The Railway page didn't cover deploy buttons or agent features. The Render deploy-button page, the DigitalOcean 1-Click guide and a Tailscale AI page returned 404. I couldn't verify an official Tailscale MCP server; one search snippet pointed to "Aperture", which I didn't open.
