use anyhow::Result;
use file_rotate::{compression::Compression, suffix::AppendCount, FileRotate};
use flutter_rust_bridge::frb;
use log::kv::{Error, Key, Value, VisitSource};
use log::LevelFilter;
use std::io::LineWriter;
use std::path::PathBuf;

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

#[frb]
#[derive(Clone, Debug)]
pub struct LogModuleConfig {
    pub target: String,
    pub level: LogLevel,
}

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
            ],
        }
    }
}

struct KvTextVisitor<'a> {
    out: &'a mut String,
}

impl<'a, 'kvs> VisitSource<'kvs> for KvTextVisitor<'a> {
    fn visit_pair(&mut self, key: Key<'kvs>, value: Value<'kvs>) -> Result<(), Error> {
        use std::fmt::Write;
        write!(self.out, " {}={}", key, value).map_err(|_| Error::msg("fmt error"))
    }
}

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
        file_rotate::ContentLimit::Bytes((config.max_file_size_mb * 1024 * 1024) as usize),
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
                use std::fmt::Write;
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

    for module in config.module_levels {
        dispatch = dispatch.level_for(module.target, module.level.into());
    }

    if let Err(e) = dispatch.apply() {
        println!(
            "Logger could not be initialized: {}. Logs will go to the existing logger.",
            e
        );
    } else {
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
    }

    Ok(())
}
