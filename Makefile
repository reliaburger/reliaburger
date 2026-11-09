.PHONY: build test warm-executables test-cargo test-doc test-slow test-images test-linux test-rootless-runc test-cluster test-hollow test-upgrade test-upgrade-node test-upgrade-cluster test-apple test-standard-clients test-gym test-gym-user test-gpu test-s3 test-ci-scripts check-ignored coverage check fmt lint lint-macos audit outdated clean pdf loc help bench bench-large ci ci-bench rollout-demo gitops-demo observability-demo kubernetes-demo toml-demo reference-commands agent-docs bench-task-arrays fuzz-tools fuzz-build fuzz-smoke fuzz-corpus-check fuzz-corpus-seed coverage-html test-macos mcp-registry-publish

CARGO = cargo
NEXTEST_PROFILE ?= default
NEXTEST = $(CARGO) nextest run --profile $(NEXTEST_PROFILE) --no-tests=fail
COVERAGE_MIN_LINES ?= 78.65
# Crash-recovery tests SIGKILL instrumented Bun, owner and workload processes
# on purpose; one killed while it writes its profile at exit leaves a
# truncated .profraw. Skip unreadable profiles with a warning instead of
# failing the report: they only drop that process's partial counts, which can
# lower coverage but never inflate it. A run where no profile is readable
# still fails.
COVERAGE_REPORT = $(CARGO) llvm-cov report --failure-mode all
# Extra nextest filter for test-linux, e.g. to skip suites a job already ran.
LINUX_EXCLUDE ?=
# Pinned public images for the Linux suites, fetched once and served on loopback.
TEST_IMAGE_CACHE ?= $(HOME)/.cache/reliaburger/test-images
TEST_IMAGE_MIRROR ?= 127.0.0.1:5099
WITH_TEST_IMAGES = python3 scripts/test-images/mirror.py run --cache "$(TEST_IMAGE_CACHE)" --listen $(TEST_IMAGE_MIRROR) --

# --- Rust targets ---

build: ## Compile all crates (debug)
	$(CARGO) build

release: ## Compile all crates (optimised release)
	$(CARGO) build --release

test: warm-executables ## Run the portable suite with nextest (ignored suites are separate)
	$(NEXTEST)

# `cargo test --no-run` builds the binaries exactly as the suite does. A plain
# `cargo build --bins` leaves out the dev-dependencies' features, so nextest
# then compiled the library and every binary again and replaced the warm ones.
warm-executables: ## Build the suite and its binaries, then run each binary once, so macOS assesses them before any test is timed
	$(CARGO) test --no-run
	python3 scripts/ci/warm_executables.py

# The macOS CI job (#1720). Bun ships for Linux only, so on macOS CI tests
# relish (its binary, `relish::` in the library and the suite's `relish_*`
# modules) and the tests only macOS compiles: the Apple runtime, the
# non-Linux process start times, the macOS home directories and runtime
# selection. Bun, grill, cluster and eBPF tests run on Linux. Cargo still
# links `bun` and `testapp`, because it builds every binary for an integration
# test, but none of their tests run. scripts/ci/test_macos_selection.py pins
# this filterset, and scripts/ci/macos_selection.py keeps the evidence
# manifest's portable-darwin cases inside it.
test-macos: ## Run relish's tests and the macOS-only tests (the macOS CI job; `test` runs everything)
	$(CARGO) test --no-run --lib --bin relish --test suite --test runtime_selection
	python3 scripts/ci/warm_executables.py
	$(NEXTEST) --lib --bin relish --test suite --test runtime_selection -E 'binary_id(reliaburger::bin/relish) | (binary_id(reliaburger) & (test(/^relish::/) | test(/^grill::apple::/) | test(/^grill::records::tests::absolute_start_time/))) | (binary_id(reliaburger::suite) & (test(/^relish_/) | test(/^user_directories::macos_/))) | binary_id(reliaburger::runtime_selection)'

test-cargo: ## Run the portable suite with Cargo's built-in runner
	$(CARGO) test

