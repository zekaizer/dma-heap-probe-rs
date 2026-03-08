// Stage 3 performance tests: alloc latency, full pipeline, close, order boundary,
// fallback path, and internal fragmentation measurement.

use std::error::Error;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};

/// Page size for alignment calculations.
const PAGE_SIZE: u64 = 4096;

/// Default sizes for performance measurement.
const DEFAULT_SIZES: &[u64] = &[4096, 65536, 1_048_576];

/// Sizes for order boundary sweep (around 64K boundary).
const ORDER_BOUNDARY_SIZES: &[u64] = &[
    4096, 8192, 16384, 32768, 49152, 61440, 65536, 69632, 131_072, 262_144, 524_288, 1_048_576,
    2_097_152, 4_194_304, 8_388_608,
];

/// Sizes for internal fragmentation measurement.
const FRAG_SIZES: &[u64] = &[1, 4095, 4097, 65535, 65537, 100_000];

/// Latency statistics for a benchmark run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LatencyStats {
    pub count: usize,
    pub min_us: u64,
    pub max_us: u64,
    pub avg_us: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
}

/// Compute latency statistics from a slice of microsecond measurements.
///
/// Returns `None` if the slice is empty.
pub fn compute_stats(samples: &[u64]) -> Option<LatencyStats> {
    if samples.is_empty() {
        return None;
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();

    let count = sorted.len();
    let sum: u64 = sorted.iter().sum();

    Some(LatencyStats {
        count,
        min_us: sorted[0],
        max_us: sorted[count - 1],
        avg_us: sum / count as u64,
        p50_us: percentile(&sorted, 50),
        p95_us: percentile(&sorted, 95),
        p99_us: percentile(&sorted, 99),
    })
}

/// Compute the p-th percentile from a sorted slice using nearest-rank method.
fn percentile(sorted: &[u64], p: u32) -> u64 {
    let n = sorted.len() as u64;
    #[allow(clippy::cast_possible_truncation)]
    let rank = (u64::from(p) * n).div_ceil(100) as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Round `size` up to the nearest page boundary.
fn page_align(size: u64) -> u64 {
    size.next_multiple_of(PAGE_SIZE)
}

/// Run all stage 3 performance tests.
/// Returns sub-test results (and the first error, if any).
#[allow(clippy::cast_possible_truncation)]
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: Option<&[u64]>,
    iterations: u32,
    warmup: u32,
) -> (Vec<SubTestResult>, Option<Box<dyn Error>>) {
    let sizes = sizes.unwrap_or(DEFAULT_SIZES);

    let tests: Vec<(&str, nix::Result<()>)> = vec![
        (
            "bench_alloc_only",
            bench_alloc_only(backend, heap_name, sizes, iterations, warmup),
        ),
        (
            "bench_full_pipeline",
            bench_full_pipeline(backend, heap_name, sizes, iterations, warmup),
        ),
        (
            "bench_close",
            bench_close(backend, heap_name, sizes, iterations, warmup),
        ),
        (
            "bench_order_boundary",
            bench_order_boundary(backend, heap_name, iterations, warmup),
        ),
        (
            "bench_internal_frag",
            bench_internal_frag(backend, heap_name),
        ),
    ];

    runner::collect_test_results("perf", &tests)
}

/// Benchmark alloc-only latency (ioctl call to fd return).
#[allow(clippy::cast_possible_truncation)]
fn bench_alloc_only<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    iterations: u32,
    warmup: u32,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        // Measure
        let mut samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let start = Instant::now();
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let elapsed = start.elapsed().as_micros() as u64;
            samples.push(elapsed);
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        if let Some(stats) = compute_stats(&samples) {
            tracing::info!(
                size,
                min_us = stats.min_us,
                avg_us = stats.avg_us,
                p50_us = stats.p50_us,
                p95_us = stats.p95_us,
                p99_us = stats.p99_us,
                max_us = stats.max_us,
                "alloc_only"
            );
        }
    }

    Ok(())
}

/// Benchmark full pipeline: alloc + mmap + sync(write) + write + sync(read) + unmap.
#[allow(clippy::cast_possible_truncation)]
fn bench_full_pipeline<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    iterations: u32,
    warmup: u32,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let mut buf = DmaBuf::new(backend, fd, size as usize);
            let ptr = buf.mmap()?;
            buf.sync_start(DMA_BUF_SYNC_WRITE)?;
            unsafe { std::ptr::write_bytes(ptr, 0xAA, size as usize) };
            buf.sync_end(DMA_BUF_SYNC_WRITE)?;
            drop(buf);
        }

        // Measure
        let mut samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let start = Instant::now();
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let mut buf = DmaBuf::new(backend, fd, size as usize);
            let ptr = buf.mmap()?;
            buf.sync_start(DMA_BUF_SYNC_WRITE)?;
            unsafe { std::ptr::write_bytes(ptr, 0xAA, size as usize) };
            buf.sync_end(DMA_BUF_SYNC_WRITE)?;
            buf.sync_start(DMA_BUF_SYNC_READ)?;
            buf.sync_end(DMA_BUF_SYNC_READ)?;
            let elapsed = start.elapsed().as_micros() as u64;
            samples.push(elapsed);
            drop(buf);
        }

        if let Some(stats) = compute_stats(&samples) {
            tracing::info!(
                size,
                avg_us = stats.avg_us,
                p50_us = stats.p50_us,
                p95_us = stats.p95_us,
                p99_us = stats.p99_us,
                "full_pipeline"
            );
        }
    }

    Ok(())
}

