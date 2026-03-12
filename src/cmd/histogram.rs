// Latency histogram analysis: per-heap, per-size distribution with ASCII
// visualization and extended percentiles.

use std::error::Error;
use std::time::Instant;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::cli::HistMode;
use crate::cmd::perf::{self, percentile};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::SubTestResult;

/// Run histogram analysis across all (heap, size) combinations.
#[allow(clippy::cast_possible_truncation, clippy::too_many_arguments)]
pub fn run<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heaps: &[String],
    sizes: &[u64],
    samples: u32,
    warmup: u32,
    mode: HistMode,
    buckets: usize,
) -> (Vec<SubTestResult>, Option<Box<dyn Error>>) {
    let bucket_count = if buckets == 0 {
        auto_buckets(samples as usize)
    } else {
        buckets
    };
    let bucket_source = if buckets == 0 { "auto" } else { "manual" };

    print_sequence(
        heaps,
        sizes,
        mode,
        samples,
        warmup,
        bucket_count,
        bucket_source,
    );

    let mut results = Vec::new();

    for heap_name in heaps {
        let heap = match DmaHeap::open(backend, heap_name) {
            Ok(h) => h,
            Err(e) => {
                results.push(SubTestResult {
                    name: format!("{heap_name}@open"),
                    passed: false,
                    error: Some(e.to_string()),
                });
                return (results, Some(Box::new(e)));
            }
        };

        for &size in sizes {
            let test_name = format!("{heap_name}@{size}");
            match collect_and_print(
                backend,
                &heap,
                heap_name,
                size,
                samples,
                warmup,
                mode,
                bucket_count,
            ) {
                Ok(()) => {
                    results.push(SubTestResult {
                        name: test_name,
                        passed: true,
                        error: None,
                    });
                }
                Err(e) => {
                    let err_str = e.to_string();
                    results.push(SubTestResult {
                        name: test_name,
                        passed: false,
                        error: Some(err_str.clone()),
                    });
                    return (results, Some(Box::<dyn Error>::from(err_str)));
                }
            }
        }
    }

    (results, None)
}

/// Print startup sequence diagram and legend.
fn print_sequence(
    heaps: &[String],
    sizes: &[u64],
    mode: HistMode,
    samples: u32,
    warmup: u32,
    bucket_count: usize,
    bucket_source: &str,
) {
    let heap_list: Vec<&str> = heaps.iter().map(String::as_str).collect();
    println!("histogram sequence:");
    println!("  heaps: [{}]", heap_list.join(", "));
    println!("  sizes: {sizes:?} bytes");
    println!("  mode: {}", mode.as_str());
    println!("  samples: {samples} (warmup: {warmup})");
    println!("  buckets: {bucket_count} ({bucket_source})");
    println!();
    println!("  for each (heap, size):");
    println!("    warmup({warmup}) -> collect({samples}) -> sort -> bucket -> print");
    println!();
    println!("histogram legend:");
    println!("  range_us    latency bucket range [low, high) in microseconds");
    println!("  count       samples in this bucket");
    println!("  pct         percentage of total samples");
    println!("  cum_pct     cumulative percentage");
    println!("  bar         visual distribution (each # ~ 2%)");
    println!();
}

/// Collect samples for one (heap, size) combination, then print histogram.
#[allow(clippy::cast_possible_truncation, clippy::too_many_arguments)]
fn collect_and_print<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap: &DmaHeap<'_, B>,
    heap_name: &str,
    size: u64,
    samples: u32,
    warmup: u32,
    mode: HistMode,
    bucket_count: usize,
) -> nix::Result<()> {
    // Warmup
    for _ in 0..warmup {
        run_alloc_pipeline(backend, heap, size, mode)?;
    }

    // Collect
    let latencies = match mode {
        HistMode::CloseOnly => collect_close_only(backend, heap, size, samples)?,
        _ => collect_timed(backend, heap, size, samples, mode)?,
    };

    print_histogram(heap_name, size, mode, &latencies, bucket_count);
    Ok(())
}

/// Collect latencies for alloc-only or full-pipeline modes.
#[allow(clippy::cast_possible_truncation)]
fn collect_timed<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap: &DmaHeap<'_, B>,
    size: u64,
    samples: u32,
    mode: HistMode,
) -> nix::Result<Vec<u64>> {
    let mut latencies = Vec::with_capacity(samples as usize);
    for _ in 0..samples {
        let start = Instant::now();
        run_alloc_pipeline(backend, heap, size, mode)?;
        let elapsed = start.elapsed().as_micros() as u64;
        latencies.push(elapsed);
    }
    Ok(latencies)
}

