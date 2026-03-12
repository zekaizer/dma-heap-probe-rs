// Aging tests: sustained alloc/free with periodic metric reporting.
//
// Supports two modes:
// - Normal: full pipeline round-robin across heaps
// - Fuzz: random size, operation, timing with deterministic seeding

pub(crate) mod fuzz;
pub(crate) mod worker;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use rand::Rng;
use rand::rngs::SmallRng;
use serde::{Deserialize, Serialize};

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::cmd::perf::{self, LatencyStats};
use crate::runner::{self, SubTestResult};
use crate::{procfs, sysfs};

// ── Aging result ────────────────────────────────────────────────────────────

/// Structured aging test result, serialized into `StageResult.details`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgingResult {
    // Run info
    pub mode: String,
    pub elapsed_secs: u64,
    pub total_iters: u64,
    pub threads: u32,

    // Allocation counters
    pub total_allocs: u64,
    pub total_frees: u64,
    pub total_errors: u64,
    pub enomem_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throughput_iters_per_sec: Option<f64>,

    // Latency — running stats across entire run
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencyStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_interval_avg_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_interval_avg_us: Option<u64>,
    pub peak_p99_us: u64,
    pub trend: f64,

    // Memory health (start → end delta)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_available_delta_mb: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cma_free_delta_kb: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slab_delta_kb: Option<i64>,

    // Sysfs
    pub buf_count_start: usize,
    pub buf_count_end: usize,

    // Fragmentation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction_stall_delta: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_order_free_delta: Option<i64>,

    // Verdict
    pub warnings: Vec<String>,
}

// ── Thresholds ──────────────────────────────────────────────────────────────

/// Pass/fail judgment thresholds for aging metrics.
#[derive(Debug, Clone)]
pub struct AgingThresholds {
    /// Latency trend ratio above which the test fails.
    pub trend_fail: f64,
    /// Memory leak threshold in MB (0 = disabled).
    pub leak_threshold_mb: i64,
    /// Maximum non-ENOMEM error rate (fraction, e.g. 0.01 = 1%).
    pub max_error_rate: f64,
}

impl Default for AgingThresholds {
    fn default() -> Self {
        Self {
            trend_fail: 10.0,
            leak_threshold_mb: 0,
            max_error_rate: 0.01,
        }
    }
}

/// Evaluate aging result against thresholds. Returns (passed, warnings).
#[allow(clippy::cast_precision_loss)]
pub(crate) fn evaluate_thresholds(result: &AgingResult, thresholds: &AgingThresholds) -> bool {
    let mut passed = true;

    // Data corruption is always a failure (tracked via total_errors already).

    // Error rate check
    if result.total_iters > 0 {
        let error_rate = result.total_errors as f64 / result.total_iters as f64;
        if error_rate > thresholds.max_error_rate {
            tracing::error!(
                error_rate,
                max = thresholds.max_error_rate,
                "error rate exceeded threshold"
            );
            passed = false;
        }
    }

    // Latency trend check
    if result.trend > thresholds.trend_fail {
        tracing::error!(
            trend = result.trend,
            max = thresholds.trend_fail,
            "latency trend exceeded threshold"
        );
        passed = false;
    }

    // Memory leak check (only when enabled and allocs == frees)
    if thresholds.leak_threshold_mb > 0
        && result.total_allocs == result.total_frees
        && let Some(delta) = result.mem_available_delta_mb
        && delta < -thresholds.leak_threshold_mb
    {
        tracing::error!(
            delta_mb = delta,
            threshold = thresholds.leak_threshold_mb,
            "memory leak detected"
        );
        passed = false;
    }

    passed
}

// ── Shared state ────────────────────────────────────────────────────────────

/// Shared state across aging workers and the reporter thread.
pub(crate) struct AgingState {
    pub running: AtomicBool,
    pub total_iters: AtomicU64,
    pub total_errors: AtomicU64,
    pub total_allocs: AtomicU64,
    pub total_frees: AtomicU64,
    pub total_enomem: AtomicU64,
    pub interval_latencies: Mutex<Vec<u64>>,

    // Cumulative latency running stats (updated by reporter)
    pub cum_count: AtomicU64,
    pub cum_sum: AtomicU64,
    pub cum_max: AtomicU64,
    pub peak_p99: AtomicU64,
    pub first_interval_avg: AtomicU64,
    pub final_interval_avg: AtomicU64,
    /// Sentinel: 0 means `first_interval_avg` not yet set.
    pub first_interval_set: AtomicBool,
}