test-doc: ## Run doctests (nextest does not run them)
	$(CARGO) test --doc

test-slow: ## Run required wall-clock acceptance tests
	$(NEXTEST) --run-ignored=only -E 'binary(integration)'

test-images: ## Fetch the pinned test images (with retries) into the local mirror cache
	python3 scripts/test-images/mirror.py warm --cache "$(TEST_IMAGE_CACHE)"

# Build bun, which RELIABURGER_BUN_BINARY names, the way nextest builds it, so
# nextest compiles nothing again and leaves that file alone (#904).
test-linux: ## Run provisioned Linux runtime, network, eBPF, Btrfs and Buildah tests
	$(CARGO) test --no-run --features ebpf
	RELIABURGER_RUNC_TESTS=1 RELIABURGER_NETNS_TESTS=1 RELIABURGER_EBPF_TESTS=1 RELIABURGER_BTRFS_TESTS=1 RELIABURGER_BUILDAH_TESTS=1 RELIABURGER_CGROUP_TESTS=1 RELIABURGER_NODE_PRESSURE_TESTS=1 RELIABURGER_BUN_BINARY="$(CURDIR)/target/debug/bun" $(WITH_TEST_IMAGES) $(NEXTEST) --features ebpf --run-ignored=only -E '(binary(ebpf) | binary(build) | binary(node_pressure) | binary(test_storage) | binary(owned_network) | binary(owned_runc) | binary(kubernetes_demo) | test(/^grill::(volume|image)::tests::/) | test(/(runc_|netns|btrfs_|cgroup_|identity_dir_is_tmpfs|pinned_images_serve)/)) & not binary(oci_crash) & !test(/^actual_(host_reboot|bun_kernel_discovery_host_reboot)/) $(LINUX_EXCLUDE)'

test-rootless-runc: ## Prove rootless runc networking and port adoption as a non-root user
	RELIABURGER_ROOTLESS_RUNC_TESTS=1 $(NEXTEST) --features ebpf --run-ignored=only -E 'binary(owned_rootless) | test(rootless_published_port_survives_bun_replacement) | test(normal_rootless_bun)'

test-cluster: ## Run all real multi-node cluster acceptance suites and the scripted feature demos
	RELIABURGER_CLUSTER_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(cluster_failover) | binary(cluster_gossip) | binary(council_self_healing) | binary(council_disaster_recovery) | binary(placement) | binary(cluster_task_arrays) | binary(registry_durability) | binary(demos)'

test-hollow: ## Run the 20-node hollow-node harness check (#1401) and the 100-node, 1,000-app summary budget; measured fleets run through scripts/release/qualify-hollow-nodes.sh
	$(NEXTEST) --run-ignored=only -E 'binary(hollow_nodes) & (test(twenty_hollow_nodes) | test(a_hundred_hollow_nodes))'

test-upgrade: ## Run all real-binary self-upgrade acceptance tests
	RELIABURGER_UPGRADE_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(self_upgrade) | binary(self_upgrade_cluster)'

test-upgrade-node: ## Run only the single-node self-upgrade tests
	RELIABURGER_UPGRADE_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(self_upgrade)'

test-upgrade-cluster: ## Run only the cluster self-upgrade tests
	RELIABURGER_UPGRADE_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(self_upgrade_cluster)'

test-apple: ## Run deferred Apple adapter development tests on Apple silicon
	RELIABURGER_APPLE_CONTAINER_TESTS=1 $(NEXTEST) --run-ignored=only -E 'test(/^grill::apple::tests::/)'

test-standard-clients: ## Push and pull through Pickle's TLS listener with docker, podman, skopeo and crane (Linux; prepare with scripts/standard-clients-host.sh)
	scripts/standard-clients-host.sh
	$(NEXTEST) --run-ignored=only -E 'binary(suite) & test(/^registry_standard_clients::/)'

