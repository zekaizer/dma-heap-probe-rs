// Memory pressure tests: gradual exhaust, recovery, concurrent pressure.

use std::error::Error;
use std::time::Instant;

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};

/// Run all pressure tests. Returns sub-test results (and the first error, if any).
#[allow(clippy::cast_possible_truncation)]
pub fn run<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    alloc_size: u64,
    max_allocs_override: Option<usize>,
) -> (Vec<SubTestResult>, Option<Box<dyn Error>>) {
    let max_allocs = match max_allocs_override {
        Some(n) => n,
        None => safe_exhaust_limit(alloc_size),
    };

    let source = if max_allocs_override.is_some() {
        "cli override"
    } else {
        "auto-detected"
    };

    println!("pressure sequence:");
    println!("  heap: {heap_name}");
    println!("  alloc_size: {alloc_size} bytes");
    println!("  max_allocs: {max_allocs} ({source})");
    println!();
    println!("  1. gradual_exhaust");
    println!("       alloc({alloc_size}) in loop until ENOMEM or max_allocs");
    println!("       track per-alloc latency");
    println!("       -> count, total_mb, avg_latency_us");
    println!("  2. recovery");
    println!("       exhaust -> release 50% -> re-alloc");
    println!("       -> released, recovered, avg_recovery_us");
    println!("  3. pressure_concurrent");
    println!("       4 workers x 50 allocs each (concurrent)");
    println!("       -> unexpected_failures (expect 0)");
    println!();
    println!("pressure result legend:");
    println!("  count               buffers allocated before ENOMEM / limit");
    println!("  total_mb            total allocated memory (count x alloc_size)");
    println!("  avg_latency_us      mean per-alloc latency (us)");
    println!("  released            buffers freed in recovery phase (50% of exhaust)");
    println!("  recovered           successful re-allocs after release");
    println!("  avg_recovery_us     mean re-alloc latency after release (us)");
    println!("  unexpected_failures non-ENOMEM errors in concurrent test (pass = 0)");
    println!();

    let tests: Vec<(&str, nix::Result<()>)> = vec![
        (
            "gradual_exhaust",
            test_gradual_exhaust(backend, heap_name, alloc_size, max_allocs),
        ),
        (
            "recovery",
            test_recovery(backend, heap_name, alloc_size, max_allocs),
        ),
        (
            "pressure_concurrent",
            test_pressure_concurrent(backend, heap_name, alloc_size),
        ),
    ];

    runner::collect_test_results("pressure", &tests)
}

/// Absolute upper bound on exhaust allocations.
const MAX_EXHAUST_ALLOCS: usize = 10_000;

/// Conservative fallback when `/proc/meminfo` is unavailable (e.g. macOS).
const DEFAULT_EXHAUST_LIMIT: usize = 500;

/// Fraction of available memory to use for exhaust testing (1/4 = 25%).
const MEM_USAGE_FRACTION: u64 = 4;

/// Calculate a safe exhaust allocation limit based on available memory.
#[allow(clippy::cast_possible_truncation)]
fn safe_exhaust_limit(alloc_size: u64) -> usize {
    if let Ok(meminfo) = crate::procfs::read_meminfo() {
        let available_bytes = meminfo.mem_available_kb * 1024;
        let usable = available_bytes / MEM_USAGE_FRACTION;
        let limit = (usable / alloc_size) as usize;
        let clamped = limit.clamp(1, MAX_EXHAUST_ALLOCS);
        tracing::debug!(
            mem_available_kb = meminfo.mem_available_kb,
            alloc_size,
            limit = clamped,
            "dynamic exhaust limit"
        );
        return clamped;
    }
    tracing::debug!(limit = DEFAULT_EXHAUST_LIMIT, "using default exhaust limit");
    DEFAULT_EXHAUST_LIMIT
}

/// Allocate fixed-size buffers until `ENOMEM`. Track latency and total allocated.
#[allow(clippy::cast_possible_truncation)]
fn test_gradual_exhaust<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    alloc_size: u64,
    max_allocs: usize,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut buffers: Vec<DmaBuf<'_, B>> = Vec::new();
    let mut latencies_us: Vec<u64> = Vec::new();

    loop {
        if buffers.len() >= max_allocs {
            tracing::warn!(
                count = buffers.len(),
                "exhaust limit reached without ENOMEM"
            );
            break;
        }
        let start = Instant::now();
        match heap.alloc(
            alloc_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        ) {
            Ok(fd) => {
                let elapsed = start.elapsed().as_micros() as u64;
                latencies_us.push(elapsed);
                buffers.push(DmaBuf::new(backend, fd, alloc_size as usize));
            }
            Err(Errno::ENOMEM) => break,
            Err(e) => return Err(e),
        }
    }

    let total_bytes = buffers.len() as u64 * alloc_size;
    let avg_latency = if latencies_us.is_empty() {
        0
    } else {
        latencies_us.iter().sum::<u64>() / latencies_us.len() as u64
    };

    tracing::info!(
        count = buffers.len(),
        total_mb = total_bytes / (1024 * 1024),
        avg_latency_us = avg_latency,
        "gradual_exhaust"
    );

    // Clean up all buffers (Drop handles close).
    drop(buffers);
    Ok(())
}

