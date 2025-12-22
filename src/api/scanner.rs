use crate::api::db::get_db_path;
use crate::api::transactions::DisplayedTransactionDto;
use crate::frb_generated::StreamSink;
use anyhow::{anyhow, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::scan::{DisplayedTransactionsEvent, TransactionsUpdatedEvent};
use minotari_wallet::{PauseReason, ProcessingEvent, ScanMode, ScanStatusEvent, Scanner};
use std::sync::RwLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

static SCAN_TOKEN: RwLock<Option<CancellationToken>> = RwLock::new(None);

#[frb]
pub fn stop_scan() -> Result<()> {
    let guard = SCAN_TOKEN
        .read()
        .map_err(|_| anyhow!("Failed to acquire lock"))?;

    if let Some(token) = guard.as_ref() {
        token.cancel();
    }
    Ok(())
}

#[frb]
#[derive(Clone)]
pub enum ScanEventDto {
    Status(ScanStatusDto),
    TransactionsReady(TransactionsReadyDto),
    TransactionsUpdated(TransactionsUpdatedDto),
    Error(String),
}

#[frb]
#[derive(Clone)]
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
                reason: match reason {
                    PauseReason::MaxBlocksReached { limit } => {
                        format!("max blocks reached: {limit}")
                    }
                    PauseReason::Cancelled => "cancelled".to_string(),
                },
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

#[derive(Clone)]
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

#[derive(Clone)]
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

#[frb]
pub struct ScanConfiguration {
    pub password: String,
    pub base_url: String,
    pub batch_size: u64,
    pub continuous: bool,
    pub poll_interval_seconds: u64,
}

#[frb]
pub async fn start_scan(sink: StreamSink<ScanEventDto>, config: ScanConfiguration) -> Result<()> {
    let db_path = get_db_path()?;

    let cancel_token = CancellationToken::new();
    {
        let mut guard = SCAN_TOKEN.write().map_err(|_| anyhow!("Failed to lock"))?;
        *guard = Some(cancel_token.clone());
    }

    let loop_cancel_token = cancel_token.clone();
    let thread_cancel_token = cancel_token.clone();

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<()>>();

    // Spawn a dedicated OS thread.
    // Reason: The `scanner` implementation uses `rusqlite` references inside async blocks.
    // Since `rusqlite::Connection` is !Sync, these Futures become !Send.
    // FRB's default runtime is multi-threaded and requires Send.
    // By using a dedicated thread with a `current_thread` runtime, we satisfy the safety requirements.
    std::thread::spawn(move || {
        let rt_builder = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();

        let result = match rt_builder {
            Ok(rt) => rt.block_on(async {
                let local = tokio::task::LocalSet::new();
                local
                    .run_until(async {
                        let mode = if config.continuous {
                            ScanMode::Continuous {
                                poll_interval: Duration::from_secs(config.poll_interval_seconds),
                            }
                        } else {
                            ScanMode::Full
                        };

                        let scanner_builder = Scanner::new(
                            &config.password,
                            &config.base_url,
                            &db_path,
                            config.batch_size,
                        )
                        .mode(mode)
                        .cancel_token(thread_cancel_token);

                        let (mut rx, scan_future) = scanner_builder.run_with_events();
                        let stream_sink = sink.clone();

                        tokio::task::spawn_local(async move {
                            while let Some(event) = rx.recv().await {
                                let dto_opt = match event {
                                    ProcessingEvent::ScanStatus(status) => {
                                        Some(ScanEventDto::Status(status.into()))
                                    }
                                    ProcessingEvent::TransactionsReady(e) => {
                                        Some(ScanEventDto::TransactionsReady(e.into()))
                                    }
                                    ProcessingEvent::TransactionsUpdated(e) => {
                                        Some(ScanEventDto::TransactionsUpdated(e.into()))
                                    }
                                    _ => None,
                                };

                                if let Some(dto) = dto_opt {
                                    if stream_sink.add(dto).is_err() {
                                        loop_cancel_token.cancel();
                                        break;
                                    }
                                }
                            }
                        });

                        let res = scan_future.await;
                        match res {
                            Ok(_) => Ok(()),
                            Err(e) => {
                                let _ = sink.add(ScanEventDto::Error(e.to_string()));
                                Err(anyhow!(e))
                            }
                        }
                    })
                    .await
            }),
            Err(e) => Err(anyhow!("Failed to create local runtime: {}", e)),
        };

        let _ = tx.send(result);
    });

    rx.await.map_err(|_| anyhow!("Scanner thread panicked"))??;

    {
        let mut guard = SCAN_TOKEN.write().map_err(|_| anyhow!("Failed to lock"))?;
        *guard = None;
    }

    Ok(())
}
