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

### Security

- Remediated five transitive RUSTSEC advisories via semver-compatible lockfile
  bumps (no public-API or bridge change): `time` 0.3.44 → 0.3.47
  (RUSTSEC-2026-0009, RFC 2822 parsing stack-exhaustion DoS) and `rustls-webpki`
  0.103.8 → 0.103.13 (RUSTSEC-2026-0049 / -0098 / -0099 / -0104). Pruned those five
  IDs from `deny.toml`'s ignore list; only the five remaining *unmaintained*
  transitive advisories (no upstream fix available) are still ignored.
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
