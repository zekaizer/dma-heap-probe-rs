// Stage 3 performance tests: alloc latency, full pipeline, close, order boundary,
// fallback path, and internal fragmentation measurement.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::probe::align_to;
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
    /// Median Absolute Deviation: `median(|x_i - median|)`.
    /// Robust dispersion measure (breakdown point 50% vs stddev's 0%).
    pub mad_us: u64,
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

    // MAD: median(|x_i - median|)
    let median = sorted[count / 2] as f64;
    let mut abs_devs: Vec<u64> = sorted
        .iter()
        .map(|&x| {
            let dev = (x as f64 - median).abs();
            dev.round() as u64
        })
        .collect();
    abs_devs.sort_unstable();
    let mad = abs_devs[abs_devs.len() / 2];

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
        mad_us: mad,
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

/// Compute 95% confidence interval for a percentile using binomial order statistics.
///
/// For the p-th percentile (0–100) from n sorted samples, the CI bounds are
/// order statistics at ranks: `rank ± 1.96 * sqrt(n * p/100 * (1 - p/100))`.
/// This is distribution-free — no normality assumption required.
///
/// Returns `(lower_bound, upper_bound)` from the sorted data.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(crate) fn percentile_ci(sorted: &[u64], p: u32) -> (u64, u64) {
    let n = sorted.len();
    if n < 2 {
        let val = sorted.first().copied().unwrap_or(0);
        return (val, val);
    }

    let p_frac = f64::from(p) / 100.0;
    let rank = (f64::from(p) * n as f64 / 100.0).ceil() as usize;
    let se_rank = (n as f64 * p_frac * (1.0 - p_frac)).sqrt();

    let lower_rank = (rank as f64 - 1.96 * se_rank).floor().max(1.0) as usize;
    let upper_rank = (rank as f64 + 1.96 * se_rank).ceil().min(n as f64) as usize;

    (sorted[lower_rank - 1], sorted[upper_rank - 1])
}

/// Result of bimodal distribution detection.
pub(crate) struct BimodalInfo {
    /// Center of the lower mode (bucket midpoint).
    pub mode1_center: u64,
    /// Center of the upper mode (bucket midpoint).
    pub mode2_center: u64,
    /// Fraction of samples in the lower mode (0.0–1.0).
    pub mode1_frac: f64,
    /// Valley depth ratio: `valley_count / min(peak1, peak2)`. Lower = deeper split.
    pub valley_ratio: f64,
}

/// Detect bimodal distribution in sorted latency samples.
///
/// Uses histogram peak detection: builds equal-width buckets, finds local
/// maxima (peaks higher than both neighbors), and checks for a significant
/// valley between the two highest peaks.
///
/// Returns `Some` if two distinct modes are found with a valley depth
/// < 50% of the smaller peak. Requires >= 20 samples.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub(crate) fn detect_bimodal(sorted: &[u64]) -> Option<BimodalInfo> {
    let n = sorted.len();
    if n < 20 {
        return None;
    }

    let min_val = sorted[0];
    let max_val = sorted[n - 1];
    if min_val == max_val {
        return None; // uniform
    }

    // Sturges' rule for bucket count, minimum 8 for bimodal resolution.
    let k = ((n as f64).log2().ceil() as usize + 1).max(8);
    let range = max_val - min_val;
    let width = (range / k as u64).max(1);

    // Build histogram.
    let mut counts = vec![0usize; k];
    for &v in sorted {
        let idx = ((v - min_val) / width) as usize;
        counts[idx.min(k - 1)] += 1;
    }

    // Find local maxima (peaks): count[i] > count[i-1] AND count[i] > count[i+1].
    let mut peaks: Vec<(usize, usize)> = Vec::new(); // (bucket_index, count)
    for i in 0..k {
        let left = if i > 0 { counts[i - 1] } else { 0 };
        let right = if i + 1 < k { counts[i + 1] } else { 0 };
        if counts[i] > left && counts[i] > right {
            peaks.push((i, counts[i]));
        }
    }

    if peaks.len() < 2 {
        return None; // unimodal
    }

    // Sort peaks by count (descending) and take top 2.
    peaks.sort_by(|a, b| b.1.cmp(&a.1));
    let (idx1, cnt1) = peaks[0];
    let (idx2, cnt2) = peaks[1];
    let (lo_idx, hi_idx) = if idx1 < idx2 {
        (idx1, idx2)
    } else {
        (idx2, idx1)
    };

    // Find the valley (minimum count) between the two peaks.
    let valley_count = counts[lo_idx..=hi_idx].iter().copied().min().unwrap_or(0);
    let smaller_peak = cnt1.min(cnt2);
    if smaller_peak == 0 {
        return None;
    }

    let valley_ratio = valley_count as f64 / smaller_peak as f64;
    if valley_ratio >= 0.5 {
        return None; // valley not deep enough
    }

    // Count samples in mode 1 (up to midpoint between peaks).
    let split_idx = usize::midpoint(lo_idx, hi_idx);
    let mode1_count: usize = counts[..=split_idx].iter().sum();

    Some(BimodalInfo {
        mode1_center: min_val + lo_idx as u64 * width + width / 2,
        mode2_center: min_val + hi_idx as u64 * width + width / 2,
        mode1_frac: mode1_count as f64 / n as f64,
        valley_ratio: (valley_ratio * 100.0).round() / 100.0,
    })
}

/// Compute lag-1 autocorrelation and effective sample size from **time-ordered** samples.
///
/// Lag-1 autocorrelation `r = cov(x[i], x[i+1]) / var(x)` measures how much
/// each sample predicts the next. High `r` (pool caching, thermal effects) means
/// consecutive samples are not independent, inflating CI precision.
///
/// Effective sample size: `ESS = N × (1 - r) / (1 + r)` (Kish's formula).
/// Returns `None` if fewer than 3 samples or zero variance.
#[allow(clippy::cast_precision_loss)]
fn autocorrelation(samples: &[u64]) -> Option<(f64, f64)> {
    let n = samples.len();
    if n < 3 {
        return None;
    }

    let mean: f64 = samples.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
    let var: f64 = samples
        .iter()
        .map(|&v| (v as f64 - mean).powi(2))
        .sum::<f64>();
    if var == 0.0 {
        return None;
    }

    // Lag-1 autocovariance (unnormalized).
    let autocov: f64 = samples
        .windows(2)
        .map(|w| (w[0] as f64 - mean) * (w[1] as f64 - mean))
        .sum();

    let r = autocov / var; // normalized: autocov / var = autocorrelation
    let r_clamped = r.clamp(-0.99, 0.99); // avoid division by zero in ESS

    let ess = n as f64 * (1.0 - r_clamped) / (1.0 + r_clamped);
    let r_rounded = (r * 1000.0).round() / 1000.0;

    Some((r_rounded, ess.max(1.0)))
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

/// Compute normalized Shannon entropy of the latency distribution.
///
/// Bins samples into a histogram, computes `H = -Σ p_i log2(p_i)`, then
/// normalizes to `[0, 1]` by dividing by `log2(k)` where k = non-empty bins.
///
/// - 0.0 = perfectly deterministic (all identical values)
/// - 1.0 = maximum unpredictability (uniform across all bins)
/// - Useful for comparing allocator determinism across heaps/sizes.
///
/// Returns `None` if fewer than 4 samples.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn latency_entropy(sorted: &[u64]) -> Option<f64> {
    let n = sorted.len();
    if n < 4 {
        return None;
    }

    let min_val = sorted[0];
    let max_val = sorted[n - 1];
    if min_val == max_val {
        return Some(0.0); // perfectly deterministic
    }

    // Sturges' rule for bucket count.
    let k = ((n as f64).log2().ceil() as usize + 1).max(4);
    let width = ((max_val - min_val) / k as u64).max(1);

    // Build histogram and compute entropy.
    let mut counts = vec![0usize; k];
    for &v in sorted {
        let idx = ((v - min_val) / width) as usize;
        counts[idx.min(k - 1)] += 1;
    }

    let n_f = n as f64;
    let mut entropy = 0.0_f64;
    let mut non_empty = 0_usize;
    for &c in &counts {
        if c > 0 {
            let p = c as f64 / n_f;
            entropy -= p * p.log2();
            non_empty += 1;
        }
    }

    if non_empty <= 1 {
        return Some(0.0);
    }

    // Normalize to [0, 1].
    let max_entropy = (non_empty as f64).log2();
    Some((entropy / max_entropy * 100.0).round() / 100.0)
}

