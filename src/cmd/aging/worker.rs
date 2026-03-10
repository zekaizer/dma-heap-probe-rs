// Normal mode aging worker: full pipeline round-robin across heaps.

use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};

use super::{AgingState, HeapCaps, mark_init_error, should_stop};

/// Context for a single heap: the opened device and its probed capabilities.
struct HeapContext<'a, B: HeapBackend> {
    heap: DmaHeap<'a, B>,
    caps: HeapCaps,
}

/// Spawn `threads` workers, each round-robin allocating across `heaps`.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn run_workers<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heaps: &[String],
    size: u64,
    threads: u32,
    state: &AgingState,
    duration: Option<Duration>,
    iterations: Option<u64>,
) {
    let heap_caps = super::discover_and_probe(backend, Some(heaps));
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
                    worker_id,
                );
            });
        }
    });
}

/// Single worker loop: alloc → pipeline → close, round-robin across heaps.
#[allow(clippy::cast_possible_truncation)]
fn worker_loop<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    contexts: &[HeapContext<'_, B>],
    size: u64,
    state: &AgingState,
    deadline: Option<Instant>,
    max_iters: Option<u64>,
    worker_id: u32,
) {
    let mut local_index = worker_id as usize;
    tracing::debug!(worker_id, "worker started");

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

        let mut buf = DmaBuf::new(backend, fd, size as usize);

        // Full pipeline if heap supports it, otherwise alloc-close only.
        if ctx.caps.can_mmap && ctx.caps.can_sync && ctx.caps.can_write {
            if let Ok(ptr) = buf.mmap() {
                let _ = buf.sync_start(DMA_BUF_SYNC_WRITE);
                // SAFETY: ptr is valid and mapped to `size` bytes.
                unsafe {
                    std::ptr::write_bytes(ptr, 0xAA, size as usize);
                }
                let _ = buf.sync_end(DMA_BUF_SYNC_WRITE);
                let _ = buf.sync_start(DMA_BUF_SYNC_READ);
                let _ = buf.sync_end(DMA_BUF_SYNC_READ);
            }
        } else if ctx.caps.can_mmap {
            let _ = buf.mmap();
        }

        drop(buf);

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

    tracing::debug!(worker_id, "worker done");
}

#[cfg(test)]
mod tests {
    use super::super::AgingState;
    use crate::backend::mock::MockBackend;
    use std::time::Duration;

    #[test]
    fn worker_single_heap() {
        let b = MockBackend::new();
        let state = AgingState::new();
        let heaps = vec!["system".to_string()];
        super::run_workers(&b, &heaps, 4096, 1, &state, None, Some(20));
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }

    #[test]
    fn worker_multi_thread() {
        let b = MockBackend::new();
        let state = AgingState::new();
        let heaps = vec!["system".to_string()];
        super::run_workers(&b, &heaps, 4096, 2, &state, None, Some(20));
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }

    #[test]
    fn worker_with_duration() {
        let b = MockBackend::new();
        let state = AgingState::new();
        let heaps = vec!["system".to_string()];
        super::run_workers(
            &b,
            &heaps,
            4096,
            1,
            &state,
            Some(Duration::from_millis(100)),
            None,
        );
        assert_eq!(b.buffer_count(), 0);
    }
}
