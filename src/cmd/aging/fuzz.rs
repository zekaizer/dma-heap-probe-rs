// Fuzz mode aging worker: random size, operation, timing, and heap selection.

use std::collections::VecDeque;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant, SystemTime};

use nix::errno::Errno;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::SmallRng;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_RW, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};

use super::{AgingState, HoldLimit, mark_init_error, should_stop};
use crate::probe::HeapCaps;

/// Fuzz allocation sizes: 4K-aligned are common, others are rare edge cases.
pub(crate) const FUZZ_SIZES: &[u64] = &[
    // Common: 4K multiples (high weight)
    4096,        // 4K (1 page)
    8192,        // 8K
    16384,       // 16K
    65536,       // 64K (order boundary)
    262_144,     // 256K
    1_048_576,   // 1MB
    4_194_304,   // 4MB
    8_388_608,   // 8MB
    16_777_216,  // 16MB
    33_554_432,  // 32MB
    67_108_864,  // 64MB
    134_217_728, // 128MB
    268_435_456, // 256MB
    // Rare edge cases (low weight)
    1,     // minimum
    4095,  // page - 1
    4097,  // page + 1
    65535, // order - 1
    65537, // order + 1
];

// ── Pipeline variants ───────────────────────────────────────────────────────

/// Fuzz pipeline operation to execute.
#[derive(Debug, Clone, Copy)]
enum Pipeline {
    AllocClose,
    AllocMmapClose,
    PartialMmap,
    WriteOnly,
    WriteReadVerify,
    WriteNoSync,
    DoubleMmap,
    DupAndOperate,
    LlseekAfterWrite,
    SyncFileRoundtrip,
    AllocHold,
}

/// Write fill pattern.
#[derive(Debug, Clone, Copy)]
enum WritePattern {
    Zero,
    AllOnes,
    Pattern(u8),
}

// ── Weighted pipeline table ─────────────────────────────────────────────────

/// Build a weighted table of pipelines based on heap capabilities.
fn build_weighted_table(caps: &HeapCaps) -> Vec<(Pipeline, u32)> {
    let mut table = Vec::new();
    // Always available
    table.push((Pipeline::AllocClose, 10));
    table.push((Pipeline::AllocHold, 20));

    if caps.can_mmap {
        table.push((Pipeline::AllocMmapClose, 5));
        table.push((Pipeline::PartialMmap, 5));
    }
    if caps.can_mmap && caps.can_write {
        table.push((Pipeline::WriteOnly, 15));
        table.push((Pipeline::WriteReadVerify, 15));
        table.push((Pipeline::WriteNoSync, 5));
        table.push((Pipeline::DoubleMmap, 5));
    }
    if caps.can_dup {
        table.push((Pipeline::DupAndOperate, 5));
    }
    if caps.can_llseek && caps.can_mmap && caps.can_write {
        table.push((Pipeline::LlseekAfterWrite, 5));
    }
    if caps.can_sync_file {
        table.push((Pipeline::SyncFileRoundtrip, 5));
    }

    table
}

/// Select a pipeline from the weighted table using the given random value.
fn select_pipeline(table: &[(Pipeline, u32)], rng: &mut SmallRng) -> Pipeline {
    let total: u32 = table.iter().map(|(_, w)| w).sum();
    if total == 0 {
        return Pipeline::AllocClose;
    }
    let mut val = rng.random_range(0..total);
    for &(pipeline, weight) in table {
        if val < weight {
            return pipeline;
        }
        val -= weight;
    }
    Pipeline::AllocClose
}

/// Pick a random write pattern.
fn random_write_pattern(rng: &mut SmallRng) -> WritePattern {
    match rng.random_range(0u8..3) {
        0 => WritePattern::Zero,
        1 => WritePattern::AllOnes,
        _ => WritePattern::Pattern(rng.random()),
    }
}

/// Pick a random sync direction flag.
fn random_sync_flags(rng: &mut SmallRng) -> u64 {
    match rng.random_range(0u8..3) {
        0 => DMA_BUF_SYNC_READ,
        1 => DMA_BUF_SYNC_WRITE,
        _ => DMA_BUF_SYNC_RW,
    }
}

