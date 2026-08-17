# Frequently used commands. `make check` is the gate everything must pass.

CARGO ?= cargo
BIN   := eks

.DEFAULT_GOAL := help
.PHONY: help build release run test lint fmt fmt-check doc check install clean

help: ## Show this help
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-11s\033[0m %s\n", $$1, $$2}'

build: ## Debug build
	$(CARGO) build --locked

release: ## Optimised build
	$(CARGO) build --locked --release

run: ## Run the dashboard (make run ARGS="contexts")
	$(CARGO) run --locked -- $(ARGS)

test: ## Run the test suite
	$(CARGO) test --locked --all-features

lint: ## Clippy, warnings are errors
	$(CARGO) clippy --locked --all-targets --all-features -- -D warnings

fmt: ## Format the code
	$(CARGO) fmt --all

fmt-check: ## Verify formatting without changing files
	$(CARGO) fmt --all -- --check

doc: ## Build the API docs
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --locked --no-deps --all-features

check: fmt-check lint test doc ## Everything CI runs — run before pushing
	@echo "All checks passed."

install: ## Install eks into ~/.cargo/bin
	$(CARGO) install --locked --path .

clean: ## Remove build artifacts
	$(CARGO) clean