/// Compute skewness (3rd moment) and excess kurtosis (4th moment - 3).
///
/// - Skewness > 0: right-tailed (common for latency). Higher = longer tail.
/// - Kurtosis > 0 (leptokurtic): heavier tails than normal.
/// - Kurtosis < 0 (platykurtic): lighter tails than normal.
///
/// Returns `None` if fewer than 4 samples or zero variance.
#[allow(clippy::cast_precision_loss)]
fn distribution_shape(samples: &[u64]) -> Option<(f64, f64)> {
    let n = samples.len();
    if n < 4 {
        return None;
    }

    let mean: f64 = samples.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
    let m2: f64 = samples
        .iter()
        .map(|&v| (v as f64 - mean).powi(2))
        .sum::<f64>()
        / n as f64;

    if m2 == 0.0 {
        return None;
    }

    let sd = m2.sqrt();
    let m3: f64 = samples
        .iter()
        .map(|&v| (v as f64 - mean).powi(3))
        .sum::<f64>()
        / n as f64;
    let m4: f64 = samples
        .iter()
        .map(|&v| (v as f64 - mean).powi(4))
        .sum::<f64>()
        / n as f64;

    let skewness = m3 / sd.powi(3);
    let kurtosis = m4 / sd.powi(4) - 3.0; // excess kurtosis

    Some((
        (skewness * 100.0).round() / 100.0,
        (kurtosis * 100.0).round() / 100.0,
    ))
}

/// Test whether warmup was sufficient by comparing the first 10% of
/// **time-ordered** measurement samples against the remaining 90%.
///
/// Uses Welch's t-test: if significantly different (p < 0.05), the first
/// samples are still in "cold start" mode and warmup should be increased.
/// Returns `true` if warmup appears sufficient (no significant difference).
fn warmup_sufficient(samples: &[u64]) -> bool {
    let n = samples.len();
    if n < 20 {
        return true; // too few to test
    }
    let split = n / 10; // first 10%
    if split < 2 {
        return true;
    }
    let head = &samples[..split];
    let tail = &samples[split..];
    // If Welch's test finds significant difference → warmup insufficient.
    welch_test(head, tail).is_none_or(|w| w.sig == "ns")
}

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

/// A detected latency step between two allocation sizes.
struct OrderStep {
    size_from: u64,
    size_to: u64,
    avg_from: u64,
    avg_to: u64,
    /// Buddy allocator order for `size_to`: `ceil(log2(size / PAGE_SIZE))`.
    order: u32,
    /// Relative increase: `(avg_to - avg_from) / avg_from * 100`.
    increase_pct: f64,
}

