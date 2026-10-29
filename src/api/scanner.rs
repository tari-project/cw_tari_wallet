use crate::api::error::WalletError;
use crate::api::transactions::DisplayedTransactionDto;
use crate::domain::text::strip_format_chars;
use crate::{
    api::db::{get_db_connection, get_db_path, release_expired_reservations},
    frb_generated::StreamSink,
};
use anyhow::{anyhow, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::db::WalletDbError;
use minotari_wallet::scan::rollback_account_to_height;
use minotari_wallet::scan::{
    DisplayedTransactionsEvent, ReorgDetectedEvent, TransactionsUpdatedEvent,
};
use minotari_wallet::{ProcessingEvent, ScanError, ScanMode, ScanStatusEvent, Scanner};
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
    /// Cancelled once the scan's task has returned, so [`stop_scan`] can wait for it.
    stopped: CancellationToken,
}

/// The single process-global "current scan" slot. `None` when no scan is running.
/// Private to this module; every access goes through the helpers below, and the
/// `std::sync::RwLock` guard is **never held across an `.await`** (we take/clone
/// what we need, drop the guard, then await) — holding a sync lock across an await
/// could deadlock the async runtime.
static SCAN_STATE: RwLock<Option<ScanController>> = RwLock::new(None);

/// Held by a wallet maintenance operation (`abandon_send`, `change_passphrase`) from
/// stopping the scan until its DB write is done, and taken briefly by a starting scan
/// before it installs itself, so no scan starts in between. [`stop_scan`] never takes
/// it.
static MAINTENANCE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Bumped by every cancel (`stop_scan`, `disconnect_database`). A starting scan that
/// sees it change while it waits for [`MAINTENANCE`] gives up instead of installing.
static STOP_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Start a maintenance operation: no scan can start until the guard is dropped.
/// Stop the running scan (if any) after taking it.
pub(crate) async fn begin_maintenance() -> tokio::sync::MutexGuard<'static, ()> {
    MAINTENANCE.lock().await
}

/// Hands out a unique, monotonically increasing id per `start_scan` so clean-up
/// can tell "is the controller in the slot still mine?".
static SCAN_ID: AtomicU64 = AtomicU64::new(0);

/// Cancel the in-flight scan, if any, and wait for it to stop.
///
/// Signals the current scan's cancellation token and returns once the scan has
/// actually stopped (its `start_scan` call has returned), so the wallet database is
/// no longer being written by it. A no-op when no scan is running.
#[frb]
pub async fn stop_scan() -> Result<()> {
    if let Some(stopped) = cancel_current_scan()? {
        stopped.cancelled().await;
    }
    Ok(())
}

/// Cancel the in-flight scan (if any): signal its cancel token so the scan future
/// and the forwarder both wind down, and `abort()` the forwarder as a backstop
/// against a stuck `recv()`. Synchronous and lock-free across `.await`.
///
/// Shared by [`stop_scan`] and by `disconnect_database` (graceful shutdown): tearing
/// the DB pool out from under a live scan would make the scan fail mid-operation, so
/// teardown first *cooperatively cancels* the scan here. We do not block-await the
/// scan task from this synchronous boundary (that would risk deadlocking the FRB async
/// runtime); cancellation is sufficient to stop further DB access, and the scan's own
/// task clears its state on exit (`clear_scan_if_current`).
pub(crate) fn cancel_active_scan() -> Result<()> {
    cancel_current_scan()?;
    Ok(())
}

/// Cancel the in-flight scan (as [`cancel_active_scan`]) and return its `stopped`
/// token, which fires once the scan's task has returned.
///
/// The controller stays in the slot until the scan's own task removes it on exit, so
/// every later `stop_scan` and every superseding scan can still wait for it.
fn cancel_current_scan() -> Result<Option<CancellationToken>> {
    // Also stops a scan still waiting to install itself (see `STOP_GENERATION`).
    STOP_GENERATION.fetch_add(1, Ordering::SeqCst);
    let guard = SCAN_STATE
        .read()
        .map_err(|_| WalletError::internal("Failed to acquire lock"))?;
    let Some(controller) = guard.as_ref() else {
        return Ok(None);
    };
    controller.token.cancel();
    // The forwarder will observe the cancellation (token fires / `rx` closes) and
    // finish; abort as a backstop so a stuck `recv()` can't leak the task.
    controller.forwarder.abort();
    Ok(Some(controller.stopped.clone()))
}

/// Remove and return the current scan controller (if any).
#[cfg(test)]
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

