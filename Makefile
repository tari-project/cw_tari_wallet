# Variables
CODEGEN_TOOL := flutter_rust_bridge_codegen
DART_MOCK_DIR := .dart
RUST_GEN_FILE := src/frb_generated.rs

.PHONY: help gen watch install clean lint fmt fmt-check check record-fixtures verify-live

# Default target: Show help
help:
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@echo "  gen        Generate the Rust/Dart bridge code"
	@echo "  watch      Watch for file changes and auto-generate"
	@echo "  install    Install/Update the codegen tool via Cargo"
	@echo "  clean      Remove all generated bridge files"
	@echo "  fmt        Format all code with rustfmt"
	@echo "  fmt-check  Check formatting without modifying files"
	@echo "  lint       Run clippy with warnings denied"
	@echo "  check      Type-check all targets"
	@echo "  record-fixtures  Rebuild the verification fixtures (Tier A DB + Tier B RPC)"
	@echo "  verify-live      Run the Tier C live-testnet smoke runner (opt-in, needs env secrets)"

gen:
	$(CODEGEN_TOOL) generate

watch:
	$(CODEGEN_TOOL) generate --watch

install:
	cargo install flutter_rust_bridge_codegen

clean:
	@echo "Cleaning generated files..."
	rm -f $(RUST_GEN_FILE)
	rm -f $(DART_MOCK_DIR)/frb_generated.dart
	rm -f $(DART_MOCK_DIR)/frb_generated.io.dart
	rm -f $(DART_MOCK_DIR)/frb_generated.web.dart
	rm -f $(DART_MOCK_DIR)/lib.dart
	rm -rf $(DART_MOCK_DIR)/api

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets --all-features -- -D warnings

check:
	cargo check --all-targets

# Verification harness.
#
# Rebuild the committed verification fixtures: the Tier A view-only wallet DB and
# the Tier B recorded RPC JSON captures. Run this when the fixture's golden values
# or the upstream RPC shape changes, then commit the regenerated assets.
record-fixtures:
	cargo run -p verify -- record-fixtures

# Run the Tier C live-testnet smoke runner. Opt-in and gated: it needs a live
# esmeralda base node and a dedicated, minimally-funded test wallet supplied via
# env vars (VERIFY_BASE_URL, VERIFY_SEED_WORDS, VERIFY_PASSPHRASE). NEVER run on
# PRs — this is for local/manual or the nightly schedule only.
verify-live:
	cargo run -p verify --features live-e2e -- live
