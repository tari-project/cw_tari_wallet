# Architecture

This document describes the post-refactor design of `rust_lib_flutter_rust_wallet`
— the layering, error model, secret handling, state management, and the
streaming/async contract. The single overriding constraint behind all of them
is the **frozen public API contract** (see the
[README](../README.md#-public-api-stability-read-this-first)).

## Layering: `api` vs `domain`

The crate is split into two layers:

- **`src/api/**` — the bridge boundary.** This is the only layer FRB scans
  (`rust_input: crate::api` in `flutter_rust_bridge.yaml`). It owns the public
  `#[frb]` functions, the wire DTOs (`*Dto` structs/enums), their `From`
  conversions to/from `minotari` types, and the streamed event types. It is a
  **thin adapter**: parse/validate the `#[frb]` inputs → fetch any process
  globals (DB pool/path, network) → call into `domain` → map the result to a DTO
  and the Dart-visible error.
- **`src/domain/**` — pure logic.** Bridge-free, global-state-free, and
  unit-testable in isolation. It carries no `#[frb]` annotations and is
  `pub(crate)`, so nothing in it reaches the generated bridge. Dependencies
  (`Network`, secrets, computed values) flow in as **parameters** — never via
  globals. Current modules: `address` (address construction), `keys` (key-manager
  derivation), `validation` (send-input validation).

The wire contract — the `*Dto` types and their `From` impls — lives in `api`;
`domain` only returns small internal value structs or a `WalletError`. This is the
"keep the `*Dto` + `From` seam" rule: build on it, don't replace DTOs with domain
types at the boundary.

```
Dart  ──FRB──▶  src/api/**  (adapter: parse → call domain → map DTO/error)
                     │
                     ▼
                src/domain/**  (pure: keys, address, validation)
```

## Error model

There is one crate-internal error type, **`WalletError`** (`src/api/error.rs`), a
`thiserror` enum. It is **never** a public `#[frb]` return type: every bridge
function returns `anyhow::Result<T>`, and a `WalletError` reaches Dart only after
being converted into an `anyhow::Error` (via `impl From<WalletError> for
anyhow::Error`). FRB surfaces the thrown error's `Display` **string** to Dart, and
nothing else.

Consequence: the `#[error("…")]` strings on `WalletError` reproduce the exact
Dart-visible messages that predate the type (the legacy `TransactionError` texts
and scattered `anyhow!`/`.context(…)` strings). **Changing an error string is a
breaking change to the contract** and is pinned by characterization tests
(e.g. `"Wallet Error: Amount must be greater than zero"`,
`"Invalid network option: …"`, `"Database is not initialized"`).

## Secret handling

The crate handles three classes of secret: seed words/mnemonics, passphrases, and
private keys. The full threat model — what is and is not defended — lives in
[THREAT_MODEL.md](./THREAT_MODEL.md). In brief:

- Plaintext secrets are moved into zeroizing containers (`Zeroizing<…>`,
  `Hidden<…>`, or the `ZeroizeOnDrop` `SensitiveSeeds`) **inside** function bodies,
  so the copies *we* make are wiped on drop. This never changes a public field
  type — `SendTransactionDetails.seed_words: Vec<String>` and
  `ScanConfiguration.passphrase: String` stay byte-for-byte, because changing them
  would break the frozen contract.
- `SensitiveSeeds` is `#[frb(opaque)]`: seed words cross the bridge only through
  the explicit `reveal_seed_words` call.
- No secret-bearing struct prints its secret via `Debug`/`Display`, and
  `WalletError` never places secret material in its messages (enforced by a unit
  test). `SendTransactionDetails` and `ScanConfiguration` intentionally do not
  derive `Debug`.

The known, accepted residual exposure is FRB's own field buffers (FRB copies
bridged struct fields across FFI into buffers it controls and does not zeroize);
eliminating it would require an additive, Cake-Wallet-coordinated API change.

## State management

Two pieces of process-global mutable state, each encapsulated behind one module so
the "is it initialized / is one running?" check lives in exactly one place.

### Database singleton (`src/api/db.rs`)

The SQLite connection pool plus its on-disk path are held in a single typed
`Database` behind a private `static RwLock<Option<Database>>`. Every access goes
through a `pub(crate)` accessor (`get_db_connection` / `get_db_path` /
`get_db_pool`), each a thin wrapper over `Database::with_current`, which returns
`WalletError::NotInitialized` when no DB is installed. `initialize_database`
installs the pool; `disconnect_database` tears it down. **No lock guard is ever
held across an `.await`** — accessors clone a cheap handle or pull a pooled
connection under a short synchronous lock, then release it.

`disconnect_database` performs a **graceful shutdown**: it first cooperatively
cancels any in-flight scan (so the scan stops issuing DB queries) before dropping
the pool, rather than yanking connections out from under a live scan.

### Scan lifecycle (`src/api/scanner.rs`)

A single "current scan" slot (`static RwLock<Option<ScanController>>`) tracks the
in-flight scan's cancellation token and the `JoinHandle` of its event-forwarder
task. Each controller carries a monotonic `id`.

- **Latest-wins.** Starting a new scan installs a new controller and cleanly
  cancels + awaits the one it replaced, so a superseded scan never orphans its
  forwarder task.
- **Tracked task.** The forwarder handle is tracked (not detached), so `stop_scan`
  and teardown can cancel and join it.
- **Safe clean-up.** A scan only clears the slot if it still holds *its own*
  controller (matched by `id`), so a scan that has already been superseded never
  clobbers its successor.
- As with the DB, the `std::sync::RwLock` guard is never held across `.await`.

### Network selection (`src/api/network.rs`)

`tari_common` keeps the active network in a write-once global
(`Network::set_current`), and address/consensus derivation deep inside the Tari
libraries reads it via `Network::get_current()`. Two helpers centralize the
handling:

- **`parse_network(Option<TariNetwork>) -> Network`** resolves the caller's choice.
  `None` resolves to `Network::MainNet` — **frozen behavior** (ledger D3) that
  Cake Wallet depends on. The fallback is now *logged* (a warning) but the resolved
  value and signature are unchanged.
- **`apply_network(Network)`** is the single choke-point for installing the global
  via `set_network_if_choice_valid` (idempotent for the same value; an error on a
  conflicting value). Determinism does not depend on call order, because per-call
  derivation (e.g. address building) also takes `Network` as an explicit parameter.

## Configuration & constants

All numeric/string defaults consumed by the `api` layer live exactly once in
`src/api/config.rs` (`pub(crate)`, never a public surface):

| Constant | Value | Meaning |
|----------|-------|---------|
| `DEFAULT_CONFIRMATION_WINDOW` | `3` | confirmations (blocks) when unspecified |
| `DEFAULT_BASE_URL` | `https://rpc.tari.com` | base-node RPC when `base_url` omitted |
| `DEFAULT_PASSPHRASE` | `""` | passphrase fallback when `None` |
| `SECONDS_TO_LOCK_UTXO` | `86_400` (24h) | UTXO lock duration on send |
| `DEFAULT_NUM_OUTPUTS` | `1` | outputs assumed for fee estimation |

These values are byte-for-byte what they were before consolidation and are pinned
by value-guard tests. `DEFAULT_BASE_URL` is **not** derived from the requested
network (deriving it would change observable behavior — a deferred, coordinated
change).

## Streaming / async contract

Two functions stream progress to Dart over an FRB `StreamSink`:
`send_transaction` (events: `SendTransactionEvent`) and `start_scan` (events:
`ScanEventDto`). The async/stream behavior is observable and therefore frozen.

- **Ordering & backpressure (scan).** The scanner emits over an **unbounded**
  `mpsc` channel, so the producer never blocks and **no event is dropped** — events
  reach Dart strictly in emission order via `StreamSink::add`. The only
  backpressure is memory: a slow Dart consumer lets the queue grow. The terminal
  `Completed`/`Error` status is just another event in that lossless queue and is
  never dropped.
- **Cancellation.** A scan ends when its `rx` closes (drains queued events first,
  including the terminal status), when the cancel token fires (`stop_scan`, a
  superseding scan, or teardown), or when the Dart sink is closed.
- **Sink-closed semantics — deliberately asymmetric:**
  - **Scan cancels on a closed sink** — a scan with no listener has no useful work.
  - **Send continues on a closed sink** — a half-built/half-broadcast transaction
    must finish (aborting it could lose funds).

  These two are intentionally different; unifying them would be an observable
  behavior break.
- **Terminal events.** A scan failure emits exactly one `ScanEventDto::Error` and
  then resolves to `Err`. A send resolves to the broadcast `DisplayedTransactionDto`
  or an `Err`.

## Lint policy

`src/api/**` carries the stricter wallet lints (`unwrap_used`, `expect_used`,
`panic`, `print_stdout`, …); the generated `frb_generated` module is excluded.
Test modules opt out with a module-level
`#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` so assertions
read naturally while production code stays under the strict policy. CI runs
`clippy --all-targets --all-features -- -D warnings`.
</content>
