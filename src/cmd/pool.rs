// Pool/cache behavior tests: warmup, drain, size switch, release order,
// deferred free.

use std::error::Error;
use std::time::Instant;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::cmd::perf::compute_stats;
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};

/// Default buffer count for pool tests.
const POOL_COUNT: u32 = 100;

/// Size for pool warmup/drain tests.
const POOL_SIZE: u64 = 65536; // 64 KB

/// Iterations for latency measurements.
const MEASURE_ITERS: u32 = 100;

/// Run all pool/cache behavior tests.
/// Returns sub-test results (and the first error, if any).
#[allow(clippy::cast_possible_truncation)]
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> (Vec<SubTestResult>, Option<Box<dyn Error>>) {
    let tests: Vec<(&str, nix::Result<()>)> = vec![
        ("pool_warmup", test_pool_warmup(backend, heap_name)),
        ("size_switch", test_size_switch(backend, heap_name)),
        ("release_order", test_release_order(backend, heap_name)),
        ("deferred_free", test_deferred_free(backend, heap_name)),
    ];

    runner::collect_test_results("pool", &tests)
}

/// Measure alloc latency and return samples in microseconds.
#[allow(clippy::cast_possible_truncation)]
fn measure_alloc_latency<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap: &DmaHeap<'_, B>,
    size: u64,
    count: u32,
) -> nix::Result<Vec<u64>> {
    let mut samples = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let start = Instant::now();
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let elapsed = start.elapsed().as_micros() as u64;
        samples.push(elapsed);
        let buf = DmaBuf::new(backend, fd, size as usize);
        drop(buf);
    }
    Ok(samples)
}

/// Compare cold vs warm alloc latency to quantify pool effect.
#[allow(clippy::cast_possible_truncation)]
fn test_pool_warmup<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Cold: first N allocations.
    let cold_samples = measure_alloc_latency(backend, &heap, POOL_SIZE, MEASURE_ITERS)?;

    // Warm: alloc/close cycle to fill pool, then measure.
    for _ in 0..POOL_COUNT {
        let fd = heap.alloc(
            POOL_SIZE,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        )?;
        let buf = DmaBuf::new(backend, fd, POOL_SIZE as usize);
        drop(buf);
    }
    let warm_samples = measure_alloc_latency(backend, &heap, POOL_SIZE, MEASURE_ITERS)?;

    if let (Some(cold), Some(warm)) = (compute_stats(&cold_samples), compute_stats(&warm_samples)) {
        tracing::info!(
            cold_p50_us = cold.p50_us,
            cold_p95_us = cold.p95_us,
            warm_p50_us = warm.p50_us,
            warm_p95_us = warm.p95_us,
            "pool_warmup"
        );
    }

    Ok(())
}

/// Measure latency impact of switching allocation sizes.
#[allow(clippy::cast_possible_truncation)]
fn test_size_switch<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let size_a: u64 = 65536; // 64 KB
    let size_b: u64 = 4096; // 4 KB
    let phase_count = 500u32;

    // Phase 1: Fill pool with size_a.
    let phase1 = measure_alloc_latency(backend, &heap, size_a, phase_count)?;

    // Phase 2: Switch to size_b.
    let phase2 = measure_alloc_latency(backend, &heap, size_b, phase_count)?;

    // Phase 3: Switch back to size_a.
    let phase3 = measure_alloc_latency(backend, &heap, size_a, phase_count)?;

    // Compare first 10 vs last 10 of each phase.
    let first_10 = |samples: &[u64]| compute_stats(&samples[..10.min(samples.len())]);
    let last_10 = |samples: &[u64]| {
        let start = samples.len().saturating_sub(10);
        compute_stats(&samples[start..])
    };

    if let (Some(p1_first), Some(p1_last)) = (first_10(&phase1), last_10(&phase1)) {
        tracing::info!(
            phase = 1,
            size = size_a,
            first10_p50 = p1_first.p50_us,
            last10_p50 = p1_last.p50_us,
            "size_switch"
        );
    }
    if let (Some(p2_first), Some(p2_last)) = (first_10(&phase2), last_10(&phase2)) {
        tracing::info!(
            phase = 2,
            size = size_b,
            first10_p50 = p2_first.p50_us,
            last10_p50 = p2_last.p50_us,
            "size_switch"
        );
    }
    if let (Some(p3_first), Some(p3_last)) = (first_10(&phase3), last_10(&phase3)) {
        tracing::info!(
            phase = 3,
            size = size_a,
            first10_p50 = p3_first.p50_us,
            last10_p50 = p3_last.p50_us,
            "size_switch"
        );
    }

    Ok(())
}

