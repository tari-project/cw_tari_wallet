use crate::api::error::WalletError;
use crate::api::transactions::DisplayedTransactionDto;
use crate::{api::db::get_db_path, frb_generated::StreamSink};
use anyhow::{anyhow, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::scan::{DisplayedTransactionsEvent, TransactionsUpdatedEvent};
use minotari_wallet::{ProcessingEvent, ScanMode, ScanStatusEvent, Scanner};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// The handle to the one in-flight scan: its cancellation token plus the
/// `JoinHandle` of the event-forwarder task spawned for it.
///
/// Tracking the forwarder handle (rather than dropping it, as the original code
/// did) lets a superseding scan — and `stop_scan` — cancel **and await** the
/// previous scan's forwarder so it doesn't orphan. Each controller carries a
/// monotonic `id`: clean-up only ever clears the slot if it still holds the
/// controller it expects, so a later scan that has already replaced us is never
/// clobbered.
struct ScanController {
    id: u64,
    token: CancellationToken,
    forwarder: JoinHandle<()>,
}

/// The single process-global "current scan" slot. `None` when no scan is running.
/// Private to this module; every access goes through the helpers below, and the
/// `std::sync::RwLock` guard is **never held across an `.await`** (we take/clone
/// what we need, drop the guard, then await) — holding a sync lock across an await
/// could deadlock the async runtime.
static SCAN_STATE: RwLock<Option<ScanController>> = RwLock::new(None);

/// Hands out a unique, monotonically increasing id per `start_scan` so clean-up
/// can tell "is the controller in the slot still mine?".
static SCAN_ID: AtomicU64 = AtomicU64::new(0);

/// Cancel the in-flight scan, if any.
///
/// Signals the current scan's cancellation token so the scan future and its event
/// forwarder wind down. A no-op when no scan is running. Synchronous; returns once
/// cancellation has been signalled (it does not block until the scan task exits).
#[frb]
pub fn stop_scan() -> Result<()> {
    cancel_active_scan()
}

/// Cancel the in-flight scan (if any): signal its cancel token so the scan future
/// and the forwarder both wind down, and `abort()` the forwarder as a backstop
/// against a stuck `recv()`. Synchronous and lock-free across `.await` — it takes the
/// controller out under a short write lock, drops the guard, then cancels/aborts.
///
/// Shared by [`stop_scan`] and by `disconnect_database` (graceful shutdown): tearing
/// the DB pool out from under a live scan would make the scan fail mid-operation, so
/// teardown first *cooperatively cancels* the scan here. We do not block-await the
/// scan task from this synchronous boundary (that would risk deadlocking the FRB async
/// runtime); cancellation is sufficient to stop further DB access, and the scan's own
/// task clears its state on exit (`clear_scan_if_current`).
pub(crate) fn cancel_active_scan() -> Result<()> {
    // Take the controller out under a short write lock, then cancel/abort it
    // *after* releasing the lock (no async work happens under the sync guard).
    let controller = take_scan_controller()?;
    if let Some(controller) = controller {
        controller.token.cancel();
        // The forwarder will observe the cancellation (token fires / `rx` closes) and
        // finish; abort as a backstop so a stuck `recv()` can't leak the task.
        controller.forwarder.abort();
    }
    Ok(())
}

/// Remove and return the current scan controller (if any), releasing the lock
/// before the caller does any async/cancellation work.
fn take_scan_controller() -> Result<Option<ScanController>> {
    let mut guard = SCAN_STATE
        .write()
        .map_err(|_| WalletError::internal("Failed to acquire lock"))?;
    Ok(guard.take())
}

/// Install `controller` as the current scan, returning the one it replaced (if
/// any) so the caller can cancel/await it outside the lock.
fn install_scan_controller(controller: ScanController) -> Result<Option<ScanController>> {
    let mut guard = SCAN_STATE
        .write()
        .map_err(|_| WalletError::internal("Failed to acquire lock"))?;
    Ok(guard.replace(controller))
}

/// Clear the slot **only if** it still holds the controller with `id` (i.e. a
/// newer `start_scan` hasn't already taken over). Returns the removed controller
/// so its forwarder can be awaited/aborted by the caller outside the lock.
fn clear_scan_if_current(id: u64) -> Option<ScanController> {
    let Ok(mut guard) = SCAN_STATE.write() else {
        // A poisoned lock means another thread panicked mid-update. There is
        // nothing safe to clean up here; surface nothing and let the slot be.
        return None;
    };
    if guard.as_ref().is_some_and(|c| c.id == id) {
        guard.take()
    } else {
        None
    }
}

/// An event streamed during [`start_scan`].
///
/// `Status` carries scan progress ([`ScanStatusDto`]), `TransactionsReady` newly
/// discovered transactions, `TransactionsUpdated` status changes to known ones,
/// and `Error` a terminal failure message (the scan also resolves to `Err`).
#[frb]
#[derive(Debug, Clone)]
pub enum ScanEventDto {
    Status(ScanStatusDto),
    TransactionsReady(TransactionsReadyDto),
    TransactionsUpdated(TransactionsUpdatedDto),
    Error(String),
}

/// Progress detail for a scan, carried inside [`ScanEventDto::Status`].
///
/// Heights are block heights and `account_id` identifies the scanned account.
#[frb]
#[derive(Debug, Clone)]
pub enum ScanStatusDto {
    Started {
        account_id: i64,
        from_height: u64,
    },
    Progress {
        account_id: i64,
        current_height: u64,
        blocks_scanned: u64,
    },
    Completed {
        account_id: i64,
        final_height: u64,
        total_blocks_scanned: u64,
    },
    Paused {
        account_id: i64,
        last_scanned_height: u64,
        reason: String,
    },
    Waiting {
        account_id: i64,
        resume_in_seconds: u64,
    },
    MoreBlocksAvailable {
        account_id: i64,
        last_scanned_height: u64,
    },
}

impl From<ScanStatusEvent> for ScanStatusDto {
    fn from(e: ScanStatusEvent) -> Self {
        match e {
            ScanStatusEvent::Started {
                account_id,
                from_height,
            } => ScanStatusDto::Started {
                account_id,
                from_height,
            },
            ScanStatusEvent::Progress {
                account_id,
                current_height,
                blocks_scanned,
            } => ScanStatusDto::Progress {
                account_id,
                current_height,
                blocks_scanned,
            },
            ScanStatusEvent::Completed {
                account_id,
                final_height,
                total_blocks_scanned,
            } => ScanStatusDto::Completed {
                account_id,
                final_height,
                total_blocks_scanned,
            },
            ScanStatusEvent::Paused {
                account_id,
                last_scanned_height,
                reason,
            } => ScanStatusDto::Paused {
                account_id,
                last_scanned_height,
                reason: format!("{:?}", reason),
            },
            ScanStatusEvent::Waiting {
                account_id,
                resume_in,
            } => ScanStatusDto::Waiting {
                account_id,
                resume_in_seconds: resume_in.as_secs(),
            },
            ScanStatusEvent::MoreBlocksAvailable {
                account_id,
                last_scanned_height,
            } => ScanStatusDto::MoreBlocksAvailable {
                account_id,
                last_scanned_height,
            },
        }
    }
}

/// Newly discovered transactions, carried inside
/// [`ScanEventDto::TransactionsReady`].
///
/// `block_height` is the block they were found at (when known); `is_initial_sync`
/// is `true` while catching up history versus live tip-following.
#[frb]
#[derive(Debug, Clone)]
pub struct TransactionsReadyDto {
    pub account_id: i64,
    pub transactions: Vec<DisplayedTransactionDto>,
    pub block_height: Option<u64>,
    pub is_initial_sync: bool,
}

impl From<DisplayedTransactionsEvent> for TransactionsReadyDto {
    fn from(e: DisplayedTransactionsEvent) -> Self {
        Self {
            account_id: e.account_id,
            transactions: e.transactions.into_iter().map(Into::into).collect(),
            block_height: e.block_height,
            is_initial_sync: e.is_initial_sync,
        }
    }
}

/// Status updates to already-known transactions (e.g. newly confirmed), carried
/// inside [`ScanEventDto::TransactionsUpdated`].
#[frb]
#[derive(Debug, Clone)]
pub struct TransactionsUpdatedDto {
    pub account_id: i64,
    pub updated_transactions: Vec<DisplayedTransactionDto>,
}

impl From<TransactionsUpdatedEvent> for TransactionsUpdatedDto {
    fn from(e: TransactionsUpdatedEvent) -> Self {
        Self {
            account_id: e.account_id,
            updated_transactions: e.updated_transactions.into_iter().map(Into::into).collect(),
        }
    }
}

/// Inputs to [`start_scan`].
///
/// `wallet_name` selects the account; `passphrase` (**secret**) decrypts it;
/// `base_url` is the base node RPC endpoint; `batch_size` is the number of blocks
/// fetched per request; `continuous` keeps following the tip after catching up;
/// `poll_interval_seconds` is the wait between polls in continuous mode;
/// `required_confirmations` is the confirmation depth in blocks.
#[frb]
pub struct ScanConfiguration {
    pub wallet_name: String,
    pub passphrase: String,
    pub base_url: String,
    pub batch_size: u64,
    pub continuous: bool,
    pub poll_interval_seconds: u64,
    pub required_confirmations: u64,
}

/// Map a raw scanner [`ProcessingEvent`] to the public [`ScanEventDto`] the Dart
/// side pattern-matches on. Returns `None` for processing events that intentionally
/// have **no** Dto representation (so the streamed set/sequence Cake Wallet receives
/// is exactly the events that already had a Dto — adding/dropping a `Some` here would
/// change the frozen event contract; do not).
fn map_processing_event(event: ProcessingEvent) -> Option<ScanEventDto> {
    match event {
        ProcessingEvent::ScanStatus(status) => Some(ScanEventDto::Status(status.into())),
        ProcessingEvent::TransactionsReady(e) => Some(ScanEventDto::TransactionsReady(e.into())),
        ProcessingEvent::TransactionsUpdated(e) => {
            Some(ScanEventDto::TransactionsUpdated(e.into()))
        }
        _ => None,
    }
}

/// The event-forwarder loop, factored out so it can be driven hermetically in tests
/// with an in-memory channel and a fake callback.
///
/// ## Backpressure & ordering policy
/// `rx` is the scanner's **unbounded** `mpsc` receiver (`run_with_events()` returns
/// `UnboundedReceiver<ProcessingEvent>`), so the producer never blocks and the
/// channel itself **never drops events** — events reach the callback strictly in the
/// order the scanner emitted them. The only backpressure is memory: a slow Dart
/// consumer lets the unbounded queue grow. We deliberately keep this behavior (the
/// step's "preserve current behavior — do not introduce dropping" decision); the
/// terminal `Completed`/`Error` status is just another event in that ordered, lossless
/// queue, so it is **never dropped**. Delivery to Dart is via `StreamSink::add` exactly
/// as before — this function does not change the `StreamSink` mechanism.
///
/// ## Cancellation & sink-closed semantics
/// The loop ends on any of three conditions, each preserving the existing observable
/// behavior:
/// 1. `rx` closes (the scan future finished and dropped the sender) — drains all
///    still-queued events first, including the terminal status, then exits.
/// 2. The callback returns `Err` (the Dart `StreamSink` is closed) — we cancel the
///    scan token and stop. **Scan cancels on sink-closed** (contrast with `send`,
///    which continues — see `send_transaction`); these intentionally differ.
/// 3. The shared cancel token fires (`stop_scan`, a superseding scan, or graceful
///    teardown) — we stop promptly without waiting for `rx` to close, so cancellation
///    reaches the forwarder directly and not only via the callback-error path.
async fn run_forwarder<F>(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ProcessingEvent>,
    cancel_token: CancellationToken,
    callback: Arc<F>,
) where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    loop {
        let event = tokio::select! {
            // Bias towards draining queued events: when both the channel has data
            // and the token is cancelled, prefer delivering the buffered event so a
            // terminal status that raced cancellation still gets through.
            biased;
            maybe = rx.recv() => match maybe {
                Some(event) => event,
                // Sender dropped: channel fully drained, nothing left to forward.
                None => break,
            },
            // Direct cancellation (stop_scan / supersede / teardown). Stop promptly.
            () = cancel_token.cancelled() => break,
        };

        if let Some(dto) = map_processing_event(event) {
            if callback(dto).is_err() {
                // The Dart StreamSink is closed. Cancel the scan (scan-cancels-on-
                // sink-closed) and stop forwarding. We do NOT propagate this Err to
                // Dart: it is an internal signal, typed as `WalletError::Scan` at the
                // `start_scan` boundary purely for internal consistency.
                cancel_token.cancel();
                break;
            }
        }
    }
}