/// After exhausting allocations, close 50% and verify recovery.
#[allow(clippy::cast_possible_truncation)]
fn test_recovery<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    alloc_size: u64,
    max_allocs: usize,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut buffers: Vec<DmaBuf<'_, B>> = Vec::new();

    // Exhaust until ENOMEM (with safety limit).
    loop {
        if buffers.len() >= max_allocs {
            break;
        }
        match heap.alloc(
            alloc_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        ) {
            Ok(fd) => buffers.push(DmaBuf::new(backend, fd, alloc_size as usize)),
            Err(Errno::ENOMEM) => break,
            Err(e) => return Err(e),
        }
    }

    let total_before = buffers.len();
    if total_before == 0 {
        tracing::warn!("no buffers allocated before ENOMEM");
        return Ok(());
    }

    // Close 50% of buffers (every other one).
    let release_count = total_before / 2;
    for _ in 0..release_count {
        buffers.pop();
    }

    // Attempt reallocation after release.
    let mut recovered = 0u32;
    let mut recovery_latencies: Vec<u64> = Vec::new();

    for _ in 0..release_count {
        let start = Instant::now();
        match heap.alloc(
            alloc_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        ) {
            Ok(fd) => {
                let elapsed = start.elapsed().as_micros() as u64;
                recovery_latencies.push(elapsed);
                buffers.push(DmaBuf::new(backend, fd, alloc_size as usize));
                recovered += 1;
            }
            Err(Errno::ENOMEM) => break,
            Err(e) => return Err(e),
        }
    }

    let avg_recovery = if recovery_latencies.is_empty() {
        0
    } else {
        recovery_latencies.iter().sum::<u64>() / recovery_latencies.len() as u64
    };

    tracing::info!(
        total_before,
        released = release_count,
        recovered,
        avg_recovery_us = avg_recovery,
        "recovery"
    );

    drop(buffers);
    Ok(())
}

/// Concurrent alloc under memory pressure from worker threads.
#[allow(clippy::cast_possible_truncation)]
fn test_pressure_concurrent<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    alloc_size: u64,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let worker_count = 4u32;
    let allocs_per_worker = 50u32;

    let fail_count = std::sync::atomic::AtomicUsize::new(0);
    let fail_ref = &fail_count;
    let heap_ref = &heap;

    std::thread::scope(|s| {
        for worker_id in 0..worker_count {
            s.spawn(move || {
                let mut bufs: Vec<DmaBuf<'_, B>> = Vec::new();
                for _ in 0..allocs_per_worker {
                    match heap_ref.alloc(
                        alloc_size,
                        DMA_HEAP_ALLOC_FD_FLAGS,
                        DMA_HEAP_VALID_HEAP_FLAGS,
                    ) {
                        Ok(fd) => {
                            bufs.push(DmaBuf::new(backend, fd, alloc_size as usize));
                        }
                        Err(Errno::ENOMEM) => break,
                        Err(_) => {
                            fail_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
                tracing::debug!(worker_id, allocated = bufs.len(), "worker done");
                drop(bufs);
            });
        }
    });

    let failures = fail_count.load(std::sync::atomic::Ordering::Relaxed);
    tracing::info!(
        workers = worker_count,
        allocs_per_worker,
        unexpected_failures = failures,
        "pressure_concurrent"
    );

    if failures > 0 {
        return Err(Errno::EIO);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn gradual_exhaust_hits_limit() {
        // Use small alloc size + conservative limit to avoid host OOM.
        let b = MockBackend::new();
        test_gradual_exhaust(&b, "system", 4096, 500).unwrap();
    }

    #[test]
    fn recovery_after_exhaust() {
        let b = MockBackend::new();
        test_recovery(&b, "system", 4096, 500).unwrap();
    }

    #[test]
    fn pressure_concurrent_runs() {
        let b = MockBackend::new();
        test_pressure_concurrent(&b, "system", 4096).unwrap();
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        let (results, err) = run(&b, "system", 4096, None);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert_eq!(results.len(), 3);
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn exhaust_count() {
        let b = MockBackend::new();
        let heap = DmaHeap::open(&b, "system").unwrap();
        let alloc_size: u64 = 4096;
        let mut count = 0u32;
        let mut bufs = Vec::new();
        for _ in 0..100 {
            match heap.alloc(
                alloc_size,
                DMA_HEAP_ALLOC_FD_FLAGS,
                DMA_HEAP_VALID_HEAP_FLAGS,
            ) {
                Ok(fd) => {
                    count += 1;
                    bufs.push(DmaBuf::new(&b, fd, alloc_size as usize));
                }
                Err(Errno::ENOMEM) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(count > 0, "should allocate at least some buffers");
        drop(bufs);
    }

    #[test]
    fn recovery_no_leak() {
        let b = MockBackend::new();
        test_recovery(&b, "system", 4096, 500).unwrap();
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }
}
