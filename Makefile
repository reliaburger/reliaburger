.PHONY: build test check fmt lint clean pdf help

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

ci: fmt-check lint test ## Run everything CI would run

# --- Documentation targets ---

QUARTO = quarto render docs/_quarto --to pdf

pdf: ## Build all PDFs
	$(QUARTO) --profile book
	$(QUARTO) --profile design
	$(QUARTO) --profile whitepaper
	$(QUARTO) --profile roadmap

# --- Housekeeping ---

clean: ## Remove build artefacts and generated files
	$(CARGO) clean
	rm -rf docs/_book docs/_quarto/.quarto

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  make %-12s %s\n", $$1, $$2}'
