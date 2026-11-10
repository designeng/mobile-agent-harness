HARNESS ?= cargo run -q --

.PHONY: build doctor status status-json queue queue-plan

build:
	cargo build

doctor:
	$(HARNESS) doctor

# Usage: make status [RUN=issue-2] [LAST=5]
status:
	$(HARNESS) status $(RUN) --last $(or $(LAST),5)

status-json:
	$(HARNESS) status $(RUN) --last $(or $(LAST),5) --json

# Usage: make queue [MAX=3] [BUDGET=20]
queue:
	$(HARNESS) queue $(if $(MAX),--max-issues $(MAX)) $(if $(BUDGET),--budget-usd $(BUDGET))

# Show which issues the queue would run, in what order (one cheap picker call).
queue-plan:
	$(HARNESS) queue --dry-run