/// Get the byte value for a write pattern.
fn pattern_byte(pat: WritePattern) -> u8 {
    match pat {
        WritePattern::Zero => 0x00,
        WritePattern::AllOnes => 0xFF,
        WritePattern::Pattern(b) => b,
    }
}

// ── Hold pool ───────────────────────────────────────────────────────────────

/// Minimum hold pool size — below this, hold tests lose meaning.
pub(super) const MIN_HOLD_SIZE: usize = 2;
/// Consecutive ENOMEM count before shrinking `max_size`.
pub(super) const ENOMEM_SHRINK_THRESHOLD: u32 = 3;
/// Successful allocs after a shrink before attempting grow-back.
pub(super) const RECOVERY_THRESHOLD: u32 = 100;

/// FIFO buffer hold pool with adaptive sizing based on ENOMEM pressure.
pub(super) struct HoldPool<'a, B: DmaBufBackend> {
    bufs: VecDeque<DmaBuf<'a, B>>,
    state: &'a AgingState,
    max_size: usize,
    initial_max_size: usize,
    /// Per-thread byte limit (0 = use `max_size` count-based limit).
    max_bytes: u64,
    local_bytes: u64,
    consecutive_enomem: u32,
    success_since_shrink: u32,
    rng: SmallRng,
}

impl<'a, B: DmaBufBackend> HoldPool<'a, B> {
    pub(super) fn new(max_size: usize, max_bytes: u64, state: &'a AgingState, seed: u64) -> Self {
        Self {
            bufs: VecDeque::new(),
            state,
            max_size,
            initial_max_size: max_size,
            max_bytes,
            local_bytes: 0,
            consecutive_enomem: 0,
            success_since_shrink: 0,
            rng: SmallRng::seed_from_u64(seed),
        }
    }

    /// Whether this pool is at per-thread capacity.
    fn is_full(&self) -> bool {
        if self.max_bytes > 0 {
            self.local_bytes >= self.max_bytes
        } else {
            self.bufs.len() >= self.max_size
        }
    }

    /// Drain `n` buffers from the front, updating counters.
    fn drain_n(&mut self, n: usize) {
        let actual = n.min(self.bufs.len());
        let mut freed_bytes: u64 = 0;
        for _ in 0..actual {
            if let Some(buf) = self.bufs.pop_front() {
                freed_bytes += buf.len() as u64;
            }
        }
        if actual > 0 {
            self.local_bytes -= freed_bytes;
            self.state.total_frees.fetch_add(actual as u64, Relaxed);
            self.state.held_bufs.fetch_sub(actual as u64, Relaxed);
            self.state.held_bytes.fetch_sub(freed_bytes, Relaxed);
        }
    }

    pub(super) fn push(&mut self, mut buf: DmaBuf<'a, B>) {
        // 1. Large drain: ~1% when pool is at per-thread capacity.
        //    Must run before global cap check so cap-saturated pools can oscillate.
        if self.is_full() && self.rng.random_ratio(1, 100) {
            let pct = self.rng.random_range(20..=30);
            let to_drain = (self.bufs.len() * pct / 100).max(1);
            self.drain_n(to_drain);
        }

        // 2. Burst eviction when at per-thread capacity.
        if self.is_full() {
            let max_burst = self.bufs.len().div_ceil(8).max(2);
            let burst = self.rng.random_range(1..=max_burst);
            self.drain_n(burst.min(self.bufs.len()));
        }

        // 3. Global cap check: after drain, if still at limit, free immediately.
        let at_limit = match self.state.hold_limit {
            HoldLimit::Disabled => false,
            HoldLimit::Count(max) => self.state.held_bufs.load(Relaxed) >= max,
            HoldLimit::Bytes(max) => self.state.held_bytes.load(Relaxed) >= max,
        };
        if at_limit {
            self.state.total_frees.fetch_add(1, Relaxed);
            return;
        }

        // 4. 10% chance: unmap before holding (fd-only hold).
        if self.rng.random_ratio(1, 10) {
            buf.unmap();
        }

        let buf_bytes = buf.len() as u64;
        self.local_bytes += buf_bytes;
        self.state.held_bytes.fetch_add(buf_bytes, Relaxed);
        self.bufs.push_back(buf);
        self.state.held_bufs.fetch_add(1, Relaxed);
    }

