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

use super::{AgingState, HeapCaps, mark_init_error, should_stop};

/// Allocation sizes covering page and order boundary values.
const FUZZ_SIZES: &[u64] = &[
    1, 4095, 4096, 4097, // page boundary
    65535, 65536, 65537,     // order boundary
    1_048_576, // 1MB
    8_388_608, // 8MB
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
    SetNameThenWrite,
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
    if caps.can_set_name && caps.can_mmap && caps.can_write {
        table.push((Pipeline::SetNameThenWrite, 5));
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

/// FIFO buffer hold pool for delayed-release pressure.
struct HoldPool<'a, B: DmaBufBackend> {
    bufs: VecDeque<DmaBuf<'a, B>>,
    max_size: usize,
}

impl<'a, B: DmaBufBackend> HoldPool<'a, B> {
    fn new(max_size: usize) -> Self {
        Self {
            bufs: VecDeque::new(),
            max_size,
        }
    }

    fn push(&mut self, buf: DmaBuf<'a, B>) {
        if self.bufs.len() >= self.max_size {
            self.bufs.pop_front(); // FIFO eviction
        }
        self.bufs.push_back(buf);
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
    max_hold: usize,
    seed: Option<u64>,
) {
    #[allow(clippy::cast_possible_truncation)]
    let base_seed = seed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    });
    tracing::info!(seed = base_seed, "fuzz seed");

    let heap_caps = super::discover_and_probe(backend, Some(heaps));
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
                    max_hold,
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
    max_hold: usize,
    seed: u64,
    worker_id: u32,
) {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut hold_pool: HoldPool<'_, B> = HoldPool::new(max_hold);

    loop {
        if should_stop(state, deadline, max_iters) {
            break;
        }

        // Random heap selection
        let ctx_idx = rng.random_range(0..contexts.len());
        let ctx = &contexts[ctx_idx];

        // Random size
        let size = FUZZ_SIZES[rng.random_range(0..FUZZ_SIZES.len())];

        // Random pipeline
        let pipeline = select_pipeline(&ctx.weighted_table, &mut rng);

        let start = Instant::now();
        let fd = match ctx
            .heap
            .alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
        {
            Ok(fd) => fd,
            Err(Errno::ENOMEM) => {
                // Evict half the hold pool to free memory gradually.
                let drain = hold_pool.bufs.len() / 2 + 1;
                for _ in 0..drain {
                    hold_pool.bufs.pop_front();
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => {
                state.total_errors.fetch_add(1, Relaxed);
                continue;
            }
        };

        let error_occurred = execute_pipeline(
            backend,
            &mut rng,
            fd,
            size,
            pipeline,
            &ctx.caps,
            &mut hold_pool,
            state,
        );

        if error_occurred {
            state.total_errors.fetch_add(1, Relaxed);
        }

        let latency_us = start.elapsed().as_micros() as u64;
        state.interval_latencies.lock().unwrap().push(latency_us);
        state.total_iters.fetch_add(1, Relaxed);
    }

    // Drain hold pool on exit.
    drop(hold_pool);
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
    state: &AgingState,
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
            // Map a partial length (25-75% of buffer).
            let pct = rng.random_range(25u64..75);
            let partial_len = (size * pct / 100).max(1) as usize;
            let mut buf = DmaBuf::new(backend, fd, partial_len);
            if let Ok(ptr) = buf.mmap() {
                let pat = random_write_pattern(rng);
                // SAFETY: ptr valid for partial_len bytes.
                unsafe {
                    std::ptr::write_bytes(ptr, pattern_byte(pat), partial_len);
                }
            }
            drop(buf);
            false
        }

        Pipeline::WriteOnly => {
            let mut buf = DmaBuf::new(backend, fd, size_usize);
            if let Ok(ptr) = buf.mmap() {
                let flags = random_sync_flags(rng);
                let pat = random_write_pattern(rng);
                let _ = buf.sync_start(flags);
                // SAFETY: ptr valid for size_usize bytes.
                unsafe {
                    std::ptr::write_bytes(ptr, pattern_byte(pat), size_usize);
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
                    std::ptr::write_bytes(ptr, pattern_byte(pat), size_usize);
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
                    std::ptr::write_bytes(ptr, pattern_byte(pat), size_usize);
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
                        std::ptr::write_bytes(ptr, 0xBB, size_usize);
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

        Pipeline::SetNameThenWrite => {
            let mut buf = DmaBuf::new(backend, fd, size_usize);
            let iter_count = state.total_iters.load(Relaxed);
            let name = format!("fuzz_{iter_count}");
            // Truncate to 32 chars (DMA_BUF_NAME_LEN).
            let name = &name[..name.len().min(32)];
            let _ = buf.set_name(name);
            if let Ok(ptr) = buf.mmap() {
                let pat = random_write_pattern(rng);
                let _ = buf.sync_start(DMA_BUF_SYNC_WRITE);
                // SAFETY: ptr valid for size_usize bytes.
                unsafe {
                    std::ptr::write_bytes(ptr, pattern_byte(pat), size_usize);
                }
                let _ = buf.sync_end(DMA_BUF_SYNC_WRITE);
            }
            drop(buf);
            false
        }

        Pipeline::LlseekAfterWrite => {
            let mut buf = DmaBuf::new(backend, fd, size_usize);
            if let Ok(ptr) = buf.mmap() {
                let pat = random_write_pattern(rng);
                let _ = buf.sync_start(DMA_BUF_SYNC_WRITE);
                // SAFETY: ptr valid for size_usize bytes.
                unsafe {
                    std::ptr::write_bytes(ptr, pattern_byte(pat), size_usize);
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
    use super::super::AgingState;
    use crate::backend::mock::MockBackend;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn fuzz_runs() {
        let b = MockBackend::new();
        let state = AgingState::new();
        let heaps = vec!["system".to_string()];
        super::run_workers(&b, &heaps, 1, &state, None, Some(50), 8, Some(42));
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }

    #[test]
    fn fuzz_deterministic() {
        // Same seed should produce the same iteration count.
        let b1 = MockBackend::new();
        let s1 = AgingState::new();
        let heaps = vec!["system".to_string()];
        super::run_workers(&b1, &heaps, 1, &s1, None, Some(20), 8, Some(42));
        let iters1 = s1.total_iters.load(Relaxed);

        let b2 = MockBackend::new();
        let s2 = AgingState::new();
        super::run_workers(&b2, &heaps, 1, &s2, None, Some(20), 8, Some(42));
        let iters2 = s2.total_iters.load(Relaxed);

        assert_eq!(iters1, iters2, "same seed should produce same iterations");
    }

    #[test]
    fn fuzz_hold_pool_eviction() {
        let b = MockBackend::new();
        let state = AgingState::new();
        let heaps = vec!["system".to_string()];
        super::run_workers(&b, &heaps, 1, &state, None, Some(20), 4, Some(42));
        assert_eq!(
            b.buffer_count(),
            0,
            "all buffers should be freed after pool drain"
        );
    }

    #[test]
    fn fuzz_multi_thread() {
        let b = MockBackend::new();
        let state = AgingState::new();
        let heaps = vec!["system".to_string()];
        super::run_workers(&b, &heaps, 2, &state, None, Some(30), 8, Some(42));
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }
}
