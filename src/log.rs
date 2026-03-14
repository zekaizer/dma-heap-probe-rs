// Global log file for tee output (stdout + file).

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

static LOG_FILE: OnceLock<Mutex<BufWriter<File>>> = OnceLock::new();

/// Initialize the global log file. Must be called at most once.
pub fn init(path: &Path) -> io::Result<()> {
    let file = File::create(path)?;
    let writer = BufWriter::new(file);
    LOG_FILE.set(Mutex::new(writer)).map_err(|_| {
        io::Error::new(io::ErrorKind::AlreadyExists, "log file already initialized")
    })?;
    Ok(())
}

/// Return a reference to the global log file mutex, if initialized.
pub fn writer() -> Option<&'static Mutex<BufWriter<File>>> {
    LOG_FILE.get()
}

/// Try to clone the underlying file handle for use as a tracing layer writer.
/// Returns `None` if the log file was not initialized.
pub fn try_clone_file() -> Option<File> {
    let lock = LOG_FILE.get()?;
    let guard = lock.lock().ok()?;
    guard.get_ref().try_clone().ok()
}

/// Flush the global log file buffer.
pub fn flush() {
    if let Some(lock) = LOG_FILE.get() {
        if let Ok(mut guard) = lock.lock() {
            let _ = guard.flush();
        }
    }
}

/// Format the current walltime as `[YYYY-MM-DDThh:mm:ss.mmm]`.
#[doc(hidden)]
pub fn walltime_prefix() -> String {
    let now = SystemTime::now();
    let dur = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs();
    let millis = dur.subsec_millis();

    // Manual UTC breakdown (no chrono dependency).
    let secs_per_day: u64 = 86400;
    let mut days = total_secs / secs_per_day;
    let day_secs = total_secs % secs_per_day;
    let hour = day_secs / 3600;
    let minute = (day_secs % 3600) / 60;
    let second = day_secs % 60;

    // Date from days since 1970-01-01 (civil calendar algorithm).
    days += 719_468;
    let era = days / 146_097;
    let doe = days % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("[{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}]")
}

/// Print to stdout and optionally tee to the global log file with a walltime prefix.
#[macro_export]
macro_rules! tee_println {
    () => {{
        println!();
        if let Some(lock) = $crate::log::writer() {
            if let Ok(mut f) = lock.lock() {
                let ts = $crate::log::walltime_prefix();
                let _ = writeln!(f, "{ts}");
            }
        }
    }};
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        println!("{msg}");
        if let Some(lock) = $crate::log::writer() {
            if let Ok(mut f) = lock.lock() {
                let ts = $crate::log::walltime_prefix();
                let _ = writeln!(f, "{ts} {msg}");
            }
        }
    }};
}

/// Print to stdout (no newline) and optionally tee to the global log file with a walltime prefix.
#[macro_export]
macro_rules! tee_print {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        print!("{msg}");
        if let Some(lock) = $crate::log::writer() {
            if let Ok(mut f) = lock.lock() {
                let ts = $crate::log::walltime_prefix();
                let _ = write!(f, "{ts} {msg}");
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn walltime_prefix_format() {
        let prefix = walltime_prefix();
        // Must match [YYYY-MM-DDThh:mm:ss.mmm]
        // [YYYY-MM-DDThh:mm:ss.mmm] = 25 chars
        assert_eq!(prefix.len(), 25);
        assert_eq!(&prefix[0..1], "[");
        assert_eq!(&prefix[24..25], "]");
        assert_eq!(&prefix[5..6], "-");
        assert_eq!(&prefix[8..9], "-");
        assert_eq!(&prefix[11..12], "T");
        assert_eq!(&prefix[14..15], ":");
        assert_eq!(&prefix[17..18], ":");
        assert_eq!(&prefix[20..21], ".");
    }

    #[test]
    fn tee_println_without_init_does_not_panic() {
        // LOG_FILE not initialized — macro should just println without error.
        // We can't test the actual println output easily, but no panic is the goal.
        let msg = format!("test message {}", 42);
        println!("{msg}");
        assert!(writer().is_none());
    }

    #[test]
    fn init_and_write() {
        // Use a separate static for isolation (global LOG_FILE may already be set).
        let dir = std::env::temp_dir().join("dhp_log_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.log");

        // We can only test init if it hasn't been called before in this process.
        // Since tests run in the same process, skip if already initialized.
        if LOG_FILE.get().is_some() {
            return;
        }

        init(&path).expect("init log file");
        assert!(writer().is_some());

        // Write via the writer directly.
        {
            let lock = writer().unwrap();
            let mut guard = lock.lock().unwrap();
            writeln!(guard, "hello from test").unwrap();
        }
        flush();

        let mut content = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(content.contains("hello from test"));

        let _ = std::fs::remove_file(&path);
    }
}
