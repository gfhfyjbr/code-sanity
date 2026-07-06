//! Session logging: records land in `.code-sanity/logs/code-sanity.log`
//! (append), warnings and errors also mirror to stderr. `-v`/`-vv` raise both
//! levels. Outside an initialized workspace only the stderr sink is active, so
//! running `code-sanity --help` in a random directory never creates state.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

struct CliLogger {
    file: Mutex<Option<File>>,
    file_level: Level,
    stderr_level: Level,
}

impl Log for CliLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.file_level || metadata.level() <= self.stderr_level
    }

    fn log(&self, record: &Record<'_>) {
        if record.level() <= self.file_level
            && let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = writeln!(
                file,
                "{} {:5} {}",
                chrono::Utc::now().to_rfc3339(),
                record.level(),
                record.args()
            );
        }
        if record.level() <= self.stderr_level {
            eprintln!("code-sanity: {}: {}", record.level(), record.args());
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock()
            && let Some(file) = guard.as_mut()
        {
            let _ = file.flush();
        }
    }
}

/// Install the global logger. The log file is opened only when the workspace
/// state dir already exists; repeated calls (tests) are a no-op.
pub fn init(layout: &crate::config::Layout, verbosity: u8) {
    let (file_level, stderr_level) = match verbosity {
        0 => (Level::Info, Level::Warn),
        1 => (Level::Debug, Level::Info),
        _ => (Level::Trace, Level::Debug),
    };
    let file = if layout.state_dir.exists() {
        open_log_file(&layout.logs_dir)
    } else {
        None
    };
    let logger = CliLogger {
        file: Mutex::new(file),
        file_level,
        stderr_level,
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(LevelFilter::Trace);
    }
}

fn open_log_file(logs_dir: &Path) -> Option<File> {
    std::fs::create_dir_all(logs_dir).ok()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs_dir.join("code-sanity.log"))
        .ok()
}
