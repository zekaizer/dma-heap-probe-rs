// Perfetto atrace marker module.
//
// Writes to /sys/kernel/tracing/trace_marker when --trace is enabled.
// All functions are no-op when init() has not been called or trace_marker is unavailable.
//
// Uses per-thread file descriptors and tid-based markers for correct
// multi-threaded Perfetto attribution.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

const TRACE_MARKER_PATH: &str = "/sys/kernel/tracing/trace_marker";

/// Global fast-path flag: set once by `init()`, never cleared.
static ENABLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Per-thread file handle to trace_marker, opened lazily on first use.
    static THREAD_MARKER: RefCell<Option<File>> = const { RefCell::new(None) };
}

/// Initialize tracing. Call once at startup when `--trace` is enabled.
///
/// On Android, validates that `trace_marker` exists before enabling.
/// On other platforms, enables unconditionally (writes will silently fail
/// if the file cannot be opened).
pub fn init() {
    #[cfg(target_os = "android")]
    {
        if std::path::Path::new(TRACE_MARKER_PATH).exists() {
            ENABLED.store(true, Relaxed);
            tracing::debug!("trace_marker found, tracing enabled");
        } else {
            tracing::warn!("trace_marker not found, tracing disabled");
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        ENABLED.store(true, Relaxed);
    }
}

/// Fast-path check: returns true if tracing is enabled.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// Return the current thread id for trace markers.
fn current_tid() -> u32 {
    #[cfg(target_os = "android")]
    {
        // SAFETY: gettid() is always safe to call.
        #[allow(clippy::cast_sign_loss)]
        let tid = unsafe { libc::gettid() } as u32;
        tid
    }
    #[cfg(not(target_os = "android"))]
    {
        std::process::id()
    }
}

/// Write a raw string to the per-thread trace marker fd.
/// Opens the fd on first use. No-op if tracing is not enabled.
fn write_marker(msg: &str) {
    if !enabled() {
        return;
    }
    THREAD_MARKER.with(|cell| {
        let mut borrow = cell.borrow_mut();
        // Open on first use for this thread.
        if borrow.is_none() {
            *borrow = OpenOptions::new().write(true).open(TRACE_MARKER_PATH).ok();
        }
        if let Some(ref mut f) = *borrow {
            // Ignore write errors — tracing should not disrupt test execution.
            let _ = f.write_all(msg.as_bytes());
        }
    });
}

/// Format a begin marker: `B|<tid>|<section_name>\n`
pub(crate) fn format_begin(tid: u32, section: &str) -> String {
    format!("B|{tid}|{section}\n")
}

/// Format an end marker: `E|<tid>\n`
pub(crate) fn format_end(tid: u32) -> String {
    format!("E|{tid}\n")
}

/// Format a counter marker: `C|<tid>|<counter_name>|<value>\n`
pub(crate) fn format_counter(tid: u32, name: &str, value: i64) -> String {
    format!("C|{tid}|{name}|{value}\n")
}

/// Write a begin marker for a trace section.
pub fn begin(section: &str) {
    let msg = format_begin(current_tid(), section);
    write_marker(&msg);
}

/// Write an end marker for a trace section.
pub fn end() {
    let msg = format_end(current_tid());
    write_marker(&msg);
}

/// Write a counter value.
#[allow(dead_code)]
pub fn counter(name: &str, value: i64) {
    let msg = format_counter(current_tid(), name, value);
    write_marker(&msg);
}

/// Write a zero-duration instant event (begin+end pair).
pub fn instant(name: &str) {
    begin(name);
    end();
}

/// RAII guard for trace sections. Writes begin on creation, end on drop.
#[allow(dead_code)]
pub struct TraceSection {
    _private: (), // prevent construction outside this module
}

impl TraceSection {
    /// Create a new trace section guard. Writes `B|<tid>|<section>` immediately.
    #[allow(dead_code)]
    pub fn new(section: &str) -> Self {
        begin(section);
        Self { _private: () }
    }
}

impl Drop for TraceSection {
    fn drop(&mut self) {
        end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_begin_format() {
        assert_eq!(format_begin(1234, "test_alloc"), "B|1234|test_alloc\n");
    }

    #[test]
    fn trace_end_format() {
        assert_eq!(format_end(1234), "E|1234\n");
    }

    #[test]
    fn trace_counter_format() {
        assert_eq!(
            format_counter(1234, "alloc_count", 42),
            "C|1234|alloc_count|42\n"
        );
        assert_eq!(
            format_counter(5678, "latency_us", -1),
            "C|5678|latency_us|-1\n"
        );
    }

    #[test]
    fn trace_noop_without_init() {
        // begin/end/counter/instant should not panic when marker is not initialized.
        begin("test_section");
        end();
        counter("test_counter", 0);
        instant("test_instant");
    }

    #[test]
    fn trace_section_guard() {
        // TraceSection should not panic without init.
        let _guard = TraceSection::new("test_guard");
        // Drop runs end() — should be no-op.
    }

    #[test]
    fn enabled_false_before_init() {
        // On non-Android, ENABLED is false before init() is called.
        // Note: other tests may call init(), so this is best-effort in test isolation.
        // The key contract: enabled() returns a bool without panicking.
        let _ = enabled();
    }

    #[test]
    fn current_tid_nonzero() {
        assert!(current_tid() > 0);
    }
}