/// Collect close-only latencies: alloc outside timing, measure only drop.
#[allow(clippy::cast_possible_truncation)]
fn collect_close_only<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap: &DmaHeap<'_, B>,
    size: u64,
    samples: u32,
) -> nix::Result<Vec<u64>> {
    let mut latencies = Vec::with_capacity(samples as usize);
    for _ in 0..samples {
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let buf = DmaBuf::new(backend, fd, size as usize);
        let start = Instant::now();
        drop(buf);
        let elapsed = start.elapsed().as_micros() as u64;
        latencies.push(elapsed);
    }
    Ok(latencies)
}

/// Execute one alloc + pipeline iteration (used for warmup and timed collection).
#[allow(clippy::cast_possible_truncation)]
fn run_alloc_pipeline<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap: &DmaHeap<'_, B>,
    size: u64,
    mode: HistMode,
) -> nix::Result<()> {
    let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    match mode {
        HistMode::AllocOnly | HistMode::CloseOnly => {
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }
        HistMode::FullPipeline => {
            let mut buf = DmaBuf::new(backend, fd, size as usize);
            let ptr = buf.mmap()?;
            buf.sync_start(DMA_BUF_SYNC_WRITE)?;
            // SAFETY: ptr is valid for `size` bytes from mmap.
            unsafe {
                super::aging::sparse_fill(ptr, size as usize, 0xAA, None);
            }
            buf.sync_end(DMA_BUF_SYNC_WRITE)?;
            buf.sync_start(DMA_BUF_SYNC_READ)?;
            buf.sync_end(DMA_BUF_SYNC_READ)?;
            drop(buf);
        }
    }
    Ok(())
}