/// Compare alloc latency after LIFO, FIFO, and random close orders.
#[allow(clippy::cast_possible_truncation)]
fn test_release_order<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let count = POOL_COUNT;

    // Helper: allocate N buffers, close in given order, then measure realloc latency.
    let test_order = |order: &str| -> nix::Result<Vec<u64>> {
        let mut buffers: Vec<DmaBuf<'_, B>> = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let fd = heap.alloc(
                POOL_SIZE,
                DMA_HEAP_ALLOC_FD_FLAGS,
                DMA_HEAP_VALID_HEAP_FLAGS,
            )?;
            buffers.push(DmaBuf::new(backend, fd, POOL_SIZE as usize));
        }

        // Close in specified order.
        match order {
            "lifo" => {
                // Reverse order (pop from end = LIFO).
                while buffers.pop().is_some() {}
            }
            "fifo" => {
                // Forward order (drain from front = FIFO).
                for buf in buffers.drain(..) {
                    drop(buf);
                }
            }
            _ => {
                // Random-ish: close odd indices first, then even.
                let mut temp: Vec<Option<DmaBuf<'_, B>>> = buffers.into_iter().map(Some).collect();
                for i in (1..temp.len()).step_by(2) {
                    temp[i].take();
                }
                for i in (0..temp.len()).step_by(2) {
                    temp[i].take();
                }
            }
        }

        // Measure realloc latency.
        measure_alloc_latency(backend, &heap, POOL_SIZE, count)
    };

    for order in &["lifo", "fifo", "random"] {
        let samples = test_order(order)?;
        if let Some(stats) = compute_stats(&samples) {
            tracing::info!(
                order,
                p50_us = stats.p50_us,
                p95_us = stats.p95_us,
                avg_us = stats.avg_us,
                "release_order"
            );
        }
    }

    Ok(())
}

/// Allocate and free in bulk, then check if memory is immediately returned.
///
/// On real devices, reads `/proc/meminfo` to detect deferred free behavior.
/// On mock/host, validates the alloc→free cycle completes without error.
#[allow(clippy::cast_possible_truncation)]
fn test_deferred_free<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let count = 200u32;
    let size: u64 = 1_048_576; // 1 MB

    // Read meminfo before (best-effort, may fail on non-Linux).
    let mem_before = crate::procfs::read_meminfo().ok();

    // Allocate in bulk.
    let mut buffers: Vec<DmaBuf<'_, B>> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        buffers.push(DmaBuf::new(backend, fd, size as usize));
    }

    // Free all at once.
    drop(buffers);

    // Read meminfo after.
    let mem_after = crate::procfs::read_meminfo().ok();

    if let (Some(before), Some(after)) = (mem_before, mem_after) {
        let freed_kb = after.mem_free_kb.saturating_sub(before.mem_free_kb);
        let expected_kb = u64::from(count) * size / 1024;
        #[allow(clippy::cast_precision_loss)]
        let recovery_pct = if expected_kb > 0 {
            freed_kb as f64 / expected_kb as f64 * 100.0
        } else {
            0.0
        };
        tracing::info!(
            before_free_kb = before.mem_free_kb,
            after_free_kb = after.mem_free_kb,
            freed_kb,
            expected_kb,
            recovery_pct = format!("{recovery_pct:.1}"),
            "deferred_free"
        );
    } else {
        tracing::info!(count, size, "deferred_free (meminfo unavailable on host)");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn pool_warmup_runs() {
        let b = MockBackend::new();
        test_pool_warmup(&b, "system").unwrap();
    }

    #[test]
    fn size_switch_runs() {
        let b = MockBackend::new();
        test_size_switch(&b, "system").unwrap();
    }

    #[test]
    fn release_order_runs() {
        let b = MockBackend::new();
        test_release_order(&b, "system").unwrap();
    }

    #[test]
    fn deferred_free_runs() {
        let b = MockBackend::new();
        test_deferred_free(&b, "system").unwrap();
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        let (results, err) = run(&b, "system");
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn warmup_no_leak() {
        let b = MockBackend::new();
        test_pool_warmup(&b, "system").unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn release_order_no_leak() {
        let b = MockBackend::new();
        test_release_order(&b, "system").unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn deferred_free_no_leak() {
        let b = MockBackend::new();
        test_deferred_free(&b, "system").unwrap();
        assert_eq!(b.buffer_count(), 0);
    }
}