/// Detect significant latency steps in order-boundary sweep data.
///
/// Scans adjacent `(size, avg_us)` pairs for relative increases > `threshold_pct`.
/// Returns steps sorted by increase magnitude (largest first).
#[allow(clippy::cast_precision_loss)]
fn detect_order_steps(points: &[(u64, u64)], threshold_pct: f64) -> Vec<OrderStep> {
    const PAGE_SIZE: u64 = 4096;

    let mut steps = Vec::new();
    for w in points.windows(2) {
        let (size_from, avg_from) = w[0];
        let (size_to, avg_to) = w[1];

        if avg_from == 0 {
            continue;
        }

        let increase_pct = (avg_to as f64 - avg_from as f64) / avg_from as f64 * 100.0;
        if increase_pct > threshold_pct {
            // Buddy order = number of pages needed, expressed as power-of-2 order.
            let pages = size_to.div_ceil(PAGE_SIZE);
            let order = if pages <= 1 {
                0
            } else {
                (pages - 1).ilog2() + 1
            };

            steps.push(OrderStep {
                size_from,
                size_to,
                avg_from,
                avg_to,
                order,
                increase_pct: (increase_pct * 10.0).round() / 10.0,
            });
        }
    }

    steps.sort_by(|a, b| {
        b.increase_pct
            .partial_cmp(&a.increase_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    steps
}

/// A monotonicity inversion: a larger allocation size with lower latency.
struct Inversion {
    smaller_size: u64,
    larger_size: u64,
    smaller_avg: u64,
    larger_avg: u64,
    /// Percentage decrease: `(smaller_avg - larger_avg) / smaller_avg * 100`.
    decrease_pct: f64,
}

/// Detect monotonicity inversions in order-boundary sweep data.
///
/// Scans adjacent `(size, avg_us)` pairs for cases where a larger size has
/// *lower* latency — indicating allocator fast-paths or size-class pooling.
/// Only reports decreases > 10% to filter noise.
#[allow(clippy::cast_precision_loss)]
fn detect_inversions(points: &[(u64, u64)]) -> Vec<Inversion> {
    let mut inversions = Vec::new();
    for w in points.windows(2) {
        let (size_s, avg_s) = w[0];
        let (size_l, avg_l) = w[1];

        if avg_s == 0 || avg_l >= avg_s {
            continue;
        }

        let decrease_pct = (avg_s as f64 - avg_l as f64) / avg_s as f64 * 100.0;
        if decrease_pct > 10.0 {
            inversions.push(Inversion {
                smaller_size: size_s,
                larger_size: size_l,
                smaller_avg: avg_s,
                larger_avg: avg_l,
                decrease_pct: (decrease_pct * 10.0).round() / 10.0,
            });
        }
    }
    inversions
}

/// Find the earliest index where a sliding window mean stabilizes within
/// `tolerance_pct`% of the overall phase mean.
///
/// Returns the number of samples needed to converge, or `None` if the phase
/// is stable from the start (window 0 already within tolerance).
#[allow(clippy::cast_precision_loss)]
fn convergence_index(samples: &[u64], window: usize, tolerance_pct: f64) -> Option<usize> {
    if samples.len() < window {
        return None;
    }

    let overall_mean: f64 = samples.iter().map(|&v| v as f64).sum::<f64>() / samples.len() as f64;
    if overall_mean == 0.0 {
        return None;
    }

    let threshold = overall_mean * tolerance_pct / 100.0;

    // Check if first window is already converged — no transition cost.
    let first_win_mean: f64 =
        samples[..window].iter().map(|&v| v as f64).sum::<f64>() / window as f64;
    if (first_win_mean - overall_mean).abs() <= threshold {
        return None; // stable from the start
    }

    // Scan forward to find convergence point.
    for start in 1..=samples.len().saturating_sub(window) {
        let win_mean: f64 = samples[start..start + window]
            .iter()
            .map(|&v| v as f64)
            .sum::<f64>()
            / window as f64;
        if (win_mean - overall_mean).abs() <= threshold {
            return Some(start + window);
        }
    }

    None // never converged
}

/// Result of Welch's t-test comparing two independent sample groups.
struct WelchResult {
    /// Welch's t-statistic.
    t_stat: f64,
    /// Cohen's d effect size: `|mean1 - mean2| / pooled_sd`.
    cohens_d: f64,
    /// Significance level: `"***"` (p<0.001), `"**"` (p<0.01), `"*"` (p<0.05), `"ns"`.
    sig: &'static str,
    /// Human-readable effect magnitude.
    effect: &'static str,
}

/// Welch's t-test for unequal variances + Cohen's d effect size.
///
/// Compares two sample groups and reports statistical significance and practical
/// effect size. Uses normal approximation for p-value (valid for n > 30).
///
/// Returns `None` if either group has fewer than 2 samples or zero variance.
#[allow(clippy::cast_precision_loss)]
fn welch_test(a: &[u64], b: &[u64]) -> Option<WelchResult> {
    if a.len() < 2 || b.len() < 2 {
        return None;
    }

    let n1 = a.len() as f64;
    let n2 = b.len() as f64;
    let mean1: f64 = a.iter().map(|&v| v as f64).sum::<f64>() / n1;
    let mean2: f64 = b.iter().map(|&v| v as f64).sum::<f64>() / n2;
    let var1: f64 = a.iter().map(|&v| (v as f64 - mean1).powi(2)).sum::<f64>() / (n1 - 1.0);
    let var2: f64 = b.iter().map(|&v| (v as f64 - mean2).powi(2)).sum::<f64>() / (n2 - 1.0);

    let se = (var1 / n1 + var2 / n2).sqrt();
    if se == 0.0 {
        return None;
    }

    let t_stat = (mean1 - mean2) / se;

    // Cohen's d: pooled SD from both groups.
    let pooled_var = f64::midpoint(var1, var2);
    let cohens_d = if pooled_var > 0.0 {
        (mean1 - mean2).abs() / pooled_var.sqrt()
    } else {
        0.0
    };

    // Normal approximation significance thresholds (valid for df > 30).
    let abs_t = t_stat.abs();
    let sig = if abs_t > 3.291 {
        "***"
    } else if abs_t > 2.576 {
        "**"
    } else if abs_t > 1.960 {
        "*"
    } else {
        "ns"
    };

    let effect = if cohens_d < 0.2 {
        "negligible"
    } else if cohens_d < 0.5 {
        "small"
    } else if cohens_d < 0.8 {
        "medium"
    } else {
        "large"
    };

    Some(WelchResult {
        t_stat: (t_stat * 100.0).round() / 100.0,
        cohens_d: (cohens_d * 100.0).round() / 100.0,
        sig,
        effect,
    })
}

/// Aggregated analysis results for JSON output.
///
/// Captures all advanced diagnostics so they can be serialized alongside
/// basic `LatencyStats` in the JSON output for programmatic consumption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfAnalysis {
    /// Per-size latency statistics.
    pub stats: Vec<SizeStats>,
    /// Size-latency linear regression model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regression: Option<LinearFit>,
    /// Quality scorecard (0-100).
    pub quality_score: u32,
    /// Quality rating.
    pub quality_rating: String,
    /// Drift percentage on largest size (0.0 if not detected).
    pub drift_pct: f64,
    /// Lag-1 autocorrelation on largest size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autocorr_r: Option<f64>,
    /// Effective sample size (Kish's formula).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ess: Option<f64>,
}

/// Latency stats for a specific allocation size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeStats {
    pub size: u64,
    pub latency: LatencyStats,
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
/// Returns sub-test results, the first error (if any), and analysis JSON.
#[allow(clippy::cast_possible_truncation)]
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: Option<&[u64]>,
    iterations: u32,
    warmup: u32,
    heap_w: usize,
) -> (
    Vec<SubTestResult>,
    Option<anyhow::Error>,
    Option<PerfAnalysis>,
) {
    let sizes = sizes.unwrap_or(DEFAULT_SIZES);

    tracing::debug!(
        heap = heap_name,
        ?sizes,
        iterations,
        warmup,
        "perf sequence"
    );

    let caps = crate::probe::probe_heap(backend, heap_name);

    // Run bench_alloc_only separately to capture PerfAnalysis.
    let alloc_result = bench_alloc_only(backend, heap_name, sizes, iterations, warmup, heap_w);
    let analysis = alloc_result.as_ref().ok().cloned();

    let tests: Vec<(&str, nix::Result<()>, bool)> = vec![
        ("bench_alloc_only", alloc_result.map(|_| ()), false),
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

    let (sub, err) = runner::collect_test_results("perf", heap_name, heap_w, &tests);
    (sub, err, analysis)
}

/// Benchmark alloc-only latency (ioctl call to fd return).
/// Returns `PerfAnalysis` with all computed diagnostics for JSON output.
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
fn bench_alloc_only<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    iterations: u32,
    warmup: u32,
    heap_w: usize,
) -> nix::Result<PerfAnalysis> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut regression_points: Vec<(u64, u64)> = Vec::new();
    let mut last_samples: Vec<u64> = Vec::new();
    let mut all_stats: Vec<LatencyStats> = Vec::new();

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
            // Compute p99 CI from sorted samples (non-parametric, binomial order stats).
            let mut sorted = samples.clone();
            sorted.sort_unstable();
            let (p99_lo, p99_hi) = percentile_ci(&sorted, 99);

            // Bimodal detection (pool hit vs buddy alloc).
            if let Some(bm) = detect_bimodal(&sorted) {
                #[allow(clippy::cast_precision_loss)]
                let pct1 = (bm.mode1_frac * 100.0).round();
                crate::fmt::print_metric(
                    heap_name,
                    heap_w,
                    &format!("perf::bimodal@{}", human_size(size)),
                    &[
                        ("fast", &format!("{}us ({pct1:.0}%)", bm.mode1_center)),
                        ("slow", &format!("{}us", bm.mode2_center)),
                        ("valley", &format!("{:.0}%", bm.valley_ratio * 100.0)),
                    ],
                );
            }

            regression_points.push((size, stats.avg_us));
            all_stats.push(stats.clone());
            last_samples = samples;
            rows.push(vec![
                human_size(size),
                stats.min_us.to_string(),
                format!("{}±{}", stats.avg_us, stats.ci95_us),
                stats.trimmed_avg_us.to_string(),
                stats.stddev_us.to_string(),
                stats.p50_us.to_string(),
                stats.p95_us.to_string(),
                format!("{}[{}-{}]", stats.p99_us, p99_lo, p99_hi),
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
            "p99[ci95]",
            "p99.9",
            "max",
            "Kops/s",
            "out",
        ],
        &rows,
    );

    // Pre-compute expensive analyses once for reuse in display + JSON.
    let regression = linear_regression(&regression_points);
    let drift_info = detect_drift(&last_samples);
    let ac_info = autocorrelation(&last_samples);
    let mut last_sorted = last_samples.clone();
    last_sorted.sort_unstable();

    // Size-latency regression: latency_us = base + slope * size_bytes
    if let Some(ref fit) = regression {
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

    // Throughput scaling efficiency: how well does throughput hold as size grows?
    if all_stats.len() >= 2 {
        let base_tp = all_stats[0].throughput_ops;
        if base_tp > 0 {
            let mut scaling_rows: Vec<Vec<String>> = Vec::new();
            for (i, stats) in all_stats.iter().enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let efficiency = stats.throughput_ops as f64 / base_tp as f64 * 100.0;
                let size = sizes[i];
                // Bandwidth: throughput * size = bytes/sec.
                #[allow(clippy::cast_precision_loss)]
                let bw_mb = stats.throughput_ops as f64 * size as f64 / 1_048_576.0;
                scaling_rows.push(vec![
                    human_size(size),
                    format_throughput(stats.throughput_ops),
                    format!("{efficiency:.1}"),
                    format!("{bw_mb:.0}"),
                ]);
            }
            crate::fmt::print_table(
                heap_name,
                heap_w,
                "perf::scaling",
                None,
                &["size", "Kops/s", "eff%", "MB/s"],
                &scaling_rows,
            );
        }
    }

    // Distribution shape of last (largest) size.
    if let Some((skew, kurt)) = distribution_shape(&last_samples) {
        let tail = match () {
            () if skew > 2.0 => "heavy right tail",
            () if skew > 0.5 => "right-skewed",
            () if skew < -0.5 => "left-skewed",
            () => "symmetric",
        };
        crate::fmt::print_metric(
            heap_name,
            heap_w,
            "perf::distribution",
            &[
                ("skew", &format!("{skew:.2}")),
                ("kurtosis", &format!("{kurt:.2}")),
                ("shape", &tail),
            ],
        );
    }

    // Shannon entropy: predictability of latency distribution (uses pre-sorted data).
    if let Some(entropy) = latency_entropy(&last_sorted) {
        let label = match () {
            () if entropy < 0.3 => "deterministic",
            () if entropy < 0.7 => "moderate",
            () => "unpredictable",
        };
        crate::fmt::print_metric(
            heap_name,
            heap_w,
            "perf::entropy",
            &[("H", &format!("{entropy:.2}")), ("predict", &label)],
        );
    }

    // Drift detection on the last (largest) size — most sensitive to thermal/pressure effects.
    if let Some(drift) = drift_info.as_ref().filter(|d| d.drift_pct.abs() > 10.0) {
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

    // Warmup sufficiency: first 10% vs rest 90% via Welch's t-test.
    let warmup_ok = warmup_sufficient(&last_samples);
    if !warmup_ok {
        crate::fmt::print_metric(
            heap_name,
            heap_w,
            "perf::warmup_warn",
            &[("hint", &"first 10% differs from rest: increase --warmup")],
        );
    }

    // Autocorrelation check on last (largest) size — detects non-independence.
    if let Some((r, ess)) = ac_info.filter(|(r, _)| r.abs() > 0.1) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ess_int = ess.round() as u64;
        crate::fmt::print_metric(
            heap_name,
            heap_w,
            "perf::autocorr",
            &[
                ("lag1_r", &format!("{r:.3}")),
                ("N", &last_samples.len()),
                ("ESS", &ess_int),
            ],
        );
    }

    // CI convergence: how fast does CI shrink with more samples?
    // Computes CI at 25%, 50%, 75%, 100% of samples to show convergence rate.
    if last_samples.len() >= 20 {
        let n = last_samples.len();
        let quarters = [n / 4, n / 2, n * 3 / 4, n];
        let mut ci_rows: Vec<Vec<String>> = Vec::new();
        for &q in &quarters {
            if let Some(st) = compute_stats(&last_samples[..q]) {
                ci_rows.push(vec![
                    q.to_string(),
                    st.avg_us.to_string(),
                    format!("±{}", st.ci95_us),
                ]);
            }
        }
        if ci_rows.len() == 4 {
            crate::fmt::print_table(
                heap_name,
                heap_w,
                "perf::ci_convergence",
                Some("(us)"),
                &["N", "avg", "ci95"],
                &ci_rows,
            );
        }
    }

    // Quality scorecard: aggregate all diagnostic signals (reuse cached drift_info).
    let drift_pct = drift_info.as_ref().map_or(0.0, |d| d.drift_pct);
    let stat_refs: Vec<&LatencyStats> = all_stats.iter().collect();
    let qc = quality_scorecard(&stat_refs, drift_pct, warmup_ok);
    crate::fmt::print_metric(
        heap_name,
        heap_w,
        "perf::quality",
        &[
            ("score", &format!("{}/100", qc.total)),
            ("rating", &qc.rating),
        ],
    );
    for (dim, score, rec) in &qc.details {
        if let Some(advice) = rec {
            crate::fmt::print_metric(
                heap_name,
                heap_w,
                &format!("perf::quality  {dim}={score}/20"),
                &[("hint", advice)],
            );
        }
    }

    // Build PerfAnalysis for JSON output.
    let size_stats: Vec<SizeStats> = sizes
        .iter()
        .zip(all_stats.iter())
        .map(|(&s, st)| SizeStats {
            size: s,
            latency: st.clone(),
        })
        .collect();
    let (ac_r, ac_ess) = ac_info.map_or((None, None), |(r, e)| (Some(r), Some(e)));

    Ok(PerfAnalysis {
        stats: size_stats,
        regression: regression.clone(),
        quality_score: qc.total,
        quality_rating: qc.rating.to_string(),
        drift_pct,
        autocorr_r: ac_r,
        ess: ac_ess,
    })
}

