// Stage 3 performance tests: alloc latency, full pipeline, close, order boundary,
// fallback path, and internal fragmentation measurement.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};

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
    pub stddev_us: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub p99_9_us: u64,
    /// Coefficient of variation (stddev / mean * 100). Lower is more stable.
    pub cv_pct: f64,
    /// Average after removing IQR outliers (1.5 * IQR fence).
    /// Filters OS scheduler jitter and interrupt noise.
    pub trimmed_avg_us: u64,
    /// Number of samples identified as outliers by IQR method.
    pub outlier_count: usize,
    /// Throughput in operations per second (`1e6 / mean_us`). 0 if mean is 0.
    pub throughput_ops: u64,
    /// 95% confidence interval half-width: 1.96 * stddev / sqrt(n).
    /// True mean lies within [avg - ci95, avg + ci95] with 95% probability.
    pub ci95_us: u64,
}

/// Compute latency statistics from a slice of microsecond measurements.
///
/// Uses exact floating-point mean for variance calculation, then rounds for
/// integer fields. Population stddev (divide by N) is used since benchmark
/// iterations represent the full measurement, not a sample from a larger population.
///
/// Returns `None` if the slice is empty.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
pub fn compute_stats(samples: &[u64]) -> Option<LatencyStats> {
    if samples.is_empty() {
        return None;
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();

    let count = sorted.len();
    let sum: u64 = sorted.iter().sum();
    let mean_f = sum as f64 / count as f64;

    // Population variance via two-pass algorithm (numerically stable for integer data).
    let variance = sorted
        .iter()
        .map(|&x| {
            let diff = x as f64 - mean_f;
            diff * diff
        })
        .sum::<f64>()
        / count as f64;
    let stddev = variance.sqrt();
    let cv = if mean_f > 0.0 {
        stddev / mean_f * 100.0
    } else {
        0.0
    };

    // IQR outlier detection: fence = [Q1 - 1.5*IQR, Q3 + 1.5*IQR]
    let (trimmed_avg, outlier_count) = iqr_trimmed_mean(&sorted, mean_f);

    // Throughput: ops/sec = 1e6 / mean_us
    let throughput = if mean_f > 0.0 {
        (1_000_000.0 / mean_f).round() as u64
    } else {
        0
    };

    // 95% CI half-width: 1.96 * stddev / sqrt(n)
    let ci95 = (1.96 * stddev / (count as f64).sqrt()).ceil() as u64;

    Some(LatencyStats {
        count,
        min_us: sorted[0],
        max_us: sorted[count - 1],
        avg_us: mean_f.round() as u64,
        stddev_us: stddev.round() as u64,
        p50_us: percentile(&sorted, 50),
        p95_us: percentile(&sorted, 95),
        p99_us: percentile(&sorted, 99),
        p99_9_us: percentile_frac(&sorted, 999, 1000),
        cv_pct: (cv * 10.0).round() / 10.0,
        trimmed_avg_us: trimmed_avg,
        outlier_count,
        throughput_ops: throughput,
        ci95_us: ci95,
    })
}

/// Compute the p-th percentile from a sorted slice using nearest-rank method.
pub(crate) fn percentile(sorted: &[u64], p: u32) -> u64 {
    let n = sorted.len() as u64;
    #[allow(clippy::cast_possible_truncation)]
    let rank = (u64::from(p) * n).div_ceil(100) as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Compute the fractional percentile (e.g., 999/1000 = p99.9) from a sorted slice.
pub(crate) fn percentile_frac(sorted: &[u64], numer: u64, denom: u64) -> u64 {
    let n = sorted.len() as u64;
    #[allow(clippy::cast_possible_truncation)]
    let rank = (numer * n).div_ceil(denom) as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Compute trimmed mean by removing IQR outliers from a sorted slice.
///
/// Uses Tukey's fence: outliers are values outside `[Q1 - 1.5*IQR, Q3 + 1.5*IQR]`.
/// Returns `(trimmed_avg_us, outlier_count)`. Falls back to `raw_mean` if all
/// samples are outliers (degenerate case).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
fn iqr_trimmed_mean(sorted: &[u64], raw_mean: f64) -> (u64, usize) {
    let n = sorted.len();
    if n < 4 {
        // Too few samples for meaningful IQR; return raw mean.
        return (raw_mean.round() as u64, 0);
    }

    let q1 = percentile(sorted, 25) as f64;
    let q3 = percentile(sorted, 75) as f64;
    let iqr = q3 - q1;
    let lower = q1 - 1.5 * iqr;
    let upper = q3 + 1.5 * iqr;

    let mut sum = 0.0_f64;
    let mut kept = 0_usize;
    for &v in sorted {
        let vf = v as f64;
        if vf >= lower && vf <= upper {
            sum += vf;
            kept += 1;
        }
    }

    if kept == 0 {
        return (raw_mean.round() as u64, n);
    }

    let trimmed_avg = (sum / kept as f64).round() as u64;
    (trimmed_avg, n - kept)
}

use crate::probe::align_to;

/// Format throughput as Kops/s (thousands of operations per second).
fn format_throughput(ops: u64) -> String {
    if ops >= 1000 {
        format!("{}", ops / 1000)
    } else {
        format!("0.{}", ops / 100)
    }
}

/// Detect monotonic drift in measurement samples over time.
///
/// Fits a linear regression of `(sample_index, latency)` and reports the total
/// drift as a percentage of the mean. Positive drift = degradation over time
/// (thermal throttling, memory pressure), negative = warmup still settling.
///
/// Returns `None` if fewer than 10 samples (insufficient for meaningful trend).
#[allow(clippy::cast_precision_loss)]
fn detect_drift(samples: &[u64]) -> Option<DriftInfo> {
    let n = samples.len();
    if n < 10 {
        return None;
    }

    let n_f = n as f64;
    // x values are 0..n-1 (sample indices).
    // mean_x = (n-1)/2, sum_x = n*(n-1)/2
    let mean_x = (n_f - 1.0) / 2.0;
    let mean_y: f64 = samples.iter().map(|&v| v as f64).sum::<f64>() / n_f;

    if mean_y == 0.0 {
        return Some(DriftInfo {
            drift_pct: 0.0,
            first_half_avg_us: 0,
            second_half_avg_us: 0,
        });
    }

    // SS_xx for indices 0..n-1: n*(n-1)*(2n-1)/6 - n*(n-1)^2/4
    // Simplified: n*(n^2-1)/12
    let ss_xx = n_f * (n_f * n_f - 1.0) / 12.0;

    let ss_cross: f64 = samples
        .iter()
        .enumerate()
        .map(|(i, &v)| (i as f64 - mean_x) * (v as f64 - mean_y))
        .sum();

    let slope = ss_cross / ss_xx;

    // Total drift as percentage of mean: slope * (n-1) / mean * 100
    let total_drift = slope * (n_f - 1.0);
    let drift_pct = (total_drift / mean_y * 100.0 * 10.0).round() / 10.0;

    // Half-split averages for intuitive comparison.
    let mid = n / 2;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let first_half_avg =
        (samples[..mid].iter().map(|&v| v as f64).sum::<f64>() / mid as f64).round() as u64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let second_half_avg =
        (samples[mid..].iter().map(|&v| v as f64).sum::<f64>() / (n - mid) as f64).round() as u64;

    Some(DriftInfo {
        drift_pct,
        first_half_avg_us: first_half_avg,
        second_half_avg_us: second_half_avg,
    })
}

/// Measurement drift diagnostic info.
struct DriftInfo {
    /// Total drift as percentage of mean. Positive = degradation, negative = warmup settling.
    drift_pct: f64,
    /// Average latency of first half of samples.
    first_half_avg_us: u64,
    /// Average latency of second half of samples.
    second_half_avg_us: u64,
}

/// Result of ordinary least-squares linear regression: `y = intercept + slope * x`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearFit {
    /// Fixed base cost (y-intercept), in microseconds.
    pub intercept_us: f64,
    /// Marginal cost per byte (slope), in microseconds/byte.
    pub slope_us_per_byte: f64,
    /// Coefficient of determination (0.0–1.0). Values near 1.0 indicate
    /// strong linear relationship between size and latency.
    pub r_squared: f64,
}

/// Fit a simple linear model `latency_us = a + b * size_bytes` via OLS.
///
/// Requires at least 2 data points. Returns `None` if fewer or if all x values
/// are identical (zero variance in predictor).
#[allow(clippy::cast_precision_loss)]
fn linear_regression(points: &[(u64, u64)]) -> Option<LinearFit> {
    let n = points.len();
    if n < 2 {
        return None;
    }

    let n_f = n as f64;
    let sum_x: f64 = points.iter().map(|&(x, _)| x as f64).sum();
    let sum_y: f64 = points.iter().map(|&(_, y)| y as f64).sum();
    let mean_x = sum_x / n_f;
    let mean_y = sum_y / n_f;

    let mut ss_xx = 0.0_f64;
    let mut ss_cross = 0.0_f64;
    let mut ss_tot = 0.0_f64;
    for &(x, y) in points {
        let dx = x as f64 - mean_x;
        let dy = y as f64 - mean_y;
        ss_xx += dx * dx;
        ss_cross += dx * dy;
        ss_tot += dy * dy;
    }

    if ss_xx == 0.0 {
        return None; // all sizes identical
    }

    let slope = ss_cross / ss_xx;
    let intercept = mean_y - slope * mean_x;

    // R² = 1 - SS_res / SS_tot
    let ss_res: f64 = points
        .iter()
        .map(|&(x, y)| {
            let predicted = intercept + slope * x as f64;
            let residual = y as f64 - predicted;
            residual * residual
        })
        .sum();
    let r_squared = if ss_tot > 0.0 {
        1.0 - ss_res / ss_tot
    } else {
        1.0 // all y identical → perfect fit
    };

    Some(LinearFit {
        intercept_us: (intercept * 100.0).round() / 100.0,
        slope_us_per_byte: (slope * 1e9).round() / 1e9, // 9 decimal places
        r_squared: (r_squared * 1000.0).round() / 1000.0,
    })
}

/// Format a byte size as a human-readable string (e.g., 4096 → "4K", 1048576 → "1M").
///
/// Falls back to raw bytes for non-aligned sizes.
fn human_size(bytes: u64) -> String {
    if bytes >= 1_048_576 && bytes.is_multiple_of(1_048_576) {
        format!("{}M", bytes / 1_048_576)
    } else if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{}K", bytes / 1024)
    } else {
        bytes.to_string()
    }
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
    heap_w: usize,
) -> (Vec<SubTestResult>, Option<anyhow::Error>) {
    let sizes = sizes.unwrap_or(DEFAULT_SIZES);

    tracing::debug!(
        heap = heap_name,
        ?sizes,
        iterations,
        warmup,
        "perf sequence"
    );

    let caps = crate::probe::probe_heap(backend, heap_name);

    let tests: Vec<(&str, nix::Result<()>, bool)> = vec![
        (
            "bench_alloc_only",
            bench_alloc_only(backend, heap_name, sizes, iterations, warmup, heap_w),
            false,
        ),
        (
            "bench_full_pipeline",
            if caps.can_mmap {
                bench_full_pipeline(backend, heap_name, sizes, iterations, warmup, heap_w)
            } else {
                Ok(())
            },
            !caps.can_mmap,
        ),
        (
            "bench_close",
            bench_close(backend, heap_name, sizes, iterations, warmup, heap_w),
            false,
        ),
        (
            "bench_order_boundary",
            bench_order_boundary(backend, heap_name, iterations, warmup, heap_w),
            false,
        ),
        (
            "bench_internal_frag",
            bench_internal_frag(backend, heap_name, heap_w, caps.alloc_granularity),
            false,
        ),
        (
            "bench_pool_warmup",
            bench_pool_warmup(backend, heap_name, heap_w),
            false,
        ),
        (
            "bench_size_switch",
            bench_size_switch(backend, heap_name, heap_w),
            false,
        ),
    ];

    runner::collect_test_results("perf", heap_name, heap_w, &tests)
}

/// Benchmark alloc-only latency (ioctl call to fd return).
#[allow(clippy::cast_possible_truncation)]
fn bench_alloc_only<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    iterations: u32,
    warmup: u32,
    heap_w: usize,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut regression_points: Vec<(u64, u64)> = Vec::new();
    let mut last_samples: Vec<u64> = Vec::new();

    for &size in sizes {
        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        // Measure
        let mut samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let start = Instant::now();
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let elapsed = start.elapsed().as_micros() as u64;
            samples.push(elapsed);
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        if let Some(stats) = compute_stats(&samples) {
            regression_points.push((size, stats.avg_us));
            last_samples = samples;
            rows.push(vec![
                human_size(size),
                stats.min_us.to_string(),
                format!("{}±{}", stats.avg_us, stats.ci95_us),
                stats.trimmed_avg_us.to_string(),
                stats.stddev_us.to_string(),
                stats.p50_us.to_string(),
                stats.p95_us.to_string(),
                stats.p99_us.to_string(),
                stats.p99_9_us.to_string(),
                stats.max_us.to_string(),
                format_throughput(stats.throughput_ops),
                stats.outlier_count.to_string(),
            ]);
        }
    }

    crate::fmt::print_table(
        heap_name,
        heap_w,
        "perf::alloc_only",
        Some("(us)"),
        &[
            "size",
            "min",
            "avg±ci95",
            "tavg",
            "sd",
            "p50",
            "p95",
            "p99",
            "p99.9",
            "max",
            "Kops/s",
            "out",
        ],
        &rows,
    );

    // Size-latency regression: latency_us = base + slope * size_bytes
    if let Some(fit) = linear_regression(&regression_points) {
        crate::fmt::print_metric(
            heap_name,
            heap_w,
            "perf::alloc_model",
            &[
                ("base_us", &format!("{:.1}", fit.intercept_us)),
                ("us/KB", &format!("{:.3}", fit.slope_us_per_byte * 1024.0)),
                ("R\u{b2}", &format!("{:.3}", fit.r_squared)),
            ],
        );
    }

    // Drift detection on the last (largest) size — most sensitive to thermal/pressure effects.
    if let Some(drift) = detect_drift(&last_samples).filter(|d| d.drift_pct.abs() > 10.0) {
        let direction = if drift.drift_pct > 0.0 {
            "degrading"
        } else {
            "improving"
        };
        crate::fmt::print_metric(
            heap_name,
            heap_w,
            &format!("perf::drift_warn ({direction})"),
            &[
                ("drift", &format!("{:+.1}%", drift.drift_pct)),
                ("1st_half", &drift.first_half_avg_us),
                ("2nd_half", &drift.second_half_avg_us),
            ],
        );
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
    heap_w: usize,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut rows: Vec<Vec<String>> = Vec::new();

    for &size in sizes {
        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
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
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
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
            rows.push(vec![
                human_size(size),
                stats.avg_us.to_string(),
                stats.stddev_us.to_string(),
                stats.p50_us.to_string(),
                stats.p95_us.to_string(),
                stats.p99_us.to_string(),
                stats.p99_9_us.to_string(),
            ]);
        }
    }

    crate::fmt::print_table(
        heap_name,
        heap_w,
        "perf::full_pipeline",
        Some("(us)"),
        &["size", "avg", "sd", "p50", "p95", "p99", "p99.9"],
        &rows,
    );
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
    heap_w: usize,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut rows: Vec<Vec<String>> = Vec::new();

    for &size in sizes {
        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        // Pre-alloc then measure close latency
        let mut samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            let start = Instant::now();
            drop(buf);
            let elapsed = start.elapsed().as_micros() as u64;
            samples.push(elapsed);
        }

        if let Some(stats) = compute_stats(&samples) {
            rows.push(vec![
                human_size(size),
                stats.avg_us.to_string(),
                stats.stddev_us.to_string(),
                stats.p50_us.to_string(),
                stats.p95_us.to_string(),
                stats.p99_us.to_string(),
                stats.p99_9_us.to_string(),
            ]);
        }
    }

    crate::fmt::print_table(
        heap_name,
        heap_w,
        "perf::close",
        Some("(us)"),
        &["size", "avg", "sd", "p50", "p95", "p99", "p99.9"],
        &rows,
    );
    Ok(())
}

