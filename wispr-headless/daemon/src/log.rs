//! The daemon's own log file: what it reports to the shim as `log` events,
//! and how dictations start and end, so a problem on a user's machine can be
//! looked at afterwards. Wispr does not keep the `log` events anywhere.
//!
//! macOS: ~/Library/Logs/wispr-headless.log; elsewhere
//! $XDG_STATE_HOME/wispr-headless/log (~/.local/state/...). `--log-file`
//! chooses another file, and an empty one turns it off. At `MAX_BYTES` the
//! file is renamed to `<file>.1` (replacing the one before) and a new one
//! begins, so it stays small. Like the rest of the daemon it never records
//! what was said or typed, nor which keys were pressed beyond the shortcut.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 1024 * 1024;

struct LogFile {
    path: PathBuf,
    file: Option<File>,
    written: u64,
}

static LOG: OnceLock<Mutex<LogFile>> = OnceLock::new();

/// Where the log goes by default.
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Logs/wispr-headless.log"));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".local/state")))?;
    Some(state.join("wispr-headless/log"))
}

fn open(path: &Path) -> Option<(File, u64)> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    Some((file, len))
}

/// Starts logging to `path` (None: nowhere). Only the first call counts.
pub fn init(path: Option<PathBuf>) {
    let Some(path) = path else { return };
    let (file, written) = match open(&path) {
        Some((f, n)) => (Some(f), n),
        None => (None, 0),
    };
    let _ = LOG.set(Mutex::new(LogFile {
        path,
        file,
        written,
    }));
    write(&format!(
        "wispr-headless {} started (pid {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    ));
}

/// One line with the time (UTC, milliseconds).
pub fn write(msg: &str) {
    let Some(log) = LOG.get() else { return };
    let Ok(mut log) = log.lock() else { return };
    let line = format!("{} {}\n", timestamp(SystemTime::now()), msg.trim_end());
    if log.written + line.len() as u64 > MAX_BYTES {
        log.file = None;
        let old = log.path.with_extension(match log.path.extension() {
            Some(e) => format!("{}.1", e.to_string_lossy()),
            None => "1".into(),
        });
        let _ = std::fs::rename(&log.path, old);
        match open(&log.path) {
            Some((f, n)) => {
                log.file = Some(f);
                log.written = n;
            }
            None => log.written = 0,
        }
    }
    if let Some(f) = log.file.as_mut() {
        if f.write_all(line.as_bytes()).is_ok() {
            log.written += line.len() as u64;
        }
    }
}

/// `2026-10-24T13:05:09.123Z`.
fn timestamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60,
        d.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps_are_utc_dates() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_793_538_309_123);
        assert_eq!(timestamp(t), "2026-11-01T13:05:09.123Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(timestamp(leap), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn the_file_is_cut_at_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/wispr-headless/log");
        init(Some(path.clone()));
        let line = "x".repeat(1000);
        for _ in 0..1_500 {
            write(&line);
        }
        let now = std::fs::metadata(&path).unwrap().len();
        let old = std::fs::metadata(dir.path().join("state/wispr-headless/log.1"))
            .unwrap()
            .len();
        assert!(now <= MAX_BYTES && old <= MAX_BYTES, "{now} {old}");
        assert!(old > MAX_BYTES / 2);
    }
}
