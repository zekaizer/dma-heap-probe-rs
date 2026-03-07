// Perfetto atrace marker module.
//
// Writes to /sys/kernel/tracing/trace_marker when --trace is enabled.
// All functions are no-op when init() has not been called or trace_marker is unavailable.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static TRACE_MARKER: OnceLock<Mutex<Option<File>>> = OnceLock::new();

fn marker() -> &'static Mutex<Option<File>> {
    TRACE_MARKER.get_or_init(|| Mutex::new(None))
}

/// Initialize the trace marker by opening `/sys/kernel/tracing/trace_marker`.
///
/// Call once at startup when `--trace` is enabled. Silently becomes no-op
/// if the file cannot be opened (e.g. on host or without permissions).
pub fn init() {
    let file = OpenOptions::new()
        .write(true)
        .open("/sys/kernel/tracing/trace_marker")
        .ok();
    if let Ok(mut guard) = marker().lock() {
        *guard = file;
    }
}

/// Write a raw string to the trace marker. No-op if not initialized.
fn write_marker(msg: &str) {
    if let Ok(mut guard) = marker().lock()
        && let Some(ref mut f) = *guard
    {
        // Ignore write errors — tracing should not disrupt test execution.
        let _ = f.write_all(msg.as_bytes());
    }
}

/// Format a begin marker: `B|<pid>|<section_name>\n`
pub(crate) fn format_begin(pid: u32, section: &str) -> String {
    format!("B|{pid}|{section}\n")
}

/// Format an end marker: `E|<pid>\n`
pub(crate) fn format_end(pid: u32) -> String {
    format!("E|{pid}\n")
}

/// Format a counter marker: `C|<pid>|<counter_name>|<value>\n`
pub(crate) fn format_counter(pid: u32, name: &str, value: i64) -> String {
    format!("C|{pid}|{name}|{value}\n")
}

/// Write a begin marker for a trace section.
pub fn begin(section: &str) {
    let msg = format_begin(std::process::id(), section);
    write_marker(&msg);
}

/// Write an end marker for a trace section.
pub fn end() {
    let msg = format_end(std::process::id());
    write_marker(&msg);
}

/// Write a counter value.
pub fn counter(name: &str, value: i64) {
    let msg = format_counter(std::process::id(), name, value);
    write_marker(&msg);
}

/// RAII guard for trace sections. Writes begin on creation, end on drop.
pub struct TraceSection {
    _private: (), // prevent construction outside this module
}

impl TraceSection {
    /// Create a new trace section guard. Writes `B|<pid>|<section>` immediately.
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
        // begin/end/counter should not panic when marker is not initialized.
        begin("test_section");
        end();
        counter("test_counter", 0);
    }

    #[test]
    fn trace_section_guard() {
        // TraceSection should not panic without init.
        let _guard = TraceSection::new("test_guard");
        // Drop runs end() — should be no-op.
    }
}