/// Benchmark alloc latency across order-boundary sizes (4K to 8M).
#[allow(clippy::cast_possible_truncation)]
fn bench_order_boundary<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    iterations: u32,
    warmup: u32,
    heap_w: usize,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut rows: Vec<Vec<String>> = Vec::new();

    for &size in ORDER_BOUNDARY_SIZES {
        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        // Measure
        let mut samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let start = Instant::now();
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let elapsed = start.elapsed().as_micros() as u64;
            samples.push(elapsed);
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        if let Some(stats) = compute_stats(&samples) {
            rows.push(vec![
                human_size(size),
                stats.avg_us.to_string(),
                stats.stddev_us.to_string(),
                stats.p50_us.to_string(),
                stats.p95_us.to_string(),
                stats.p99_us.to_string(),
                stats.p99_9_us.to_string(),
            ]);
        }
    }

    crate::fmt::print_table(
        heap_name,
        heap_w,
        "perf::order_boundary",
        Some("(us)"),
        &["size", "avg", "sd", "p50", "p95", "p99", "p99.9"],
        &rows,
    );
    Ok(())
}

/// Default pool test buffer count.
const POOL_WARMUP_COUNT: u32 = 100;

/// Size for pool warmup test.
const POOL_WARMUP_SIZE: u64 = 65536; // 64 KB

