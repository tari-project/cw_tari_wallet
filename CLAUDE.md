# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`rust_lib_flutter_rust_wallet` is a [flutter_rust_bridge](https://github.com/fzyzcjy/flutter_rust_bridge) (FRB v2) binding that wraps the Tari `minotari` wallet for use from Flutter. It is the wallet backend consumed by **Cake Wallet in production**. The Rust crate compiles to native libs (`lib`/`cdylib`/`staticlib`); FRB generates the committed Dart glue.

## The frozen public API contract (read this first)

**The Dart-facing API is a frozen contract. Changes must be additive — never breaking.** This single constraint drives most of the design and is the most important thing to internalize before editing anything under `src/api/`.

The contract is **every `pub` item in `crate::api`** not marked `#[frb(ignore)]`, including their input/output struct/enum field names, types, order, and variant shapes, the streamed event types, and the **Dart-visible error message strings**. Two non-obvious points:

- FRB v2 exports **all** public items in `crate::api` by default — so functions *without* any `#[frb]` attribute (e.g. `get_tip_info`, `is_node_synced` in `src/api/base_node.rs`) are also part of the contract.
- Every bridge fn returns `anyhow::Result<T>`; the internal `WalletError` (`src/api/error.rs`) is never a public return type. Only the error's `Display` **string** is observable from Dart, so **changing a `#[error("…")]` string is a breaking change** (pinned by characterization tests).

**Additive = OK:** new functions, new structs/enums, new optional fields on *internal* types. **Break = removing/renaming/retyping/reordering anything in the contract, changing an error string, or changing the set/sequence of streamed events.** If a "better design" would break the contract, don't apply it — record it as a future Cake-Wallet-coordinated proposal.

**Safe to refactor freely** (NOT the contract): `#[frb(ignore)]` items (`start_scan_with_handler`, `send_transaction_with_handler`), `pub(crate)` items, private fns, and everything under `src/domain/`.

The only escape hatch is a coordinated break carrying the `breaking-api-approved` PR label. See `CONTRIBUTING.md`.

## Architecture

Two layers (full detail in `docs/ARCHITECTURE.md`):

- **`src/api/`** — the FRB bridge boundary (the *only* layer FRB scans, per `rust_input: crate::api`). A thin adapter: parse/validate `#[frb]` inputs → fetch process globals (DB pool/path, network) → call into `domain` → map result to a `*Dto` and the Dart-visible error. Owns the wire DTOs (`*Dto` structs/enums) and their `From` conversions to/from `minotari` types. **Keep the `*Dto` + `From` seam** — don't replace DTOs with domain types at the boundary.
- **`src/domain/`** — pure, bridge-free, global-state-free, unit-testable logic (`address`, `keys`, `validation`). No `#[frb]`, `pub(crate)` only. Dependencies (`Network`, secrets, computed values) flow in as **parameters**, never via globals.

**Process-global state** (each encapsulated behind one module; a `std::sync::RwLock` guard is **never held across `.await`**):
- **DB singleton** (`src/api/db.rs`) — SQLite pool + path behind `static RwLock<Option<Database>>`; access only via `get_db_connection`/`get_db_path`/`get_db_pool`. `disconnect_database` does a graceful shutdown (cancels any in-flight scan before dropping the pool).
- **Scan lifecycle** (`src/api/scanner.rs`) — single "current scan" slot with cancel token + forwarder `JoinHandle`. Latest-wins (a new scan cancels+awaits the one it replaces); a scan only clears the slot if it still owns it (matched by monotonic `id`).
- **Network** (`src/api/network.rs`) — write-once global in `tari_common`. `parse_network(None)` resolves to `Network::MainNet` — **frozen behavior** Cake Wallet depends on. `apply_network` is the single choke-point for installing it.

**Other invariants:** all numeric/string defaults live once in `src/api/config.rs` (`pub(crate)`, value-guarded by tests). Secrets (seed words, passphrases, keys) are moved into zeroizing containers *inside* fn bodies without changing public field types; `SensitiveSeeds` is `#[frb(opaque)]` and crosses only via `reveal_seed_words`. Two streaming fns (`send_transaction`, `start_scan`) emit over an unbounded `mpsc` (lossless, in-order). Sink-closed is deliberately asymmetric: **scan cancels** on a closed sink, **send continues** (aborting a half-broadcast tx could lose funds).

## The codegen workflow (most important contributor rule)

Rust source and the generated bridge (`src/frb_generated.rs` and `.dart/**`) are both committed and must stay in lockstep:

```sh
# 1. Edit the bridge surface under src/api/** (or its doc comments).
make gen          # 2. regenerate src/frb_generated.rs and .dart/**
# 3. Commit the regenerated output in the SAME change.
```

CI fails if the committed bridge is stale (`make gen` drift) or if `.dart/api/**` changed in a breaking way (`scripts/check_api_stability.sh` diffs against the PR base). The `flutter_rust_bridge_codegen` CLI **must match** the FRB runtime pinned in `Cargo.toml` (`=2.11.1`); a mismatched CLI reformats output and causes spurious drift. Install: `cargo install flutter_rust_bridge_codegen --version 2.11.1` (or `make install`).

## Commands

```sh
cargo build --workspace --all-targets                                 # host build = the merge gate (validates all 3 crate types)
cargo test --workspace --all-features                                 # unit (colocated) + verify harness Tier A/B
make fmt-check                                                         # cargo fmt --all -- --check
make lint                                                             # clippy --all-targets --all-features -D warnings
make gen                                                              # regenerate the bridge (see above)
make record-fixtures                                                  # rebuild verify fixtures (Tier A DB + Tier B RPC)
make verify-live                                                      # Tier C live-testnet smoke runner (opt-in, needs env secrets; NEVER on PRs)
make help                                                             # full target list
```

Run a single test: `cargo test --workspace <test_name_substring>` (e.g. `cargo test parse_network_none_is_mainnet`). Cross-compiling for Android/iOS is out of scope for CI; the host build is the gate.

## Testing

Unit tests are **colocated** in inline `#[cfg(test)] mod tests` so they can exercise private fns/types without widening visibility — do **not** make an item `pub` just to test it. Tests cover pure logic and type conversions only (no network, DB, or globals); fixtures are deterministic (addresses derived in-test from fixed key bytes, never real funded addresses). Test modules opt out of the strict wallet lints with a module-level `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]`.

**Contract-guard tests** (in `docs/TESTING.md`) encode externally-observed behavior of the frozen API (e.g. `parse_network_none_is_mainnet`, error-string characterizations, the `From` upstream-drift tripwires for `minotari` enums). Flipping one is a breaking change — reconsider the change rather than editing the test.

The `verify/` workspace member is the end-to-end harness: hermetic **Tier A** (fixture replay) and **Tier B** (recorded RPC) run on every PR; opt-in **Tier C** (live testnet) is gated behind the `live-e2e` feature.

## Lint policy

`src/api/**` carries strict wallet lints (`unwrap_used`, `expect_used`, `panic`, `print_stdout`, …); the generated `frb_generated` module is excluded. CI runs `clippy --all-targets --all-features -- -D warnings`.

## Key dependencies

`minotari` is a **git pin** (`tari-project/minotari-cli`, specific rev); `tari_*` crates are versioned (`5.3.1-pre.0`). Bumping them is a coordinated procedure (see `CONTRIBUTING.md` → dependency & release management). Run `cargo deny check` per `deny.toml`.