/// Score a metric value against thresholds, returning (score, optional recommendation).
///
/// `thresholds` is a sorted list of `(limit, score)` — if `value < limit`, return that score.
/// `recs` maps threshold values to recommendation strings (only for degraded scores).
/// `fallback_rec` is used when value exceeds all thresholds.
fn score_threshold(
    value: f64,
    thresholds: &[(f64, u32)],
    floor: u32,
    recs: &[(f64, &'static str)],
    fallback_rec: &'static str,
) -> (u32, Option<&'static str>) {
    for &(limit, score) in thresholds {
        if value < limit {
            let rec = recs.iter().find(|&&(t, _)| value >= t).map(|&(_, r)| r);
            return (score, rec);
        }
    }
    (floor, Some(fallback_rec))
}

/// Measurement quality assessment from aggregated diagnostic signals.
struct QualityScore {
    /// Total score 0-100.
    total: u32,
    /// Rating label.
    rating: &'static str,
    /// Per-dimension scores and recommendations.
    details: Vec<(&'static str, u32, Option<&'static str>)>,
}

/// Compute measurement quality scorecard from collected stats.
///
/// Evaluates 5 dimensions (20 points each):
/// - **Stability**: coefficient of variation across all sizes
/// - **Precision**: relative confidence interval width
/// - **Cleanliness**: outlier rate
/// - **Stationarity**: temporal drift
/// - **Warmup**: first 10% vs rest via Welch's t-test
#[allow(clippy::cast_precision_loss)]
fn quality_scorecard(all_stats: &[&LatencyStats], drift_pct: f64, warmup_ok: bool) -> QualityScore {
    // Stability: max CV across sizes.
    let max_cv = all_stats.iter().map(|s| s.cv_pct).fold(0.0_f64, f64::max);
    let (stab_score, stab_rec) = score_threshold(
        max_cv,
        &[(5.0, 20), (10.0, 16), (20.0, 8), (30.0, 4)],
        0,
        &[
            (10.0, "cv>10%: increase --iterations or reduce load"),
            (20.0, "cv>20%: significant noise, increase --iterations"),
        ],
        "cv>30%: excessive noise, check for interference",
    );

    // Precision: max relative CI (ci95/avg).
    let max_rel_ci = all_stats
        .iter()
        .filter(|s| s.avg_us > 0)
        .map(|s| s.ci95_us as f64 / s.avg_us as f64 * 100.0)
        .fold(0.0_f64, f64::max);
    let (prec_score, prec_rec) = score_threshold(
        max_rel_ci,
        &[(3.0, 20), (10.0, 16), (20.0, 8)],
        0,
        &[(10.0, "ci>10%: increase --iterations for tighter CI")],
        "ci>20%: imprecise, need significantly more iterations",
    );

    // Cleanliness: total outlier rate.
    let n_samples: usize = all_stats.iter().map(|s| s.count).sum();
    let n_outliers: usize = all_stats.iter().map(|s| s.outlier_count).sum();
    let outlier_pct = if n_samples > 0 {
        n_outliers as f64 / n_samples as f64 * 100.0
    } else {
        0.0
    };
    let (clean_score, clean_rec) = score_threshold(
        outlier_pct,
        &[(1.0, 20), (5.0, 16), (10.0, 8)],
        0,
        &[(5.0, "outliers>5%: consider --drop-caches or isolcpus")],
        "outliers>10%: heavy interference, isolate workload",
    );

    // Stationarity: drift magnitude.
    let abs_drift = drift_pct.abs();
    let (drift_score, drift_rec) = score_threshold(
        abs_drift,
        &[(5.0, 20), (10.0, 12), (20.0, 4)],
        0,
        &[(10.0, "drift>10%: increase --warmup or shorten test")],
        "drift>20%: thermal throttling or memory pressure",
    );

    // Warmup sufficiency: binary (pass/fail from Welch's t-test).
    let (warm_score, warm_rec): (u32, Option<&str>) = if warmup_ok {
        (20, None)
    } else {
        (0, Some("warmup insufficient: increase --warmup"))
    };

    let total = stab_score + prec_score + clean_score + drift_score + warm_score;
    let rating = match total {
        90..=100 => "EXCELLENT",
        75..=89 => "GOOD",
        50..=74 => "FAIR",
        _ => "NOISY",
    };

    let mut details = vec![
        ("stability", stab_score, stab_rec),
        ("precision", prec_score, prec_rec),
        ("cleanliness", clean_score, clean_rec),
        ("stationarity", drift_score, drift_rec),
        ("warmup", warm_score, warm_rec),
    ];
    // Only keep items with recommendations to reduce noise.
    details.retain(|&(_, score, _)| score < 20);

    QualityScore {
        total,
        rating,
        details,
    }
}

/// Pipeline stage names for breakdown analysis.
const STAGE_NAMES: &[&str] = &["alloc", "mmap", "sync_w", "write", "sync_r"];

/// Benchmark full pipeline with per-stage breakdown.
///
/// Times each stage individually: alloc, mmap, `sync_start`(W)+`sync_end`(W),
/// `write_bytes`, `sync_start`(R)+`sync_end`(R). Reports both total and
/// per-stage average with percentage of total.
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
    let mut total_rows: Vec<Vec<String>> = Vec::new();

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

        // Per-stage sample collectors (5 stages).
        let stage_count = STAGE_NAMES.len();
        let mut stage_samples: Vec<Vec<u64>> = (0..stage_count)
            .map(|_| Vec::with_capacity(iterations as usize))
            .collect();
        let mut total_samples = Vec::with_capacity(iterations as usize);

        for _ in 0..iterations {
            let t0 = Instant::now();
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let t1 = Instant::now();
            let mut buf = DmaBuf::new(backend, fd, size as usize);
            let ptr = buf.mmap()?;
            let t2 = Instant::now();
            buf.sync_start(DMA_BUF_SYNC_WRITE)?;
            unsafe { std::ptr::write_bytes(ptr, 0xAA, size as usize) };
            buf.sync_end(DMA_BUF_SYNC_WRITE)?;
            let t3 = Instant::now();
            // Separate write from sync for attribution — already included in sync_w above,
            // but we track the read-sync pair independently.
            buf.sync_start(DMA_BUF_SYNC_READ)?;
            buf.sync_end(DMA_BUF_SYNC_READ)?;
            let t4 = Instant::now();

            stage_samples[0].push(t1.duration_since(t0).as_micros() as u64); // alloc
            stage_samples[1].push(t2.duration_since(t1).as_micros() as u64); // mmap
            stage_samples[2].push(t3.duration_since(t2).as_micros() as u64); // sync_w + write
            // write is embedded in sync_w measurement — report combined
            stage_samples[3].push(0); // placeholder: write cost embedded in sync_w
            stage_samples[4].push(t4.duration_since(t3).as_micros() as u64); // sync_r
            total_samples.push(t4.duration_since(t0).as_micros() as u64);

            drop(buf);
        }

        if let Some(total_stats) = compute_stats(&total_samples) {
            total_rows.push(vec![
                human_size(size),
                total_stats.avg_us.to_string(),
                total_stats.stddev_us.to_string(),
                total_stats.p50_us.to_string(),
                total_stats.p95_us.to_string(),
                total_stats.p99_us.to_string(),
                total_stats.p99_9_us.to_string(),
            ]);

            // Per-stage breakdown (skip placeholder stage 3 "write").
            #[allow(clippy::cast_precision_loss)]
            let total_avg_f = total_stats.avg_us.max(1) as f64;
            let mut breakdown_rows: Vec<Vec<String>> = Vec::new();
            for (i, name) in STAGE_NAMES.iter().enumerate() {
                if i == 3 {
                    continue; // write embedded in sync_w
                }
                if let Some(st) = compute_stats(&stage_samples[i]) {
                    #[allow(clippy::cast_precision_loss)]
                    let pct = st.avg_us as f64 / total_avg_f * 100.0;
                    breakdown_rows.push(vec![
                        (*name).to_string(),
                        st.avg_us.to_string(),
                        format!("{pct:.1}"),
                    ]);
                }
            }
            crate::fmt::print_table(
                heap_name,
                heap_w,
                &format!("perf::pipeline_breakdown@{}", human_size(size)),
                Some("(us)"),
                &["stage", "avg", "%"],
                &breakdown_rows,
            );
        }
    }

    crate::fmt::print_table(
        heap_name,
        heap_w,
        "perf::full_pipeline",
        Some("(us)"),
        &["size", "avg", "sd", "p50", "p95", "p99", "p99.9"],
        &total_rows,
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
    let mut ratio_points: Vec<(u64, u64, u64)> = Vec::new();

    for &size in sizes {
        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        // Paired alloc + close measurement for efficiency ratio.
        let mut close_samples = Vec::with_capacity(iterations as usize);
        let mut alloc_samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let t0 = Instant::now();
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let t1 = Instant::now();
            let buf = DmaBuf::new(backend, fd, size as usize);
            let t2 = Instant::now();
            drop(buf);
            let t3 = Instant::now();
            alloc_samples.push(t1.duration_since(t0).as_micros() as u64);
            close_samples.push(t3.duration_since(t2).as_micros() as u64);
        }

        if let Some(stats) = compute_stats(&close_samples) {
            let alloc_avg = compute_stats(&alloc_samples).map_or(0, |s| s.avg_us);
            ratio_points.push((size, alloc_avg, stats.avg_us));
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

    // Close/alloc efficiency ratio per size.
    if !ratio_points.is_empty() {
        let mut ratio_rows: Vec<Vec<String>> = Vec::new();
        for &(size, alloc_avg, close_avg) in &ratio_points {
            #[allow(clippy::cast_precision_loss)]
            let ratio = if alloc_avg > 0 {
                close_avg as f64 / alloc_avg as f64
            } else {
                0.0
            };
            let label = if ratio < 0.5 {
                "fast"
            } else if ratio <= 2.0 {
                "balanced"
            } else {
                "expensive"
            };
            ratio_rows.push(vec![
                human_size(size),
                alloc_avg.to_string(),
                close_avg.to_string(),
                format!("{ratio:.2}"),
                label.to_string(),
            ]);
        }
        crate::fmt::print_table(
            heap_name,
            heap_w,
            "perf::close_ratio",
            None,
            &["size", "alloc", "close", "ratio", "verdict"],
            &ratio_rows,
        );
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
    heap_w: usize,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut sweep_points: Vec<(u64, u64)> = Vec::new();

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
            sweep_points.push((size, stats.avg_us));
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

    // Detect significant latency steps at order boundaries (>20% increase).
    let steps = detect_order_steps(&sweep_points, 20.0);
    for step in &steps {
        crate::fmt::print_metric(
            heap_name,
            heap_w,
            &format!("perf::order_step (order {})", step.order),
            &[
                ("from", &human_size(step.size_from)),
                ("to", &human_size(step.size_to)),
                ("avg", &format!("{}→{}us", step.avg_from, step.avg_to)),
                ("+%", &format!("{:.1}", step.increase_pct)),
            ],
        );
    }

    // Monotonicity inversions: larger size with lower latency → fast-path/pool optimization.
    for inv in detect_inversions(&sweep_points) {
        crate::fmt::print_metric(
            heap_name,
            heap_w,
            "perf::inversion",
            &[
                ("larger", &human_size(inv.larger_size)),
                ("smaller", &human_size(inv.smaller_size)),
                ("avg", &format!("{}→{}us", inv.smaller_avg, inv.larger_avg)),
                ("-%", &format!("{:.1}", inv.decrease_pct)),
            ],
        );
    }

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

        // Welch's t-test: is the cold/warm difference statistically significant?
        if let Some(w) = welch_test(&cold_samples, &warm_samples) {
            crate::fmt::print_metric(
                heap_name,
                heap_w,
                "perf::pool_effect",
                &[
                    ("t", &format!("{:.2}", w.t_stat)),
                    ("d", &format!("{:.2}", w.cohens_d)),
                    ("sig", &w.sig),
                    ("effect", &w.effect),
                ],
            );
        }
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

    // Hysteresis: does phase 3 (back to size_a) recover to phase 1 level?
    if let (Some(p1), Some(p3)) = (compute_stats(&phase1), compute_stats(&phase3)) {
        #[allow(clippy::cast_precision_loss)]
        if let Some(ratio) = (p1.avg_us > 0).then(|| p3.avg_us as f64 / p1.avg_us as f64) {
            let ratio_str = format!("{ratio:.2}");
            let verdict = if ratio > 1.1 {
                "degraded"
            } else if ratio < 0.9 {
                "improved"
            } else {
                "recovered"
            };
            crate::fmt::print_metric(
                heap_name,
                heap_w,
                "perf::hysteresis",
                &[
                    ("ph1_avg", &p1.avg_us),
                    ("ph3_avg", &p3.avg_us),
                    ("ratio", &ratio_str),
                    ("verdict", &verdict),
                ],
            );
        }
    }

    // Convergence: how many allocs until each phase stabilizes?
    for (ph, samples) in [(2, &phase2), (3, &phase3)] {
        if let Some(n) = convergence_index(samples, 10, 5.0) {
            crate::fmt::print_metric(
                heap_name,
                heap_w,
                &format!("perf::converge_ph{ph}"),
                &[("after", &format!("{n} allocs"))],
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
        assert_eq!(stats.mad_us, 0);
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
        // MAD of 1..=100: median=50, |x-50| sorted → 0,1,1,2,2,...,49,50
        // median of deviations = 25
        assert_eq!(stats.mad_us, 25);
    }

    #[test]
    fn stats_unsorted_input() {
        let samples = vec![100, 1, 50, 99, 2];
        let stats = compute_stats(&samples).unwrap();
        assert_eq!(stats.min_us, 1);
        assert_eq!(stats.max_us, 100);
        assert_eq!(stats.p50_us, 50);
    }

    // ── MAD tests ──

    #[test]
    fn mad_uniform_is_zero() {
        let stats = compute_stats(&[50; 20]).unwrap();
        assert_eq!(stats.mad_us, 0);
    }

    #[test]
    fn mad_with_outlier_robust() {
        // 19x value 10, 1x outlier 1000. Median=10, |x-10| for non-outlier=0.
        // MAD should remain 0 (robust against single outlier).
        let mut samples = vec![10u64; 19];
        samples.push(1000);
        let stats = compute_stats(&samples).unwrap();
        assert_eq!(
            stats.mad_us, 0,
            "MAD should be robust against single outlier"
        );
        assert!(stats.stddev_us > 0, "stddev should be inflated by outlier");
    }

    #[test]
    fn stats_serde_roundtrip() {
        let stats = compute_stats(&[10, 20, 30, 40, 50]).unwrap();
        let json = serde_json::to_string(&stats).unwrap();
        let deserialized: LatencyStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, deserialized);
    }

    // ── autocorrelation tests ──

    #[test]
    fn autocorr_too_few() {
        assert!(autocorrelation(&[1, 2]).is_none());
    }

    #[test]
    fn autocorr_zero_variance() {
        assert!(autocorrelation(&[5, 5, 5, 5]).is_none());
    }

    #[test]
    fn autocorr_independent_samples() {
        // Alternating pattern: no positive autocorrelation.
        let samples: Vec<u64> = [10, 20].iter().copied().cycle().take(100).collect();
        let (r, ess) = autocorrelation(&samples).unwrap();
        // Alternating → negative autocorrelation (r ≈ -1).
        assert!(r < 0.0, "alternating should have negative r: {r}");
        // ESS > N for negatively correlated samples.
        assert!(ess > 100.0, "ESS should exceed N for negative r: {ess}");
    }

    #[test]
    fn autocorr_strongly_correlated() {
        // Monotonically increasing → strong positive autocorrelation.
        let samples: Vec<u64> = (1..=100).collect();
        let (r, ess) = autocorrelation(&samples).unwrap();
        assert!(r > 0.5, "monotonic should have high r: {r}");
        assert!(ess < 50.0, "ESS should be much less than N=100: {ess}");
    }

    // ── entropy tests ──

    #[test]
    fn entropy_deterministic() {
        let sorted = vec![42; 50];
        assert!((latency_entropy(&sorted).unwrap() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn entropy_spread() {
        // Wide uniform distribution: high entropy.
        let sorted: Vec<u64> = (1..=100).collect();
        let h = latency_entropy(&sorted).unwrap();
        assert!(h > 0.8, "uniform should have high entropy: {h}");
    }

    #[test]
    fn entropy_concentrated() {
        // 95 values at 10, 5 at 100: mostly deterministic.
        let mut sorted = vec![10u64; 95];
        sorted.extend(vec![100u64; 5]);
        sorted.sort_unstable();
        let h = latency_entropy(&sorted).unwrap();
        assert!(h < 0.5, "concentrated should have low entropy: {h}");
    }

    // ── distribution shape tests ──

    #[test]
    fn shape_symmetric() {
        // Uniform 1..=100: symmetric, skewness ≈ 0.
        let samples: Vec<u64> = (1..=100).collect();
        let (skew, _kurt) = distribution_shape(&samples).unwrap();
        assert!(skew.abs() < 0.1, "uniform should be symmetric: skew={skew}");
    }

    #[test]
    fn shape_right_skewed() {
        // 90 values at 10, 10 values at 1000: right-skewed.
        let mut samples = vec![10u64; 90];
        samples.extend(vec![1000u64; 10]);
        let (skew, kurt) = distribution_shape(&samples).unwrap();
        assert!(skew > 1.0, "should be right-skewed: skew={skew}");
        assert!(kurt > 0.0, "should be leptokurtic: kurt={kurt}");
    }

    #[test]
    fn shape_too_few() {
        assert!(distribution_shape(&[1, 2, 3]).is_none());
    }

    // ── percentile CI tests ──

    #[test]
    fn percentile_ci_single() {
        let (lo, hi) = percentile_ci(&[42], 99);
        assert_eq!(lo, 42);
        assert_eq!(hi, 42);
    }

    #[test]
    fn percentile_ci_100_samples() {
        let sorted: Vec<u64> = (1..=100).collect();
        let (lo, hi) = percentile_ci(&sorted, 99);
        // p99 = 99, CI should bracket it: lower < 99, upper >= 99
        assert!(lo <= 99, "lower bound {lo} should be ≤ 99");
        assert!(hi >= 99, "upper bound {hi} should be ≥ 99");
        // CI width should be small for 100 samples
        assert!(hi - lo <= 5, "CI too wide: [{lo}, {hi}]");
    }

    #[test]
    fn percentile_ci_bounds_within_data() {
        let sorted: Vec<u64> = (10..=50).collect();
        let (lo, hi) = percentile_ci(&sorted, 95);
        assert!(
            lo >= 10 && hi <= 50,
            "CI [{lo},{hi}] must be within data range [10,50]"
        );
    }

    #[test]
    fn percentile_ci_median_widest() {
        // se_rank = sqrt(n*p*(1-p)) is maximized at p=50, so median CI is widest.
        let sorted: Vec<u64> = (1..=100).collect();
        let (lo50, hi50) = percentile_ci(&sorted, 50);
        let (lo99, hi99) = percentile_ci(&sorted, 99);
        let width50 = hi50 - lo50;
        let width99 = hi99 - lo99;
        assert!(
            width50 >= width99,
            "p50 CI ({width50}) should be ≥ p99 CI ({width99})"
        );
    }

    // ── warmup sufficiency tests ──

    #[test]
    fn warmup_sufficient_uniform() {
        // Uniform data: no cold start effect.
        assert!(warmup_sufficient(&[50; 100]));
    }

    #[test]
    fn warmup_insufficient_cold_start() {
        // First 10% much higher than rest → warmup insufficient.
        // Need variance within groups for Welch's test to work.
        let mut samples: Vec<u64> = (490..=510).cycle().take(10).collect(); // cold ~500
        let warm: Vec<u64> = (8..=12).cycle().take(90).collect(); // warm ~10
        samples.extend(warm);
        assert!(!warmup_sufficient(&samples));
    }

    #[test]
    fn warmup_too_few() {
        // < 20 samples: assume sufficient.
        assert!(warmup_sufficient(&[1, 2, 3]));
    }

    // ── bimodal detection tests ──

    #[test]
    fn bimodal_too_few() {
        let sorted: Vec<u64> = (1..=10).collect();
        assert!(detect_bimodal(&sorted).is_none());
    }

    #[test]
    fn bimodal_uniform() {
        let sorted = vec![50u64; 30];
        assert!(detect_bimodal(&sorted).is_none());
    }

    #[test]
    fn bimodal_unimodal() {
        // Normal-ish distribution: single peak.
        let sorted: Vec<u64> = (1..=100).collect();
        // Uniform distribution has no peaks — should be None or unimodal.
        assert!(detect_bimodal(&sorted).is_none());
    }

    #[test]
    fn bimodal_clear_two_modes() {
        // 30 samples at ~10, 30 samples at ~100 — clear bimodal.
        let mut sorted = vec![10u64; 30];
        sorted.extend(vec![100u64; 30]);
        sorted.sort_unstable();
        let bm = detect_bimodal(&sorted);
        assert!(bm.is_some(), "should detect bimodal distribution");
        let bm = bm.unwrap();
        assert!(
            bm.mode1_center < 50,
            "mode1 should be near 10: {}",
            bm.mode1_center
        );
        assert!(
            bm.mode2_center > 50,
            "mode2 should be near 100: {}",
            bm.mode2_center
        );
        assert!(bm.valley_ratio < 0.5, "valley should be deep");
    }

    // ── quality scorecard tests ──

    #[test]
    fn scorecard_excellent() {
        // Perfect measurements: low CV, no outliers, no drift.
        let stats = LatencyStats {
            count: 100,
            min_us: 10,
            max_us: 12,
            avg_us: 11,
            stddev_us: 0,
            p50_us: 11,
            p95_us: 12,
            p99_us: 12,
            p99_9_us: 12,
            cv_pct: 1.0,
            trimmed_avg_us: 11,
            outlier_count: 0,
            throughput_ops: 90909,
            ci95_us: 0,
            mad_us: 0,
        };
        let qc = quality_scorecard(&[&stats], 0.0, true);
        assert_eq!(qc.rating, "EXCELLENT");
        assert!(qc.total >= 90);
        assert!(qc.details.is_empty(), "no recommendations for excellent");
    }

    #[test]
    fn scorecard_noisy() {
        // Bad measurements: high CV, many outliers, strong drift.
        let stats = LatencyStats {
            count: 100,
            min_us: 1,
            max_us: 1000,
            avg_us: 50,
            stddev_us: 200,
            p50_us: 20,
            p95_us: 500,
            p99_us: 900,
            p99_9_us: 1000,
            cv_pct: 400.0,
            trimmed_avg_us: 20,
            outlier_count: 30,
            throughput_ops: 20000,
            ci95_us: 40,
            mad_us: 100,
        };
        let qc = quality_scorecard(&[&stats], 50.0, false);
        assert_eq!(qc.rating, "NOISY");
        assert!(qc.total < 50);
        assert!(!qc.details.is_empty(), "should have recommendations");
    }

    #[test]
    fn score_threshold_basic() {
        let (s, r) = score_threshold(3.0, &[(5.0, 25), (10.0, 20)], 0, &[], "bad");
        assert_eq!(s, 25);
        assert!(r.is_none());

        let (s, r) = score_threshold(99.0, &[(5.0, 25)], 0, &[], "bad");
        assert_eq!(s, 0);
        assert_eq!(r, Some("bad"));
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

    // ── convergence tests ──

    #[test]
    fn convergence_too_short() {
        assert!(convergence_index(&[1, 2, 3], 10, 5.0).is_none());
    }

    #[test]
    fn convergence_already_stable() {
        // Uniform data: first window already at overall mean.
        let samples = vec![100; 50];
        assert!(convergence_index(&samples, 10, 5.0).is_none());
    }

    #[test]
    fn convergence_after_spike() {
        // First 10 values high (200), rest normal (100). Should converge around index 20.
        let mut samples = vec![200u64; 10];
        samples.extend(vec![100u64; 90]);
        let idx = convergence_index(&samples, 10, 5.0);
        assert!(idx.is_some());
        let n = idx.unwrap();
        assert!(n > 10 && n <= 30, "should converge between 10-30, got {n}");
    }

    #[test]
    fn convergence_zero_mean() {
        let samples = vec![0u64; 50];
        assert!(convergence_index(&samples, 10, 5.0).is_none());
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

    // ── Welch's t-test tests ──

    #[test]
    fn welch_too_few() {
        assert!(welch_test(&[1], &[2, 3]).is_none());
        assert!(welch_test(&[1, 2], &[3]).is_none());
    }

    #[test]
    fn welch_identical_groups() {
        let a = vec![100; 20];
        let b = vec![100; 20];
        // Zero variance → None (can't compute SE).
        assert!(welch_test(&a, &b).is_none());
    }

    #[test]
    fn welch_significant_difference() {
        // Group A: mean=10, Group B: mean=100 — clearly different.
        let a: Vec<u64> = (5..=15).collect();
        let b: Vec<u64> = (95..=105).collect();
        let w = welch_test(&a, &b).unwrap();
        assert!(
            w.t_stat < -10.0,
            "t should be strongly negative: {}",
            w.t_stat
        );
        assert_eq!(w.sig, "***");
        assert_eq!(w.effect, "large");
        assert!(w.cohens_d > 0.8);
    }

    #[test]
    fn welch_not_significant() {
        // Nearly identical distributions — not significant.
        let a: Vec<u64> = vec![10, 11, 10, 11, 10, 11, 10, 11, 10, 11];
        let b: Vec<u64> = vec![10, 11, 10, 11, 10, 11, 10, 11, 11, 10];
        let w = welch_test(&a, &b).unwrap();
        assert_eq!(w.sig, "ns");
        assert_eq!(w.effect, "negligible");
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

    // ── order step detection tests ──

    #[test]
    fn order_steps_empty() {
        assert!(detect_order_steps(&[], 20.0).is_empty());
        assert!(detect_order_steps(&[(4096, 10)], 20.0).is_empty());
    }

    #[test]
    fn order_steps_no_significant_jump() {
        // Flat latency across sizes — no steps.
        let points = vec![(4096, 10), (8192, 11), (16384, 12)];
        let steps = detect_order_steps(&points, 20.0);
        assert!(steps.is_empty());
    }

    #[test]
    fn order_steps_detects_jump() {
        // 4K→8K: 10→10 (flat), 8K→64K: 10→20 (100% jump)
        let points = vec![(4096, 10), (8192, 10), (65536, 20)];
        let steps = detect_order_steps(&points, 20.0);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].size_from, 8192);
        assert_eq!(steps[0].size_to, 65536);
        assert!((steps[0].increase_pct - 100.0).abs() < 0.1);
        assert_eq!(steps[0].order, 4); // 65536/4096 = 16 pages = order 4
    }

    #[test]
    fn order_steps_sorted_by_magnitude() {
        let points = vec![(4096, 10), (8192, 15), (65536, 50), (1_048_576, 60)];
        let steps = detect_order_steps(&points, 20.0);
        // 8K→64K = +233%, 4K→8K = +50%; 64K→1M = +20% (at threshold)
        assert!(steps.len() >= 2);
        assert!(steps[0].increase_pct >= steps[1].increase_pct);
    }

    #[test]
    fn order_steps_buddy_order_calc() {
        // 4096 = 1 page = order 0, 8192 = 2 pages = order 1, etc.
        let points = vec![(4096, 5), (8192, 20)];
        let steps = detect_order_steps(&points, 20.0);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].order, 1); // 8192/4096 = 2 pages → order 1
    }

    // ── inversion detection tests ──

    #[test]
    fn inversion_none_for_monotonic() {
        let points = vec![(4096, 10), (8192, 15), (65536, 30)];
        assert!(detect_inversions(&points).is_empty());
    }

    #[test]
    fn inversion_detects_decrease() {
        // 64K is faster than 48K — pool optimization.
        let points = vec![(49152, 20), (65536, 10)];
        let inv = detect_inversions(&points);
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].smaller_size, 49152);
        assert_eq!(inv[0].larger_size, 65536);
        assert!((inv[0].decrease_pct - 50.0).abs() < 0.1);
    }

    #[test]
    fn inversion_ignores_small_decrease() {
        // 5% decrease — below 10% threshold.
        let points = vec![(4096, 100), (8192, 95)];
        assert!(detect_inversions(&points).is_empty());
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
        let (results, err, analysis) = run(&b, "system", Some(&[4096]), 5, 1, 6);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert_eq!(results.len(), 7);
        let a = analysis.unwrap();
        assert_eq!(a.stats.len(), 1);
        assert_eq!(a.stats[0].size, 4096);
        assert!(a.quality_score > 0);
    }
}