#[frb(ignore)]
pub async fn start_scan_with_handler<F>(config: ScanConfiguration, event_callback: F) -> Result<()>
where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    let db_path = get_db_path()?;

    let cancel_token = CancellationToken::new();
    let scan_id = SCAN_ID.fetch_add(1, Ordering::Relaxed);

    let mode = if config.continuous {
        ScanMode::Continuous {
            poll_interval: Duration::from_secs(config.poll_interval_seconds),
        }
    } else {
        ScanMode::Full
    };

    // The passphrase arrives in the frozen public `ScanConfiguration.passphrase`
    // (plain `String`). Move a copy into a zeroizing container so this function's
    // local plaintext is wiped on drop, and pass it to `Scanner::new` by `&`
    // (deref to `&str`) so nothing else clones it in the clear (Shared Contracts §3).
    let passphrase = Zeroizing::new(config.passphrase.clone());

    let scanner_builder = Scanner::new(
        &passphrase,
        &config.base_url,
        db_path,
        config.batch_size,
        config.required_confirmations,
    )
    .mode(mode)
    .account(&config.wallet_name)
    .cancel_token(cancel_token.clone());

    let (rx, scan_future) = scanner_builder.run_with_events();

    let loop_cancel_token = cancel_token.clone();

    let event_callback = Arc::new(event_callback);
    let callback_for_spawn = event_callback.clone();

    let forwarder = tokio::spawn(run_forwarder(rx, loop_cancel_token, callback_for_spawn));

    // Install this scan as the current one. Any controller we replace is a
    // *previous* scan that was never stopped: preserve "latest scan wins" by
    // cleanly cancelling and awaiting it here (the original code silently
    // overwrote the slot, orphaning the old scan's forwarder task).
    let previous = install_scan_controller(ScanController {
        id: scan_id,
        token: cancel_token.clone(),
        forwarder,
    })?;
    if let Some(previous) = previous {
        previous.token.cancel();
        // Best-effort: let the old forwarder drain and exit; abort as a backstop.
        previous.forwarder.abort();
        let _ = previous.forwarder.await;
    }

    let result = scan_future.await;

    // Clear our state on **every** exit path (success, error, and — because this
    // runs after the await regardless of the `Result` — cancellation). Only clears
    // the slot if it still holds *our* controller, so a scan that has already
    // superseded us is left untouched.
    //
    // On this (our own) exit path we **await** the forwarder rather than abort it:
    // `scan_future` completing drops the event sender, which closes the channel, so
    // the forwarder drains any still-buffered events (e.g. the terminal status) and
    // then exits on its own. Awaiting therefore both delivers those final events
    // (preserving observable behavior — the original detached forwarder also drained
    // to completion) and joins the task so its handle never leaks.
    if let Some(controller) = clear_scan_if_current(scan_id) {
        // Awaiting joins the task so it can never leak. A panic inside the forwarder
        // would otherwise be silently swallowed by `JoinHandle`; surface it in the
        // log (it cannot poison any lock — the forwarder holds none across `.await`)
        // and continue to the terminal-event/return logic below.
        if let Err(join_err) = controller.forwarder.await {
            if join_err.is_panic() {
                log::error!("Scan event-forwarder task panicked: {join_err}");
            }
        }
    }

    // State is cleared *above*, before this fallible terminal-event callback, so the
    // scan slot is guaranteed empty even if `event_callback` itself fails here.
    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            // Terminal `Error` event for the stream (preserved exactly: scan failure
            // still emits one `ScanEventDto::Error` then returns `Err`).
            let _ = event_callback(ScanEventDto::Error(e.to_string()));
            Err(anyhow!(e))
        }
    }
}

