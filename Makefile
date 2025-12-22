# Variables
CODEGEN_TOOL := flutter_rust_bridge_codegen
DART_MOCK_DIR := .dart
RUST_GEN_FILE := src/frb_generated.rs

.PHONY: help gen watch install clean lint check

# Default target: Show help
help:
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@echo "  gen       Generate the Rust/Dart bridge code"
	@echo "  watch     Watch for file changes and auto-generate"
	@echo "  install   Install/Update the codegen tool via Cargo"
	@echo "  clean     Remove all generated bridge files"
	@echo "  lint      Run cargo fmt and clippy"

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

lint:
	cargo +nightly fmt --all
	cargo lints clippy --all-targets --all-features