/// Print ASCII histogram, extended percentiles, and summary for one dataset.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn print_histogram(
    heap_name: &str,
    size: u64,
    mode: HistMode,
    samples: &[u64],
    bucket_count: usize,
) {
    if samples.is_empty() {
        println!(
            "--- {heap_name} @ {size} bytes ({}) --- (no samples)",
            mode.as_str()
        );
        println!();
        return;
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();

    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    let count = sorted.len();

    println!("--- {heap_name} @ {size} bytes ({}) ---", mode.as_str());
    println!();

    // Build buckets
    let buckets = build_buckets(&sorted, min, max, bucket_count);

    // Find widths for alignment
    let max_count = buckets.iter().map(|b| b.count).max().unwrap_or(0);
    let count_width = max_count.to_string().len().max(5);
    let val_width = max.to_string().len().max(3);

    let mut cum_pct = 0.0_f64;
    for b in &buckets {
        let pct = b.count as f64 / count as f64 * 100.0;
        cum_pct += pct;
        let bar_len = if b.count > 0 {
            ((pct / 2.0).ceil() as usize).max(1)
        } else {
            0
        };
        let bar: String = "#".repeat(bar_len);

        if b.is_last {
            println!(
                "  [{:>w$}, {:>w2$}] | {:>cw$} {:>5.1}% {:>5.1}% {bar}",
                b.low,
                max,
                b.count,
                pct,
                cum_pct,
                w = val_width,
                w2 = val_width,
                cw = count_width,
            );
        } else {
            println!(
                "  [{:>w$}, {:>w2$}) | {:>cw$} {:>5.1}% {:>5.1}% {bar}",
                b.low,
                b.high,
                b.count,
                pct,
                cum_pct,
                w = val_width,
                w2 = val_width,
                cw = count_width,
            );
        }
    }

    // Extended percentiles
    println!();
    println!("  percentiles (us):");
    println!(
        "    p1={} p5={} p10={} p25={} p50={} p75={} p90={} p95={} p99={} p99.9={}",
        percentile(&sorted, 1),
        percentile(&sorted, 5),
        percentile(&sorted, 10),
        percentile(&sorted, 25),
        percentile(&sorted, 50),
        percentile(&sorted, 75),
        percentile(&sorted, 90),
        percentile(&sorted, 95),
        percentile(&sorted, 99),
        percentile_frac(&sorted, 999, 1000),
    );

    // Summary
    if let Some(stats) = perf::compute_stats(samples) {
        println!(
            "  summary: count={} min={} avg={} max={}",
            stats.count, stats.min_us, stats.avg_us, stats.max_us,
        );
    }
    println!();
}

/// Bucket descriptor for histogram rendering.
struct Bucket {
    low: u64,
    high: u64,
    count: usize,
    is_last: bool,
}

/// Build histogram buckets from sorted samples.
#[allow(clippy::cast_possible_truncation)]
fn build_buckets(sorted: &[u64], min: u64, max: u64, bucket_count: usize) -> Vec<Bucket> {
    if min == max {
        return vec![Bucket {
            low: min,
            high: max,
            count: sorted.len(),
            is_last: true,
        }];
    }

    let range = max - min + 1;
    // Cap bucket count to range so we don't create empty trailing buckets.
    let actual_buckets = (bucket_count as u64).min(range) as usize;
    let width = (range / actual_buckets as u64).max(1);

    let mut buckets = Vec::with_capacity(actual_buckets);
    let mut idx = 0;

    for i in 0..actual_buckets {
        let low = min + i as u64 * width;
        let is_last = i == actual_buckets - 1;
        let high = if is_last { max + 1 } else { low + width };

        let mut count = 0;
        while idx < sorted.len() && (is_last || sorted[idx] < high) {
            if sorted[idx] >= low {
                count += 1;
            }
            if !is_last && sorted[idx] >= high {
                break;
            }
            idx += 1;
        }

        buckets.push(Bucket {
            low,
            high,
            count,
            is_last,
        });
    }

    buckets
}

/// Compute the p-th fractional percentile (e.g., 999/1000 = p99.9).
fn percentile_frac(sorted: &[u64], numer: u64, denom: u64) -> u64 {
    let n = sorted.len() as u64;
    #[allow(clippy::cast_possible_truncation)]
    let rank = (numer * n).div_ceil(denom) as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Auto-determine bucket count via Sturges' rule: ceil(1 + log2(n)).
fn auto_buckets(n: usize) -> usize {
    if n <= 1 {
        return 1;
    }
    let log2 = (usize::BITS - n.leading_zeros()) as usize;
    (1 + log2).max(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn auto_buckets_values() {
        assert_eq!(auto_buckets(1), 1);
        assert_eq!(auto_buckets(2), 4);
        assert_eq!(auto_buckets(1000), 11);
        assert_eq!(auto_buckets(10_000), 15);
    }

    #[test]
    fn build_buckets_single_value() {
        let sorted = vec![42; 10];
        let buckets = build_buckets(&sorted, 42, 42, 5);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].count, 10);
        assert!(buckets[0].is_last);
    }

    #[test]
    fn build_buckets_spread() {
        let sorted: Vec<u64> = (0..100).collect();
        let buckets = build_buckets(&sorted, 0, 99, 10);
        assert_eq!(buckets.len(), 10);
        let total: usize = buckets.iter().map(|b| b.count).sum();
        assert_eq!(total, 100);
        assert!(buckets.last().unwrap().is_last);
    }

    #[test]
    fn percentile_frac_p999() {
        let sorted: Vec<u64> = (1..=1000).collect();
        let p999 = percentile_frac(&sorted, 999, 1000);
        assert_eq!(p999, 999);
    }

    #[test]
    fn run_alloc_only() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string()];
        let (results, err) = run(&b, &heaps, &[4096], 100, 10, HistMode::AllocOnly, 0);
        assert!(err.is_none());
        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn run_full_pipeline() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string()];
        let (results, err) = run(&b, &heaps, &[4096], 50, 5, HistMode::FullPipeline, 5);
        assert!(err.is_none());
        assert!(results[0].passed);
    }

    #[test]
    fn run_close_only() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string()];
        let (results, err) = run(&b, &heaps, &[4096], 50, 5, HistMode::CloseOnly, 0);
        assert!(err.is_none());
        assert!(results[0].passed);
    }

    #[test]
    fn run_multi_size() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string()];
        let sizes = [4096, 65536, 1_048_576];
        let (results, err) = run(&b, &heaps, &sizes, 20, 5, HistMode::AllocOnly, 0);
        assert!(err.is_none());
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.passed));
    }

    #[test]
    fn run_multi_heap() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string(), "reserved".to_string()];
        let (results, err) = run(&b, &heaps, &[4096], 20, 5, HistMode::AllocOnly, 0);
        assert!(err.is_none());
        assert_eq!(results.len(), 2);
    }
}
