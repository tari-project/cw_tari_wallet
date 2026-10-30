use anyhow::Result;
use file_rotate::{compression::Compression, suffix::AppendCount, FileRotate};
use flutter_rust_bridge::frb;
use log::kv::{Error, Key, Value, VisitSource};
use log::LevelFilter;
use std::fmt::Write;
use std::io::LineWriter;
use std::path::PathBuf;

const BYTES_PER_MB: u64 = 1_048_576;

/// Upstream logs every offline-signing payload summary (recipients, amounts, memos,
/// fee, change) at Info under this target when a send is signed. Capped at Warn
/// unless the caller sets a level for it explicitly.
const PAYLOAD_SUMMARY_TARGET: &str = "c::tx::offline_signing";

/// Logging verbosity, from `Error` (least) to `Trace` (most), or `Off`.
#[frb]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    Off,
}

impl From<LogLevel> for LevelFilter {
    fn from(l: LogLevel) -> Self {
        match l {
            LogLevel::Error => LevelFilter::Error,
            LogLevel::Warn => LevelFilter::Warn,
            LogLevel::Info => LevelFilter::Info,
            LogLevel::Debug => LevelFilter::Debug,
            LogLevel::Trace => LevelFilter::Trace,
            LogLevel::Off => LevelFilter::Off,
        }
    }
}

/// A per-module log-level override: log records whose `target` matches are
/// filtered at `level` instead of the default.
#[frb]
#[derive(Clone, Debug)]
pub struct LogModuleConfig {
    pub target: String,
    pub level: LogLevel,
}

/// Configuration for [`init_logger`].
///
/// `default_level` applies to all modules without a `module_levels` override;
/// `max_file_size_mb` and `max_files` bound the rotating on-disk log.
#[frb]
#[derive(Clone, Debug)]
pub struct LoggerConfig {
    pub default_level: LogLevel,
    pub module_levels: Vec<LogModuleConfig>,
    pub max_file_size_mb: u64,
    pub max_files: usize,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            default_level: LogLevel::Info,
            max_file_size_mb: 10,
            max_files: 5,
            module_levels: vec![
                LogModuleConfig {
                    target: "minotari".to_string(),
                    level: LogLevel::Info,
                },
                LogModuleConfig {
                    target: "hyper".to_string(),
                    level: LogLevel::Warn,
                },
                LogModuleConfig {
                    target: "h2".to_string(),
                    level: LogLevel::Error,
                },
                LogModuleConfig {
                    target: PAYLOAD_SUMMARY_TARGET.to_string(),
                    level: LogLevel::Warn,
                },
            ],
        }
    }
}

struct KvTextVisitor<'a> {
    out: &'a mut String,
}

impl<'a, 'kvs> VisitSource<'kvs> for KvTextVisitor<'a> {
    fn visit_pair(&mut self, key: Key<'kvs>, value: Value<'kvs>) -> Result<(), Error> {
        write!(self.out, " {}={}", key, value).map_err(|_| Error::msg("fmt error"))
    }
}

