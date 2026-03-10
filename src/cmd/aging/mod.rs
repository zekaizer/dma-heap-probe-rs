// Aging tests: sustained alloc/free with periodic metric reporting.
//
// Supports two modes:
// - Normal: full pipeline round-robin across heaps
// - Fuzz: random size, operation, timing with deterministic seeding

pub(crate) mod fuzz;
pub(crate) mod worker;

use std::error::Error;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::cmd::perf;
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::DMA_BUF_SYNC_WRITE;
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};
use crate::{procfs, sysfs};

// ── Shared state ────────────────────────────────────────────────────────────

/// Shared state across aging workers and the reporter thread.
pub(crate) struct AgingState {
    pub running: AtomicBool,
    pub total_iters: AtomicU64,
    pub total_errors: AtomicU64,
    pub interval_latencies: Mutex<Vec<u64>>,
}

impl AgingState {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            total_iters: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            interval_latencies: Mutex::new(Vec::new()),
        }
    }
}

// ── SIGINT handling ─────────────────────────────────────────────────────────

static SIGINT_FLAG: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_: libc::c_int) {
    SIGINT_FLAG.store(true, Relaxed);
}

pub(crate) fn install_sigint_handler() {
    // SAFETY: setting a simple signal handler for SIGINT.
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_sigint as *const () as libc::sighandler_t,
        );
    }
}

pub(crate) fn sigint_received() -> bool {
    SIGINT_FLAG.load(Relaxed)
}

// ── Termination check ───────────────────────────────────────────────────────

pub(crate) fn should_stop(
    state: &AgingState,
    deadline: Option<Instant>,
    max_iters: Option<u64>,
) -> bool {
    if !state.running.load(Relaxed) {
        return true;
    }
    if sigint_received() {
        tracing::debug!("sigint received, stopping");
        state.running.store(false, Relaxed);
        return true;
    }
    if let Some(dl) = deadline
        && Instant::now() >= dl
    {
        tracing::debug!("deadline reached, stopping");
        state.running.store(false, Relaxed);
        return true;
    }
    if let Some(max) = max_iters
        && state.total_iters.load(Relaxed) >= max
    {
        tracing::debug!(
            iterations = state.total_iters.load(Relaxed),
            max,
            "iteration limit reached, stopping"
        );
        state.running.store(false, Relaxed);
        return true;
    }
    false
}

// ── Heap discovery ──────────────────────────────────────────────────────────

/// Discover available heap names. Uses overrides if provided, else scans
/// `/dev/dma_heap/`, falling back to `["system"]`.
pub(crate) fn discover_heaps(override_heaps: Option<&[String]>) -> Vec<String> {
    if let Some(heaps) = override_heaps {
        tracing::debug!(count = heaps.len(), "using override heaps");
        return heaps.to_vec();
    }
    if let Ok(entries) = std::fs::read_dir("/dev/dma_heap") {
        let mut heaps: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        heaps.sort();
        if !heaps.is_empty() {
            tracing::debug!(count = heaps.len(), "discovered heaps from /dev/dma_heap");
            return heaps;
        }
    }
    tracing::debug!("no heaps found, falling back to system");
    vec!["system".to_string()]
}

// ── Heap capability probing ─────────────────────────────────────────────────

/// Per-heap capability flags determined by probing.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct HeapCaps {
    pub name: String,
    pub can_alloc: bool,
    pub can_mmap: bool,
    pub can_sync: bool,
    pub can_write: bool,
    pub can_llseek: bool,
    pub can_set_name: bool,
    pub can_sync_file: bool,
    pub can_dup: bool,
}

impl HeapCaps {
    fn new_false(name: &str) -> Self {
        Self {
            name: name.to_string(),
            can_alloc: false,
            can_mmap: false,
            can_sync: false,
            can_write: false,
            can_llseek: false,
            can_set_name: false,
            can_sync_file: false,
            can_dup: false,
        }
    }
}