/// Iterations for pool latency measurements.
const POOL_MEASURE_ITERS: u32 = 100;

/// Measure alloc latency and return samples in microseconds (for pool benchmarks).
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

/// Compare cold vs warm alloc latency to quantify pool/cache effect.
#[allow(clippy::cast_possible_truncation)]
fn bench_pool_warmup<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    heap_w: usize,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Cold: first N allocations.
    let cold_samples = measure_alloc_latency(backend, &heap, POOL_WARMUP_SIZE, POOL_MEASURE_ITERS)?;

    // Warm: alloc/close cycle to fill pool, then measure.
    for _ in 0..POOL_WARMUP_COUNT {
        let fd = heap.alloc(
            POOL_WARMUP_SIZE,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        )?;
        let buf = DmaBuf::new(backend, fd, POOL_WARMUP_SIZE as usize);
        drop(buf);
    }
    let warm_samples = measure_alloc_latency(backend, &heap, POOL_WARMUP_SIZE, POOL_MEASURE_ITERS)?;

    if let (Some(cold), Some(warm)) = (compute_stats(&cold_samples), compute_stats(&warm_samples)) {
        crate::fmt::print_metric(
            heap_name,
            heap_w,
            "perf::pool_warmup",
            &[
                ("cold_p50", &cold.p50_us),
                ("cold_p95", &cold.p95_us),
                ("warm_p50", &warm.p50_us),
                ("warm_p95", &warm.p95_us),
            ],
        );
    }

    Ok(())
}