/// Initialize the global logger, writing to a rotating file under `base_path/logs`
/// and to stdout, and install a panic hook that logs Rust panics.
///
/// `base_path` is the directory to create the `logs/` subfolder in; `config`
/// (`None` → sensible defaults) tunes levels and rotation. Call once at startup.
/// Synchronous (`#[frb(sync)]`); errors if the logger cannot be installed.
#[frb(sync)]
pub fn init_logger(base_path: String, config: Option<LoggerConfig>) -> Result<()> {
    let config = config.unwrap_or_default();

    let mut log_dir = PathBuf::from(base_path);
    log_dir.push("logs");
    std::fs::create_dir_all(&log_dir)?;

    let log_file_path = log_dir.join("minotari.log");

    let file_rotator = FileRotate::new(
        log_file_path,
        AppendCount::new(config.max_files),
        file_rotate::ContentLimit::Bytes((config.max_file_size_mb * BYTES_PER_MB) as usize),
        Compression::None,
        #[cfg(unix)]
        None,
    );
    let log_writer = LineWriter::new(file_rotator);

    let mut dispatch = fern::Dispatch::new()
        .format(|out, message, record| {
            let mut kv_output = String::new();
            let mut visitor = KvTextVisitor {
                out: &mut kv_output,
            };

            if let Err(e) = record.key_values().visit(&mut visitor) {
                let _ = write!(kv_output, " [KV Error: {}]", e);
            }

            out.finish(format_args!(
                "{}[{}][{}] {}{}",
                chrono::Local::now().format("[%Y-%m-%d %H:%M:%S]"),
                record.target(),
                record.level(),
                message,
                kv_output
            ))
        })
        .level(config.default_level.into())
        .chain(Box::new(log_writer) as Box<dyn std::io::Write + Send>)
        .chain(std::io::stdout());

    for module in with_payload_summary_cap(config.module_levels) {
        dispatch = dispatch.level_for(module.target, module.level.into());
    }

    if let Err(e) = dispatch.apply() {
        return Err(anyhow::anyhow!(
            "Logger could not be initialized: {}. Logs will go to the existing logger.",
            e
        ));
    }
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(
            "RUST PANIC: {}\nBacktrace:\n{}",
            info,
            std::backtrace::Backtrace::capture()
        );
        prev_hook(info);
    }));
    log::info!("Minotari Logger Initialized");
    Ok(())
}

/// Add the Warn cap for [`PAYLOAD_SUMMARY_TARGET`] unless `levels` already sets it.
fn with_payload_summary_cap(mut levels: Vec<LogModuleConfig>) -> Vec<LogModuleConfig> {
    if !levels.iter().any(|m| m.target == PAYLOAD_SUMMARY_TARGET) {
        levels.push(LogModuleConfig {
            target: PAYLOAD_SUMMARY_TARGET.to_string(),
            level: LogLevel::Warn,
        });
    }
    levels
}

#[cfg(test)]
mod tests {
    //! Enum-mapping tests. Pure conversions, no I/O. These are upstream-drift
    //! tripwires: if `log::LevelFilter` changes, this must be revisited.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn log_level_maps_to_level_filter_exhaustively() {
        assert_eq!(LevelFilter::from(LogLevel::Error), LevelFilter::Error);
        assert_eq!(LevelFilter::from(LogLevel::Warn), LevelFilter::Warn);
        assert_eq!(LevelFilter::from(LogLevel::Info), LevelFilter::Info);
        assert_eq!(LevelFilter::from(LogLevel::Debug), LevelFilter::Debug);
        assert_eq!(LevelFilter::from(LogLevel::Trace), LevelFilter::Trace);
        assert_eq!(LevelFilter::from(LogLevel::Off), LevelFilter::Off);
    }

    fn payload_summary_level(levels: &[LogModuleConfig]) -> Option<LogLevel> {
        levels
            .iter()
            .find(|m| m.target == "c::tx::offline_signing")
            .map(|m| m.level)
    }

    #[test]
    fn payload_summaries_are_capped_at_warn() {
        // The default config caps the target.
        let default = LoggerConfig::default().module_levels;
        assert_eq!(payload_summary_level(&default), Some(LogLevel::Warn));

        // A caller config without it gets the cap added.
        let custom = with_payload_summary_cap(vec![LogModuleConfig {
            target: "minotari".to_string(),
            level: LogLevel::Debug,
        }]);
        assert_eq!(payload_summary_level(&custom), Some(LogLevel::Warn));

        // An explicit caller choice is kept.
        let explicit = with_payload_summary_cap(vec![LogModuleConfig {
            target: "c::tx::offline_signing".to_string(),
            level: LogLevel::Info,
        }]);
        assert_eq!(explicit.len(), 1);
        assert_eq!(payload_summary_level(&explicit), Some(LogLevel::Info));
    }
}