/// Probe a single heap to determine which operations are supported.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn probe_heap<B: HeapBackend + DmaBufBackend>(backend: &B, heap_name: &str) -> HeapCaps {
    let mut caps = HeapCaps::new_false(heap_name);

    let Ok(heap) = DmaHeap::open(backend, heap_name) else {
        tracing::trace!(heap = heap_name, "probe: open failed");
        return caps;
    };
    let Ok(fd) = heap.alloc(4096, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS) else {
        tracing::trace!(heap = heap_name, "probe: alloc failed");
        return caps;
    };
    caps.can_alloc = true;

    let mut buf = DmaBuf::new(backend, fd, 4096);

    // mmap
    if let Ok(ptr) = buf.mmap() {
        caps.can_mmap = true;
        tracing::trace!(heap = heap_name, "probe: mmap ok");

        // sync + write
        if buf.sync_start(DMA_BUF_SYNC_WRITE).is_ok() {
            caps.can_sync = true;
            let write_ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: ptr is valid and mapped to 4096 bytes.
                unsafe {
                    std::ptr::write_bytes(ptr, 0xAA, 4096);
                }
            }));
            caps.can_write = write_ok.is_ok();
            tracing::trace!(heap = heap_name, ok = caps.can_write, "probe: write");
            let _ = buf.sync_end(DMA_BUF_SYNC_WRITE);
        } else {
            tracing::trace!(heap = heap_name, "probe: sync failed");
        }
    } else {
        tracing::trace!(heap = heap_name, "probe: mmap failed");
    }

    caps.can_llseek = buf.llseek_size().is_ok();
    tracing::trace!(heap = heap_name, ok = caps.can_llseek, "probe: llseek");
    caps.can_set_name = buf.set_name("probe").is_ok();
    tracing::trace!(heap = heap_name, ok = caps.can_set_name, "probe: set_name");

    #[allow(clippy::cast_possible_truncation)]
    {
        caps.can_sync_file = buf.export_sync_file(DMA_BUF_SYNC_WRITE as u32).is_ok();
    }
    tracing::trace!(
        heap = heap_name,
        ok = caps.can_sync_file,
        "probe: sync_file"
    );

    if let Ok(dup_buf) = buf.dup() {
        caps.can_dup = true;
        drop(dup_buf);
    }
    tracing::trace!(heap = heap_name, ok = caps.can_dup, "probe: dup");

    drop(buf);

    tracing::info!(
        heap = heap_name,
        alloc = caps.can_alloc,
        mmap = caps.can_mmap,
        sync = caps.can_sync,
        write = caps.can_write,
        llseek = caps.can_llseek,
        set_name = caps.can_set_name,
        sync_file = caps.can_sync_file,
        dup = caps.can_dup,
        "heap capabilities probed"
    );

    caps
}

/// Discover heaps and probe each one, returning only those that can alloc.
pub(crate) fn discover_and_probe<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    override_heaps: Option<&[String]>,
) -> Vec<HeapCaps> {
    let names = discover_heaps(override_heaps);
    let caps: Vec<HeapCaps> = names
        .iter()
        .map(|name| probe_heap(backend, name))
        .filter(|c| c.can_alloc)
        .collect();

    if caps.is_empty() {
        tracing::error!("no usable heaps found");
    } else {
        tracing::info!(
            count = caps.len(),
            heaps = ?caps.iter().map(|c| &c.name).collect::<Vec<_>>(),
            "usable heaps"
        );
    }
    caps
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Compute memory delta in MB between two optional `MemAvailable` values (in KB).
#[allow(clippy::cast_possible_wrap)]
fn compute_mem_delta_mb(initial_kb: Option<u64>, current_kb: Option<u64>) -> Option<i64> {
    match (initial_kb, current_kb) {
        (Some(init), Some(now)) => Some((now as i64 - init as i64) / 1024),
        _ => None,
    }
}

/// Mark initialization failure in shared state.
pub(crate) fn mark_init_error(state: &AgingState) {
    state.total_errors.fetch_add(1, Relaxed);
    state.running.store(false, Relaxed);
}

// ── Reporter loop ───────────────────────────────────────────────────────────

/// Periodic reporter that drains interval latencies and logs metrics.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn reporter_loop(
    state: &AgingState,
    report_interval: Duration,
    start_time: Instant,
    initial_mem_available_kb: Option<u64>,
) {
    let mut first_interval_avg: Option<u64> = None;

    loop {
        // Sleep in 1-second chunks for responsive shutdown.
        let mut waited = Duration::ZERO;
        while waited < report_interval {
            if !state.running.load(Relaxed) {
                return;
            }
            let chunk = Duration::from_secs(1).min(report_interval.saturating_sub(waited));
            std::thread::sleep(chunk);
            waited += chunk;
        }
        if !state.running.load(Relaxed) {
            return;
        }

        tracing::trace!("reporter wakeup");
        let latencies = std::mem::take(&mut *state.interval_latencies.lock().unwrap());
        let lat_stats = perf::compute_stats(&latencies);

        let mem_available = procfs::read_meminfo().ok().map(|m| m.mem_available_kb);
        let mem_delta_mb = compute_mem_delta_mb(initial_mem_available_kb, mem_available);
        let buf_count = sysfs::snapshot()
            .ok()
            .map_or(0, |snap| sysfs::buffer_count(&snap));

        let avg_us = lat_stats.as_ref().map(|ls| ls.avg_us);
        if first_interval_avg.is_none() {
            first_interval_avg = avg_us;
        }
        let trend = match (avg_us, first_interval_avg) {
            (Some(cur), Some(first)) if first > 0 => cur as f64 / first as f64,
            _ => 1.0,
        };

        tracing::info!(
            elapsed_s = start_time.elapsed().as_secs(),
            iterations = state.total_iters.load(Relaxed),
            interval_count = latencies.len(),
            avg_us = avg_us.unwrap_or(0),
            p99_us = lat_stats.as_ref().map_or(0, |ls| ls.p99_us),
            errors = state.total_errors.load(Relaxed),
            mem_delta_mb = mem_delta_mb.unwrap_or(0),
            buf_count,
            trend = format!("{trend:.1}x"),
            "aging report"
        );
    }
}

// ── Run framework ───────────────────────────────────────────────────────────

