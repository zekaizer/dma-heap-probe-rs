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
use crate::runner::{self, SubTestResult};
use crate::{procfs, sysfs};

// ── Shared state ────────────────────────────────────────────────────────────

/// Shared state across aging workers and the reporter thread.
pub(crate) struct AgingState {
    pub running: AtomicBool,
    pub total_iters: AtomicU64,
    pub total_errors: AtomicU64,
    pub total_allocs: AtomicU64,
    pub total_frees: AtomicU64,
    pub interval_latencies: Mutex<Vec<u64>>,
}

impl AgingState {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            total_iters: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            total_allocs: AtomicU64::new(0),
            total_frees: AtomicU64::new(0),
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
    let mut prev_allocs: u64 = 0;
    let mut prev_frees: u64 = 0;

    tracing::info!(
        "aging report fields:\n  \
         time:    elapsed(s) iters samples\n  \
         alloc:   allocs frees bufs\n  \
         latency: avg_us p99_us trend(x)\n  \
         system:  errs mem_mb(avail delta)"
    );

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

        let cur_allocs = state.total_allocs.load(Relaxed);
        let cur_frees = state.total_frees.load(Relaxed);
        let interval_allocs = cur_allocs - prev_allocs;
        let interval_frees = cur_frees - prev_frees;
        prev_allocs = cur_allocs;
        prev_frees = cur_frees;

        tracing::info!(
            elapsed = start_time.elapsed().as_secs(),
            iters = state.total_iters.load(Relaxed),
            samples = latencies.len(),
            allocs = interval_allocs,
            frees = interval_frees,
            avg_us = avg_us.unwrap_or(0),
            p99_us = lat_stats.as_ref().map_or(0, |ls| ls.p99_us),
            errs = state.total_errors.load(Relaxed),
            mem_mb = mem_delta_mb.unwrap_or(0),
            bufs = buf_count,
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
        elapsed = start_time.elapsed().as_secs(),
        tot_iters = state.total_iters.load(Relaxed),
        tot_allocs = state.total_allocs.load(Relaxed),
        tot_frees = state.total_frees.load(Relaxed),
        tot_errs = state.total_errors.load(Relaxed),
        samples = remaining.len(),
        avg_us = final_stats.as_ref().map_or(0, |ls| ls.avg_us),
        p99_us = final_stats.as_ref().map_or(0, |ls| ls.p99_us),
        mem_mb = mem_delta_mb.unwrap_or(0),
        bufs = buf_count,
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