# The agent gym (#1086): every scenario on fresh local clusters, scored against
# ground truth. The default solver is the scripted baseline, which costs
# nothing; GYM_SOLVER=command:PROGRAM plugs in an agent, which costs API spend,
# so no CI job runs this. Results and the Markdown record land in GYM_OUT.
GYM_SOLVER ?= scripted
GYM_RUNS ?= 1
GYM_PORT_BASE ?= 29400
GYM_OUT ?= target/gym/$(shell date -u +%Y%m%dT%H%M%SZ)
test-gym: ## Run the agent gym's scenarios on throwaway local clusters (#1086); GYM_SOLVER=command:PROGRAM runs an agent instead of the baseline
	python3 scripts/release/require_python.py
	$(CARGO) build --locked --bin relish --bin bun --bin testapp
	python3 scripts/gym/gym.py run --solver '$(GYM_SOLVER)' --runs $(GYM_RUNS) --port-base $(GYM_PORT_BASE) --out '$(GYM_OUT)'

# `relish gym run` (#1683), tested by the gym: the baseline over every
# scenario on a fresh local cluster, then a Ctrl-C mid-run, each followed by a
# check that no fault, gym app or solver token is left. No API spend.
test-gym-user: ## Test `relish gym run` on a throwaway local cluster (#1683): the baseline, then Ctrl-C mid-run, checking cleanup
	python3 scripts/release/require_python.py
	$(CARGO) build --locked --bin relish --bin bun --bin testapp
	python3 scripts/gym/gym.py user --port-base $(GYM_PORT_BASE) --out '$(GYM_OUT)/user'

test-gpu: ## Detect a real NVIDIA GPU (needs the GPU and nvidia-smi)
	RELIABURGER_GPU_TESTS=1 $(NEXTEST) --run-ignored=only -E 'test(=bun::gpu::tests::nvidia_detector_finds_hardware)'

test-s3: ## Export logs and round-trip a council backup through a throwaway local S3 server, or a real bucket with RELIABURGER_TEST_S3_URL=s3://bucket/prefix and AWS credentials
	scripts/test-s3.sh $(NEXTEST) --lib --run-ignored=only -E 'test(=ketchup::export::tests::export_to_real_s3_manual) | test(=council::backup::tests::council_backup_round_trips_through_real_s3)'

check: ## Type-check without producing binaries (fast)
	$(CARGO) check

fmt: ## Format all Rust source with rustfmt
	$(CARGO) fmt

fmt-check: ## Check formatting without modifying files
	$(CARGO) fmt -- --check

lint: ## Run clippy for every target, with all features and with none, warnings as errors
	$(CARGO) clippy --all-targets --all-features -- -D warnings
	$(CARGO) clippy --all-targets --no-default-features -- -D warnings

# One Clippy pass for the macOS CI job. The `lint` job on Linux runs both
# passes over every target. What only macOS compiles is `cfg(target_os =
# "macos")` code and the `not(all(feature = "ebpf", target_os = "linux"))`
# fallbacks, and every feature leaves both in, so the all-features pass
# covers them. The code that only the no-features pass reaches is gated on a
# feature alone, and that compiles the same on Linux. It lints the library
# and the two binaries, not every target: Linux lints the test code, and
# every target means compiling every Linux-only test binary on a Mac too.
# Test code that only macOS compiles (the Apple runtime's tests, say) is
# built by `test-macos` but linted only by `make lint` on a Mac. Linting
# `bun` is its compile check on macOS; its tests run on Linux (#1720).
lint-macos: ## Run clippy once on the library, relish and bun, with all features (the macOS CI job; `lint` runs both passes on every target)
	$(CARGO) clippy --lib --bin relish --bin bun --all-features -- -D warnings

audit: ## Fail on new RustSec findings or an expired advisory exception
	@today=$$(date -u +%Y%m%d); expiry=20261118; \
	if [ "$$today" -gt "$$expiry" ]; then \
		echo "dependency advisory exceptions expired on 2026-11-18; review .cargo/audit.toml" >&2; \
		exit 1; \
	fi
	$(CARGO) audit

# Reports, never fails: the lag is information for the release's refresh
# step (docs/releasing.md), not a gate. `--dry-run` leaves Cargo.lock alone.
outdated: ## Report crates that cargo update would move and those a major version behind (never fails)
	-$(CARGO) update --dry-run --verbose