/// Run workers with a reporter thread. Emits a final report on shutdown.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn run_with_reporter<F>(state: &AgingState, report_interval: Duration, worker_fn: F)
where
    F: FnOnce(),
{
    install_sigint_handler();
    tracing::trace!("sigint handler installed");
    let start_time = Instant::now();
    let initial_mem = procfs::read_meminfo().ok().map(|m| m.mem_available_kb);

    std::thread::scope(|s| {
        s.spawn(|| reporter_loop(state, report_interval, start_time, initial_mem));
        worker_fn();
        state.running.store(false, Relaxed);
    });

    // Final report with remaining interval data.
    let remaining = std::mem::take(&mut *state.interval_latencies.lock().unwrap());
    let final_stats = perf::compute_stats(&remaining);
    let final_mem = procfs::read_meminfo().ok().map(|m| m.mem_available_kb);
    let mem_delta_mb = compute_mem_delta_mb(initial_mem, final_mem);
    let buf_count = sysfs::snapshot()
        .ok()
        .map_or(0, |snap| sysfs::buffer_count(&snap));

    tracing::info!(
        elapsed_s = start_time.elapsed().as_secs(),
        total_iterations = state.total_iters.load(Relaxed),
        total_errors = state.total_errors.load(Relaxed),
        last_interval_count = remaining.len(),
        last_avg_us = final_stats.as_ref().map_or(0, |ls| ls.avg_us),
        last_p99_us = final_stats.as_ref().map_or(0, |ls| ls.p99_us),
        mem_delta_mb = mem_delta_mb.unwrap_or(0),
        buf_count,
        "aging complete — final report"
    );
}

// ── Public entry point ──────────────────────────────────────────────────────

/// Run the aging test.
#[allow(clippy::too_many_arguments)]
pub fn run<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heaps: &[String],
    size: u64,
    threads: u32,
    duration: Option<Duration>,
    iterations: Option<u64>,
    report_interval: Duration,
    fuzz_mode: bool,
    max_hold: usize,
    seed: Option<u64>,
) -> (Vec<SubTestResult>, Option<Box<dyn Error>>) {
    let mode = if fuzz_mode { "fuzz" } else { "normal" };
    tracing::debug!(
        mode,
        threads,
        heaps = heaps.len(),
        size,
        ?duration,
        ?iterations,
        report_interval_s = report_interval.as_secs(),
        "aging start"
    );

    let state = AgingState::new();

    run_with_reporter(&state, report_interval, || {
        if fuzz_mode {
            fuzz::run_workers(
                backend, heaps, threads, &state, duration, iterations, max_hold, seed,
            );
        } else {
            worker::run_workers(backend, heaps, size, threads, &state, duration, iterations);
        }
    });

    let errors = state.total_errors.load(Relaxed);
    if errors > 0 {
        runner::collect_test_results("aging", &[("aging", Err(Errno::EIO))])
    } else {
        runner::collect_test_results("aging", &[("aging", Ok(()))])
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_heaps_with_override() {
        let heaps = vec!["a".to_string(), "b".to_string()];
        let result = discover_heaps(Some(&heaps));
        assert_eq!(result, heaps);
    }

    #[test]
    fn discover_heaps_fallback() {
        // On non-Android hosts, /dev/dma_heap doesn't exist.
        let result = discover_heaps(None);
        assert_eq!(result, vec!["system".to_string()]);
    }

    #[test]
    fn should_stop_iteration_limit() {
        let state = AgingState::new();
        state.total_iters.store(10, Relaxed);
        assert!(should_stop(&state, None, Some(10)));
    }

    #[test]
    fn should_stop_running_false() {
        let state = AgingState::new();
        state.running.store(false, Relaxed);
        assert!(should_stop(&state, None, None));
    }

    #[test]
    fn probe_heap_mock_all_caps() {
        let b = crate::backend::mock::MockBackend::new();
        let caps = probe_heap(&b, "system");
        assert!(caps.can_alloc);
        assert!(caps.can_mmap);
        assert!(caps.can_sync);
        assert!(caps.can_write);
        assert!(caps.can_llseek);
        assert!(caps.can_set_name);
        assert!(caps.can_sync_file);
        assert!(caps.can_dup);
    }

    #[test]
    fn probe_heap_bad_heap() {
        let b = crate::backend::mock::MockBackend::new();
        let caps = probe_heap(&b, "");
        assert!(!caps.can_alloc);
    }

    #[test]
    fn run_normal_passes() {
        let b = crate::backend::mock::MockBackend::new();
        let heaps = vec!["system".to_string()];
        let (results, err) = run(
            &b,
            &heaps,
            4096,
            1,
            None,
            Some(10),
            Duration::from_secs(60),
            false,
            32,
            None,
        );
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(results.iter().all(|t| t.passed));
    }

    #[test]
    fn run_fuzz_passes() {
        let b = crate::backend::mock::MockBackend::new();
        let heaps = vec!["system".to_string()];
        let (results, err) = run(
            &b,
            &heaps,
            4096,
            1,
            None,
            Some(10),
            Duration::from_secs(60),
            true,
            8,
            Some(42),
        );
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(results.iter().all(|t| t.passed));
    }
}
