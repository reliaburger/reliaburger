.PHONY: pdf clean help

QUARTO = quarto render docs/_quarto --to pdf

pdf: ## Build all PDFs
	$(QUARTO) --profile book
	$(QUARTO) --profile design
	$(QUARTO) --profile whitepaper
	$(QUARTO) --profile roadmap

clean: ## Remove generated files
	rm -rf docs/_book docs/_quarto/.quarto

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  make %-12s %s\n", $$1, $$2}'
