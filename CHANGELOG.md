# Changelog

All notable changes to `rust_lib_flutter_rust_wallet` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## Versioning policy

This crate's version (`Cargo.toml` `version`) tracks the **public Dart-facing API
contract** consumed by [Cake Wallet](https://cakewallet.com), not the internal Rust
refactors. The contract is every non-`#[frb(ignore)]` `pub` item in `crate::api`
plus every input/output struct/enum, streamed event type, and Dart-visible error
string (see [README](./README.md#-public-api-stability-read-this-first) and
[CONTRIBUTING.md](./CONTRIBUTING.md#public-api-stability-policy)).

Because the contract is **frozen**, the practical mapping is intentionally narrow:

- **MAJOR** (`X.0.0`) — a deliberate, breaking change to the public contract. This
  is forbidden under the normal process; it is only ever made via a
  Cake-Wallet-coordinated migration, with a migration note in this file and the
  `breaking-api-approved` review trail on the PR (see CONTRIBUTING.md). Until that
  happens, the major version does not move.
- **MINOR** (`0.X.0`) — an **additive** change to the contract: a new bridge
  function, a new struct/enum, or a new optional internal field. These never break
  an existing consumer.
- **PATCH** (`0.0.X`) — internal-only changes with **no** observable effect on the
  contract: refactors, dependency bumps (incl. `minotari`/`tari_*`), CI/tooling,
  docs, and security fixes that do not alter the bridge. A dependency bump that
  would leak a contract change is not a patch — the CI bridge codegen-drift and
  public-API-stability guards are the gate, and a leaked change must be reverted or
  promoted to a coordinated MAJOR.

Every release must pass the full gate (`cargo build`, `cargo test`, `cargo fmt
--check`, `cargo clippy`, `cargo deny check`, and `make gen` with a zero `.dart/**`
diff) before the version is tagged.

## [Unreleased]

### Changed

- Bumped the `minotari-cli` git pin from `52a7287` to `af78477` (tip of its
  default `main` branch) and the `tari_*` crates from `5.3.1-pre.0` to
  `5.7.0-pre.8`, in both `Cargo.toml` and `verify/Cargo.toml` (the pins stay
  identical). `tari_crypto` moved 0.23.0 → 0.23.3 within the existing requirement;
  `tari_utilities` is unchanged at 0.10.0. Note `5.7.0-pre.9` is not published on
  crates.io; `5.7.0-pre.8` is the latest and is what the new `minotari` rev pins.

  **No contract change.** The full test suite (every contract guard plus the
  Tier A/B harness) passes unedited. `make gen` changes only flutter_rust_bridge's
  *ignored-item comment lines* in `.dart/api/scanner.dart` (the new private
  `map_scan_status_event` is listed as not-`pub`, and one fewer `From` trait impl
  is listed) — **no declaration line changes**, and
  `scripts/check_api_stability.sh` reports "removals were non-declaration lines
  only".

  The bump does, however, change **runtime behaviour** in three ways that Cake
  Wallet should know about. None of them alters a type, field, variant, event or
  error string, so none is a contract break — but each is observable:

- **`estimate_transaction_fee` now quotes materially higher fees, and input
  selection shifts with it.** Upstream rewrote
  `get_default_features_and_scripts_size()`
  (`minotari/src/transactions/fee_estimator.rs`): it previously measured
  `features + an empty script + covenant`, and now measures a realistic change
  output — a `PushPubKey` script plus a `TransactionInfo` memo (dual address, one
  sent-output hash, payment id) padded to a **130-byte floor**. That size is
  charged once per output *and* once for change, and the same value feeds
  `input_selector.fetch_unspent_outputs`, so both the quoted fee and the set of
  UTXOs selected change. This is an upstream **bug fix**, not a regression: the
  old estimate under-counted, so selection could lock inputs that could not cover
  the fee the builder later computed and the send then failed *after* the funds
  were reserved. Expect a step change in quoted fees; no action needed on the Dart
  side.

- **Scanning can now fail on a non-contiguous block batch.** The new rev added a
  strict contiguity check in `minotari/src/scan/scan_db_handler.rs`: the first
  block of a batch must equal `next_block_to_scan`, and each subsequent height
  must be `last` or `last + 1` (a repeated height is legitimate — a large block is
  split across several results). Otherwise the scan aborts with
  `WalletDbError::Unexpected("Base node returned a non-contiguous block batch for
  account …")`, which surfaces as a terminal `ScanEventDto::Error` and `start_scan`
  resolving `Err`. This guards a real correctness bug (a height jump ages pending
  outputs to "confirmed and mature" in one step, letting the wallet spend
  unconfirmed funds), but it is on **exactly** the `ScanMode::Full` /
  `ScanMode::Continuous` path this bridge drives, so a misbehaving or
  out-of-sync base node that previously scanned through will now terminate the
  scan. If a support report mentions "non-contiguous block batch", it traces to
  this bump.

- `ScanStatusDto::Paused` with reason `Cancelled` can now be emitted mid-batch as
  well as at loop-top (upstream `coordinator.rs`), so cancellation is observed
  sooner. The event type and the set of streamed events are unchanged.

### Security

- **`send_transaction` now requires the supplied seed words to match the stored
  wallet account.** This is a **new precondition** introduced by the bump; callers
  that previously got away with a mismatch will now see the send fail.

  The offline-signing payload version moved 4.0.0 → 5.0.0 and gained a
  `PayloadIntegritySignature`. `prepare_one_sided_transaction_for_signing` signs
  the canonical Borsh payload with the *preparing* key manager's **view key**, and
  `sign_locked_transaction` now calls `verify_payload_signature` **before** it
  signs, checking that signature against the *signing* key manager's view key
  (`tari_transaction_components-5.7.0-pre.8/src/offline_signing/offline_signer.rs`).

  In this bridge those two key managers come from **different sources**: the
  payload is prepared by the `TransactionSender` built from the DB account
  (`SendTransactionDetails.wallet_name` + `passphrase`), while signing uses
  `key_manager_from_seed_words(&details.seed_words)`. The two therefore have to
  derive the same view key. When they do not, the send fails at the signing stage
  and Dart receives the existing `Signing Error: …` variant carrying upstream's
  message ("Offline payload integrity check failed: payload signature is
  invalid…"). No error string in this crate was added or changed.

  Note the check is a *tamper* guard, not an authorisation one: upstream documents
  that the view key is shareable, so passing it means "the payload was not mangled
  by someone without view access", not "the payload is what the owner asked for".

- `send_transaction` no longer materialises a non-zeroizing plaintext copy of the
  wallet passphrase. `TransactionSender::new` now accepts an owned
  `Zeroizing<String>`, so the zeroizing container is moved end-to-end instead of
  being converted back to a bare `String` at the call boundary.
- The `minotari`/`tari_*` bump dropped `proc-macro-error`, `proc-macro-error2`,
  `paste` and `structopt` from the dependency tree, clearing four *unmaintained*
  advisories. Pruned the two now-stale ignores (RUSTSEC-2024-0370,
  RUSTSEC-2024-0436) from `deny.toml`; three unmaintained entries remain.

- Remediated five transitive RUSTSEC advisories via semver-compatible lockfile
  bumps (no public-API or bridge change): `time` 0.3.44 → 0.3.47
  (RUSTSEC-2026-0009, RFC 2822 parsing stack-exhaustion DoS) and `rustls-webpki`
  0.103.8 → 0.103.13 (RUSTSEC-2026-0049 / -0098 / -0099 / -0104). Pruned those five
  IDs from `deny.toml`'s ignore list, leaving five *unmaintained* transitive
  advisories ignored at the time (since reduced to three by the `minotari`/`tari_*`
  bump above).
- Updated the yanked `keccak` 0.1.5 → 0.1.6 in the lockfile.

### Added

- Dependency-update automation via Renovate (`renovate.json`): weekly grouped
  crates.io PRs; `flutter_rust_bridge` and the `tari_*` crates flagged as
  manual-review (no auto-merge); a custom manager that proposes git-rev updates for
  the pinned `minotari-cli` dependency. Every PR is gated by the CI matrix.
- `CHANGELOG.md` (this file) and a written versioning policy tied to the frozen
  contract.
- Dependency, `minotari` bump, and release policy documented in
  [CONTRIBUTING.md](./CONTRIBUTING.md#dependency--release-management).

## [0.1.0]

Initial backend surface: database lifecycle; wallet create/restore/import/list/
rename/delete; address and balance reads; transaction history; fee estimation;
one-sided send (streamed); blockchain scanning (streamed); base-node tip/sync
queries; and logging. This is the baseline frozen contract.