/// Scan the blockchain for the wallet's transactions, streaming progress.
///
/// Streams [`ScanEventDto`]s over `sink` as blocks are scanned. With
/// `config.continuous` set, it keeps following the chain tip until cancelled via
/// [`stop_scan`]; starting a new scan supersedes any previous one (latest wins).
/// Requires [`initialize_database`] first.
///
/// Async and streamed. The scan **cancels when the Dart stream is closed** (a scan
/// with no listener has no useful work) — this deliberately differs from
/// [`send_transaction`](crate::api::send_transaction::send_transaction), which
/// continues. A scan failure emits a terminal [`ScanEventDto::Error`] and resolves
/// to `Err`.
#[frb]
pub async fn start_scan(sink: StreamSink<ScanEventDto>, config: ScanConfiguration) -> Result<()> {
    let stream_sink = sink.clone();

    start_scan_with_handler(config, move |event| {
        // Map sink-closed to a typed `WalletError::Scan`. This `Err` is consumed
        // internally by `run_forwarder` to detect closure and cancel the scan (the
        // scan-cancels-on-sink-closed rationale, and its contrast with SEND, lives
        // there) — it is never thrown to Dart, so its Dart-visible representation is
        // unchanged (there is none).
        stream_sink
            .add(event)
            .map_err(|e| WalletError::scan(format!("{e:?}")).into())
    })
    .await
}

