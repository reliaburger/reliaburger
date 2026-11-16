.PHONY: setup claims smoke run run-ops report down purge dry-run lint test

PRODUCT ?= v0.2.0
RBEVAL := ./rbeval --product $(PRODUCT)

# Check host tools, download and verify the release and the guest image.
setup:
	$(RBEVAL) setup

# Verify every claim citation against the source at the pinned docs ref.
claims:
	$(RBEVAL) claims

# Parse every feature and match every step without touching the lab.
dry-run:
	uv run python -m behave --dry-run -f progress features

lint:
	uvx ruff check --select F,E9 features harness tests

# Offline tests of the harness's own logic (no lab).
test:
	uv run pytest -q tests

# The four seed scenarios.
smoke:
	$(RBEVAL) run --tags smoke

# Everything, in axis order, then tear the lab down.
run:
	$(RBEVAL) run

# Only the operations axis, for a second product target.
run-ops:
	$(RBEVAL) run features/operations

# Rebuild reports for the newest run directory.
report:
	$(RBEVAL) report

# Destroy the lab VMs, network and pool; keep downloaded images.
down:
	$(RBEVAL) lab down

# Also delete the base image volume.
purge:
	$(RBEVAL) lab purge
