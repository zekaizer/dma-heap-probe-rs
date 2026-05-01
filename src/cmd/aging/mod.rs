// Aging tests: sustained alloc/free with periodic metric reporting.
//
// Supports two modes:
// - Normal: full pipeline round-robin across heaps
// - Fuzz: random size, operation, timing with deterministic seeding

pub(crate) mod fuzz;
pub(crate) mod worker;

use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use rand::Rng;
use rand::rngs::SmallRng;
use serde::{Deserialize, Serialize};

use crate::backend::{ContainerBackend, DmaBufBackend, HeapBackend};
use crate::procfs;
use crate::runner::{self, SubTestResult};
use crate::stats::{self, LatencyStats};
use crate::tee_println;

// ── Hold limit ─────────────────────────────────────────────────────────────

/// Hold pool limit mode: by buffer count, byte size, or disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HoldLimit {
    Disabled,
    Count(u64),
    Bytes(u64),
}

/// Parse a size string with optional suffix (K/KiB, M/MiB, G/GiB). Pure number = bytes.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let pos = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("invalid size: {s}"))?;
    let (num_str, suffix) = s.split_at(pos);
    let num: u64 = num_str
        .parse()
        .map_err(|_| format!("invalid number in size: {s}"))?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "k" | "kib" => 1024,
        "m" | "mib" => 1024 * 1024,
        "g" | "gib" => 1024 * 1024 * 1024,
        _ => return Err(format!("unknown size suffix '{suffix}' in: {s}")),
    };
    num.checked_mul(multiplier)
        .ok_or_else(|| format!("size overflow: {s}"))
}

/// Parse a hold limit string: pure number → count, suffix → bytes, 0 → disabled.
pub fn parse_hold_limit(s: &str) -> Result<HoldLimit, String> {
    let s = s.trim();
    if s == "0" {
        return Ok(HoldLimit::Disabled);
    }
    // Pure integer → buffer count.
    if s.parse::<u64>().is_ok() {
        return Ok(HoldLimit::Count(s.parse().unwrap()));
    }
    // Has suffix → byte limit.
    parse_size(s).map(HoldLimit::Bytes)
}

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
    pub emfile_count: u64,
    pub total_merges: u64,
    pub total_merge_errors: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throughput_iters_per_sec: Option<f64>,

    // Latency — running stats across entire run
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencyStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_avg_us: Option<u64>,
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

    // Per-heap breakdown
    pub heap_results: Vec<HeapResult>,
    pub drain_bufs: u64,
    pub drain_bytes: u64,

    // Verdict
    pub warnings: Vec<String>,
}

/// Per-operation latency result (serialized from `OpLatency` atomics).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpResult {
    pub count: u64,
    pub avg_us: u64,
    pub per_4k_us: f64,
    pub min_us: u64,
    pub max_us: u64,
    pub p50_us: u64,
    pub p99_us: u64,
}

impl OpResult {
    fn from_op_latency(op: &OpLatency) -> Self {
        let min_raw = op.min_us.load(Relaxed);
        Self {
            count: op.count.load(Relaxed),
            avg_us: op.avg_us(),
            per_4k_us: op.per_4k_us(),
            min_us: if min_raw == u64::MAX { 0 } else { min_raw },
            max_us: op.max_us.load(Relaxed),
            p50_us: op.percentile(50.0),
            p99_us: op.percentile(99.0),
        }
    }
}

/// Per-heap allocation and latency result (serialized from `HeapCounters`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeapResult {
    pub name: String,
    pub allocs: u64,
    pub frees: u64,
    pub errors: u64,
    pub enomem: u64,
    pub emfile: u64,
    pub alloc_lat: OpResult,
    pub mmap_lat: OpResult,
    pub sync_lat: OpResult,
    pub free_lat: OpResult,
}

/// Number of initial report intervals averaged for the trend baseline.
const BASELINE_INTERVALS: u64 = 5;

// ── Per-heap operation latency ──────────────────────────────────────────────

/// Log-scale bucket upper bounds (microseconds).
const BUCKET_BOUNDS: [u64; 7] = [1, 10, 100, 1_000, 10_000, 100_000, 1_000_000];

/// Streaming latency statistics with log-scale histogram for approximate percentiles.
pub(crate) struct OpLatency {
    pub count: AtomicU64,
    pub sum_us: AtomicU64,
    pub min_us: AtomicU64,
    pub max_us: AtomicU64,
    /// Total bytes processed (for per-4K normalization).
    pub size_sum: AtomicU64,
    /// Log-scale histogram: `[<1us, 1-10, 10-100, 100-1K, 1K-10K, 10K-100K, 100K-1M, >1M]`.
    pub buckets: [AtomicU64; 8],
}