#[cfg(test)]
mod tests {
    //! Tests for the scan-state seam (`ScanController` slot + its lifecycle
    //! helpers) introduced in the state-management refactor.
    //!
    //! The full `start_scan_with_handler` needs a real DB + a live `Scanner`, so it
    //! can't run hermetically; instead these tests drive the exact install / take /
    //! clear-if-current logic that implements the "latest-scan-wins without orphan"
    //! and "always clear on exit" guarantees, using real tokio tasks as stand-in
    //! forwarders.
    //!
    //! Because they all mutate the single process-global `SCAN_STATE`, the four
    //! scenarios run as **one** `#[tokio::test]` (sequential within a single task)
    //! rather than separate tests — separate tests would execute in parallel on the
    //! shared slot and race each other. Within the one test, each scenario takes the
    //! slot first to start from a known-empty state.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A forwarder stand-in: a task that lives until its token is cancelled, exactly
    /// like the real event-forwarder reacting to `rx` closing.
    fn spawn_forwarder(token: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            token.cancelled().await;
        })
    }

    fn make_controller(id: u64) -> (ScanController, CancellationToken) {
        let token = CancellationToken::new();
        let forwarder = spawn_forwarder(token.clone());
        (
            ScanController {
                id,
                token: token.clone(),
                forwarder,
            },
            token,
        )
    }

    #[tokio::test]
    async fn scan_state_lifecycle() {
        // Serialize against the DB tests: `disconnect_database` now also takes
        // `SCAN_STATE` (graceful shutdown), so this test and those must not run
        // concurrently or they would race the shared global. A `tokio::sync::Mutex`
        // guard is held across the `.await`s below without tripping
        // `await_holding_lock` (it does not poison either).
        let _guard = crate::api::db::GLOBAL_STATE_TEST_LOCK.lock().await;

        // --- Scenario 1: a second start cleanly supersedes the first (no orphan) ---
        let _ = take_scan_controller().unwrap();

        let (first, first_token) = make_controller(1);
        let replaced = install_scan_controller(first).unwrap();
        assert!(replaced.is_none(), "no prior scan to replace");
        assert!(!first_token.is_cancelled());

        // Second scan installs over it; we get the first back to clean up.
        let (second, _second_token) = make_controller(2);
        let previous = install_scan_controller(second).unwrap();
        let previous = previous.expect("the first scan must be returned for cleanup");

        // "latest wins without orphan": cancel + await the previous forwarder.
        previous.token.cancel();
        previous.forwarder.abort();
        let _ = previous.forwarder.await;
        assert!(first_token.is_cancelled(), "first scan must be cancelled");

        let current = take_scan_controller().unwrap();
        assert_eq!(current.map(|c| c.id), Some(2), "second scan is current");

        // --- Scenario 2: stop_scan cancels the active scan and clears the slot ---
        let _ = take_scan_controller().unwrap();

        let (controller, token) = make_controller(10);
        install_scan_controller(controller).unwrap();

        // Mirror `stop_scan`: take the controller, cancel + abort it.
        let taken = take_scan_controller().unwrap().expect("a scan was active");
        taken.token.cancel();
        taken.forwarder.abort();
        let _ = taken.forwarder.await;

        assert!(token.is_cancelled(), "active scan token must be cancelled");
        assert!(
            take_scan_controller().unwrap().is_none(),
            "slot must be cleared after stop"
        );

        // --- Scenario 3: clear-if-current only clears the matching generation ---
        let _ = take_scan_controller().unwrap();

        let (controller, _t) = make_controller(20);
        install_scan_controller(controller).unwrap();

        // A *stale* clean-up (an already-superseded scan's exit path) must NOT
        // clobber the current scan.
        assert!(
            clear_scan_if_current(19).is_none(),
            "stale id must not clear the slot"
        );
        assert!(
            take_scan_controller().unwrap().is_some(),
            "current scan must survive a stale clear"
        );

        // Re-install and clear with the matching id: slot is emptied.
        let (controller, _t2) = make_controller(21);
        install_scan_controller(controller).unwrap();
        let cleared = clear_scan_if_current(21);
        assert_eq!(
            cleared.map(|c| c.id),
            Some(21),
            "matching id clears the slot"
        );
        assert!(
            take_scan_controller().unwrap().is_none(),
            "slot empty after matching clear"
        );

        // --- Scenario 4: the exit-path clear runs on both success and failure ---
        // Both the Ok and Err arms of `start_scan_with_handler` run the same
        // `clear_scan_if_current` *before* the match, so the slot ends empty either
        // way. We model both by installing then clearing with the matching id.
        for id in [30u64, 31u64] {
            let _ = take_scan_controller().unwrap();
            let (controller, _t) = make_controller(id);
            install_scan_controller(controller).unwrap();

            if let Some(c) = clear_scan_if_current(id) {
                // Mirror the real normal-exit path: await (don't abort) so buffered
                // events drain. The stand-in forwarder ends when its token cancels.
                c.token.cancel();
                let _ = c.forwarder.await;
            }
            assert!(
                take_scan_controller().unwrap().is_none(),
                "slot must be cleared regardless of scan outcome (id={id})"
            );
        }
    }

    // --- run_forwarder async/stream-lifecycle tests -----------------------------
    //
    // These drive the extracted `run_forwarder` loop hermetically with an in-memory
    // `mpsc` channel and a fake callback (no DB, no live `Scanner`). They use only
    // function-local channels/tokens — NOT the process-global `SCAN_STATE` — so unlike
    // the scan-state lifecycle test above they are safe as independent parallel
    // `#[tokio::test]`s. They pin the async/streaming guarantees: terminal events are never
    // dropped, ordering is preserved, sink-closed cancels the scan, and the task joins
    // (never leaks).

    use minotari_wallet::scan::BlockProcessedEvent;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// A fake event-callback recording every DTO it receives. `fail` flips it to
    /// returning `Err` (modelling a closed Dart `StreamSink`).
    struct FakeSink {
        received: Mutex<Vec<ScanEventDto>>,
        fail: bool,
    }

    impl FakeSink {
        fn new(fail: bool) -> Arc<Self> {
            Arc::new(Self {
                received: Mutex::new(Vec::new()),
                fail,
            })
        }

        fn callback(
            self: &Arc<Self>,
        ) -> impl Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static {
            let me = self.clone();
            move |event| {
                me.received.lock().unwrap().push(event);
                if me.fail {
                    Err(anyhow!("sink closed"))
                } else {
                    Ok(())
                }
            }
        }
    }

    fn started_event(account_id: i64, from_height: u64) -> ProcessingEvent {
        ProcessingEvent::ScanStatus(ScanStatusEvent::Started {
            account_id,
            from_height,
        })
    }

    fn completed_event(account_id: i64, final_height: u64) -> ProcessingEvent {
        ProcessingEvent::ScanStatus(ScanStatusEvent::Completed {
            account_id,
            final_height,
            total_blocks_scanned: final_height,
        })
    }

    /// Terminal `Completed` is delivered in order and the forwarder exits cleanly when
    /// the sender drops (channel drains) — the task joins, no leak.
    #[tokio::test]
    async fn forwarder_delivers_terminal_completed_in_order_then_joins() {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let sink = FakeSink::new(false);

        let handle = tokio::spawn(run_forwarder(rx, token.clone(), Arc::new(sink.callback())));

        tx.send(started_event(1, 100)).unwrap();
        tx.send(completed_event(1, 110)).unwrap();
        drop(tx); // closes the channel → forwarder drains then exits

        // Joins without leaking; not cancelled by us.
        handle.await.unwrap();
        assert!(!token.is_cancelled(), "clean drain must not cancel");

        let got = sink.received.lock().unwrap();
        assert_eq!(got.len(), 2, "both events delivered (terminal not dropped)");
        assert!(
            matches!(got[0], ScanEventDto::Status(ScanStatusDto::Started { .. })),
            "first event preserved in order"
        );
        assert!(
            matches!(
                got[1],
                ScanEventDto::Status(ScanStatusDto::Completed { .. })
            ),
            "terminal Completed delivered last, in order"
        );
    }

    /// Direct token cancellation stops the loop promptly even while the channel is
    /// still open (sender alive), and the forwarder task joins (no leak).
    #[tokio::test]
    async fn forwarder_stops_on_token_cancel_and_joins() {
        let (tx, rx) = mpsc::unbounded_channel::<ProcessingEvent>();
        let token = CancellationToken::new();
        let sink = FakeSink::new(false);

        let handle = tokio::spawn(run_forwarder(rx, token.clone(), Arc::new(sink.callback())));

        token.cancel();
        // Must complete despite `tx` still being alive (channel open).
        handle.await.unwrap();
        drop(tx);
    }

    /// Sink-closed (callback `Err`) cancels the scan token and ends the loop — the
    /// "scan cancels on sink-closed" contract.
    #[tokio::test]
    async fn forwarder_cancels_scan_on_sink_closed() {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let sink = FakeSink::new(true); // callback returns Err

        let handle = tokio::spawn(run_forwarder(rx, token.clone(), Arc::new(sink.callback())));

        tx.send(started_event(1, 100)).unwrap();
        handle.await.unwrap();

        assert!(
            token.is_cancelled(),
            "sink-closed must cancel the scan token"
        );
        assert_eq!(
            sink.received.lock().unwrap().len(),
            1,
            "the one delivered event triggered the cancel-on-error path"
        );
        drop(tx);
    }

    /// Processing events without a Dto (`_ => None`) are skipped without ending the
    /// loop — the streamed event SET is exactly the events that already had a Dto.
    #[tokio::test]
    async fn forwarder_skips_events_without_dto() {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let sink = FakeSink::new(false);

        let handle = tokio::spawn(run_forwarder(rx, token.clone(), Arc::new(sink.callback())));

        // A BlockProcessed event maps to None; sandwich it between two mapped events
        // to prove it's silently skipped and ordering is preserved.
        tx.send(started_event(1, 100)).unwrap();
        // `map_processing_event` returns None for any non-Status/-Ready/-Updated event.
        tx.send(ProcessingEvent::BlockProcessed(BlockProcessedEvent {
            account_id: 1,
            height: 105,
            block_hash: Vec::new(),
            outputs_detected: Vec::new(),
            inputs_spent: Vec::new(),
            balance_changes: Vec::new(),
        }))
        .unwrap();
        tx.send(completed_event(1, 110)).unwrap();
        drop(tx);

        handle.await.unwrap();
        let got = sink.received.lock().unwrap();
        assert_eq!(
            got.len(),
            2,
            "the None-mapped event was skipped, not delivered"
        );
    }
}
