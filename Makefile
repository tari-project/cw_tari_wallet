# Variables
CODEGEN_TOOL := flutter_rust_bridge_codegen
DART_MOCK_DIR := .dart
RUST_GEN_FILE := src/frb_generated.rs

.PHONY: help gen watch setup install clean lint check

# Default target: Show help
help:
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@echo "  gen       Generate the Rust/Dart bridge code (runs setup first)"
	@echo "  watch     Watch for file changes and auto-generate"
	@echo "  setup     Create the dummy .dart/pubspec.yaml required by the tool"
	@echo "  install   Install/Update the codegen tool via Cargo"
	@echo "  clean     Remove all generated bridge files"
	@echo "  lint      Run cargo fmt and clippy"

setup:
	@mkdir -p $(DART_MOCK_DIR)
	@if [ ! -f $(DART_MOCK_DIR)/pubspec.yaml ]; then \
		echo "Creating dummy pubspec.yaml..."; \
		echo "name: codegen_mock\ndescription: Mock package\nversion: 1.0.0\nenvironment:\n  sdk: '>=3.0.0 <4.0.0'\ndependencies:\n  flutter_rust_bridge: any" > $(DART_MOCK_DIR)/pubspec.yaml; \
	fi

gen: setup
	$(CODEGEN_TOOL) generate

watch: setup
	$(CODEGEN_TOOL) generate --watch

install:
	cargo install flutter_rust_bridge_codegen

clean:
	@echo "Cleaning generated files..."
	rm -f $(RUST_GEN_FILE)
	rm -f $(DART_MOCK_DIR)/frb_generated.dart
	rm -f $(DART_MOCK_DIR)/frb_generated.io.dart
	rm -f $(DART_MOCK_DIR)/frb_generated.web.dart
	rm -rf $(DART_MOCK_DIR)/api

lint:
	cargo +nightly fmt --all
	cargo lints clippy --all-targets --all-features
