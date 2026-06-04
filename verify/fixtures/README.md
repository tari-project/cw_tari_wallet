# Verification fixtures

Committed test assets for the end-to-end verification harness. **Do not edit by
hand** — regenerate them with:

```sh
make record-fixtures        # or: cargo run -p verify -- record-fixtures
```

## `wallet.db` — Tier A view-only fixture wallet

A small, **view-only**, deterministic wallet database.

- **Provenance:** built programmatically by `verify::fixture::build_fixture_db`.
  It is **not** a capture of a real, funded wallet. The account is imported with a
  view **private** key and a spend **public** key derived from fixed, non-secret
  test bytes (`verify/src/fixture.rs`), so it holds **no seed words and no spend
  key** — it is structurally impossible to spend from, and committing it leaks
  nothing.
- **Contents:** one account (`verify-fixture`), three synthetic balance-change
  rows (two credits + one debit ⇒ golden total `6_500_000 µT`), and one synthetic
  confirmed coinbase transaction (id `42`, `5_000_000 µT`).
- **Important — not byte-reproducible:** the encrypted account blob and SQLite
  internals embed per-build nonces/timestamps, so regenerating produces a
  byte-different file. The committed copy is a **documentation / inspection
  snapshot only**; it is **not** asserted byte-for-byte and CI does not diff it.
  The Tier A tests do **not** read this file — they call
  `verify::fixture::materialize_fixture_db()` to rebuild an identical DB into a
  temp dir at test time, so test determinism comes from the recorder (the source
  of truth), not from these committed bytes.

## `rpc/*.json` — Tier B recorded RPC captures

Recorded `get_tip_info` responses, replayed by a local `wiremock` server in the
Tier B tests (pointed at via the `base_url` parameter).

- **Provenance:** produced by serializing a real
  `tari_transaction_components::rpc::models::TipInfoResponse` (so the JSON is
  exactly the shape the upstream type emits) with fixed golden values.
- `get_tip_info_synced.json` — `is_synced: true`, tip height `250_000`.
- `get_tip_info_unsynced.json` — `is_synced: false`.
- These **are** byte-deterministic; regenerate them whenever the upstream RPC
  shape changes.