impl OpLatency {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            min_us: AtomicU64::new(u64::MAX),
            max_us: AtomicU64::new(0),
            size_sum: AtomicU64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Record a latency sample with associated buffer size.
    pub fn record(&self, lat_us: u64, size: u64) {
        self.count.fetch_add(1, Relaxed);
        self.sum_us.fetch_add(lat_us, Relaxed);
        self.size_sum.fetch_add(size, Relaxed);
        // Update min.
        let mut cur = self.min_us.load(Relaxed);
        while lat_us < cur {
            match self
                .min_us
                .compare_exchange_weak(cur, lat_us, Relaxed, Relaxed)
            {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        // Update max.
        let mut cur = self.max_us.load(Relaxed);
        while lat_us > cur {
            match self
                .max_us
                .compare_exchange_weak(cur, lat_us, Relaxed, Relaxed)
            {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        // Histogram bucket.
        let idx = BUCKET_BOUNDS.partition_point(|&b| b <= lat_us).min(7);
        self.buckets[idx].fetch_add(1, Relaxed);
    }

    /// Approximate percentile from histogram (returns bucket upper bound).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn percentile(&self, pct: f64) -> u64 {
        let total = self.count.load(Relaxed);
        if total == 0 {
            return 0;
        }
        let target = (total as f64 * pct / 100.0).ceil() as u64;
        let mut cum = 0u64;
        let bounds = [1, 10, 100, 1_000, 10_000, 100_000, 1_000_000, u64::MAX];
        for (i, &bound) in bounds.iter().enumerate() {
            cum += self.buckets[i].load(Relaxed);
            if cum >= target {
                return bound;
            }
        }
        u64::MAX
    }

    /// Average latency in microseconds.
    pub fn avg_us(&self) -> u64 {
        self.sum_us
            .load(Relaxed)
            .checked_div(self.count.load(Relaxed))
            .unwrap_or(0)
    }

    /// Normalized latency per 4K bytes.
    #[allow(clippy::cast_precision_loss)]
    pub fn per_4k_us(&self) -> f64 {
        let s = self.size_sum.load(Relaxed);
        if s == 0 {
            0.0
        } else {
            self.sum_us.load(Relaxed) as f64 * 4096.0 / s as f64
        }
    }
}

/// Per-heap allocation and latency counters.
pub(crate) struct HeapCounters {
    pub name: String,
    pub allocs: AtomicU64,
    pub frees: AtomicU64,
    pub errors: AtomicU64,
    pub enomem: AtomicU64,
    pub emfile: AtomicU64,
    pub alloc_lat: OpLatency,
    pub mmap_lat: OpLatency,
    pub sync_lat: OpLatency,
    pub free_lat: OpLatency,
}

impl HeapCounters {
    fn new(name: String) -> Self {
        Self {
            name,
            allocs: AtomicU64::new(0),
            frees: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            enomem: AtomicU64::new(0),
            emfile: AtomicU64::new(0),
            alloc_lat: OpLatency::new(),
            mmap_lat: OpLatency::new(),
            sync_lat: OpLatency::new(),
            free_lat: OpLatency::new(),
        }
    }
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
    pub total_emfile: AtomicU64,
    pub total_merges: AtomicU64,
    pub total_merge_errors: AtomicU64,
    pub held_bufs: AtomicU64,
    pub held_bytes: AtomicU64,
    pub drain_bufs: AtomicU64,
    pub drain_bytes: AtomicU64,
    pub hold_limit: HoldLimit,
    pub interval_latencies: Mutex<Vec<u64>>,
    pub heap_counters: Vec<HeapCounters>,

    // Cumulative latency running stats (updated by reporter)
    pub cum_count: AtomicU64,
    pub cum_sum: AtomicU64,
    pub cum_max: AtomicU64,
    pub peak_p99: AtomicU64,
    pub baseline_sum: AtomicU64,
    pub baseline_count: AtomicU64,
    pub baseline_intervals: AtomicU64,
    pub baseline_ready: AtomicBool,
    pub final_interval_avg: AtomicU64,
}

impl AgingState {
    pub fn new(hold_limit: HoldLimit, heap_names: &[String]) -> Self {
        Self {
            running: AtomicBool::new(true),
            total_iters: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            total_allocs: AtomicU64::new(0),
            total_frees: AtomicU64::new(0),
            total_enomem: AtomicU64::new(0),
            total_emfile: AtomicU64::new(0),
            total_merges: AtomicU64::new(0),
            total_merge_errors: AtomicU64::new(0),
            held_bufs: AtomicU64::new(0),
            held_bytes: AtomicU64::new(0),
            drain_bufs: AtomicU64::new(0),
            drain_bytes: AtomicU64::new(0),
            hold_limit,
            interval_latencies: Mutex::new(Vec::new()),
            heap_counters: heap_names
                .iter()
                .map(|n| HeapCounters::new(n.clone()))
                .collect(),
            cum_count: AtomicU64::new(0),
            cum_sum: AtomicU64::new(0),
            cum_max: AtomicU64::new(0),
            peak_p99: AtomicU64::new(0),
            baseline_sum: AtomicU64::new(0),
            baseline_count: AtomicU64::new(0),
            baseline_intervals: AtomicU64::new(0),
            baseline_ready: AtomicBool::new(false),
            final_interval_avg: AtomicU64::new(0),
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
        // Accumulate sample-weighted baseline from first BASELINE_INTERVALS intervals.
        if !self.baseline_ready.load(Relaxed) {
            self.baseline_sum
                .fetch_add(stats.avg_us * stats.count as u64, Relaxed);
            self.baseline_count.fetch_add(stats.count as u64, Relaxed);
            let tick = self.baseline_intervals.fetch_add(1, Relaxed) + 1;
            if tick >= BASELINE_INTERVALS {
                self.baseline_ready.store(true, Relaxed);
                let sample_count = self.baseline_count.load(Relaxed);
                let avg = self
                    .baseline_sum
                    .load(Relaxed)
                    .checked_div(sample_count)
                    .unwrap_or(0);
                tracing::info!(
                    baseline_avg_us = avg,
                    intervals = tick,
                    samples = sample_count,
                    "trend baseline established"
                );
            }
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
            stddev_us: 0,     // not tracked in running stats
            p50_us: avg,      // approximation
            p95_us: peak_p99, // approximation
            p99_us: peak_p99,
            p99_9_us: peak_p99, // approximation
            cv_pct: 0.0,        // not tracked in running stats
            trimmed_avg_us: avg,
            outlier_count: 0,
            throughput_ops: 1_000_000_u64.checked_div(avg).unwrap_or(0),
            ci95_us: 0,
            mad_us: 0,
        })
    }

    /// Compute latency trend ratio (`final_avg` / `baseline_avg`).
    fn trend(&self) -> f64 {
        if !self.baseline_ready.load(Relaxed) {
            return 1.0;
        }
        let sum = self.baseline_sum.load(Relaxed);
        let count = self.baseline_count.load(Relaxed);
        let final_avg = self.final_interval_avg.load(Relaxed);
        if count == 0 || sum == 0 {
            return 1.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            final_avg as f64 / (sum as f64 / count as f64)
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

/// Compute per-thread hold pool limits: `(max_count, max_bytes)`.
///
/// - Count mode: `(count/threads, 0)` — count-based eviction.
/// - Bytes mode: `(usize::MAX, bytes/threads)` — byte-based eviction.
/// - Disabled: `(0, 0)`.
#[allow(clippy::cast_possible_truncation)]
fn per_thread_pool_limits(limit: HoldLimit, threads: u32) -> (Option<usize>, u64) {
    match limit {
        HoldLimit::Disabled => (Some(0), 0),
        HoldLimit::Count(n) => (Some((n as usize / threads as usize).max(1)), 0),
        HoldLimit::Bytes(max) => (None, max / u64::from(threads)),
    }
}

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
}

fn take_snapshot() -> SystemSnapshot {
    let meminfo = procfs::read_meminfo().ok();
    let vmstat = procfs::read_vmstat().ok();
    let buddyinfo = procfs::read_buddyinfo().ok();

    SystemSnapshot {
        mem_available_kb: meminfo.as_ref().map(|m| m.mem_available_kb),
        cma_free_kb: meminfo.as_ref().and_then(|m| m.cma_free_kb),
        slab_kb: meminfo.as_ref().and_then(|m| m.slab_kb),
        compact_stall: vmstat.as_ref().and_then(|v| v.compact_stall),
        high_order_free: buddyinfo.as_ref().map(|b| high_order_free_sum(b)),
    }
}

// ── Reporter loop ───────────────────────────────────────────────────────────

/// Periodic reporter that drains interval latencies and logs metrics.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn reporter_loop(
    state: &AgingState,
    report_interval: Duration,
    start_time: Instant,
    _initial_mem_available_kb: Option<u64>,
    heap_label: &str,
    heap_w: usize,
    fuzz_mode: bool,
) {
    let mut prev_allocs: u64 = 0;
    let mut prev_frees: u64 = 0;

    loop {
        // Sleep in 10 ms chunks for responsive shutdown.
        let mut waited = Duration::ZERO;
        while waited < report_interval {
            if !state.running.load(Relaxed) {
                return;
            }
            let chunk = Duration::from_millis(10).min(report_interval.saturating_sub(waited));
            std::thread::sleep(chunk);
            waited += chunk;
        }
        if !state.running.load(Relaxed) {
            return;
        }

        tracing::trace!("reporter wakeup");
        let latencies = std::mem::take(&mut *state.interval_latencies.lock().unwrap());
        let lat_stats = stats::compute_stats(&latencies);

        // Update cumulative running stats.
        if let Some(ref stats) = lat_stats {
            state.update_cumulative(stats);
        }

        let buf_count = state.held_bufs.load(Relaxed);

        let trend = state.trend();

        // Emit Perfetto trace counters for timeline visualization.
        #[allow(clippy::cast_possible_wrap)]
        if crate::trace::enabled() {
            crate::trace::counter("held_bufs", buf_count as i64);
            crate::trace::counter(
                "held_bytes_mb",
                (state.held_bytes.load(Relaxed) / 1_048_576) as i64,
            );
            crate::trace::counter("trend_x100", (trend * 100.0) as i64);
            crate::trace::counter("enomem_total", state.total_enomem.load(Relaxed) as i64);
            crate::trace::counter("emfile_total", state.total_emfile.load(Relaxed) as i64);
        }

        let cur_allocs = state.total_allocs.load(Relaxed);
        let cur_frees = state.total_frees.load(Relaxed);
        let interval_allocs = cur_allocs - prev_allocs;
        let interval_frees = cur_frees - prev_frees;
        prev_allocs = cur_allocs;
        prev_frees = cur_frees;

        let elapsed_s = start_time.elapsed().as_secs();
        let iters = state.total_iters.load(Relaxed);
        let errs = state.total_errors.load(Relaxed);
        let held_str = format!(
            "{}({})",
            buf_count,
            crate::cmd::info::format_size(state.held_bytes.load(Relaxed))
        );
        let trend_str = format!("{trend:.1}x");
        if fuzz_mode {
            crate::fmt::print_metric(
                heap_label,
                heap_w,
                "aging::progress",
                &[
                    ("elapsed", &format_args!("{elapsed_s}s")),
                    ("iters", &iters),
                    ("\u{2191}", &interval_allocs),
                    ("\u{2193}", &interval_frees),
                    ("err", &errs),
                    ("held", &held_str as &dyn std::fmt::Display),
                    ("trend", &trend_str as &dyn std::fmt::Display),
                ],
            );
        } else {
            let avg_us = lat_stats.as_ref().map_or(0, |ls| ls.avg_us);
            let p99_us = lat_stats.as_ref().map_or(0, |ls| ls.p99_us);
            crate::fmt::print_metric(
                heap_label,
                heap_w,
                "aging::progress",
                &[
                    ("elapsed", &format_args!("{elapsed_s}s")),
                    ("iters", &iters),
                    ("\u{2191}", &interval_allocs),
                    ("\u{2193}", &interval_frees),
                    ("avg", &format_args!("{avg_us}us")),
                    ("p99", &format_args!("{p99_us}us")),
                    ("err", &errs),
                    ("held", &held_str as &dyn std::fmt::Display),
                    ("trend", &trend_str as &dyn std::fmt::Display),
                ],
            );
        }
    }
}

// ── Run framework ───────────────────────────────────────────────────────────

/// Run workers with a reporter thread. Returns system snapshots for result building.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub(crate) fn run_with_reporter<F>(
    state: &AgingState,
    report_interval: Duration,
    heap_label: &str,
    heap_w: usize,
    fuzz_mode: bool,
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
        s.spawn(|| {
            reporter_loop(
                state,
                report_interval,
                start_time,
                initial_mem,
                heap_label,
                heap_w,
                fuzz_mode,
            );
        });
        worker_fn();
        state.running.store(false, Relaxed);
    });

    // Process any remaining interval latencies into cumulative stats.
    let remaining = std::mem::take(&mut *state.interval_latencies.lock().unwrap());
    if let Some(stats) = stats::compute_stats(&remaining) {
        state.update_cumulative(&stats);
    }

    let elapsed = start_time.elapsed();
    let final_snap = take_snapshot();

    let trend_str = format!("{:.1}x", state.trend());
    let held_str = format!(
        "{}({})",
        state.held_bufs.load(Relaxed),
        crate::cmd::info::format_size(state.held_bytes.load(Relaxed))
    );
    crate::fmt::print_metric(
        heap_label,
        heap_w,
        "aging::complete",
        &[
            ("elapsed", &format_args!("{}s", elapsed.as_secs())),
            ("iters", &state.total_iters.load(Relaxed)),
            ("\u{2191}", &state.total_allocs.load(Relaxed)),
            ("\u{2193}", &state.total_frees.load(Relaxed)),
            ("err", &state.total_errors.load(Relaxed)),
            ("held", &held_str as &dyn std::fmt::Display),
            ("trend", &trend_str as &dyn std::fmt::Display),
        ],
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

    let baseline_avg = if state.baseline_ready.load(Relaxed) {
        let sum = state.baseline_sum.load(Relaxed);
        let count = state.baseline_count.load(Relaxed);
        sum.checked_div(count)
    } else {
        None
    };
    let final_avg = if state.baseline_ready.load(Relaxed) {
        Some(state.final_interval_avg.load(Relaxed))
    } else {
        None
    };

    let heap_results: Vec<HeapResult> = state
        .heap_counters
        .iter()
        .map(|hc| HeapResult {
            name: hc.name.clone(),
            allocs: hc.allocs.load(Relaxed),
            frees: hc.frees.load(Relaxed),
            errors: hc.errors.load(Relaxed),
            enomem: hc.enomem.load(Relaxed),
            emfile: hc.emfile.load(Relaxed),
            alloc_lat: OpResult::from_op_latency(&hc.alloc_lat),
            mmap_lat: OpResult::from_op_latency(&hc.mmap_lat),
            sync_lat: OpResult::from_op_latency(&hc.sync_lat),
            free_lat: OpResult::from_op_latency(&hc.free_lat),
        })
        .collect();

    AgingResult {
        mode: mode.to_string(),
        elapsed_secs,
        total_iters,
        threads,
        total_allocs: state.total_allocs.load(Relaxed),
        total_frees: state.total_frees.load(Relaxed),
        total_errors: state.total_errors.load(Relaxed),
        enomem_count: state.total_enomem.load(Relaxed),
        emfile_count: state.total_emfile.load(Relaxed),
        total_merges: state.total_merges.load(Relaxed),
        total_merge_errors: state.total_merge_errors.load(Relaxed),
        throughput_iters_per_sec: throughput,
        latency: state.cumulative_stats(),
        baseline_avg_us: baseline_avg,
        final_interval_avg_us: final_avg,
        peak_p99_us: state.peak_p99.load(Relaxed),
        trend: state.trend(),
        mem_available_delta_mb: compute_delta_mb(
            initial.mem_available_kb,
            final_snap.mem_available_kb,
        ),
        cma_free_delta_kb: compute_delta_kb(initial.cma_free_kb, final_snap.cma_free_kb),
        slab_delta_kb: compute_delta_kb(initial.slab_kb, final_snap.slab_kb),
        buf_count_start: 0,
        buf_count_end: state.held_bufs.load(Relaxed) as usize,
        compaction_stall_delta: match (initial.compact_stall, final_snap.compact_stall) {
            (Some(i), Some(f)) => Some(f.saturating_sub(i)),
            _ => None,
        },
        high_order_free_delta: compute_delta_kb(
            initial.high_order_free,
            final_snap.high_order_free,
        ),
        heap_results,
        drain_bufs: state.drain_bufs.load(Relaxed),
        drain_bytes: state.drain_bytes.load(Relaxed),
        warnings: Vec::new(),
    }
}

// ── Summary printer ──────────────────────────────────────────────────────────

/// Format a number with thousands separators (e.g. `1,234,567`).
fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(c);
    }
    result
}