    /// Handle `ENOMEM`: drain half the pool and shrink `max_size` if repeated.
    pub(super) fn notify_enomem(&mut self, worker_id: u32) {
        let to_drain = (self.bufs.len() / 2 + 1).min(self.bufs.len());
        self.drain_n(to_drain);

        self.consecutive_enomem += 1;
        self.success_since_shrink = 0;

        if self.consecutive_enomem >= ENOMEM_SHRINK_THRESHOLD {
            let old_max = self.max_size;
            self.max_size = (self.max_size / 2).max(MIN_HOLD_SIZE);
            // Trim pool to new max_size.
            if self.bufs.len() > self.max_size {
                self.drain_n(self.bufs.len() - self.max_size);
            }
            self.consecutive_enomem = 0;
            tracing::info!(
                worker_id,
                old_max,
                new_max = self.max_size,
                pool_len = self.bufs.len(),
                "adaptive hold pool shrink"
            );
        } else {
            tracing::debug!(
                worker_id,
                drained = to_drain,
                remaining = self.bufs.len(),
                consecutive_enomem = self.consecutive_enomem,
                "ENOMEM, evicting hold pool"
            );
        }
    }

    /// Handle successful alloc: reset ENOMEM counter, attempt grow-back.
    pub(super) fn notify_success(&mut self, worker_id: u32) {
        self.consecutive_enomem = 0;
        if self.max_size < self.initial_max_size {
            self.success_since_shrink += 1;
            if self.success_since_shrink >= RECOVERY_THRESHOLD {
                let old_max = self.max_size;
                self.max_size = (self.max_size * 2).min(self.initial_max_size);
                self.success_since_shrink = 0;
                tracing::debug!(
                    worker_id,
                    old_max,
                    new_max = self.max_size,
                    "adaptive hold pool recovery"
                );
            }
        }
    }

    /// Drain all remaining buffers, counting frees.
    pub(super) fn drain_all(&mut self) {
        self.drain_n(self.bufs.len());
    }
}

// ── Heap context for fuzz ───────────────────────────────────────────────────

struct FuzzHeapContext<'a, B: HeapBackend> {
    heap: DmaHeap<'a, B>,
    caps: HeapCaps,
    weighted_table: Vec<(Pipeline, u32)>,
}

// ── Worker entry point ──────────────────────────────────────────────────────

/// Spawn fuzz workers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_workers<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heaps: &[String],
    threads: u32,
    state: &AgingState,
    duration: Option<Duration>,
    iterations: Option<u64>,
    per_thread_max: usize,
    per_thread_max_bytes: u64,
    seed: Option<u64>,
) {
    #[allow(clippy::cast_possible_truncation)]
    let base_seed = seed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    });
    tracing::info!(seed = base_seed, "fuzz seed");

    let heap_caps = crate::probe::discover_and_probe(backend, Some(heaps));
    if heap_caps.is_empty() {
        mark_init_error(state);
        return;
    }

    let contexts: Vec<FuzzHeapContext<'_, B>> = heap_caps
        .into_iter()
        .filter_map(|caps| {
            let heap = DmaHeap::open(backend, &caps.name).ok()?;
            let weighted_table = build_weighted_table(&caps);
            Some(FuzzHeapContext {
                heap,
                caps,
                weighted_table,
            })
        })
        .collect();

    if contexts.is_empty() {
        mark_init_error(state);
        return;
    }

    tracing::debug!(
        threads,
        heaps = contexts.len(),
        per_thread_max,
        "fuzz workers starting"
    );

    let deadline = duration.map(|d| Instant::now() + d);
    let contexts_ref = &contexts;

    std::thread::scope(|s| {
        for worker_id in 0..threads {
            let worker_seed = base_seed.wrapping_add(u64::from(worker_id));
            s.spawn(move || {
                fuzz_worker_loop(
                    backend,
                    contexts_ref,
                    state,
                    deadline,
                    iterations,
                    per_thread_max,
                    per_thread_max_bytes,
                    worker_seed,
                    worker_id,
                );
            });
        }
    });
}