/// Benchmark close (release path) latency.
#[allow(clippy::cast_possible_truncation)]
fn bench_close<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    iterations: u32,
    warmup: u32,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        // Pre-alloc then measure close latency
        let mut samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            let start = Instant::now();
            drop(buf);
            let elapsed = start.elapsed().as_micros() as u64;
            samples.push(elapsed);
        }

        if let Some(stats) = compute_stats(&samples) {
            tracing::info!(
                size,
                avg_us = stats.avg_us,
                p50_us = stats.p50_us,
                p95_us = stats.p95_us,
                p99_us = stats.p99_us,
                "close"
            );
        }
    }

    Ok(())
}

/// Benchmark alloc latency across order-boundary sizes (4K to 8M).
#[allow(clippy::cast_possible_truncation)]
fn bench_order_boundary<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    iterations: u32,
    warmup: u32,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in ORDER_BOUNDARY_SIZES {
        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        // Measure
        let mut samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let start = Instant::now();
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let elapsed = start.elapsed().as_micros() as u64;
            samples.push(elapsed);
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        if let Some(stats) = compute_stats(&samples) {
            tracing::info!(
                size,
                avg_us = stats.avg_us,
                p50_us = stats.p50_us,
                p95_us = stats.p95_us,
                p99_us = stats.p99_us,
                "order_boundary"
            );
        }
    }

    Ok(())
}

/// Measure internal fragmentation: request unaligned sizes, check actual via `llseek`.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn bench_internal_frag<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in FRAG_SIZES {
        let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let buf = DmaBuf::new(backend, fd, size as usize);

        let actual = buf.llseek_size()?;
        #[allow(clippy::cast_possible_wrap)]
        let expected_aligned = page_align(size) as i64;
        #[allow(clippy::cast_precision_loss)]
        let frag_ratio = if size > 0 {
            (actual as f64 - size as f64) / size as f64 * 100.0
        } else {
            0.0
        };

        tracing::info!(
            requested = size,
            actual = actual,
            expected = expected_aligned,
            frag_pct = format!("{frag_ratio:.1}"),
            "internal_frag"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    // ── compute_stats tests ──

    #[test]
    fn stats_empty() {
        assert!(compute_stats(&[]).is_none());
    }

    #[test]
    fn stats_single() {
        let stats = compute_stats(&[42]).unwrap();
        assert_eq!(stats.count, 1);
        assert_eq!(stats.min_us, 42);
        assert_eq!(stats.max_us, 42);
        assert_eq!(stats.avg_us, 42);
        assert_eq!(stats.p50_us, 42);
        assert_eq!(stats.p95_us, 42);
        assert_eq!(stats.p99_us, 42);
    }

    #[test]
    fn stats_sorted_input() {
        let samples: Vec<u64> = (1..=100).collect();
        let stats = compute_stats(&samples).unwrap();
        assert_eq!(stats.count, 100);
        assert_eq!(stats.min_us, 1);
        assert_eq!(stats.max_us, 100);
        assert_eq!(stats.avg_us, 50); // (1+100)*100/2/100 = 50.5 → 50
        assert_eq!(stats.p50_us, 50);
        assert_eq!(stats.p95_us, 95);
        assert_eq!(stats.p99_us, 99);
    }

    #[test]
    fn stats_unsorted_input() {
        let samples = vec![100, 1, 50, 99, 2];
        let stats = compute_stats(&samples).unwrap();
        assert_eq!(stats.min_us, 1);
        assert_eq!(stats.max_us, 100);
        assert_eq!(stats.p50_us, 50);
    }

    #[test]
    fn stats_serde_roundtrip() {
        let stats = compute_stats(&[10, 20, 30, 40, 50]).unwrap();
        let json = serde_json::to_string(&stats).unwrap();
        let deserialized: LatencyStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, deserialized);
    }

    // ── percentile edge cases ──

    #[test]
    fn percentile_two_elements() {
        let sorted = vec![10, 20];
        assert_eq!(percentile(&sorted, 50), 10);
        assert_eq!(percentile(&sorted, 99), 20);
    }

    // ── bench function tests ──

    #[test]
    fn alloc_only_runs() {
        let b = MockBackend::new();
        bench_alloc_only(&b, "system", &[4096], 10, 2).unwrap();
    }

    #[test]
    fn full_pipeline_runs() {
        let b = MockBackend::new();
        bench_full_pipeline(&b, "system", &[4096], 10, 2).unwrap();
    }

    #[test]
    fn close_runs() {
        let b = MockBackend::new();
        bench_close(&b, "system", &[4096], 10, 2).unwrap();
    }

    #[test]
    fn order_boundary_runs() {
        let b = MockBackend::new();
        bench_order_boundary(&b, "system", 5, 1).unwrap();
    }

    #[test]
    fn internal_frag_runs() {
        let b = MockBackend::new();
        bench_internal_frag(&b, "system").unwrap();
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        let (results, err) = run(&b, "system", Some(&[4096]), 5, 1);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert_eq!(results.len(), 5);
    }
}