/// Format elapsed seconds as human-readable duration.
fn fmt_elapsed(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Format an optional signed delta value with a sign prefix.
fn fmt_delta(val: Option<i64>, unit: &str) -> String {
    match val {
        Some(v) => {
            let sign = if v >= 0 { "+" } else { "" };
            format!("{sign}{v} {unit}")
        }
        None => "-".to_string(),
    }
}

/// Print a right-aligned table with 4-space indent. `headers` and each row
/// must have the same number of columns.
fn print_aligned_table(headers: &[&str], rows: &[Vec<String>]) {
    let ncols = headers.len();
    let mut col_w: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < ncols {
                col_w[i] = col_w[i].max(cell.len());
            }
        }
    }
    let mut hdr = String::from("    ");
    for (i, h) in headers.iter().enumerate() {
        if i > 0 {
            hdr.push_str("  ");
        }
        let _ = write!(hdr, "{h:>w$}", w = col_w[i]);
    }
    tee_println!("{hdr}");
    for row in rows {
        let mut line = String::from("    ");
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let w = if i < ncols { col_w[i] } else { cell.len() };
            let _ = write!(line, "{cell:>w$}");
        }
        tee_println!("{line}");
    }
}

/// Format an `OpResult` per-4K latency, showing "-" when no samples recorded.
fn fmt_op_per_4k(op: &OpResult) -> String {
    if op.count == 0 {
        "-".into()
    } else {
        format!("{:.3} us", op.per_4k_us)
    }
}

