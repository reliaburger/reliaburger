.PHONY: build test test-cargo test-doc test-no-default test-slow test-linux test-cluster test-upgrade test-upgrade-node test-upgrade-cluster test-apple coverage check fmt lint audit clean pdf loc help examples bench bench-large bench-10k pickle-test-macos ci ci-full observability-demo kubernetes-demo toml-demo

CARGO = cargo
NEXTEST_PROFILE ?= default
NEXTEST = $(CARGO) nextest run --profile $(NEXTEST_PROFILE) --no-tests=fail
COVERAGE_MIN_LINES ?= 78.65

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

test-no-default: ## Run the portable suite without default features
	$(NEXTEST) --no-default-features

test-slow: ## Run required wall-clock acceptance tests
	$(NEXTEST) --run-ignored=only -E 'binary(integration)'

test-linux: ## Run provisioned Linux runtime, network, eBPF, Btrfs and Buildah tests
	RELIABURGER_RUNC_TESTS=1 RELIABURGER_NETNS_TESTS=1 RELIABURGER_EBPF_TESTS=1 RELIABURGER_BTRFS_TESTS=1 RELIABURGER_BUILDAH_TESTS=1 RELIABURGER_CGROUP_TESTS=1 $(NEXTEST) --features ebpf --run-ignored=only -E 'binary(ebpf) | binary(build) | test(/(runc_|netns|btrfs_|cgroup_|identity_dir_is_tmpfs)/)'

test-cluster: ## Run all real multi-node cluster acceptance suites
	RELIABURGER_CLUSTER_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(cluster_failover) | binary(cluster_gossip) | binary(council_self_healing) | binary(council_disaster_recovery) | binary(placement) | binary(chaos)'

test-upgrade: ## Run all real-binary self-upgrade acceptance tests
	RELIABURGER_UPGRADE_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(self_upgrade) | binary(self_upgrade_cluster)'

test-upgrade-node: ## Run only the single-node self-upgrade tests
	RELIABURGER_UPGRADE_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(self_upgrade)'

test-upgrade-cluster: ## Run only the cluster self-upgrade tests
	RELIABURGER_UPGRADE_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(self_upgrade_cluster)'

test-apple: ## Run the manual Apple Container acceptance tests on Apple silicon
	RELIABURGER_APPLE_CONTAINER_TESTS=1 $(NEXTEST) --run-ignored=only -E 'test(apple_container_grill_creates_instance) | test(adopt_re_tracks_a_running_apple_container)'

check: ## Type-check without producing binaries (fast)
	$(CARGO) check

fmt: ## Format all Rust source with rustfmt
	$(CARGO) fmt

fmt-check: ## Check formatting without modifying files
	$(CARGO) fmt -- --check

lint: ## Run clippy for every target and feature with warnings as errors
	$(CARGO) clippy --all-targets --all-features -- -D warnings

audit: ## Fail on new RustSec findings or an expired advisory exception
	@today=$$(date -u +%Y%m%d); expiry=20260818; \
	if [ "$$today" -gt "$$expiry" ]; then \
		echo "dependency advisory exceptions expired on 2026-08-18; review .cargo/audit.toml" >&2; \
		exit 1; \
	fi
	$(CARGO) audit

examples: build ## Dry-run every example config with relish
	@failed=0; total=0; \
	for f in $$(find examples -name '*.toml' | sort); do \
		total=$$((total + 1)); \
		if $(CARGO) run --quiet --bin relish -- apply "$$f" >/dev/null 2>&1; then \
			printf "  ✓ %s\n" "$$f"; \
		else \
			printf "  ✗ %s\n" "$$f"; \
			failed=$$((failed + 1)); \
		fi; \
	done; \
	echo ""; \
	echo "$$total examples, $$failed failed."; \
	[ $$failed -eq 0 ]

bench: ## Run reproducible transport and 5-250 node gossip benchmarks
	$(CARGO) bench --bench gossip

bench-large: ## Run reproducible 500 and 1000 node gossip benchmarks
	$(CARGO) bench --bench gossip_large

bench-10k: ## Run the deterministic 10k-member per-node scale acceptance
	$(CARGO) test --release --test gossip_10k -- --ignored --nocapture

coverage: ## Combine default and no-default nextest line coverage
	$(CARGO) llvm-cov clean --workspace
	$(CARGO) llvm-cov --no-report nextest --profile $(NEXTEST_PROFILE)
	$(CARGO) llvm-cov --no-clean --no-default-features nextest --profile $(NEXTEST_PROFILE)
	mkdir -p target/coverage
	$(CARGO) llvm-cov report --lcov --output-path target/coverage/lcov.info
	$(CARGO) llvm-cov report --html --output-dir target/coverage/html
	$(CARGO) llvm-cov report --fail-under-lines $(COVERAGE_MIN_LINES)

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

ci: fmt-check lint test test-doc test-no-default ## Run portable CI checks

ci-full: fmt-check lint test bench ## Run everything including benchmarks

# --- Documentation targets ---

QUARTO = quarto render docs/_quarto --to pdf

pdf: ## Build all PDFs
	$(QUARTO) --profile book
	$(QUARTO) --profile design
	$(QUARTO) --profile whitepaper
	$(QUARTO) --profile roadmap

# --- Stats ---

loc: ## Count lines of .rs, .md, and .toml files
	@echo "  .rs (src):  $$(find ./src -name '*.rs' | xargs awk 'FNR==1{t=0} /^#\[cfg\(test\)\]/{t=1} !t{n++} END{print n+0}')"
	@echo "  .rs (test): $$(( $$(find ./src -name '*.rs' | xargs awk 'FNR==1{t=0} /^#\[cfg\(test\)\]/{t=1} t{n++} END{print n+0}') + $$(find ./tests -name '*.rs' | xargs cat 2>/dev/null | wc -l | tr -d ' ') ))"
	@echo "  .md:   $$(find . -name '*.md'   | xargs cat 2>/dev/null | wc -l)"
	@echo "  .toml: $$(find . -name '*.toml' | xargs cat 2>/dev/null | wc -l)"
	@echo "  total: $$(find . -name '*.rs' -o -name '*.md' -o -name '*.toml' | xargs cat 2>/dev/null | wc -l)"

# --- Housekeeping ---

clean: ## Remove build artefacts and generated files
	$(CARGO) clean
	rm -rf docs/_book docs/_quarto/.quarto

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  make %-12s %s\n", $$1, $$2}'
