.PHONY: build test check fmt lint clean pdf loc help examples bench bench-large bench-10k pickle-test-macos ci ci-full observability-demo

CARGO = cargo

# --- Rust targets ---

build: ## Compile all crates (debug)
	$(CARGO) build

release: ## Compile all crates (optimised release)
	$(CARGO) build --release

test: ## Run all tests
	$(CARGO) test

check: ## Type-check without producing binaries (fast)
	$(CARGO) check

fmt: ## Format all Rust source with rustfmt
	$(CARGO) fmt

fmt-check: ## Check formatting without modifying files
	$(CARGO) fmt -- --check

lint: ## Run clippy with warnings as errors
	$(CARGO) clippy -- -D warnings

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

bench: ## Run fast benchmarks (transport, single round, convergence 5-250)
	$(CARGO) bench --bench gossip

bench-large: ## Run large cluster benchmarks (500, 1000 nodes — ~10 min)
	$(CARGO) bench --bench gossip_large

bench-10k: ## Run 10k node convergence test (~1 hour)
	$(CARGO) test --release --test gossip_10k -- --ignored --nocapture

observability-demo: build ## Start bun, collect metrics, query them, show dashboard
	./scripts/observability-demo.sh

pickle-test-macos: build ## Push/pull a real Docker image through Pickle (macOS + Docker Desktop)
	./scripts/pickle-push-test.sh

ci: fmt-check lint test ## Run CI checks (fmt, clippy, tests — no benchmarks)

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