/// Format a latency value in microseconds, showing "-" when no samples recorded.
fn fmt_op_us(val: u64, count: u64) -> String {
    if count == 0 {
        "-".into()
    } else {
        format!("{val} us")
    }
}

/// Print per-heap normalized latency table.
fn print_per_heap_table(result: &AgingResult) {
    if result.heap_results.is_empty() {
        return;
    }
    tee_println!("  Per-heap (lat normalized per 4K)");
    let headers = [
        "heap", "alloc", "free", "ENOMEM", "EMFILE", "alloc/4K", "mmap/4K", "sync/4K", "free/4K",
    ];
    let rows: Vec<Vec<String>> = result
        .heap_results
        .iter()
        .map(|h| {
            vec![
                h.name.clone(),
                fmt_num(h.allocs),
                fmt_num(h.frees),
                fmt_num(h.enomem),
                fmt_num(h.emfile),
                fmt_op_per_4k(&h.alloc_lat),
                fmt_op_per_4k(&h.mmap_lat),
                fmt_op_per_4k(&h.sync_lat),
                fmt_op_per_4k(&h.free_lat),
            ]
        })
        .collect();
    print_aligned_table(&headers, &rows);
    tee_println!();
}

/// Print per-op latency detail for each heap (normal mode only).
fn print_per_op_latency(result: &AgingResult) {
    for hr in &result.heap_results {
        tee_println!("  Latency per-op: {}", hr.name);
        let headers = ["", "avg", "p50", "p99", "max"];
        let rows: Vec<Vec<String>> = [
            ("alloc", &hr.alloc_lat),
            ("mmap", &hr.mmap_lat),
            ("sync", &hr.sync_lat),
            ("free", &hr.free_lat),
        ]
        .iter()
        .map(|(name, op)| {
            vec![
                (*name).into(),
                fmt_op_us(op.avg_us, op.count),
                fmt_op_us(op.p50_us, op.count),
                fmt_op_us(op.p99_us, op.count),
                fmt_op_us(op.max_us, op.count),
            ]
        })
        .collect();
        print_aligned_table(&headers, &rows);
        tee_println!();
    }
}