/// Measure latency impact of switching allocation sizes.
#[allow(clippy::cast_possible_truncation)]
fn bench_size_switch<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    heap_w: usize,
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

    let switch_data: [(u32, u64, &[u64]); 3] = [
        (1, size_a, &phase1),
        (2, size_b, &phase2),
        (3, size_a, &phase3),
    ];
    let headers = &["ph", "size", "first10_p50", "last10_p50"];
    let mut rows: Vec<Vec<String>> = Vec::new();
    for (ph, size, samples) in &switch_data {
        if let (Some(first), Some(last)) = (first_10(samples), last_10(samples)) {
            rows.push(vec![
                ph.to_string(),
                human_size(*size),
                first.p50_us.to_string(),
                last.p50_us.to_string(),
            ]);
        }
    }
    crate::fmt::print_table(heap_name, heap_w, "perf::size_switch", None, headers, &rows);

    Ok(())
}

/// Measure internal fragmentation: request unaligned sizes, check actual via `llseek`.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn bench_internal_frag<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    heap_w: usize,
    granularity: u64,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut rows: Vec<Vec<String>> = Vec::new();

    for &size in FRAG_SIZES {
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let buf = DmaBuf::new(backend, fd, size as usize);

        let actual = buf.llseek_size()?;
        #[allow(clippy::cast_possible_wrap)]
        let expected_aligned = align_to(size, granularity) as i64;
        #[allow(clippy::cast_precision_loss)]
        let frag_pct = if size >= granularity {
            let ratio = (actual as f64 - size as f64) / size as f64 * 100.0;
            format!("{ratio:.1}")
        } else {
            // Sub-granularity requests: fragmentation is expected, mark as not meaningful.
            "*".to_string()
        };

        rows.push(vec![
            size.to_string(),
            actual.to_string(),
            expected_aligned.to_string(),
            frag_pct,
        ]);
    }

    crate::fmt::print_table(
        heap_name,
        heap_w,
        "perf::internal_frag",
        None,
        &["req", "actual", "expected", "frag%"],
        &rows,
    );
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
        assert_eq!(stats.stddev_us, 0);
        assert_eq!(stats.p50_us, 42);
        assert_eq!(stats.p95_us, 42);
        assert_eq!(stats.p99_us, 42);
        assert_eq!(stats.p99_9_us, 42);
        assert!((stats.cv_pct - 0.0).abs() < f64::EPSILON);
        // throughput: 1e6/42 ≈ 23809.5 → 23810
        assert_eq!(stats.throughput_ops, 23810);
        // ci95: 1.96 * 0 / 1 = 0
        assert_eq!(stats.ci95_us, 0);
    }

    #[test]
    fn stats_sorted_input() {
        let samples: Vec<u64> = (1..=100).collect();
        let stats = compute_stats(&samples).unwrap();
        assert_eq!(stats.count, 100);
        assert_eq!(stats.min_us, 1);
        assert_eq!(stats.max_us, 100);
        assert_eq!(stats.avg_us, 51); // (1+100)*100/2/100 = 50.5 → rounds to 51
        assert_eq!(stats.p50_us, 50);
        assert_eq!(stats.p95_us, 95);
        assert_eq!(stats.p99_us, 99);
        assert_eq!(stats.p99_9_us, 100);
        // stddev of 1..=100: sqrt((100^2-1)/12) ≈ 28.87 → rounds to 29
        assert_eq!(stats.stddev_us, 29);
        // cv = 28.87/50.5*100 ≈ 57.2
        assert!((stats.cv_pct - 57.2).abs() < 0.1);
        // ci95: ceil(1.96 * 28.87 / 10) = ceil(5.66) = 6
        assert_eq!(stats.ci95_us, 6);
        // throughput: 1e6/50.5 ≈ 19802
        assert_eq!(stats.throughput_ops, 19802);
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

    #[test]
    fn percentile_frac_p999() {
        let sorted: Vec<u64> = (1..=1000).collect();
        assert_eq!(percentile_frac(&sorted, 999, 1000), 999);
    }

    #[test]
    fn percentile_frac_small_sample() {
        let sorted = vec![5, 10, 15];
        // 999/1000 of 3 elements → rank = ceil(2997/1000) = 3 → index 2
        assert_eq!(percentile_frac(&sorted, 999, 1000), 15);
    }

    // ── stddev / cv tests ──

    #[test]
    fn stats_uniform_zero_stddev() {
        let samples = vec![100; 50];
        let stats = compute_stats(&samples).unwrap();
        assert_eq!(stats.avg_us, 100);
        assert_eq!(stats.stddev_us, 0);
        assert!((stats.cv_pct - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_two_values_stddev() {
        // [10, 20]: mean=15, variance=(25+25)/2=25, stddev=5
        let stats = compute_stats(&[10, 20]).unwrap();
        assert_eq!(stats.avg_us, 15);
        assert_eq!(stats.stddev_us, 5);
        // cv = 5/15*100 = 33.3
        assert!((stats.cv_pct - 33.3).abs() < 0.1);
    }

    // ── human_size tests ──

    #[test]
    fn human_size_kilobytes() {
        assert_eq!(human_size(4096), "4K");
        assert_eq!(human_size(65536), "64K");
    }

    #[test]
    fn human_size_megabytes() {
        assert_eq!(human_size(1_048_576), "1M");
        assert_eq!(human_size(8_388_608), "8M");
    }

    #[test]
    fn human_size_unaligned() {
        assert_eq!(human_size(4095), "4095");
        assert_eq!(human_size(49152), "48K");
        assert_eq!(human_size(1), "1");
    }

    // ── IQR outlier tests ──

    #[test]
    fn iqr_no_outliers_uniform() {
        // Uniform data: no outliers expected.
        let samples = vec![100; 20];
        let stats = compute_stats(&samples).unwrap();
        assert_eq!(stats.outlier_count, 0);
        assert_eq!(stats.trimmed_avg_us, 100);
    }

    #[test]
    fn iqr_detects_outlier() {
        // 19 values of 10, one extreme outlier at 1000.
        let mut samples = vec![10u64; 19];
        samples.push(1000);
        let stats = compute_stats(&samples).unwrap();
        assert!(stats.outlier_count >= 1);
        // Trimmed avg should be close to 10 (the non-outlier value).
        assert!(stats.trimmed_avg_us <= 10);
        // Raw avg is pulled up by the outlier.
        assert!(stats.avg_us > stats.trimmed_avg_us);
    }

    #[test]
    fn iqr_small_sample_no_filter() {
        // < 4 samples: IQR not applied.
        let stats = compute_stats(&[5, 10, 1000]).unwrap();
        assert_eq!(stats.outlier_count, 0);
        // trimmed_avg equals raw avg for small samples.
        assert_eq!(stats.trimmed_avg_us, stats.avg_us);
    }

    #[test]
    fn iqr_preserves_json_roundtrip() {
        let mut samples = vec![50u64; 30];
        samples.push(500); // outlier
        let stats = compute_stats(&samples).unwrap();
        let json = serde_json::to_string(&stats).unwrap();
        let back: LatencyStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
        assert!(back.outlier_count > 0);
    }

    // ── throughput / CI tests ──

    #[test]
    fn throughput_zero_mean() {
        // All 0µs samples (e.g., mock backend fast path).
        let stats = compute_stats(&[0, 0, 0, 0]).unwrap();
        assert_eq!(stats.throughput_ops, 0);
        assert_eq!(stats.ci95_us, 0);
    }

    #[test]
    fn ci95_decreases_with_more_samples() {
        // Same value distribution, more samples → tighter CI (via sqrt(n)).
        // Use alternating 10/20 pattern so stddev is identical per sample.
        let small: Vec<u64> = [10, 20].iter().copied().cycle().take(10).collect();
        let large: Vec<u64> = [10, 20].iter().copied().cycle().take(100).collect();
        let ci_small = compute_stats(&small).unwrap().ci95_us;
        let ci_large = compute_stats(&large).unwrap().ci95_us;
        // stddev is same (5), but sqrt(100)/sqrt(10) ≈ 3.16x → CI shrinks.
        assert!(
            ci_small > ci_large,
            "CI should shrink: small={ci_small} large={ci_large}"
        );
    }

    #[test]
    fn format_throughput_kops() {
        assert_eq!(format_throughput(100_000), "100");
        assert_eq!(format_throughput(23810), "23");
        assert_eq!(format_throughput(500), "0.5");
    }

    // ── drift detection tests ──

    #[test]
    fn drift_too_few_samples() {
        assert!(detect_drift(&[1, 2, 3]).is_none());
    }

    #[test]
    fn drift_stable_samples() {
        let samples = vec![100; 20];
        let drift = detect_drift(&samples).unwrap();
        assert!((drift.drift_pct - 0.0).abs() < f64::EPSILON);
        assert_eq!(drift.first_half_avg_us, 100);
        assert_eq!(drift.second_half_avg_us, 100);
    }

    #[test]
    fn drift_increasing_trend() {
        // Linearly increasing: 10, 20, 30, ..., 200
        let samples: Vec<u64> = (1..=20).map(|i| i * 10).collect();
        let drift = detect_drift(&samples).unwrap();
        // Strong positive drift expected.
        assert!(
            drift.drift_pct > 50.0,
            "drift should be large positive: {}",
            drift.drift_pct
        );
        assert!(drift.second_half_avg_us > drift.first_half_avg_us);
    }

    #[test]
    fn drift_decreasing_trend() {
        // Linearly decreasing: 200, 190, ..., 10
        let samples: Vec<u64> = (1..=20).rev().map(|i| i * 10).collect();
        let drift = detect_drift(&samples).unwrap();
        assert!(
            drift.drift_pct < -50.0,
            "drift should be large negative: {}",
            drift.drift_pct
        );
        assert!(drift.first_half_avg_us > drift.second_half_avg_us);
    }

    // ── linear regression tests ──

    #[test]
    fn regression_too_few_points() {
        assert!(linear_regression(&[(4096, 10)]).is_none());
        assert!(linear_regression(&[]).is_none());
    }

    #[test]
    fn regression_perfect_linear() {
        // y = 5 + 0.001 * x → exactly linear
        let points = vec![(0, 5), (1000, 6), (2000, 7), (3000, 8)];
        let fit = linear_regression(&points).unwrap();
        assert!((fit.intercept_us - 5.0).abs() < 0.01);
        assert!((fit.slope_us_per_byte * 1024.0 - 1.024).abs() < 0.01);
        assert!((fit.r_squared - 1.0).abs() < 0.001);
    }

    #[test]
    fn regression_constant_latency() {
        // No size dependence: flat latency.
        let points = vec![(4096, 10), (65536, 10), (1_048_576, 10)];
        let fit = linear_regression(&points).unwrap();
        assert!(fit.slope_us_per_byte.abs() < 1e-9);
        assert!((fit.intercept_us - 10.0).abs() < 0.01);
    }

    #[test]
    fn regression_identical_x() {
        // All same size → undefined slope.
        assert!(linear_regression(&[(4096, 5), (4096, 10)]).is_none());
    }

    // ── bench function tests ──

    #[test]
    fn alloc_only_runs() {
        let b = MockBackend::new();
        bench_alloc_only(&b, "system", &[4096], 10, 2, 6).unwrap();
    }

    #[test]
    fn full_pipeline_runs() {
        let b = MockBackend::new();
        bench_full_pipeline(&b, "system", &[4096], 10, 2, 6).unwrap();
    }

    #[test]
    fn close_runs() {
        let b = MockBackend::new();
        bench_close(&b, "system", &[4096], 10, 2, 6).unwrap();
    }

    #[test]
    fn order_boundary_runs() {
        let b = MockBackend::new();
        bench_order_boundary(&b, "system", 5, 1, 6).unwrap();
    }

    #[test]
    fn internal_frag_runs() {
        let b = MockBackend::new();
        bench_internal_frag(&b, "system", 6, 4096).unwrap();
    }

    #[test]
    fn pool_warmup_runs() {
        let b = MockBackend::new();
        bench_pool_warmup(&b, "system", 6).unwrap();
    }

    #[test]
    fn size_switch_runs() {
        let b = MockBackend::new();
        bench_size_switch(&b, "system", 6).unwrap();
    }

    #[test]
    fn pool_warmup_no_leak() {
        let b = MockBackend::new();
        bench_pool_warmup(&b, "system", 6).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        let (results, err) = run(&b, "system", Some(&[4096]), 5, 1, 6);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert_eq!(results.len(), 7);
    }
}