bench: ## Run reproducible transport and 5-250 node gossip benchmarks, the 1k and 10k node scheduling pass, and policy evaluation for 10-1,000 policies
	$(CARGO) bench --bench gossip
	$(CARGO) bench --bench scheduler
	$(CARGO) bench --bench policy

bench-large: ## Run reproducible 500 and 1000 node gossip benchmarks
	$(CARGO) bench --bench gossip_large

bench-task-arrays: ## Run in-process task-array benchmarks (leader cost, pool overhead, fork/exec floor)
	$(CARGO) bench --bench task_arrays

# One report call writes the lcov file and then checks the floor: each call
# merges every profile and reads every binary's coverage map again, about two
# minutes on CI (plan 2026-11-06, F3). scripts/ci/coverage_owner.py pins this
# recipe, so CI's evidence owner runs exactly these stages.
coverage: ## Run the portable suite once under line coverage, write the lcov report and enforce the floor
	$(CARGO) llvm-cov clean --workspace
	$(CARGO) llvm-cov --no-report nextest --profile $(NEXTEST_PROFILE) --no-tests=fail
	mkdir -p target/coverage
	$(COVERAGE_REPORT) --lcov --output-path target/coverage/lcov.info --fail-under-lines $(COVERAGE_MIN_LINES)

# CI writes the HTML on pushes to main and the nightly only (D5).
coverage-html: ## Write the HTML coverage report from the last make coverage run
	$(COVERAGE_REPORT) --html --output-dir target/coverage/html

rollout-demo: ## Roll an app forward, roll it back, and watch a bad version roll back by itself
	scripts/demo/rollout.sh

gitops-demo: ## Sync a one-node cluster from a file:// Git repository
	scripts/demo/gitops.sh

observability-demo: build ## Start bun, collect metrics, query them, show dashboard
	./scripts/observability-demo.sh

kubernetes-demo: build ## Demo Kubernetes YAML import/export round-trip
	./scripts/kubernetes-yamls-demo.sh

toml-demo: build ## Demo config tooling (lint, fmt, compile, diff)
	./scripts/relish-toml-demo.sh

test-ci-scripts: ## Test the CI scripts: job selection, ignored-test owners and JUnit keeping, and the agent gym's scorer and runner
	python3 scripts/release/require_python.py
	python3 -m unittest discover -s scripts/ci -p 'test_*.py'
	python3 -m unittest discover -s scripts/release -p 'test_*.py'
	python3 -m unittest discover -s scripts/gym -p 'test_*.py'

check-ignored: ## Fail when an #[ignore] reason names no gate, script, fixture parent or issue
	python3 scripts/ci/ignored_owners.py reasons

# The checks that take seconds come first, so a format slip or an unowned
# #[ignore] fails before Clippy and the suite compile anything (#1724). Make
# runs prerequisites in the order listed unless you pass -j.
ci: fmt-check check-ignored test-ci-scripts lint test test-doc ## Run the portable CI checks, quickest first

ci-bench: ci bench ## Run the portable CI checks, then the 5-250 node gossip benchmarks (no privileged, cluster or upgrade suites)

reference-commands: ## Regenerate the relish command list in docs/manual/13_reference.md from the CLI definition
	RELIABURGER_UPDATE_REFERENCE=1 $(CARGO) test --bin relish reference_command_list_matches_the_cli

agent-docs: ## Regenerate the agent-facing files from the manual's agent chapter, and every website page's Markdown copy
	RELIABURGER_UPDATE_AGENT_DOCS=1 $(CARGO) test --test suite -- --exact \
		agent_docs::every_agent_facing_file_matches_the_chapter \
		website_markdown::every_page_has_a_current_markdown_copy