/// Print memory delta section (skipped on host where procfs is unavailable).
fn print_memory_section(result: &AgingResult) {
    let has_mem = result.mem_available_delta_mb.is_some()
        || result.cma_free_delta_kb.is_some()
        || result.slab_delta_kb.is_some()
        || result.compaction_stall_delta.is_some();
    if !has_mem {
        return;
    }
    tee_println!("  Memory (start -> end delta)");
    tee_println!(
        "    available : {}",
        fmt_delta(result.mem_available_delta_mb, "MB")
    );
    tee_println!(
        "    CMA free  : {}",
        fmt_delta(result.cma_free_delta_kb, "KB")
    );
    tee_println!("    slab      : {}", fmt_delta(result.slab_delta_kb, "KB"));
    #[allow(clippy::cast_possible_wrap)]
    let compact_delta = result.compaction_stall_delta.map(|v| v as i64);
    tee_println!("    compact   : {}", fmt_delta(compact_delta, "stalls"));
    tee_println!();
}

/// Print aging test summary to stdout (and tee to log file if enabled).
#[allow(clippy::cast_precision_loss)]
fn print_summary(result: &AgingResult, fuzz_mode: bool) {
    let sep = "\u{2500}".repeat(61);
    tee_println!();
    tee_println!("\u{2500}\u{2500} aging summary {sep}");

    // Run info
    let heap_names: Vec<&str> = result
        .heap_results
        .iter()
        .map(|h| h.name.as_str())
        .collect();
    let heap_list = if heap_names.is_empty() {
        result.mode.clone()
    } else {
        heap_names.join(", ")
    };
    let throughput_str = result
        .throughput_iters_per_sec
        .map_or_else(|| "-".to_string(), |t| format!("{t:.0}"));

    tee_println!(
        "  mode        : {} x {} threads",
        result.mode,
        result.threads
    );
    tee_println!("  heaps       : {heap_list}");
    tee_println!("  elapsed     : {}", fmt_elapsed(result.elapsed_secs));
    tee_println!(
        "  iters       : {} ({}/s)",
        fmt_num(result.total_iters),
        throughput_str
    );
    tee_println!(
        "  alloc/free  : {} / {}",
        fmt_num(result.total_allocs),
        fmt_num(result.total_frees)
    );
    if result.total_merges > 0 || result.total_merge_errors > 0 {
        tee_println!(
            "  merges      : {} (err {})",
            fmt_num(result.total_merges),
            result.total_merge_errors
        );
    }
    tee_println!();

    // Trend
    tee_println!("  Trend");
    let baseline_str = result
        .baseline_avg_us
        .map_or_else(|| "-".to_string(), |v| format!("{v} us"));
    let baseline_detail = if result.baseline_avg_us.is_some() {
        format!(" ({BASELINE_INTERVALS}-interval avg)")
    } else {
        String::new()
    };
    tee_println!("    baseline  : {baseline_str}{baseline_detail}");
    let final_str = result
        .final_interval_avg_us
        .map_or_else(|| "-".to_string(), |v| format!("{v} us"));
    tee_println!("    final     : {final_str}");
    tee_println!("    trend     : {:.1}x", result.trend);
    tee_println!("    peak p99  : {} us", fmt_num(result.peak_p99_us));
    tee_println!();

    print_per_heap_table(result);
    if !fuzz_mode {
        print_per_op_latency(result);
    }
    print_memory_section(result);

    tee_println!("{}", "\u{2500}".repeat(76));
}

