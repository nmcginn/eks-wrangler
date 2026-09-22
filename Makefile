# Frequently used commands. `make check` is the gate everything must pass.

CARGO ?= cargo
BIN   := eks

.DEFAULT_GOAL := help
.PHONY: help build release run test bench lint fmt fmt-check doc check install dist clean

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

bench: ## Run the startup benchmarks (see benches/startup.rs)
	$(CARGO) bench --locked --bench startup

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

dist: release ## Package the release binary with its completions and man page into dist/
	rm -rf dist
	mkdir -p dist/completions
	cp target/release/$(BIN) README.md LICENSE dist/
	./target/release/$(BIN) completions bash > dist/completions/$(BIN).bash
	./target/release/$(BIN) completions zsh > dist/completions/_$(BIN)
	./target/release/$(BIN) completions fish > dist/completions/$(BIN).fish
	./target/release/$(BIN) man > dist/$(BIN).1
	@echo "Packaged into dist/"

clean: ## Remove build artifacts
	$(CARGO) clean
	rm -rf dist