/// Single fuzz worker loop.
#[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
fn fuzz_worker_loop<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    contexts: &[FuzzHeapContext<'_, B>],
    state: &AgingState,
    deadline: Option<Instant>,
    max_iters: Option<u64>,
    per_thread_max: usize,
    per_thread_max_bytes: u64,
    seed: u64,
    worker_id: u32,
) {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut hold_pool: HoldPool<'_, B> =
        HoldPool::new(per_thread_max, per_thread_max_bytes, state, seed);
    tracing::debug!(worker_id, seed, "fuzz worker started");

    // Weighted size selection: 4K-aligned common + inverse-size bias, edge cases rare.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let cum_weights: Vec<u64> = FUZZ_SIZES
        .iter()
        .scan(0u64, |acc, &s| {
            let base: u64 = if s.is_multiple_of(4096) { 10000 } else { 1 };
            let log2 = (s.max(2) as f64).log2() as u64;
            *acc += base / (log2 * log2).max(1);
            Some(*acc)
        })
        .collect();
    let total_weight = *cum_weights.last().unwrap();

    loop {
        if should_stop(state, deadline, max_iters) {
            break;
        }

        // Random heap selection
        let ctx_idx = rng.random_range(0..contexts.len());
        let ctx = &contexts[ctx_idx];

        // Byte-fair random size (inverse-weighted).
        let r = rng.random_range(0..total_weight);
        let size = FUZZ_SIZES[cum_weights.partition_point(|&c| c <= r)];

        // Random pipeline
        let pipeline = select_pipeline(&ctx.weighted_table, &mut rng);

        let start = Instant::now();
        let fd = match ctx
            .heap
            .alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
        {
            Ok(fd) => fd,
            Err(Errno::ENOMEM) => {
                state.total_enomem.fetch_add(1, Relaxed);
                hold_pool.notify_enomem(worker_id);
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => {
                tracing::debug!(worker_id, "alloc error");
                state.total_errors.fetch_add(1, Relaxed);
                continue;
            }
        };

        state.total_allocs.fetch_add(1, Relaxed);
        hold_pool.notify_success(worker_id);

        let error_occurred = execute_pipeline(
            backend,
            &mut rng,
            fd,
            size,
            pipeline,
            &ctx.caps,
            &mut hold_pool,
        );

        if error_occurred {
            state.total_errors.fetch_add(1, Relaxed);
        }
        if !matches!(pipeline, Pipeline::AllocHold) {
            state.total_frees.fetch_add(1, Relaxed);
        }

        let latency_us = start.elapsed().as_micros() as u64;
        state.interval_latencies.lock().unwrap().push(latency_us);
        state.total_iters.fetch_add(1, Relaxed);
        tracing::trace!(
            worker_id,
            heap = ctx.caps.name.as_str(),
            size,
            ?pipeline,
            latency_us,
            "fuzz iteration"
        );
    }

    // Drain hold pool on exit, counting remaining frees.
    hold_pool.drain_all();
    tracing::debug!(worker_id, "fuzz worker done");
}

/// Execute a single fuzz pipeline. Returns true if an error occurred.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]
fn execute_pipeline<'a, B: HeapBackend + DmaBufBackend>(
    backend: &'a B,
    rng: &mut SmallRng,
    fd: std::os::unix::io::RawFd,
    size: u64,
    pipeline: Pipeline,
    caps: &HeapCaps,
    hold_pool: &mut HoldPool<'a, B>,
) -> bool {
    let size_usize = size as usize;

    match pipeline {
        Pipeline::AllocClose => {
            let buf = DmaBuf::new(backend, fd, size_usize);
            drop(buf);
            false
        }

        Pipeline::AllocMmapClose => {
            let mut buf = DmaBuf::new(backend, fd, size_usize);
            let _ = buf.mmap();
            drop(buf);
            false
        }

        Pipeline::PartialMmap => {
            // Map a partial length (25-75% of buffer) with proper sync.
            let pct = rng.random_range(25u64..75);
            let partial_len = (size * pct / 100).max(1) as usize;
            let mut buf = DmaBuf::new(backend, fd, partial_len);
            if let Ok(ptr) = buf.mmap() {
                let pat = random_write_pattern(rng);
                let _ = buf.sync_start(DMA_BUF_SYNC_WRITE);
                // SAFETY: ptr valid for partial_len bytes.
                unsafe {
                    super::sparse_fill(ptr, partial_len, pattern_byte(pat), Some(rng));
                }
                let _ = buf.sync_end(DMA_BUF_SYNC_WRITE);
            }
            drop(buf);
            false
        }

        Pipeline::WriteOnly => {
            // Intentionally uses random sync flags (including READ for write ops)
            // to stress-test mismatched sync direction handling in drivers.
            let mut buf = DmaBuf::new(backend, fd, size_usize);
            if let Ok(ptr) = buf.mmap() {
                let flags = random_sync_flags(rng);
                let pat = random_write_pattern(rng);
                let _ = buf.sync_start(flags);
                // SAFETY: ptr valid for size_usize bytes.
                unsafe {
                    super::sparse_fill(ptr, size_usize, pattern_byte(pat), Some(rng));
                }
                let _ = buf.sync_end(flags);
            }
            drop(buf);
            false
        }

        Pipeline::WriteReadVerify => {
            let mut buf = DmaBuf::new(backend, fd, size_usize);
            let mut error = false;
            if let Ok(ptr) = buf.mmap() {
                let pat = random_write_pattern(rng);
                let expected = pattern_byte(pat);

                let _ = buf.sync_start(DMA_BUF_SYNC_WRITE);
                // SAFETY: ptr valid for size_usize bytes.
                unsafe {
                    std::ptr::write_bytes(ptr, expected, size_usize);
                }
                let _ = buf.sync_end(DMA_BUF_SYNC_WRITE);

                let _ = buf.sync_start(DMA_BUF_SYNC_READ);
                // SAFETY: ptr valid for size_usize bytes.
                let slice = unsafe { std::slice::from_raw_parts(ptr, size_usize) };
                if let Some(offset) = slice.iter().position(|&b| b != expected) {
                    tracing::error!(
                        offset,
                        expected,
                        actual = slice[offset],
                        "data corruption detected"
                    );
                    error = true;
                }
                let _ = buf.sync_end(DMA_BUF_SYNC_READ);
            }
            drop(buf);
            error
        }

        Pipeline::WriteNoSync => {
            let mut buf = DmaBuf::new(backend, fd, size_usize);
            if let Ok(ptr) = buf.mmap() {
                let pat = random_write_pattern(rng);
                // SAFETY: ptr valid for size_usize bytes.
                unsafe {
                    super::sparse_fill(ptr, size_usize, pattern_byte(pat), Some(rng));
                }
            }
            drop(buf);
            false
        }

        Pipeline::DoubleMmap => {
            let mut buf = DmaBuf::new(backend, fd, size_usize);
            let _ = buf.mmap();
            // Second mmap should be idempotent.
            if let Ok(ptr) = buf.mmap() {
                let pat = random_write_pattern(rng);
                if caps.can_sync {
                    let _ = buf.sync_start(DMA_BUF_SYNC_WRITE);
                }
                // SAFETY: ptr valid for size_usize bytes.
                unsafe {
                    super::sparse_fill(ptr, size_usize, pattern_byte(pat), Some(rng));
                }
                if caps.can_sync {
                    let _ = buf.sync_end(DMA_BUF_SYNC_WRITE);
                }
            }
            drop(buf);
            false
        }

        Pipeline::DupAndOperate => {
            let buf = DmaBuf::new(backend, fd, size_usize);
            if let Ok(mut dup_buf) = buf.dup() {
                if caps.can_mmap
                    && caps.can_write
                    && let Ok(ptr) = dup_buf.mmap()
                {
                    let _ = dup_buf.sync_start(DMA_BUF_SYNC_WRITE);
                    // SAFETY: ptr valid for size_usize bytes.
                    unsafe {
                        super::sparse_fill(ptr, size_usize, 0xBB, Some(rng));
                    }
                    let _ = dup_buf.sync_end(DMA_BUF_SYNC_WRITE);
                }
                drop(buf);
                drop(dup_buf);
            } else {
                drop(buf);
            }
            false
        }

        Pipeline::LlseekAfterWrite => {
            let mut buf = DmaBuf::new(backend, fd, size_usize);
            if let Ok(ptr) = buf.mmap() {
                let pat = random_write_pattern(rng);
                let _ = buf.sync_start(DMA_BUF_SYNC_WRITE);
                // SAFETY: ptr valid for size_usize bytes.
                unsafe {
                    super::sparse_fill(ptr, size_usize, pattern_byte(pat), Some(rng));
                }
                let _ = buf.sync_end(DMA_BUF_SYNC_WRITE);
            }
            let _ = buf.llseek_size();
            drop(buf);
            false
        }

        Pipeline::SyncFileRoundtrip => {
            let buf = DmaBuf::new(backend, fd, size_usize);
            #[allow(clippy::cast_possible_truncation)]
            if let Ok(sync_fd) = buf.export_sync_file(DMA_BUF_SYNC_READ as u32) {
                let _ = buf.import_sync_file(DMA_BUF_SYNC_READ as u32, sync_fd);
            }
            drop(buf);
            false
        }

        Pipeline::AllocHold => {
            let buf = DmaBuf::new(backend, fd, size_usize);
            hold_pool.push(buf);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{AgingState, HoldLimit};
    use super::{ENOMEM_SHRINK_THRESHOLD, HoldPool, MIN_HOLD_SIZE, RECOVERY_THRESHOLD};
    use crate::backend::mock::MockBackend;
    use crate::dmabuf::DmaBuf;
    use std::sync::atomic::Ordering::Relaxed;

    fn test_heaps() -> Vec<String> {
        vec!["system".to_string()]
    }

    fn make_buf(backend: &MockBackend) -> DmaBuf<'_, MockBackend> {
        use crate::heap::DmaHeap;
        use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
        let heap = DmaHeap::open(backend, "system").unwrap();
        let fd = heap
            .alloc(4096, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
            .unwrap();
        DmaBuf::new(backend, fd, 4096)
    }

    #[test]
    fn adaptive_shrink() {
        let heaps = test_heaps();
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(16, 0, &state, 42);
        // Fill pool
        for _ in 0..16 {
            pool.push(make_buf(&b));
        }
        assert_eq!(pool.max_size, 16);
        // Trigger shrink: ENOMEM_SHRINK_THRESHOLD consecutive ENOMEMs
        for _ in 0..ENOMEM_SHRINK_THRESHOLD {
            pool.notify_enomem(0);
        }
        assert_eq!(pool.max_size, 8);
    }

    #[test]
    fn adaptive_min_floor() {
        let heaps = test_heaps();
        let _b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(MIN_HOLD_SIZE, 0, &state, 42);
        // Even after repeated shrinks, should not go below MIN_HOLD_SIZE
        for _ in 0..ENOMEM_SHRINK_THRESHOLD * 3 {
            pool.notify_enomem(0);
        }
        assert_eq!(pool.max_size, MIN_HOLD_SIZE);
    }

    #[test]
    fn adaptive_recovery() {
        let heaps = test_heaps();
        let _b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(16, 0, &state, 42);
        // Shrink first
        for _ in 0..ENOMEM_SHRINK_THRESHOLD {
            pool.notify_enomem(0);
        }
        assert_eq!(pool.max_size, 8);
        // Recover after RECOVERY_THRESHOLD successes
        for _ in 0..RECOVERY_THRESHOLD {
            pool.notify_success(0);
        }
        assert_eq!(pool.max_size, 16);
    }

    #[test]
    fn adaptive_recovery_ceiling() {
        let heaps = test_heaps();
        let _b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(8, 0, &state, 42);
        // Shrink to 4
        for _ in 0..ENOMEM_SHRINK_THRESHOLD {
            pool.notify_enomem(0);
        }
        assert_eq!(pool.max_size, 4);
        // Recover to 8 (ceiling)
        for _ in 0..RECOVERY_THRESHOLD {
            pool.notify_success(0);
        }
        assert_eq!(pool.max_size, 8);
        // Further successes should not exceed initial_max_size
        for _ in 0..RECOVERY_THRESHOLD {
            pool.notify_success(0);
        }
        assert_eq!(pool.max_size, 8);
    }

    #[test]
    fn drain_on_every_enomem() {
        let heaps = test_heaps();
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(16, 0, &state, 42);
        for _ in 0..10 {
            pool.push(make_buf(&b));
        }
        assert_eq!(pool.bufs.len(), 10);
        // Single ENOMEM (below threshold) should still drain half
        pool.notify_enomem(0);
        assert!(pool.bufs.len() <= 5, "should drain at least half");
    }

    #[test]
    fn held_bufs_tracks_push_and_drain() {
        let heaps = test_heaps();
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(16, 0, &state, 42);
        for i in 1..=5 {
            pool.push(make_buf(&b));
            assert_eq!(state.held_bufs.load(Relaxed), i);
        }
        pool.drain_all();
        assert_eq!(state.held_bufs.load(Relaxed), 0);
    }

    #[test]
    fn held_bufs_tracks_eviction() {
        let heaps = test_heaps();
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(4, 0, &state, 42);
        // Fill to capacity
        for _ in 0..4 {
            pool.push(make_buf(&b));
        }
        assert_eq!(state.held_bufs.load(Relaxed), 4);
        // Push beyond capacity — burst eviction keeps count at or below max_size
        pool.push(make_buf(&b));
        assert!(state.held_bufs.load(Relaxed) <= 4);
        pool.push(make_buf(&b));
        assert!(state.held_bufs.load(Relaxed) <= 4);
        pool.drain_all();
        assert_eq!(state.held_bufs.load(Relaxed), 0);
    }

    #[test]
    fn held_bufs_tracks_enomem_drain() {
        let heaps = test_heaps();
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(16, 0, &state, 42);
        for _ in 0..10 {
            pool.push(make_buf(&b));
        }
        assert_eq!(state.held_bufs.load(Relaxed), 10);
        pool.notify_enomem(0);
        // After draining half: counter must match actual pool length
        assert_eq!(state.held_bufs.load(Relaxed), pool.bufs.len() as u64);
        pool.drain_all();
        assert_eq!(state.held_bufs.load(Relaxed), 0);
    }

    #[test]
    fn held_bufs_zero_after_fuzz_workers() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string()];
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        super::run_workers(&b, &heaps, 1, &state, None, Some(50), 8, 0, Some(42));
        assert_eq!(
            state.held_bufs.load(Relaxed),
            0,
            "held_bufs must be zero after all workers drain"
        );
        assert_eq!(
            state.held_bytes.load(Relaxed),
            0,
            "held_bytes must be zero after all workers drain"
        );
    }

    #[test]
    fn held_bufs_global_cap() {
        let heaps = test_heaps();
        let b = MockBackend::new();
        // Global cap = 3 buffers, per-thread pool max = 16
        let state = AgingState::new(HoldLimit::Count(3), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(16, 0, &state, 42);
        // Push 5 buffers — only 3 should be held, rest freed at global cap
        for _ in 0..5 {
            pool.push(make_buf(&b));
        }
        assert_eq!(state.held_bufs.load(Relaxed), 3);
        assert_eq!(pool.bufs.len(), 3);
        // total_frees should count the 2 that hit the global cap
        assert_eq!(state.total_frees.load(Relaxed), 2);
        pool.drain_all();
        assert_eq!(state.held_bufs.load(Relaxed), 0);
        assert_eq!(state.held_bytes.load(Relaxed), 0);
    }

    #[test]
    fn held_bytes_global_cap() {
        let heaps = test_heaps();
        let b = MockBackend::new();
        // Global cap = 8192 bytes (2 x 4096-byte buffers)
        let state = AgingState::new(HoldLimit::Bytes(8192), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(usize::MAX, 8192, &state, 42);
        // Push 4 buffers — only 2 fit within 8192 byte limit (per-thread)
        for _ in 0..4 {
            pool.push(make_buf(&b));
        }
        assert_eq!(state.held_bufs.load(Relaxed), 2);
        assert_eq!(state.held_bytes.load(Relaxed), 2 * 4096);
        assert_eq!(state.total_frees.load(Relaxed), 2);
        pool.drain_all();
        assert_eq!(state.held_bytes.load(Relaxed), 0);
    }

    #[test]
    fn held_bytes_tracks_size() {
        let heaps = test_heaps();
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(16, 0, &state, 42);
        // Each mock buf is 4096 bytes
        for _ in 0..3 {
            pool.push(make_buf(&b));
        }
        assert_eq!(state.held_bytes.load(Relaxed), 3 * 4096);
        pool.drain_all();
        assert_eq!(state.held_bytes.load(Relaxed), 0);
    }

    #[test]
    fn held_bytes_tracks_eviction() {
        let heaps = test_heaps();
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        let mut pool: HoldPool<'_, MockBackend> = HoldPool::new(2, 0, &state, 42);
        // Push 3 → burst eviction, held should be ≤ 2 bufs
        for _ in 0..3 {
            pool.push(make_buf(&b));
        }
        let bufs = state.held_bufs.load(Relaxed);
        assert!(bufs <= 2);
        assert_eq!(state.held_bytes.load(Relaxed), bufs * 4096);
        pool.drain_all();
        assert_eq!(state.held_bytes.load(Relaxed), 0);
    }

    #[test]
    fn fuzz_runs() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string()];
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        super::run_workers(&b, &heaps, 1, &state, None, Some(50), 8, 0, Some(42));
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
        let allocs = state.total_allocs.load(Relaxed);
        let frees = state.total_frees.load(Relaxed);
        assert_eq!(allocs, frees, "fuzz: allocs must equal frees after drain");
    }

    #[test]
    fn fuzz_deterministic() {
        // Same seed should produce the same iteration count.
        let heaps = vec!["system".to_string()];
        let b1 = MockBackend::new();
        let s1 = AgingState::new(HoldLimit::Count(1000), &heaps);
        super::run_workers(&b1, &heaps, 1, &s1, None, Some(20), 8, 0, Some(42));
        let iters1 = s1.total_iters.load(Relaxed);

        let b2 = MockBackend::new();
        let s2 = AgingState::new(HoldLimit::Count(1000), &heaps);
        super::run_workers(&b2, &heaps, 1, &s2, None, Some(20), 8, 0, Some(42));
        let iters2 = s2.total_iters.load(Relaxed);

        assert_eq!(iters1, iters2, "same seed should produce same iterations");
    }

    #[test]
    fn fuzz_hold_pool_eviction() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string()];
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        super::run_workers(&b, &heaps, 1, &state, None, Some(20), 4, 0, Some(42));
        assert_eq!(
            b.buffer_count(),
            0,
            "all buffers should be freed after pool drain"
        );
        assert_eq!(
            state.total_allocs.load(Relaxed),
            state.total_frees.load(Relaxed),
            "allocs must equal frees after pool drain"
        );
    }

    #[test]
    fn fuzz_multi_thread() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string()];
        let state = AgingState::new(HoldLimit::Count(1000), &heaps);
        super::run_workers(&b, &heaps, 2, &state, None, Some(30), 8, 0, Some(42));
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
        assert_eq!(
            state.total_allocs.load(Relaxed),
            state.total_frees.load(Relaxed),
            "allocs must equal frees after drain"
        );
    }
}