/// An event streamed during [`start_scan_v2`] and [`rescan_v2`].
///
/// The superset of [`ScanEventDto`]: the same status, transaction and error events
/// (flattened), plus `tip_height` on `Progress` (the height the current pass scans
/// towards) and `ReorgDetected`. `Waiting` means the scan has caught up with the tip
/// and is polling (continuous mode); `Completed` ends a one-shot scan; `Error` is a
/// terminal failure (the scan also resolves to `Err`).
#[frb]
#[derive(Debug, Clone)]
pub enum ScanEventV2Dto {
    Started {
        account_id: i64,
        from_height: u64,
    },
    Progress {
        account_id: i64,
        current_height: u64,
        tip_height: u64,
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
    TransactionsReady(TransactionsReadyDto),
    TransactionsUpdated(TransactionsUpdatedDto),
    ReorgDetected(ReorgDetectedDto),
    Error(String),
}

/// A blockchain reorganization the scan detected and rolled back, carried inside
/// [`ScanEventV2Dto::ReorgDetected`].
///
/// Blocks from `reorg_from_height` up were rolled back and the chain is now at
/// `new_height`. `cancelled_transaction_ids` are the wallet transactions the reorg
/// removed; `reorganized_transactions` are those whose state changed (show them as
/// pending again, or drop them).
#[frb]
#[derive(Debug, Clone)]
pub struct ReorgDetectedDto {
    pub account_id: i64,
    pub reorg_from_height: u64,
    pub new_height: u64,
    pub blocks_rolled_back: u64,
    pub cancelled_transaction_ids: Vec<String>,
    pub reorganized_transactions: Vec<DisplayedTransactionDto>,
}

impl From<ReorgDetectedEvent> for ReorgDetectedDto {
    fn from(e: ReorgDetectedEvent) -> Self {
        Self {
            account_id: e.account_id,
            reorg_from_height: e.reorg_from_height,
            new_height: e.new_height,
            blocks_rolled_back: e.blocks_rolled_back,
            cancelled_transaction_ids: e.cancelled_transaction_ids,
            reorganized_transactions: e
                .reorganized_displayed_transactions
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

/// Map a raw scanner [`ProcessingEvent`] to a [`ScanEventV2Dto`], the one internal
/// event stream both scan APIs are fed from.
///
/// Returns `None` for events with no Dto. `BlockProcessed` is per-block bookkeeping
/// Dart has never been shown. `FastSyncPhaseStarted` / `FastSyncPhaseCompleted` are
/// emitted **only** under `ScanMode::FastSync` and by the separate backfill pass,
/// neither of which this bridge selects, so dropping them is observationally a no-op.
///
/// Both matches are deliberately **exhaustive** (no `_` arm): a new upstream variant
/// may well be balance- or security-relevant, and must fail the build so the
/// drop-or-surface decision is made deliberately.
fn map_processing_event_v2(event: ProcessingEvent) -> Option<ScanEventV2Dto> {
    let status = match event {
        ProcessingEvent::ScanStatus(status) => status,
        ProcessingEvent::TransactionsReady(e) => {
            return Some(ScanEventV2Dto::TransactionsReady(e.into()))
        }
        ProcessingEvent::TransactionsUpdated(e) => {
            return Some(ScanEventV2Dto::TransactionsUpdated(e.into()))
        }
        ProcessingEvent::ReorgDetected(e) => return Some(ScanEventV2Dto::ReorgDetected(e.into())),
        ProcessingEvent::BlockProcessed(_) => return None,
    };
    let dto = match status {
        // The exhaustive match cannot catch the *other* way this could go wrong:
        // upstream routing the Full/Continuous path through fast sync would add no
        // new variant, it would just start emitting these. Log it so that shows up
        // in a bug report instead of silently vanishing.
        ScanStatusEvent::FastSyncPhaseStarted { .. }
        | ScanStatusEvent::FastSyncPhaseCompleted { .. } => {
            log::warn!(
                "Dropping a fast-sync ScanStatusEvent with no Dto representation; this bridge \
                 only selects ScanMode::Full / ScanMode::Continuous, so receiving one means \
                 upstream changed which scan path those modes take"
            );
            return None;
        }
        ScanStatusEvent::Started {
            account_id,
            from_height,
        } => ScanEventV2Dto::Started {
            account_id,
            from_height,
        },
        ScanStatusEvent::Progress {
            account_id,
            current_height,
            tip_height,
            blocks_scanned,
        } => ScanEventV2Dto::Progress {
            account_id,
            current_height,
            tip_height,
            blocks_scanned,
        },
        ScanStatusEvent::Completed {
            account_id,
            final_height,
            total_blocks_scanned,
        } => ScanEventV2Dto::Completed {
            account_id,
            final_height,
            total_blocks_scanned,
        },
        ScanStatusEvent::Paused {
            account_id,
            last_scanned_height,
            reason,
        } => ScanEventV2Dto::Paused {
            account_id,
            last_scanned_height,
            reason: format!("{:?}", reason),
        },
        ScanStatusEvent::Waiting {
            account_id,
            resume_in,
        } => ScanEventV2Dto::Waiting {
            account_id,
            resume_in_seconds: resume_in.as_secs(),
        },
        ScanStatusEvent::MoreBlocksAvailable {
            account_id,
            last_scanned_height,
        } => ScanEventV2Dto::MoreBlocksAvailable {
            account_id,
            last_scanned_height,
        },
    };
    Some(dto)
}

/// Narrow a [`ScanEventV2Dto`] to the frozen [`ScanEventDto`] that [`start_scan`] and
/// [`rescan`] stream. `ReorgDetected` and `Progress.tip_height` have no place in the
/// frozen types and are dropped, so the v1 event set is exactly what it always was.
fn v1_event(event: ScanEventV2Dto) -> Option<ScanEventDto> {
    let status = match event {
        ScanEventV2Dto::Started {
            account_id,
            from_height,
        } => ScanStatusDto::Started {
            account_id,
            from_height,
        },
        ScanEventV2Dto::Progress {
            account_id,
            current_height,
            tip_height: _,
            blocks_scanned,
        } => ScanStatusDto::Progress {
            account_id,
            current_height,
            blocks_scanned,
        },
        ScanEventV2Dto::Completed {
            account_id,
            final_height,
            total_blocks_scanned,
        } => ScanStatusDto::Completed {
            account_id,
            final_height,
            total_blocks_scanned,
        },
        ScanEventV2Dto::Paused {
            account_id,
            last_scanned_height,
            reason,
        } => ScanStatusDto::Paused {
            account_id,
            last_scanned_height,
            reason,
        },
        ScanEventV2Dto::Waiting {
            account_id,
            resume_in_seconds,
        } => ScanStatusDto::Waiting {
            account_id,
            resume_in_seconds,
        },
        ScanEventV2Dto::MoreBlocksAvailable {
            account_id,
            last_scanned_height,
        } => ScanStatusDto::MoreBlocksAvailable {
            account_id,
            last_scanned_height,
        },
        ScanEventV2Dto::TransactionsReady(e) => return Some(ScanEventDto::TransactionsReady(e)),
        ScanEventV2Dto::TransactionsUpdated(e) => {
            return Some(ScanEventDto::TransactionsUpdated(e))
        }
        ScanEventV2Dto::Error(e) => return Some(ScanEventDto::Error(e)),
        ScanEventV2Dto::ReorgDetected(_) => return None,
    };
    Some(ScanEventDto::Status(status))
}

/// Wrap a v1 callback so it can be fed the internal v2 stream.
fn v1_callback<F>(callback: F) -> impl Fn(ScanEventV2Dto) -> Result<()> + Send + Sync + 'static
where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    move |event| match v1_event(event) {
        Some(event) => callback(event),
        None => Ok(()),
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
    F: Fn(ScanEventV2Dto) -> Result<()> + Send + Sync + 'static,
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

        if let Some(dto) = map_processing_event_v2(event) {
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
    start_scan_v2_with_handler(config, v1_callback(event_callback)).await
}

#[frb(ignore)]
pub async fn start_scan_v2_with_handler<F>(
    config: ScanConfiguration,
    event_callback: F,
) -> Result<()>
where
    F: Fn(ScanEventV2Dto) -> Result<()> + Send + Sync + 'static,
{
    run_scan(config, None, event_callback).await
}

/// The one scan implementation behind every scan API. With `rescan_from` set, the
/// wallet is rolled back to that height after this scan owns the slot (so any
/// previous scan has stopped) and before scanning starts.
async fn run_scan<F>(
    mut config: ScanConfiguration,
    rescan_from: Option<u64>,
    event_callback: F,
) -> Result<()>
where
    F: Fn(ScanEventV2Dto) -> Result<()> + Send + Sync + 'static,
{
    let stop_generation = STOP_GENERATION.load(Ordering::SeqCst);
    let db_path = get_db_path()?;

    let cancel_token = CancellationToken::new();
    let scan_id = SCAN_ID.fetch_add(1, Ordering::Relaxed);
    // Fires when this function returns on any path (or its future is dropped).
    let stopped = CancellationToken::new();
    let _stopped_on_return = stopped.clone().drop_guard();

    let mode = if config.continuous {
        ScanMode::Continuous {
            poll_interval: Duration::from_secs(config.poll_interval_seconds),
        }
    } else {
        ScanMode::Full
    };

    // The passphrase arrives in the frozen public `ScanConfiguration.passphrase`
    // (plain `String`). Move it (not a copy) into a zeroizing container so the
    // plaintext is wiped on drop, and pass it to `Scanner::new` by `&` (deref to
    // `&str`) so nothing else clones it in the clear (Shared Contracts §3).
    let passphrase = Zeroizing::new(std::mem::take(&mut config.passphrase));

    let scanner_builder = Scanner::new(
        &passphrase,
        &config.base_url,
        db_path.clone(),
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
    // Wait out a maintenance operation in progress before taking the slot. A stop
    // issued meanwhile found no scan in the slot to cancel: honour it here.
    let maintenance = MAINTENANCE.lock().await;
    if STOP_GENERATION.load(Ordering::SeqCst) != stop_generation {
        forwarder.abort();
        return Ok(());
    }
    let previous = install_scan_controller(ScanController {
        id: scan_id,
        token: cancel_token.clone(),
        forwarder,
        stopped,
    })?;
    drop(maintenance);
    if let Some(previous) = previous {
        stop_replaced_scan(previous).await;
    }

    // The scan future is lazy: nothing touches the chain or the DB until it is polled.
    let result: Result<()> = async {
        // Stopped (stop_scan, a newer scan, teardown) while waiting for the scan it
        // replaced: leave the DB untouched.
        if cancel_token.is_cancelled() {
            return Ok(());
        }
        // Best-effort: free inputs a stale reservation still holds before the scan
        // runs, now that the previous scan has stopped.
        match get_db_connection() {
            Ok(conn) => release_expired_reservations(&conn),
            Err(e) => log::warn!("Could not check for expired UTXO reservations: {e}"),
        }
        if let Some(from_height) = rescan_from {
            rollback_account_to_height(
                db_path,
                &passphrase,
                &config.wallet_name,
                from_height,
                None,
            )
            .await
            .map_err(map_rollback_error)?;
            // Stopped (stop_scan, a newer scan, teardown) during the rollback.
            if cancel_token.is_cancelled() {
                return Ok(());
            }
        }
        scan_future.await.map(|_| ()).map_err(|e| anyhow!(e))
    }
    .await;

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
    if let Err(e) = result {
        let (message, error) = scan_failure(e);
        let _ = event_callback(ScanEventV2Dto::Error(message));
        return Err(error);
    }
    Ok(())
}

/// The terminal `Error` event text and the returned error for a failed scan.
///
/// Both can carry node-supplied words, so invisible characters are dropped. Dart
/// receives an error as its `{:?}` rendering (message plus "Caused by" chain), so the
/// returned error carries that whole rendering, cleaned, as its message.
fn scan_failure(e: anyhow::Error) -> (String, anyhow::Error) {
    let message = strip_format_chars(&e.to_string());
    let error = anyhow!(strip_format_chars(&format!("{e:?}")));
    (message, error)
}

/// Stop a scan this one replaced: cancel it and wait until its task has returned,
/// so it can no longer write the DB.
async fn stop_replaced_scan(previous: ScanController) {
    previous.token.cancel();
    // Best-effort: let the old forwarder drain and exit; abort as a backstop.
    previous.forwarder.abort();
    let _ = previous.forwarder.await;
    previous.stopped.cancelled().await;
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

/// [`start_scan`] streaming [`ScanEventV2Dto`]s: the same scan, plus reorg events and
/// the tip height in progress events.
///
/// Shares the scan slot with [`start_scan`] and [`rescan`] (latest wins, [`stop_scan`]
/// stops whichever is running) and cancels when the Dart stream is closed.
#[frb]
pub async fn start_scan_v2(
    sink: StreamSink<ScanEventV2Dto>,
    config: ScanConfiguration,
) -> Result<()> {
    start_scan_v2_with_handler(config, move |event| {
        sink.add(event)
            .map_err(|e| WalletError::scan(format!("{e:?}")).into())
    })
    .await
}

/// Roll the wallet back to `from_height` and scan again from there, streaming the
/// same [`ScanEventDto`]s as [`start_scan`].
///
/// `config` is used exactly as by [`start_scan`] (its `wallet_name` selects the
/// account, its `passphrase` must decrypt it). Any running scan is stopped, and
/// waited for, first. The rollback is done by minotari and committed before scanning
/// starts; if the scan then fails, the wallet stays rolled back until a later scan
/// covers the range. A wrong passphrase errors as `Invalid Passphrase` (streamed as
/// the terminal error event too) and changes nothing.
/// Requires [`initialize_database`] first.
#[frb]
pub async fn rescan(
    sink: StreamSink<ScanEventDto>,
    from_height: u64,
    config: ScanConfiguration,
) -> Result<()> {
    rescan_with_handler(from_height, config, move |event| {
        sink.add(event)
            .map_err(|e| WalletError::scan(format!("{e:?}")).into())
    })
    .await
}

#[frb(ignore)]
pub async fn rescan_with_handler<F>(
    from_height: u64,
    config: ScanConfiguration,
    event_callback: F,
) -> Result<()>
where
    F: Fn(ScanEventDto) -> Result<()> + Send + Sync + 'static,
{
    rescan_v2_with_handler(from_height, config, v1_callback(event_callback)).await
}

/// [`rescan`] streaming [`ScanEventV2Dto`]s, as [`start_scan_v2`] does.
#[frb]
pub async fn rescan_v2(
    sink: StreamSink<ScanEventV2Dto>,
    from_height: u64,
    config: ScanConfiguration,
) -> Result<()> {
    rescan_v2_with_handler(from_height, config, move |event| {
        sink.add(event)
            .map_err(|e| WalletError::scan(format!("{e:?}")).into())
    })
    .await
}

#[frb(ignore)]
pub async fn rescan_v2_with_handler<F>(
    from_height: u64,
    config: ScanConfiguration,
    event_callback: F,
) -> Result<()>
where
    F: Fn(ScanEventV2Dto) -> Result<()> + Send + Sync + 'static,
{
    run_scan(config, Some(from_height), event_callback).await
}

/// A wrong passphrase is reported as such; every other rollback failure is a
/// database error carrying minotari's message.
fn map_rollback_error(e: ScanError) -> WalletError {
    match e {
        ScanError::DbError(WalletDbError::DecryptionFailed(_)) => WalletError::InvalidPassphrase,
        other => WalletError::database(other.to_string()),
    }
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
                stopped: CancellationToken::new(),
            },
            token,
        )
    }

    /// A stand-in scan task: winds down a little after it is cancelled, then clears
    /// its slot and fires `stopped`, as `run_scan` does on exit.
    fn spawn_stand_in_scan(
        id: u64,
        token: CancellationToken,
        stopped: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            token.cancelled().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = clear_scan_if_current(id);
            stopped.cancel();
        })
    }

    /// Run `stop_scan` and report whether the scan had stopped by the time it returned.
    async fn stop_and_check(stopped: CancellationToken) -> bool {
        stop_scan().await.unwrap();
        stopped.is_cancelled()
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

        // --- Scenario 5: stop_scan returns only once the scan has stopped ---
        let _ = take_scan_controller().unwrap();
        let token = CancellationToken::new();
        let stopped = CancellationToken::new();
        install_scan_controller(ScanController {
            id: 40,
            token: token.clone(),
            forwarder: spawn_forwarder(token.clone()),
            stopped: stopped.clone(),
        })
        .unwrap();
        let scan_task = spawn_stand_in_scan(40, token, stopped.clone());
        // Two concurrent stops: both return only once the scan has stopped.
        let stops = [
            tokio::spawn(stop_and_check(stopped.clone())),
            tokio::spawn(stop_and_check(stopped.clone())),
        ];
        for stop in stops {
            assert!(
                stop.await.unwrap(),
                "stop_scan must wait for the scan to stop"
            );
        }
        assert!(take_scan_controller().unwrap().is_none());
        scan_task.await.unwrap();

        // --- Scenario 5b: a scan started after cancel_active_scan waits for it ---
        let token = CancellationToken::new();
        let stopped = CancellationToken::new();
        install_scan_controller(ScanController {
            id: 45,
            token: token.clone(),
            forwarder: spawn_forwarder(token.clone()),
            stopped: stopped.clone(),
        })
        .unwrap();
        let scan_task = spawn_stand_in_scan(45, token, stopped.clone());
        cancel_active_scan().unwrap();
        // A stop after the cancel still finds the scan and waits for it.
        assert!(stop_and_check(stopped.clone()).await);
        scan_task.await.unwrap();

        let token = CancellationToken::new();
        let stopped = CancellationToken::new();
        install_scan_controller(ScanController {
            id: 46,
            token: token.clone(),
            forwarder: spawn_forwarder(token.clone()),
            stopped: stopped.clone(),
        })
        .unwrap();
        let scan_task = spawn_stand_in_scan(46, token, stopped.clone());
        cancel_active_scan().unwrap();
        // The next scan finds the cancelled one in the slot and waits for it.
        let (next, _next_token) = make_controller(47);
        let previous = install_scan_controller(next)
            .unwrap()
            .expect("the cancelled scan is still in the slot");
        stop_replaced_scan(previous).await;
        assert!(
            stopped.is_cancelled(),
            "the new scan waited for the cancelled one"
        );
        scan_task.await.unwrap();
        let _ = take_scan_controller().unwrap();

        // With no scan running, stop_scan returns at once.
        stop_scan().await.unwrap();

        // --- Scenario 6: a replaced scan is awaited until it has stopped ---
        let token = CancellationToken::new();
        let stopped = CancellationToken::new();
        let replaced = ScanController {
            id: 50,
            token: token.clone(),
            forwarder: spawn_forwarder(token.clone()),
            stopped: stopped.clone(),
        };
        let scan_task = {
            let stopped = stopped.clone();
            tokio::spawn(async move {
                token.cancelled().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                stopped.cancel();
            })
        };
        stop_replaced_scan(replaced).await;
        assert!(
            stopped.is_cancelled(),
            "a superseding scan must wait for the replaced scan to stop"
        );
        scan_task.await.unwrap();
    }

    /// A rescan stopped while it waits for the scan it replaced must not roll back.
    /// The wallet does not exist, so a rollback would fail with "Account not found";
    /// the rescan returning `Ok` with no events proves it never reached the DB.
    #[tokio::test]
    async fn rescan_cancelled_while_waiting_does_not_roll_back() {
        let _guard = crate::api::db::GLOBAL_STATE_TEST_LOCK.lock().await;
        let _ = take_scan_controller().unwrap();
        let db_path =
            std::env::temp_dir().join(format!("cw-rescan-cancel-{}.db", std::process::id()));
        crate::api::db::initialize_database(db_path.to_string_lossy().to_string()).unwrap();

        // A predecessor that only stops when told to.
        let token = CancellationToken::new();
        let predecessor_stopped = CancellationToken::new();
        install_scan_controller(ScanController {
            id: u64::MAX,
            token: token.clone(),
            forwarder: spawn_forwarder(token),
            stopped: predecessor_stopped.clone(),
        })
        .unwrap();

        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rescan = {
            let events = events.clone();
            tokio::spawn(rescan_v2_with_handler(
                0,
                ScanConfiguration {
                    wallet_name: "missing".to_string(),
                    passphrase: String::new(),
                    base_url: "http://127.0.0.1:1".to_string(),
                    batch_size: 10,
                    continuous: false,
                    poll_interval_seconds: 1,
                    required_confirmations: 3,
                },
                move |event| {
                    events.lock().unwrap().push(event);
                    Ok(())
                },
            ))
        };
        let slot_id = || SCAN_STATE.read().unwrap().as_ref().map(|c| c.id);
        // Wait until the rescan holds the slot (and so waits on the predecessor).
        while slot_id() == Some(u64::MAX) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let stop = tokio::spawn(stop_scan());
        let rescan_cancelled = || {
            SCAN_STATE
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|c| c.token.is_cancelled())
        };
        while !rescan_cancelled() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        predecessor_stopped.cancel();

        rescan
            .await
            .unwrap()
            .expect("a cancelled rescan returns Ok without a rollback");
        stop.await.unwrap().unwrap();
        assert!(events.lock().unwrap().is_empty(), "nothing was streamed");

        crate::api::db::disconnect_database().unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    /// A scan started while a maintenance operation holds the guard does not take
    /// the slot (or touch the DB) until the guard is dropped.
    #[tokio::test]
    async fn scan_waits_for_maintenance() {
        let _guard = crate::api::db::GLOBAL_STATE_TEST_LOCK.lock().await;
        let _ = take_scan_controller().unwrap();
        let db_path =
            std::env::temp_dir().join(format!("cw-maintenance-{}.db", std::process::id()));
        crate::api::db::initialize_database(db_path.to_string_lossy().to_string()).unwrap();

        let maintenance = begin_maintenance().await;
        // A rescan of a missing wallet fails as soon as it reaches the DB.
        let rescan = tokio::spawn(rescan_v2_with_handler(
            0,
            ScanConfiguration {
                wallet_name: "missing".to_string(),
                passphrase: String::new(),
                base_url: "http://127.0.0.1:1".to_string(),
                batch_size: 10,
                continuous: false,
                poll_interval_seconds: 1,
                required_confirmations: 3,
            },
            |_| Ok(()),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!rescan.is_finished(), "the scan waits for the maintenance");
        assert!(SCAN_STATE.read().unwrap().is_none(), "nor takes the slot");

        drop(maintenance);
        let err = rescan.await.unwrap().expect_err("then it runs and fails");
        assert!(err.to_string().contains("Account not found"), "got {err}");

        crate::api::db::disconnect_database().unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    /// A scan waiting on the maintenance guard when `stop_scan` runs never installs.
    #[tokio::test]
    async fn scan_stopped_while_waiting_for_maintenance_never_runs() {
        let _guard = crate::api::db::GLOBAL_STATE_TEST_LOCK.lock().await;
        let _ = take_scan_controller().unwrap();
        let db_path = std::env::temp_dir().join(format!("cw-maint-stop-{}.db", std::process::id()));
        crate::api::db::initialize_database(db_path.to_string_lossy().to_string()).unwrap();

        let maintenance = begin_maintenance().await;
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        // A rescan of a missing wallet fails as soon as it reaches the DB.
        let rescan = {
            let events = events.clone();
            tokio::spawn(rescan_v2_with_handler(
                0,
                ScanConfiguration {
                    wallet_name: "missing".to_string(),
                    passphrase: String::new(),
                    base_url: "http://127.0.0.1:1".to_string(),
                    batch_size: 10,
                    continuous: false,
                    poll_interval_seconds: 1,
                    required_confirmations: 3,
                },
                move |event| {
                    events.lock().unwrap().push(event);
                    Ok(())
                },
            ))
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop_scan().await.unwrap();
        drop(maintenance);

        rescan
            .await
            .unwrap()
            .expect("a stopped scan returns Ok without running");
        assert!(events.lock().unwrap().is_empty(), "nothing ran");
        assert!(SCAN_STATE.read().unwrap().is_none(), "it never installed");

        crate::api::db::disconnect_database().unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    /// Dart gets an error as its `{:?}` rendering; a failed scan's returned error
    /// renders exactly as the plain `anyhow!(ScanError)` it replaced.
    #[test]
    fn scan_failure_keeps_the_error_chain() {
        let scan_error = || {
            ScanError::DbError(WalletDbError::DecryptionFailed(
                "Failed to decrypt wallet data".to_string(),
            ))
        };
        let old = format!("{:?}", anyhow!(scan_error()));
        let (message, error) = scan_failure(anyhow!(scan_error()));
        assert_eq!(format!("{error:?}"), old);
        assert_eq!(message, scan_error().to_string());
        assert!(old.contains("Caused by"), "the chain is rendered: {old}");

        let (message, error) = scan_failure(anyhow!("bad\u{202E}node"));
        assert_eq!(message, "badnode");
        assert_eq!(format!("{error:?}"), "badnode");
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

    use minotari_wallet::scan::{BlockProcessedEvent, FastSyncPhase};
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

        let handle = tokio::spawn(run_forwarder(
            rx,
            token.clone(),
            Arc::new(v1_callback(sink.callback())),
        ));

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

        let handle = tokio::spawn(run_forwarder(
            rx,
            token.clone(),
            Arc::new(v1_callback(sink.callback())),
        ));

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

        let handle = tokio::spawn(run_forwarder(
            rx,
            token.clone(),
            Arc::new(v1_callback(sink.callback())),
        ));

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

    /// Upstream-drift tripwire for the frozen **event** contract.
    ///
    /// `ScanStatusEvent` gained `FastSyncPhaseStarted` / `FastSyncPhaseCompleted` in
    /// minotari `af78477`. Neither may surface to Dart: adding a `ScanStatusDto`
    /// variant for them would change the streamed event set, which is a breaking
    /// change. They must map to `None` and therefore never reach the sink.
    #[test]
    fn fast_sync_status_events_have_no_dto_representation() {
        assert!(
            map_processing_event_v2(ProcessingEvent::ScanStatus(
                ScanStatusEvent::FastSyncPhaseStarted {
                    account_id: 1,
                    phase: FastSyncPhase::FastUtxoScan,
                    from_height: 100,
                    to_height: Some(200),
                }
            ))
            .is_none(),
            "FastSyncPhaseStarted must not surface to Dart"
        );
        assert!(
            map_processing_event_v2(ProcessingEvent::ScanStatus(
                ScanStatusEvent::FastSyncPhaseCompleted {
                    account_id: 1,
                    phase: FastSyncPhase::FastUtxoScan,
                }
            ))
            .is_none(),
            "FastSyncPhaseCompleted must not surface to Dart"
        );

        // ...while the contract's own variants still map through unchanged.
        assert!(matches!(
            map_processing_event_v2(started_event(1, 100)).and_then(v1_event),
            Some(ScanEventDto::Status(ScanStatusDto::Started {
                account_id: 1,
                from_height: 100
            }))
        ));
    }

    fn reorg_event() -> ProcessingEvent {
        ProcessingEvent::ReorgDetected(ReorgDetectedEvent {
            account_id: 1,
            reorg_from_height: 95,
            new_height: 97,
            blocks_rolled_back: 6,
            invalidated_output_hashes: Vec::new(),
            cancelled_transaction_ids: vec!["tx-1".to_string(), "tx-2".to_string()],
            reorganized_displayed_transactions: Vec::new(),
        })
    }

    fn progress_event() -> ProcessingEvent {
        ProcessingEvent::ScanStatus(ScanStatusEvent::Progress {
            account_id: 1,
            current_height: 100,
            tip_height: 250,
            blocks_scanned: 10,
        })
    }

    /// A reorg and the scan tip reach a v2 sink, in order, through the forwarder.
    #[tokio::test]
    async fn v2_forwarder_delivers_reorg_and_tip() {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let received = Arc::new(Mutex::new(Vec::new()));
        let callback = {
            let received = received.clone();
            move |event: ScanEventV2Dto| -> Result<()> {
                received.lock().unwrap().push(event);
                Ok(())
            }
        };
        let handle = tokio::spawn(run_forwarder(rx, token.clone(), Arc::new(callback)));

        tx.send(progress_event()).unwrap();
        tx.send(reorg_event()).unwrap();
        drop(tx);
        handle.await.unwrap();

        let got = received.lock().unwrap();
        assert_eq!(got.len(), 2);
        assert!(matches!(
            got[0],
            ScanEventV2Dto::Progress {
                current_height: 100,
                tip_height: 250,
                ..
            }
        ));
        let ScanEventV2Dto::ReorgDetected(reorg) = &got[1] else {
            panic!("expected a reorg, got {:?}", got[1]);
        };
        assert_eq!(reorg.reorg_from_height, 95);
        assert_eq!(reorg.new_height, 97);
        assert_eq!(reorg.blocks_rolled_back, 6);
        assert_eq!(reorg.cancelled_transaction_ids, vec!["tx-1", "tx-2"]);
    }

    /// The v1 path, fed from the same stream, still drops the reorg and keeps the
    /// frozen `Progress` shape.
    #[tokio::test]
    async fn v1_forwarder_still_drops_reorg() {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let sink = FakeSink::new(false);
        let handle = tokio::spawn(run_forwarder(
            rx,
            token.clone(),
            Arc::new(v1_callback(sink.callback())),
        ));

        tx.send(progress_event()).unwrap();
        tx.send(reorg_event()).unwrap();
        tx.send(completed_event(1, 110)).unwrap();
        drop(tx);
        handle.await.unwrap();

        let got = sink.received.lock().unwrap();
        assert_eq!(got.len(), 2, "the reorg must not reach a v1 sink");
        assert!(matches!(
            got[0],
            ScanEventDto::Status(ScanStatusDto::Progress {
                current_height: 100,
                blocks_scanned: 10,
                ..
            })
        ));
        assert!(matches!(
            got[1],
            ScanEventDto::Status(ScanStatusDto::Completed { .. })
        ));
    }

    /// Processing events without a Dto (`_ => None`) are skipped without ending the
    /// loop — the streamed event SET is exactly the events that already had a Dto.
    #[tokio::test]
    async fn forwarder_skips_events_without_dto() {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        let sink = FakeSink::new(false);

        let handle = tokio::spawn(run_forwarder(
            rx,
            token.clone(),
            Arc::new(v1_callback(sink.callback())),
        ));

        // A BlockProcessed event maps to None; sandwich it between two mapped events
        // to prove it's silently skipped and ordering is preserved.
        tx.send(started_event(1, 100)).unwrap();
        // `BlockProcessed` has no Dto in either stream.
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
