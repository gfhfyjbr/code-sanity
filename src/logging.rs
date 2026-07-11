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
    /// `None` disables the stderr sink entirely: the MCP stdio server runs
    /// with file-only logging because hosts commonly fold server stderr into
    /// their own logs, and warn/error text can carry unredacted real terms.
    stderr_level: Option<Level>,
}

impl Log for CliLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.file_level
            || self
                .stderr_level
                .is_some_and(|level| metadata.level() <= level)
    }

    fn log(&self, record: &Record<'_>) {
        if record.level() <= self.file_level {
            if let Ok(mut guard) = self.file.lock() {
                if let Some(file) = guard.as_mut() {
                    let _ = writeln!(
                        file,
                        "{} {:5} {}",
                        chrono::Utc::now().to_rfc3339(),
                        record.level(),
                        record.args()
                    );
                }
            }
        }
        if self
            .stderr_level
            .is_some_and(|level| record.level() <= level)
        {
            eprintln!("code-sanity: {}: {}", record.level(), record.args());
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

/// Install the global logger. The log file is opened only when the workspace
/// state dir already exists; repeated calls (tests) are a no-op.
/// `stderr` disables the stderr sink when false (file-only logging for the
/// MCP stdio server — see `CliLogger::stderr_level`).
pub fn init(layout: &crate::config::Layout, verbosity: u8, stderr: bool) {
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
        stderr_level: stderr.then_some(stderr_level),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(LevelFilter::Trace);
    }
}

/// Rotate at open time: one `.old` generation, overwritten. Bounds total log
/// disk use at ~2×MAX without new deps or signal handling; per-edit hook
/// syncs used to grow the log forever.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

fn open_log_file(logs_dir: &Path) -> Option<File> {
    std::fs::create_dir_all(logs_dir).ok()?;
    let path = logs_dir.join("code-sanity.log");
    rotate_if_oversized(&path, MAX_LOG_BYTES);
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Best-effort by design: logging must never fail the command.
fn rotate_if_oversized(path: &Path, max_bytes: u64) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() > max_bytes {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
}

#[cfg(test)]
mod tests {
    use super::rotate_if_oversized;

    #[test]
    fn rotation_keeps_one_old_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code-sanity.log");
        let old = dir.path().join("code-sanity.log.old");

        std::fs::write(&path, "small").unwrap();
        rotate_if_oversized(&path, 10);
        assert!(path.exists() && !old.exists(), "small file must not rotate");

        std::fs::write(&path, "0123456789ab").unwrap();
        rotate_if_oversized(&path, 10);
        assert!(!path.exists(), "oversized file must rotate away");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "0123456789ab");

        // A second rotation overwrites the previous generation.
        std::fs::write(&path, "newer-content").unwrap();
        rotate_if_oversized(&path, 10);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "newer-content");
    }
}
