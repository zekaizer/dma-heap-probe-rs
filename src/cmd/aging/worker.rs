// Normal mode aging worker: full pipeline round-robin across heaps.
// Every 3rd iteration holds a buffer in the pool instead of freeing,
// simulating realistic long-lived allocation patterns.

use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::probe::HeapCaps;

use super::fuzz::HoldPool;
use super::{AgingState, mark_init_error, should_stop};

/// Hold every Nth iteration's buffer instead of freeing immediately.
const HOLD_EVERY_NTH: usize = 3;

/// Context for a single heap: the opened device and its probed capabilities.
struct HeapContext<'a, B: HeapBackend> {
    heap: DmaHeap<'a, B>,
    caps: HeapCaps,
}

/// Spawn `threads` workers, each round-robin allocating across `heaps`.
#[allow(clippy::cast_possible_truncation, clippy::too_many_arguments)]
pub(crate) fn run_workers<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heaps: &[String],
    size: u64,
    threads: u32,
    state: &AgingState,
    duration: Option<Duration>,
    iterations: Option<u64>,
    per_thread_max: usize,
    per_thread_max_bytes: u64,
) {
    let heap_caps = crate::probe::discover_and_probe(backend, Some(heaps));
    if heap_caps.is_empty() {
        mark_init_error(state);
        return;
    }

    // Pre-open heaps for all workers to share.
    let contexts: Vec<HeapContext<'_, B>> = heap_caps
        .into_iter()
        .filter_map(|caps| {
            let heap = DmaHeap::open(backend, &caps.name).ok()?;
            Some(HeapContext { heap, caps })
        })
        .collect();

    if contexts.is_empty() {
        mark_init_error(state);
        return;
    }

    tracing::debug!(
        threads,
        heaps = contexts.len(),
        size,
        per_thread_max,
        "normal workers starting"
    );

    let deadline = duration.map(|d| Instant::now() + d);
    let contexts_ref = &contexts;

    std::thread::scope(|s| {
        for worker_id in 0..threads {
            s.spawn(move || {
                worker_loop(
                    backend,
                    contexts_ref,
                    size,
                    state,
                    deadline,
                    iterations,
                    per_thread_max,
                    per_thread_max_bytes,
                    worker_id,
                );
            });
        }
    });
}

/// Single worker loop: alloc → pipeline → hold/close, round-robin across heaps.
#[allow(clippy::cast_possible_truncation, clippy::too_many_arguments)]
fn worker_loop<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    contexts: &[HeapContext<'_, B>],
    size: u64,
    state: &AgingState,
    deadline: Option<Instant>,
    max_iters: Option<u64>,
    per_thread_max: usize,
    per_thread_max_bytes: u64,
    worker_id: u32,
) {
    let mut local_index = worker_id as usize;
    let mut hold_pool: HoldPool<'_, B> = HoldPool::new(
        per_thread_max,
        per_thread_max_bytes,
        state,
        u64::from(worker_id),
    );
    tracing::debug!(worker_id, per_thread_max, "worker started");

    loop {
        if should_stop(state, deadline, max_iters) {
            break;
        }

        let ctx = &contexts[local_index % contexts.len()];
        local_index = local_index.wrapping_add(1);

        let start = Instant::now();
        let fd = match ctx
            .heap
            .alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
        {
            Ok(fd) => fd,
            Err(Errno::ENOMEM) => {
                state.total_enomem.fetch_add(1, Relaxed);
                hold_pool.notify_enomem(worker_id);
                tracing::debug!(
                    worker_id,
                    heap = ctx.caps.name.as_str(),
                    size,
                    "alloc ENOMEM, backing off"
                );
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => {
                tracing::debug!(
                    worker_id,
                    heap = ctx.caps.name.as_str(),
                    size,
                    "alloc error"
                );
                state.total_errors.fetch_add(1, Relaxed);
                continue;
            }
        };

        state.total_allocs.fetch_add(1, Relaxed);
        hold_pool.notify_success(worker_id);
        let mut buf = DmaBuf::new(backend, fd, size as usize);

        // Full pipeline if heap supports it, otherwise alloc-close only.
        if ctx.caps.can_mmap && ctx.caps.can_sync && ctx.caps.can_write {
            if let Ok(ptr) = buf.mmap() {
                let _ = buf.sync_start(DMA_BUF_SYNC_WRITE);
                // SAFETY: ptr is valid and mapped to `size` bytes.
                unsafe {
                    super::sparse_fill(ptr, size as usize, 0xAA, None);
                }
                let _ = buf.sync_end(DMA_BUF_SYNC_WRITE);
                let _ = buf.sync_start(DMA_BUF_SYNC_READ);
                let _ = buf.sync_end(DMA_BUF_SYNC_READ);
            }
        } else if ctx.caps.can_mmap {
            let _ = buf.mmap();
        }

        // Hold every Nth buffer, free the rest immediately.
        if per_thread_max > 0 && local_index.is_multiple_of(HOLD_EVERY_NTH) {
            hold_pool.push(buf);
        } else {
            drop(buf);
            state.total_frees.fetch_add(1, Relaxed);
        }

        let latency_us = start.elapsed().as_micros() as u64;
        state.interval_latencies.lock().unwrap().push(latency_us);
        state.total_iters.fetch_add(1, Relaxed);
        tracing::trace!(
            worker_id,
            heap = ctx.caps.name.as_str(),
            size,
            latency_us,
            "iteration"
        );
    }

    hold_pool.drain_all();
    tracing::debug!(worker_id, "worker done");
}

#[cfg(test)]
mod tests {
    use super::super::{AgingState, HoldLimit};
    use crate::backend::mock::MockBackend;
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::Duration;

    #[test]
    fn worker_single_heap() {
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000));
        let heaps = vec!["system".to_string()];
        super::run_workers(&b, &heaps, 4096, 1, &state, None, Some(20), 8, 0);
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
        let allocs = state.total_allocs.load(Relaxed);
        let frees = state.total_frees.load(Relaxed);
        assert_eq!(allocs, frees, "normal mode: allocs must equal frees");
    }

    #[test]
    fn worker_multi_thread() {
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000));
        let heaps = vec!["system".to_string()];
        super::run_workers(&b, &heaps, 4096, 2, &state, None, Some(20), 8, 0);
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }

    #[test]
    fn worker_with_duration() {
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000));
        let heaps = vec!["system".to_string()];
        super::run_workers(
            &b,
            &heaps,
            4096,
            1,
            &state,
            Some(Duration::from_millis(100)),
            None,
            8,
            0,
        );
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn worker_no_hold() {
        // max_hold=0: all buffers freed immediately, no hold pool behavior.
        let b = MockBackend::new();
        let state = AgingState::new(HoldLimit::Count(1000));
        let heaps = vec!["system".to_string()];
        super::run_workers(&b, &heaps, 4096, 1, &state, None, Some(20), 0, 0);
        assert_eq!(b.buffer_count(), 0);
        let allocs = state.total_allocs.load(Relaxed);
        let frees = state.total_frees.load(Relaxed);
        assert_eq!(allocs, frees);
        // With max_hold=0, all allocs = iters (no hold overhead)
        assert_eq!(allocs, state.total_iters.load(Relaxed));
    }
}
