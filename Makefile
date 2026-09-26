.PHONY: build test test-cargo test-doc test-slow test-images test-linux test-rootless-runc test-cluster test-upgrade test-upgrade-node test-upgrade-cluster test-apple coverage check fmt lint audit clean pdf loc help bench bench-large pickle-test-macos ci ci-full observability-demo kubernetes-demo toml-demo readme-commands

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

test: ## Run the portable suite with nextest (ignored suites are separate)
	$(NEXTEST)

test-cargo: ## Run the portable suite with Cargo's built-in runner
	$(CARGO) test

test-doc: ## Run doctests (nextest does not run them)
	$(CARGO) test --doc

test-slow: ## Run required wall-clock acceptance tests
	$(NEXTEST) --run-ignored=only -E 'binary(integration)'

test-images: ## Fetch the pinned test images (with retries) into the local mirror cache
	python3 scripts/test-images/mirror.py warm --cache "$(TEST_IMAGE_CACHE)"

test-linux: ## Run provisioned Linux runtime, network, eBPF, Btrfs and Buildah tests
	$(CARGO) build --features ebpf --bin bun
	RELIABURGER_RUNC_TESTS=1 RELIABURGER_NETNS_TESTS=1 RELIABURGER_EBPF_TESTS=1 RELIABURGER_BTRFS_TESTS=1 RELIABURGER_BUILDAH_TESTS=1 RELIABURGER_CGROUP_TESTS=1 RELIABURGER_NODE_PRESSURE_TESTS=1 RELIABURGER_BUN_BINARY="$(CURDIR)/target/debug/bun" $(WITH_TEST_IMAGES) $(NEXTEST) --features ebpf --run-ignored=only -E '(binary(ebpf) | binary(build) | binary(node_pressure) | binary(test_storage) | binary(owned_network) | binary(owned_runc) | binary(kubernetes_demo) | test(/(runc_|netns|btrfs_|cgroup_|identity_dir_is_tmpfs|pinned_images_serve)/)) & not binary(oci_crash) & !test(/^actual_(host_reboot|bun_kernel_discovery_host_reboot)/) $(LINUX_EXCLUDE)'

test-rootless-runc: ## Prove rootless runc networking and port adoption as a non-root user
	RELIABURGER_ROOTLESS_RUNC_TESTS=1 $(NEXTEST) --features ebpf --run-ignored=only -E 'binary(owned_rootless) | test(rootless_published_port_survives_bun_replacement) | test(normal_rootless_bun)'

test-cluster: ## Run all real multi-node cluster acceptance suites
	RELIABURGER_CLUSTER_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(cluster_failover) | binary(cluster_gossip) | binary(council_self_healing) | binary(council_disaster_recovery) | binary(placement) '

test-upgrade: ## Run all real-binary self-upgrade acceptance tests
	RELIABURGER_UPGRADE_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(self_upgrade) | binary(self_upgrade_cluster)'

test-upgrade-node: ## Run only the single-node self-upgrade tests
	RELIABURGER_UPGRADE_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(self_upgrade)'

test-upgrade-cluster: ## Run only the cluster self-upgrade tests
	RELIABURGER_UPGRADE_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(self_upgrade_cluster)'

test-apple: ## Run deferred Apple adapter development tests on Apple silicon
	RELIABURGER_APPLE_CONTAINER_TESTS=1 $(NEXTEST) --run-ignored=only -E 'test(/^grill::apple::tests::/)'

check: ## Type-check without producing binaries (fast)
	$(CARGO) check

fmt: ## Format all Rust source with rustfmt
	$(CARGO) fmt

fmt-check: ## Check formatting without modifying files
	$(CARGO) fmt -- --check

lint: ## Run clippy for every target, with all features and with none, warnings as errors
	$(CARGO) clippy --all-targets --all-features -- -D warnings
	$(CARGO) clippy --all-targets --no-default-features -- -D warnings

audit: ## Fail on new RustSec findings or an expired advisory exception
	@today=$$(date -u +%Y%m%d); expiry=20261118; \
	if [ "$$today" -gt "$$expiry" ]; then \
		echo "dependency advisory exceptions expired on 2026-11-18; review .cargo/audit.toml" >&2; \
		exit 1; \
	fi
	@active=$$($(CARGO) tree --locked --all-features --target all -i rkyv --prefix none --format '{p}') || exit 1; \
	if [ -n "$$active" ]; then \
		echo "rkyv advisory exception requires an inactive dependency; review .cargo/audit.toml" >&2; \
		exit 1; \
	fi
	$(CARGO) audit

bench: ## Run reproducible transport and 5-250 node gossip benchmarks
	$(CARGO) bench --bench gossip

bench-large: ## Run reproducible 500 and 1000 node gossip benchmarks
	$(CARGO) bench --bench gossip_large

coverage: ## Run the portable suite once under line coverage and enforce the floor
	$(CARGO) llvm-cov clean --workspace
	$(CARGO) llvm-cov --no-report nextest --profile $(NEXTEST_PROFILE) --no-tests=fail
	mkdir -p target/coverage
	$(COVERAGE_REPORT) --lcov --output-path target/coverage/lcov.info
	$(COVERAGE_REPORT) --html --output-dir target/coverage/html
	$(COVERAGE_REPORT) --fail-under-lines $(COVERAGE_MIN_LINES)

deploy-demo: build ## Deploy an app, show history, lint config
	./scripts/deploy-demo.sh

observability-demo: build ## Start bun, collect metrics, query them, show dashboard
	./scripts/observability-demo.sh

kubernetes-demo: build ## Demo Kubernetes YAML import/export round-trip
	./scripts/kubernetes-yamls-demo.sh

toml-demo: build ## Demo config tooling (lint, fmt, compile, diff)
	./scripts/relish-toml-demo.sh

pickle-test-macos: build ## Push/pull a real Docker image through Pickle (macOS + Docker Desktop)
	./scripts/pickle-push-test.sh

ci: fmt-check lint test test-doc ## Run portable CI checks

ci-full: fmt-check lint test bench ## Run everything including benchmarks

readme-commands: ## Regenerate the relish command list in README.md from the CLI definition
	RELIABURGER_UPDATE_README=1 $(CARGO) test --bin relish readme_command_list_matches_the_cli

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
