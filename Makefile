VERSION ?= $(shell cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name=="smtp-s3-dump") | .version')
IMAGE   ?= smtp-s3-dump
LEVEL   ?= patch

.DEFAULT_GOAL := help
.PHONY: help all build release-build test clippy fmt fmt-check check image release release-dry clean

help: ## Show this help
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-14s %s\n", $$1, $$2}'

all: fmt-check clippy test build ## Format check, clippy, test, build

build: ## Debug build
	cargo build --workspace

release-build: ## Release build
	cargo build --workspace --release

test: ## Run tests
	cargo test --workspace

clippy: ## Run clippy (warnings are errors)
	cargo clippy --workspace --all-targets -- -D warnings

fmt: ## Format code
	cargo fmt --all

fmt-check: ## Check formatting
	cargo fmt --all -- --check

check: ## Run cargo check
	cargo check --workspace --all-targets

image: release-build ## Build container image
	podman build --build-arg VERSION=$(VERSION) -t $(IMAGE):$(VERSION) .

release-dry: ## Dry-run cargo release (LEVEL=patch|minor|major)
	cargo release $(LEVEL)

release: fmt-check clippy test ## Verify, then cut a release (LEVEL=patch|minor|major)
	cargo release $(LEVEL) --execute

clean: ## Remove build artifacts
	cargo clean
