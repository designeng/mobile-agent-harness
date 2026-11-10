HARNESS ?= cargo run -q --

.PHONY: build doctor status status-json

build:
	cargo build

doctor:
	$(HARNESS) doctor

# Usage: make status [RUN=issue-2] [LAST=5]
status:
	$(HARNESS) status $(RUN) --last $(or $(LAST),5)

status-json:
	$(HARNESS) status $(RUN) --last $(or $(LAST),5) --json