// ── Public entry point ──────────────────────────────────────────────────────

/// Run the aging test.
#[allow(clippy::too_many_arguments)]
pub fn run<B: HeapBackend + DmaBufBackend + ContainerBackend + Send + Sync>(
    backend: &B,
    heaps: &[String],
    size: Option<u64>,
    threads: u32,
    duration: Option<Duration>,
    iterations: Option<u64>,
    report_interval: Duration,
    fuzz_mode: bool,
    hold_limit: HoldLimit,
    close_settle_us: u64,
    seed: Option<u64>,
    heap_w: usize,
) -> (
    Vec<SubTestResult>,
    Option<anyhow::Error>,
    Option<AgingResult>,
) {
    let mode = if fuzz_mode { "fuzz" } else { "normal" };
    let normal_size = size.unwrap_or(4096);
    tracing::debug!(
        mode,
        threads,
        heaps = heaps.len(),
        size = ?size,
        ?duration,
        ?iterations,
        report_interval_s = report_interval.as_secs(),
        "aging start"
    );

    if let (true, Some(max)) = (fuzz_mode, size) {
        tracing::info!(max_size = max, "fuzz mode: capping sizes at --size");
    }

    if fuzz_mode && close_settle_us > 0 {
        tracing::warn!(
            close_settle_us,
            "--close-settle-us is ignored in fuzz mode (normal mode only)"
        );
    }

    let (pt_max_count, pt_max_bytes) = per_thread_pool_limits(hold_limit, threads);
    let state = AgingState::new(hold_limit, heaps);
    let heap_label = heaps.join(",");

    let (initial_snap, final_snap, elapsed) = run_with_reporter(
        &state,
        report_interval,
        &heap_label,
        heap_w,
        fuzz_mode,
        || {
            if fuzz_mode {
                fuzz::run_workers(
                    backend,
                    heaps,
                    threads,
                    &state,
                    duration,
                    iterations,
                    pt_max_count,
                    pt_max_bytes,
                    seed,
                    size,
                );
            } else {
                worker::run_workers(
                    backend,
                    heaps,
                    normal_size,
                    threads,
                    &state,
                    duration,
                    iterations,
                    pt_max_count,
                    pt_max_bytes,
                    close_settle_us,
                );
            }
        },
    );

    let aging_result = build_result(&state, mode, threads, &initial_snap, &final_snap, elapsed);
    print_summary(&aging_result, fuzz_mode);

    let total_errors = state
        .total_errors
        .load(std::sync::atomic::Ordering::Relaxed);
    let test_outcome: nix::Result<()> = if total_errors > 0 {
        Err(nix::errno::Errno::EIO)
    } else {
        Ok(())
    };
    let (sub_results, err) = runner::collect_test_results(
        "aging",
        &heap_label,
        heap_w,
        &[("aging", test_outcome, false)],
    );

    (sub_results, err, Some(aging_result))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_stop_iteration_limit() {
        let state = AgingState::new(HoldLimit::Count(1000), &["system".into()]);
        state.total_iters.store(10, Relaxed);
        assert!(should_stop(&state, None, Some(10)));
    }

    #[test]
    fn should_stop_running_false() {
        let state = AgingState::new(HoldLimit::Count(1000), &["system".into()]);
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
            None,
            1,
            None,
            Some(10),
            Duration::from_mins(1),
            false,
            HoldLimit::Count(32),
            0,
            None,
            6,
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
            None,
            1,
            None,
            Some(10),
            Duration::from_mins(1),
            true,
            HoldLimit::Count(8),
            0,
            Some(42),
            6,
        );
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(results.iter().all(|t| t.passed));
        let ar = aging_result.unwrap();
        assert_eq!(ar.mode, "fuzz");
        assert!(ar.total_iters >= 10);
    }

    #[test]
    fn run_fuzz_with_size_cap() {
        let b = crate::backend::mock::MockBackend::new();
        let heaps = vec!["system".to_string()];
        let (results, err, aging_result) = run(
            &b,
            &heaps,
            Some(65536),
            1,
            None,
            Some(20),
            Duration::from_mins(1),
            true,
            HoldLimit::Count(8),
            0,
            Some(42),
            6,
        );
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(results.iter().all(|t| t.passed));
        let ar = aging_result.unwrap();
        assert_eq!(ar.mode, "fuzz");
        assert!(ar.total_iters >= 20);
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
            emfile_count: 0,
            total_merges: 0,
            total_merge_errors: 0,
            throughput_iters_per_sec: Some(16.7),
            latency: Some(LatencyStats {
                count: 1000,
                min_us: 10,
                max_us: 500,
                avg_us: 50,
                stddev_us: 0,
                p50_us: 45,
                p95_us: 200,
                p99_us: 400,
                p99_9_us: 400,
                cv_pct: 0.0,
                trimmed_avg_us: 50,
                outlier_count: 0,
                throughput_ops: 20000,
                ci95_us: 0,
                mad_us: 0,
            }),
            baseline_avg_us: Some(40),
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
            heap_results: vec![],
            drain_bufs: 0,
            drain_bytes: 0,
            warnings: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: AgingResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.mode, "normal");
        assert_eq!(deserialized.total_iters, 1000);
        assert!((deserialized.trend - 1.375).abs() < f64::EPSILON);
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
            None,
            1,
            None,
            Some(30),
            Duration::from_mins(1),
            false,
            HoldLimit::Count(8),
            0,
            None,
            6,
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
            None,
            1,
            None,
            Some(50),
            Duration::from_mins(1),
            false,
            HoldLimit::Count(8),
            0,
            None,
            6,
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
            None,
            1,
            None,
            Some(100),
            Duration::from_mins(1),
            true,
            HoldLimit::Count(8),
            0,
            Some(42),
            6,
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
        let state = AgingState::new(HoldLimit::Count(1000), &["system".into()]);
        // Push latencies and simulate what reporter does
        {
            let mut lat = state.interval_latencies.lock().unwrap();
            lat.extend_from_slice(&[10, 20, 30, 40, 50]);
        }
        let latencies = std::mem::take(&mut *state.interval_latencies.lock().unwrap());
        let stats = stats::compute_stats(&latencies).unwrap();
        state.update_cumulative(&stats);

        assert!(!state.baseline_ready.load(Relaxed)); // need 5 intervals
        assert_eq!(state.baseline_intervals.load(Relaxed), 1);
        // Weighted: avg_us * count
        assert_eq!(
            state.baseline_sum.load(Relaxed),
            stats.avg_us * stats.count as u64
        );
        assert_eq!(state.baseline_count.load(Relaxed), stats.count as u64);
        assert_eq!(state.peak_p99.load(Relaxed), stats.p99_us);

        // Second interval with higher p99
        {
            let mut lat = state.interval_latencies.lock().unwrap();
            lat.extend_from_slice(&[100, 200, 300, 400, 500]);
        }
        let latencies2 = std::mem::take(&mut *state.interval_latencies.lock().unwrap());
        let stats2 = stats::compute_stats(&latencies2).unwrap();
        state.update_cumulative(&stats2);

        // baseline_sum should accumulate weighted sums from both intervals
        assert_eq!(
            state.baseline_sum.load(Relaxed),
            stats.avg_us * stats.count as u64 + stats2.avg_us * stats2.count as u64
        );
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
            None,
            2,
            None,
            Some(30),
            Duration::from_mins(1),
            false,
            HoldLimit::Count(16),
            0,
            None,
            6,
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
        let state = AgingState::new(HoldLimit::Count(1000), &["system".into()]);
        // Simulate two intervals
        let stats1 = LatencyStats {
            count: 10,
            min_us: 5,
            max_us: 100,
            avg_us: 50,
            stddev_us: 0,
            p50_us: 45,
            p95_us: 90,
            p99_us: 95,
            p99_9_us: 95,
            cv_pct: 0.0,
            trimmed_avg_us: 50,
            outlier_count: 0,
            throughput_ops: 20000,
            ci95_us: 0,
            mad_us: 0,
        };
        let stats2 = LatencyStats {
            count: 20,
            min_us: 3,
            max_us: 200,
            avg_us: 60,
            stddev_us: 0,
            p50_us: 55,
            p95_us: 180,
            p99_us: 190,
            p99_9_us: 190,
            cv_pct: 0.0,
            trimmed_avg_us: 60,
            outlier_count: 0,
            throughput_ops: 16667,
            ci95_us: 0,
            mad_us: 0,
        };
        state.update_cumulative(&stats1);
        state.update_cumulative(&stats2);

        let cum = state.cumulative_stats().unwrap();
        assert_eq!(cum.count, 30);
        // sum = 50*10 + 60*20 = 500 + 1200 = 1700, avg = 1700/30 = 56
        assert_eq!(cum.avg_us, 56);
        assert_eq!(cum.max_us, 200);
        assert_eq!(cum.p99_us, 190); // peak p99
        // Weighted: 50*10 + 60*20 = 1700, count = 30 samples
        assert_eq!(state.baseline_sum.load(Relaxed), 1700);
        assert_eq!(state.baseline_count.load(Relaxed), 30);
        assert_eq!(state.final_interval_avg.load(Relaxed), 60);
    }

    // ── parse_hold_limit tests ──────────────────────────────────────────

    #[test]
    fn parse_hold_limit_disabled() {
        assert_eq!(parse_hold_limit("0").unwrap(), HoldLimit::Disabled);
    }

    #[test]
    fn parse_hold_limit_count() {
        assert_eq!(parse_hold_limit("100").unwrap(), HoldLimit::Count(100));
        assert_eq!(parse_hold_limit("10000").unwrap(), HoldLimit::Count(10000));
    }

    #[test]
    fn parse_hold_limit_bytes_short() {
        assert_eq!(
            parse_hold_limit("512K").unwrap(),
            HoldLimit::Bytes(512 * 1024)
        );
        assert_eq!(
            parse_hold_limit("64M").unwrap(),
            HoldLimit::Bytes(64 * 1024 * 1024)
        );
        assert_eq!(
            parse_hold_limit("1G").unwrap(),
            HoldLimit::Bytes(1024 * 1024 * 1024)
        );
    }

    #[test]
    fn parse_hold_limit_bytes_long() {
        assert_eq!(
            parse_hold_limit("512KiB").unwrap(),
            HoldLimit::Bytes(512 * 1024)
        );
        assert_eq!(
            parse_hold_limit("64MiB").unwrap(),
            HoldLimit::Bytes(64 * 1024 * 1024)
        );
        assert_eq!(
            parse_hold_limit("2GiB").unwrap(),
            HoldLimit::Bytes(2 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn parse_hold_limit_case_insensitive() {
        assert_eq!(
            parse_hold_limit("512m").unwrap(),
            HoldLimit::Bytes(512 * 1024 * 1024)
        );
        assert_eq!(
            parse_hold_limit("1gib").unwrap(),
            HoldLimit::Bytes(1024 * 1024 * 1024)
        );
    }

    #[test]
    fn parse_hold_limit_invalid() {
        assert!(parse_hold_limit("abc").is_err());
        assert!(parse_hold_limit("512X").is_err());
        assert!(parse_hold_limit("").is_err());
    }
}