# --- Fuzzing (#1398) ---
#
# One cargo-fuzz target per decoder that reads bytes from another machine;
# the bodies are in src/fuzz/. Fuzzing needs nightly Rust and cargo-fuzz.
# The corpus checks run the same bodies on stable and are part of `make test`.
# New inputs libFuzzer finds go to fuzz/target/corpus/, never into the
# checked-in seeds; crashes go to fuzz/artifacts/<target>/.
# fuzz.yml pins a dated nightly here so its build cache stays valid.
FUZZ_TOOLCHAIN ?= nightly
FUZZ = $(CARGO) +$(FUZZ_TOOLCHAIN) fuzz
FUZZ_SECONDS ?= 10
# cargo-fuzz defaults --target to the triple it was built for; a prebuilt
# musl cargo-fuzz then asks for a static CRT that AddressSanitizer refuses.
# Name the nightly toolchain's own host instead.
FUZZ_TARGET ?= $(shell $(CARGO) +$(FUZZ_TOOLCHAIN) -vV 2>/dev/null | sed -n 's/^host: //p')
FUZZ_TARGETS = $(sort $(basename $(notdir $(wildcard fuzz/fuzz_targets/*.rs))))

fuzz-tools: ## Check that nightly Rust and cargo-fuzz are installed, and say how to get them
	@$(CARGO) +$(FUZZ_TOOLCHAIN) --version >/dev/null 2>&1 || { \
		echo "fuzzing needs Rust $(FUZZ_TOOLCHAIN): rustup toolchain install $(FUZZ_TOOLCHAIN) --profile minimal" >&2; exit 1; }
	@$(FUZZ) --version >/dev/null 2>&1 || { \
		echo "fuzzing needs cargo-fuzz: cargo install cargo-fuzz --locked" >&2; exit 1; }

fuzz-build: fuzz-tools ## Build every fuzz target (nightly, cargo-fuzz)
	$(FUZZ) build --target $(FUZZ_TARGET)

fuzz-smoke: fuzz-build ## Run every fuzz target for FUZZ_SECONDS (default 10) each; fails if any crashes
	@failed=""; for target in $(FUZZ_TARGETS); do \
		mkdir -p fuzz/target/corpus/$$target fuzz/target/logs; \
		echo "fuzz: $$target for $(FUZZ_SECONDS)s (log: fuzz/target/logs/$$target.log)"; \
		$(FUZZ) run --target $(FUZZ_TARGET) $$target fuzz/target/corpus/$$target fuzz/corpus/$$target -- \
			-max_total_time=$(FUZZ_SECONDS) -print_final_stats=1 \
			>fuzz/target/logs/$$target.log 2>&1 || failed="$$failed $$target"; \
		grep -E '^#[0-9]+.*(DONE|cov:)' fuzz/target/logs/$$target.log | tail -n 1; \
	done; \
	if [ -n "$$failed" ]; then echo "fuzz: crashed:$$failed (inputs in fuzz/artifacts/)" >&2; exit 1; fi

fuzz-corpus-check: ## Run every fuzz target body over its seeds and checked-in corpus on stable
	$(NEXTEST) --lib -E 'test(/^fuzz::/)'

fuzz-corpus-seed: ## Write the fixture seeds into fuzz/corpus/ (after a format change)
	RELIABURGER_UPDATE_FUZZ_CORPUS=1 $(NEXTEST) --lib -E 'test(=fuzz::tests::fixture_seeds_are_in_the_checked_in_corpus)'

# --- Documentation targets ---

QUARTO = quarto render docs/_quarto --to pdf

pdf: ## Build all PDFs
	$(QUARTO) --profile book
	$(QUARTO) --profile design
	$(QUARTO) --profile whitepaper
	$(QUARTO) --profile roadmap

# --- Stats ---

loc: ## Count lines of tracked .rs, .md, and .toml files
	@scripts/loc.sh

# --- Housekeeping ---

clean: ## Remove build artefacts and generated files
	$(CARGO) clean
	rm -rf docs/_book docs/_quarto/.quarto

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  make %-12s %s\n", $$1, $$2}'

.PHONY: test-contract-boundaries
test-contract-boundaries: ## Run the actual-source optimized cron boundary contract
	$(NEXTEST) --manifest-path tests/contracts/cron/Cargo.toml --config-file tests/contracts/cron/nextest.toml --release --locked