impl AgingState {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            total_iters: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            total_allocs: AtomicU64::new(0),
            total_frees: AtomicU64::new(0),
            total_enomem: AtomicU64::new(0),
            interval_latencies: Mutex::new(Vec::new()),
            cum_count: AtomicU64::new(0),
            cum_sum: AtomicU64::new(0),
            cum_max: AtomicU64::new(0),
            peak_p99: AtomicU64::new(0),
            first_interval_avg: AtomicU64::new(0),
            final_interval_avg: AtomicU64::new(0),
            first_interval_set: AtomicBool::new(false),
        }
    }

    /// Update cumulative stats from an interval's latency data.
    #[allow(clippy::cast_possible_truncation)]
    fn update_cumulative(&self, stats: &LatencyStats) {
        self.cum_count.fetch_add(stats.count as u64, Relaxed);
        self.cum_sum
            .fetch_add(stats.avg_us * stats.count as u64, Relaxed);
        // Update max via compare-and-swap loop.
        let mut cur_max = self.cum_max.load(Relaxed);
        while stats.max_us > cur_max {
            match self
                .cum_max
                .compare_exchange_weak(cur_max, stats.max_us, Relaxed, Relaxed)
            {
                Ok(_) => break,
                Err(actual) => cur_max = actual,
            }
        }
        // Update peak p99.
        let mut cur_p99 = self.peak_p99.load(Relaxed);
        while stats.p99_us > cur_p99 {
            match self
                .peak_p99
                .compare_exchange_weak(cur_p99, stats.p99_us, Relaxed, Relaxed)
            {
                Ok(_) => break,
                Err(actual) => cur_p99 = actual,
            }
        }
        // Set first interval avg once.
        if !self.first_interval_set.load(Relaxed) {
            self.first_interval_avg.store(stats.avg_us, Relaxed);
            self.first_interval_set.store(true, Relaxed);
        }
        // Always update final interval avg.
        self.final_interval_avg.store(stats.avg_us, Relaxed);
    }

    /// Build approximate cumulative `LatencyStats` from running counters.
    #[allow(clippy::cast_possible_truncation)]
    fn cumulative_stats(&self) -> Option<LatencyStats> {
        let count = self.cum_count.load(Relaxed);
        if count == 0 {
            return None;
        }
        let sum = self.cum_sum.load(Relaxed);
        let avg = sum / count;
        let max_us = self.cum_max.load(Relaxed);
        // Running stats don't track percentiles; use peak_p99 as approximation.
        let peak_p99 = self.peak_p99.load(Relaxed);
        Some(LatencyStats {
            count: count as usize,
            min_us: 0, // not tracked in running stats
            max_us,
            avg_us: avg,
            p50_us: avg,      // approximation
            p95_us: peak_p99, // approximation
            p99_us: peak_p99,
        })
    }

    /// Compute latency trend ratio (`final_avg` / `first_avg`).
    fn trend(&self) -> f64 {
        if !self.first_interval_set.load(Relaxed) {
            return 1.0;
        }
        let first = self.first_interval_avg.load(Relaxed);
        let final_avg = self.final_interval_avg.load(Relaxed);
        if first == 0 {
            return 1.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            final_avg as f64 / first as f64
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

#[cfg(test)]
pub(crate) fn reset_sigint() {
    SIGINT_FLAG.store(false, Relaxed);
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

/// Compute delta between two optional values (in KB), return as MB.
#[allow(clippy::cast_possible_wrap)]
fn compute_delta_mb(initial_kb: Option<u64>, current_kb: Option<u64>) -> Option<i64> {
    match (initial_kb, current_kb) {
        (Some(init), Some(now)) => Some((now as i64 - init as i64) / 1024),
        _ => None,
    }
}

/// Compute delta between two optional KB values, return as KB.
#[allow(clippy::cast_possible_wrap)]
fn compute_delta_kb(initial_kb: Option<u64>, current_kb: Option<u64>) -> Option<i64> {
    match (initial_kb, current_kb) {
        (Some(init), Some(now)) => Some(now as i64 - init as i64),
        _ => None,
    }
}

/// Sum of free pages at order >= 4 from buddyinfo.
fn high_order_free_sum(entries: &[procfs::BuddyInfoEntry]) -> u64 {
    entries
        .iter()
        .flat_map(|e| e.free_counts.iter().skip(4))
        .sum()
}

/// Mark initialization failure in shared state.
pub(crate) fn mark_init_error(state: &AgingState) {
    state.total_errors.fetch_add(1, Relaxed);
    state.running.store(false, Relaxed);
}

// ── Sparse fill ──────────────────────────────────────────────────────────────

const CACHE_LINE: usize = 64;
const PAGE_SIZE: usize = 4096;

/// Sparse fill: first + last cache line, plus interior cache lines at
/// page-aligned offsets.  Exercises the mmap/sync/coherency path without
/// the cost of a full memset (320 B vs 8 MB for an 8 MB buffer).
///
/// - `rng = None`  → deterministic evenly-spaced interior pages (normal mode)
/// - `rng = Some(_)` → random interior page offsets (fuzz mode)
///
/// For buffers ≤ 4 cache lines (256 B) the entire buffer is filled.
///
/// # Safety
/// `ptr` must be valid for `size` bytes.
pub(crate) unsafe fn sparse_fill(
    ptr: *mut u8,
    size: usize,
    pattern: u8,
    rng: Option<&mut SmallRng>,
) {
    if size <= CACHE_LINE * 4 {
        unsafe { std::ptr::write_bytes(ptr, pattern, size) };
        return;
    }
    // First cache line
    unsafe { std::ptr::write_bytes(ptr, pattern, CACHE_LINE) };
    // Last cache line
    unsafe { std::ptr::write_bytes(ptr.add(size - CACHE_LINE), pattern, CACHE_LINE) };
    // Interior: cache-line writes at page-aligned offsets
    let num_pages = size / PAGE_SIZE;
    if num_pages <= 2 {
        return;
    }
    let count = ((num_pages - 2) / 4).clamp(1, 3);
    match rng {
        Some(rng) => {
            for _ in 0..count {
                let idx = rng.random_range(1..num_pages - 1);
                unsafe { std::ptr::write_bytes(ptr.add(idx * PAGE_SIZE), pattern, CACHE_LINE) };
            }
        }
        None => {
            for i in 0..count {
                let idx = 1 + (i * (num_pages - 2)) / count;
                unsafe { std::ptr::write_bytes(ptr.add(idx * PAGE_SIZE), pattern, CACHE_LINE) };
            }
        }
    }
}

// ── System snapshot ─────────────────────────────────────────────────────────

/// Snapshot of system metrics at a point in time.
pub(crate) struct SystemSnapshot {
    mem_available_kb: Option<u64>,
    cma_free_kb: Option<u64>,
    slab_kb: Option<u64>,
    compact_stall: Option<u64>,
    high_order_free: Option<u64>,
    buf_count: usize,
}

fn take_snapshot() -> SystemSnapshot {
    let meminfo = procfs::read_meminfo().ok();
    let vmstat = procfs::read_vmstat().ok();
    let buddyinfo = procfs::read_buddyinfo().ok();
    let buf_count = sysfs::snapshot()
        .ok()
        .map_or(0, |snap| sysfs::buffer_count(&snap));

    SystemSnapshot {
        mem_available_kb: meminfo.as_ref().map(|m| m.mem_available_kb),
        cma_free_kb: meminfo.as_ref().and_then(|m| m.cma_free_kb),
        slab_kb: meminfo.as_ref().and_then(|m| m.slab_kb),
        compact_stall: vmstat.as_ref().and_then(|v| v.compact_stall),
        high_order_free: buddyinfo.as_ref().map(|b| high_order_free_sum(b)),
        buf_count,
    }
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
    let mut prev_allocs: u64 = 0;
    let mut prev_frees: u64 = 0;

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

        // Update cumulative running stats.
        if let Some(ref stats) = lat_stats {
            state.update_cumulative(stats);
        }

        let mem_available = procfs::read_meminfo().ok().map(|m| m.mem_available_kb);
        let mem_delta_mb = compute_delta_mb(initial_mem_available_kb, mem_available);
        let buf_count = sysfs::snapshot()
            .ok()
            .map_or(0, |snap| sysfs::buffer_count(&snap));

        let trend = state.trend();

        let cur_allocs = state.total_allocs.load(Relaxed);
        let cur_frees = state.total_frees.load(Relaxed);
        let interval_allocs = cur_allocs - prev_allocs;
        let interval_frees = cur_frees - prev_frees;
        prev_allocs = cur_allocs;
        prev_frees = cur_frees;

        println!(
            "aging: elapsed={} iters={} samples={} allocs={} frees={} avg_us={} p99_us={} errs={} enomem={} mem_mb={} bufs={} trend={:.1}x",
            start_time.elapsed().as_secs(),
            state.total_iters.load(Relaxed),
            latencies.len(),
            interval_allocs,
            interval_frees,
            lat_stats.as_ref().map_or(0, |ls| ls.avg_us),
            lat_stats.as_ref().map_or(0, |ls| ls.p99_us),
            state.total_errors.load(Relaxed),
            state.total_enomem.load(Relaxed),
            mem_delta_mb.unwrap_or(0),
            buf_count,
            trend,
        );
    }
}

// ── Run framework ───────────────────────────────────────────────────────────

/// Run workers with a reporter thread. Returns system snapshots for result building.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn run_with_reporter<F>(
    state: &AgingState,
    report_interval: Duration,
    worker_fn: F,
) -> (SystemSnapshot, SystemSnapshot, Duration)
where
    F: FnOnce(),
{
    install_sigint_handler();
    tracing::trace!("sigint handler installed");
    let start_time = Instant::now();
    let initial_snap = take_snapshot();
    let initial_mem = initial_snap.mem_available_kb;

    std::thread::scope(|s| {
        s.spawn(|| reporter_loop(state, report_interval, start_time, initial_mem));
        worker_fn();
        state.running.store(false, Relaxed);
    });

    // Process any remaining interval latencies into cumulative stats.
    let remaining = std::mem::take(&mut *state.interval_latencies.lock().unwrap());
    if let Some(stats) = perf::compute_stats(&remaining) {
        state.update_cumulative(&stats);
    }

    let elapsed = start_time.elapsed();
    let final_snap = take_snapshot();

    println!(
        "aging complete: elapsed={} iters={} allocs={} frees={} errs={} enomem={} trend={:.1}x",
        elapsed.as_secs(),
        state.total_iters.load(Relaxed),
        state.total_allocs.load(Relaxed),
        state.total_frees.load(Relaxed),
        state.total_errors.load(Relaxed),
        state.total_enomem.load(Relaxed),
        state.trend(),
    );

    (initial_snap, final_snap, elapsed)
}

// ── Result builder ──────────────────────────────────────────────────────────

/// Build `AgingResult` from state and snapshots.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn build_result(
    state: &AgingState,
    mode: &str,
    threads: u32,
    initial: &SystemSnapshot,
    final_snap: &SystemSnapshot,
    elapsed: Duration,
) -> AgingResult {
    let elapsed_secs = elapsed.as_secs();
    let total_iters = state.total_iters.load(Relaxed);
    let throughput = if elapsed_secs > 0 {
        Some(total_iters as f64 / elapsed_secs as f64)
    } else {
        None
    };

    let first_avg = if state.first_interval_set.load(Relaxed) {
        Some(state.first_interval_avg.load(Relaxed))
    } else {
        None
    };
    let final_avg = if state.first_interval_set.load(Relaxed) {
        Some(state.final_interval_avg.load(Relaxed))
    } else {
        None
    };

    AgingResult {
        mode: mode.to_string(),
        elapsed_secs,
        total_iters,
        threads,
        total_allocs: state.total_allocs.load(Relaxed),
        total_frees: state.total_frees.load(Relaxed),
        total_errors: state.total_errors.load(Relaxed),
        enomem_count: state.total_enomem.load(Relaxed),
        throughput_iters_per_sec: throughput,
        latency: state.cumulative_stats(),
        first_interval_avg_us: first_avg,
        final_interval_avg_us: final_avg,
        peak_p99_us: state.peak_p99.load(Relaxed),
        trend: state.trend(),
        mem_available_delta_mb: compute_delta_mb(
            initial.mem_available_kb,
            final_snap.mem_available_kb,
        ),
        cma_free_delta_kb: compute_delta_kb(initial.cma_free_kb, final_snap.cma_free_kb),
        slab_delta_kb: compute_delta_kb(initial.slab_kb, final_snap.slab_kb),
        buf_count_start: initial.buf_count,
        buf_count_end: final_snap.buf_count,
        compaction_stall_delta: match (initial.compact_stall, final_snap.compact_stall) {
            (Some(i), Some(f)) => Some(f.saturating_sub(i)),
            _ => None,
        },
        high_order_free_delta: compute_delta_kb(
            initial.high_order_free,
            final_snap.high_order_free,
        ),
        warnings: Vec::new(),
    }
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
    thresholds: &AgingThresholds,
) -> (
    Vec<SubTestResult>,
    Option<anyhow::Error>,
    Option<AgingResult>,
) {
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

    if fuzz_mode && size != 4096 {
        tracing::warn!(size, "fuzz mode ignores --size, using built-in FUZZ_SIZES");
    }

    // Print sequence diagram with actual runtime parameters.
    let mode_desc = if fuzz_mode {
        let seed_str = seed.map_or("random".to_string(), |s| s.to_string());
        format!(
            "fuzz (seed={seed_str}, {} size variants, weighted pipeline selection)",
            fuzz::FUZZ_SIZES.len()
        )
    } else {
        format!("normal (size={size}, hold every 3rd)")
    };
    let stop_desc = match (iterations, duration) {
        (Some(n), Some(d)) => format!("{n} iterations or {}s, Ctrl-C", d.as_secs()),
        (Some(n), None) => format!("{n} iterations, Ctrl-C"),
        (None, Some(d)) => format!("{}s, Ctrl-C", d.as_secs()),
        (None, None) => "Ctrl-C".to_string(),
    };
    println!("aging sequence:");
    println!("  mode: {mode_desc}");
    println!("  main ─┬─ {threads} workers");
    if fuzz_mode {
        println!("        │    loop {{ alloc → weighted pipeline → close/hold }}");
        println!("        │    pipelines (weight, availability depends on heap caps):");
        println!("        │      AllocHold(20)         alloc → hold");
        println!("        │      WriteOnly(15)         mmap → sync(R|W|RW) → sparse_fill → sync");
        println!(
            "        │      WriteReadVerify(15)   mmap → sync(W) → fill(full) → sync(R) → verify"
        );
        println!("        │      AllocClose(10)        alloc → close");
        println!("        │      AllocMmapClose(5)     mmap → close");
        println!(
            "        │      PartialMmap(5)        mmap(25-75%) → sync(W) → sparse_fill → sync"
        );
        println!("        │      WriteNoSync(5)        mmap → sparse_fill (no sync)");
        println!("        │      DoubleMmap(5)         mmap → mmap → sync(W) → sparse_fill → sync");
        println!("        │      DupAndOperate(5)      dup → mmap → sync(W) → sparse_fill → sync");
        println!(
            "        │      SetNameThenWrite(5)   set_name → mmap → sync(W) → sparse_fill → sync"
        );
        println!(
            "        │      LlseekAfterWrite(5)   mmap → sync(W) → sparse_fill → sync → llseek"
        );
        println!("        │      SyncFileRoundtrip(5)  export → import");
    } else {
        println!(
            "        │    loop {{ alloc → mmap → sync(W) → sparse_fill → sync(R) → close/hold }}"
        );
    }
    println!("        ├─ reporter (every {}s)", report_interval.as_secs());
    println!("        │    loop {{ drain latencies → snapshot → print }}");
    println!("        └─ stop ({stop_desc})");
    println!("             drain hold pool → final report → evaluate thresholds");
    println!();
    println!("aging report legend:");
    println!("  elapsed   wall clock seconds since start");
    println!("  iters     total completed iterations (cumulative)");
    println!("  samples   latency samples collected this interval");
    println!("  allocs    allocations this interval");
    println!("  frees     frees this interval");
    println!("  avg_us    mean alloc-to-close latency (us), this interval");
    println!("  p99_us    99th percentile latency (us), this interval");
    println!("  errs      non-ENOMEM errors (cumulative)");
    println!("  enomem    ENOMEM count (cumulative)");
    println!("  mem_mb    MemAvailable delta from start (MB, negative = leak)");
    println!("  bufs      active dma-buf count (sysfs)");
    println!("  trend     latency degradation ratio (final_avg / first_avg, 1.0x = stable)");

    let state = AgingState::new();

    let (initial_snap, final_snap, elapsed) = run_with_reporter(&state, report_interval, || {
        if fuzz_mode {
            fuzz::run_workers(
                backend, heaps, threads, &state, duration, iterations, max_hold, seed,
            );
        } else {
            worker::run_workers(
                backend, heaps, size, threads, &state, duration, iterations, max_hold,
            );
        }
    });

    let aging_result = build_result(&state, mode, threads, &initial_snap, &final_snap, elapsed);
    let passed = evaluate_thresholds(&aging_result, thresholds);

    let (sub_results, err) = if passed {
        runner::collect_test_results("aging", &[("aging", Ok(()))])
    } else {
        runner::collect_test_results("aging", &[("aging", Err(Errno::EIO))])
    };

    (sub_results, err, Some(aging_result))
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
        let (results, err, aging_result) = run(
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
            &AgingThresholds::default(),
        );
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(results.iter().all(|t| t.passed));
        let ar = aging_result.unwrap();
        assert_eq!(ar.mode, "normal");
        assert!(ar.total_iters >= 10);
        assert_eq!(ar.total_errors, 0);
        assert_eq!(ar.enomem_count, 0);
    }

    #[test]
    fn run_fuzz_passes() {
        let b = crate::backend::mock::MockBackend::new();
        let heaps = vec!["system".to_string()];
        let (results, err, aging_result) = run(
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
            &AgingThresholds::default(),
        );
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(results.iter().all(|t| t.passed));
        let ar = aging_result.unwrap();
        assert_eq!(ar.mode, "fuzz");
        assert!(ar.total_iters >= 10);
    }

    #[test]
    fn aging_result_json_roundtrip() {
        let result = AgingResult {
            mode: "normal".to_string(),
            elapsed_secs: 60,
            total_iters: 1000,
            threads: 2,
            total_allocs: 1000,
            total_frees: 1000,
            total_errors: 0,
            enomem_count: 0,
            throughput_iters_per_sec: Some(16.7),
            latency: Some(LatencyStats {
                count: 1000,
                min_us: 10,
                max_us: 500,
                avg_us: 50,
                p50_us: 45,
                p95_us: 200,
                p99_us: 400,
            }),
            first_interval_avg_us: Some(40),
            final_interval_avg_us: Some(55),
            peak_p99_us: 400,
            trend: 1.375,
            mem_available_delta_mb: Some(-5),
            cma_free_delta_kb: None,
            slab_delta_kb: None,
            buf_count_start: 0,
            buf_count_end: 0,
            compaction_stall_delta: None,
            high_order_free_delta: None,
            warnings: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: AgingResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.mode, "normal");
        assert_eq!(deserialized.total_iters, 1000);
        assert!((deserialized.trend - 1.375).abs() < f64::EPSILON);
    }

    #[test]
    fn threshold_trend_fail() {
        let result = AgingResult {
            mode: "normal".to_string(),
            elapsed_secs: 60,
            total_iters: 100,
            threads: 1,
            total_allocs: 100,
            total_frees: 100,
            total_errors: 0,
            enomem_count: 0,
            throughput_iters_per_sec: None,
            latency: None,
            first_interval_avg_us: Some(10),
            final_interval_avg_us: Some(150),
            peak_p99_us: 200,
            trend: 15.0,
            mem_available_delta_mb: None,
            cma_free_delta_kb: None,
            slab_delta_kb: None,
            buf_count_start: 0,
            buf_count_end: 0,
            compaction_stall_delta: None,
            high_order_free_delta: None,
            warnings: vec![],
        };
        let thresholds = AgingThresholds {
            trend_fail: 10.0,
            ..Default::default()
        };
        assert!(!evaluate_thresholds(&result, &thresholds));
    }

    #[test]
    fn threshold_trend_pass() {
        let result = AgingResult {
            mode: "normal".to_string(),
            elapsed_secs: 60,
            total_iters: 100,
            threads: 1,
            total_allocs: 100,
            total_frees: 100,
            total_errors: 0,
            enomem_count: 0,
            throughput_iters_per_sec: None,
            latency: None,
            first_interval_avg_us: Some(10),
            final_interval_avg_us: Some(20),
            peak_p99_us: 30,
            trend: 2.0,
            mem_available_delta_mb: None,
            cma_free_delta_kb: None,
            slab_delta_kb: None,
            buf_count_start: 0,
            buf_count_end: 0,
            compaction_stall_delta: None,
            high_order_free_delta: None,
            warnings: vec![],
        };
        assert!(evaluate_thresholds(&result, &AgingThresholds::default()));
    }

    #[test]
    fn threshold_error_rate_fail() {
        let result = AgingResult {
            mode: "fuzz".to_string(),
            elapsed_secs: 10,
            total_iters: 100,
            threads: 1,
            total_allocs: 100,
            total_frees: 100,
            total_errors: 50,
            enomem_count: 0,
            throughput_iters_per_sec: None,
            latency: None,
            first_interval_avg_us: None,
            final_interval_avg_us: None,
            peak_p99_us: 0,
            trend: 1.0,
            mem_available_delta_mb: None,
            cma_free_delta_kb: None,
            slab_delta_kb: None,
            buf_count_start: 0,
            buf_count_end: 0,
            compaction_stall_delta: None,
            high_order_free_delta: None,
            warnings: vec![],
        };
        assert!(!evaluate_thresholds(&result, &AgingThresholds::default()));
    }

    #[test]
    fn threshold_leak_fail() {
        let result = AgingResult {
            mode: "normal".to_string(),
            elapsed_secs: 60,
            total_iters: 100,
            threads: 1,
            total_allocs: 100,
            total_frees: 100,
            total_errors: 0,
            enomem_count: 0,
            throughput_iters_per_sec: None,
            latency: None,
            first_interval_avg_us: None,
            final_interval_avg_us: None,
            peak_p99_us: 0,
            trend: 1.0,
            mem_available_delta_mb: Some(-100),
            cma_free_delta_kb: None,
            slab_delta_kb: None,
            buf_count_start: 0,
            buf_count_end: 0,
            compaction_stall_delta: None,
            high_order_free_delta: None,
            warnings: vec![],
        };
        let thresholds = AgingThresholds {
            leak_threshold_mb: 50,
            ..Default::default()
        };
        assert!(!evaluate_thresholds(&result, &thresholds));
    }

    #[test]
    fn threshold_leak_disabled() {
        let result = AgingResult {
            mode: "normal".to_string(),
            elapsed_secs: 60,
            total_iters: 100,
            threads: 1,
            total_allocs: 100,
            total_frees: 100,
            total_errors: 0,
            enomem_count: 0,
            throughput_iters_per_sec: None,
            latency: None,
            first_interval_avg_us: None,
            final_interval_avg_us: None,
            peak_p99_us: 0,
            trend: 1.0,
            mem_available_delta_mb: Some(-100),
            cma_free_delta_kb: None,
            slab_delta_kb: None,
            buf_count_start: 0,
            buf_count_end: 0,
            compaction_stall_delta: None,
            high_order_free_delta: None,
            warnings: vec![],
        };
        // leak_threshold_mb = 0 means disabled
        assert!(evaluate_thresholds(&result, &AgingThresholds::default()));
    }

    #[test]
    fn threshold_error_rate_pass() {
        let result = AgingResult {
            mode: "fuzz".to_string(),
            elapsed_secs: 10,
            total_iters: 1000,
            threads: 1,
            total_allocs: 1000,
            total_frees: 1000,
            total_errors: 0,
            enomem_count: 0,
            throughput_iters_per_sec: None,
            latency: None,
            first_interval_avg_us: None,
            final_interval_avg_us: None,
            peak_p99_us: 0,
            trend: 1.0,
            mem_available_delta_mb: None,
            cma_free_delta_kb: None,
            slab_delta_kb: None,
            buf_count_start: 0,
            buf_count_end: 0,
            compaction_stall_delta: None,
            high_order_free_delta: None,
            warnings: vec![],
        };
        assert!(evaluate_thresholds(&result, &AgingThresholds::default()));
    }

    #[test]
    fn aging_with_enomem_pressure() {
        use crate::backend::mock::{MockBackend, SimConfig};
        let b = MockBackend::with_sim(SimConfig {
            enomem_threshold: Some(5),
            ..Default::default()
        });
        let heaps = vec!["system".to_string()];
        reset_sigint();
        let (results, err, aging_result) = run(
            &b,
            &heaps,
            4096,
            1,
            None,
            Some(30),
            Duration::from_secs(60),
            false,
            8,
            None,
            &AgingThresholds::default(),
        );
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(results.iter().all(|t| t.passed));
        let ar = aging_result.unwrap();
        assert!(ar.enomem_count > 0, "should have seen ENOMEM");
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }

    #[test]
    fn aging_with_error_injection() {
        use crate::backend::mock::{MockBackend, SimConfig};
        let b = MockBackend::with_sim(SimConfig {
            fail_every_nth: 5,
            ..Default::default()
        });
        let heaps = vec!["system".to_string()];
        reset_sigint();
        let (_results, _err, aging_result) = run(
            &b,
            &heaps,
            4096,
            1,
            None,
            Some(50),
            Duration::from_secs(60),
            false,
            8,
            None,
            &AgingThresholds::default(),
        );
        let ar = aging_result.unwrap();
        assert!(ar.total_errors > 0, "should have injected errors");
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }

    #[test]
    fn aging_fuzz_with_enomem_pressure() {
        use crate::backend::mock::{MockBackend, SimConfig};
        let b = MockBackend::with_sim(SimConfig {
            enomem_threshold: Some(4),
            ..Default::default()
        });
        let heaps = vec!["system".to_string()];
        reset_sigint();
        let (results, err, aging_result) = run(
            &b,
            &heaps,
            4096,
            1,
            None,
            Some(100),
            Duration::from_secs(60),
            true,
            8,
            Some(42),
            &AgingThresholds::default(),
        );
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(results.iter().all(|t| t.passed));
        let ar = aging_result.unwrap();
        assert!(ar.enomem_count > 0, "fuzz should have seen ENOMEM");
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn reporter_updates_cumulative_stats() {
        let state = AgingState::new();
        // Push latencies and simulate what reporter does
        {
            let mut lat = state.interval_latencies.lock().unwrap();
            lat.extend_from_slice(&[10, 20, 30, 40, 50]);
        }
        let latencies = std::mem::take(&mut *state.interval_latencies.lock().unwrap());
        let stats = perf::compute_stats(&latencies).unwrap();
        state.update_cumulative(&stats);

        assert!(state.first_interval_set.load(Relaxed));
        assert_eq!(state.first_interval_avg.load(Relaxed), stats.avg_us);
        assert_eq!(state.peak_p99.load(Relaxed), stats.p99_us);

        // Second interval with higher p99
        {
            let mut lat = state.interval_latencies.lock().unwrap();
            lat.extend_from_slice(&[100, 200, 300, 400, 500]);
        }
        let latencies2 = std::mem::take(&mut *state.interval_latencies.lock().unwrap());
        let stats2 = perf::compute_stats(&latencies2).unwrap();
        state.update_cumulative(&stats2);

        // first_interval_avg should not change
        assert_eq!(state.first_interval_avg.load(Relaxed), stats.avg_us);
        // final_interval_avg should be updated
        assert_eq!(state.final_interval_avg.load(Relaxed), stats2.avg_us);
        // peak_p99 should be the higher one
        assert!(state.peak_p99.load(Relaxed) >= stats2.p99_us);
    }

    #[test]
    fn normal_hold_pool_balance() {
        let b = crate::backend::mock::MockBackend::new();
        let heaps = vec!["system".to_string()];
        reset_sigint();
        let (results, err, aging_result) = run(
            &b,
            &heaps,
            4096,
            2,
            None,
            Some(30),
            Duration::from_secs(60),
            false,
            16,
            None,
            &AgingThresholds::default(),
        );
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(results.iter().all(|t| t.passed));
        let ar = aging_result.unwrap();
        assert_eq!(
            ar.total_allocs, ar.total_frees,
            "allocs must equal frees after drain"
        );
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }

    #[test]
    fn cumulative_stats_accuracy() {
        let state = AgingState::new();
        // Simulate two intervals
        let stats1 = LatencyStats {
            count: 10,
            min_us: 5,
            max_us: 100,
            avg_us: 50,
            p50_us: 45,
            p95_us: 90,
            p99_us: 95,
        };
        let stats2 = LatencyStats {
            count: 20,
            min_us: 3,
            max_us: 200,
            avg_us: 60,
            p50_us: 55,
            p95_us: 180,
            p99_us: 190,
        };
        state.update_cumulative(&stats1);
        state.update_cumulative(&stats2);

        let cum = state.cumulative_stats().unwrap();
        assert_eq!(cum.count, 30);
        // sum = 50*10 + 60*20 = 500 + 1200 = 1700, avg = 1700/30 = 56
        assert_eq!(cum.avg_us, 56);
        assert_eq!(cum.max_us, 200);
        assert_eq!(cum.p99_us, 190); // peak p99
        assert_eq!(state.first_interval_avg.load(Relaxed), 50);
        assert_eq!(state.final_interval_avg.load(Relaxed), 60);
    }
}
