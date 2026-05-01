// Pure statistical functions for latency analysis.
//
// Provides LatencyStats, percentiles, distribution analysis, hypothesis tests,
// drift detection, and regression — all independent of DMA heap specifics.

use serde::{Deserialize, Serialize};

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
    let median = percentile(&sorted, 50) as f64;
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
    peaks.sort_by_key(|p| std::cmp::Reverse(p.1));
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
pub(crate) fn autocorrelation(samples: &[u64]) -> Option<(f64, f64)> {
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
pub(crate) fn latency_entropy(sorted: &[u64]) -> Option<f64> {
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
pub(crate) fn distribution_shape(samples: &[u64]) -> Option<(f64, f64)> {
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
pub(crate) fn warmup_sufficient(samples: &[u64]) -> bool {
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

/// Detect monotonic drift in measurement samples over time.
///
/// Fits a linear regression of `(sample_index, latency)` and reports the total
/// drift as a percentage of the mean. Positive drift = degradation over time
/// (thermal throttling, memory pressure), negative = warmup still settling.
///
/// Returns `None` if fewer than 10 samples (insufficient for meaningful trend).
#[allow(clippy::cast_precision_loss)]
pub(crate) fn detect_drift(samples: &[u64]) -> Option<DriftInfo> {
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
pub(crate) struct DriftInfo {
    /// Total drift as percentage of mean. Positive = degradation, negative = warmup settling.
    pub(crate) drift_pct: f64,
    /// Average latency of first half of samples.
    pub(crate) first_half_avg_us: u64,
    /// Average latency of second half of samples.
    pub(crate) second_half_avg_us: u64,
}

/// Find the earliest index where a sliding window mean stabilizes within
/// `tolerance_pct`% of the overall phase mean.
///
/// Returns the number of samples needed to converge, or `None` if the phase
/// is stable from the start (window 0 already within tolerance).
#[allow(clippy::cast_precision_loss)]
pub(crate) fn convergence_index(
    samples: &[u64],
    window: usize,
    tolerance_pct: f64,
) -> Option<usize> {
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
pub(crate) struct WelchResult {
    /// Welch's t-statistic.
    pub(crate) t_stat: f64,
    /// Cohen's d effect size: `|mean1 - mean2| / pooled_sd`.
    pub(crate) cohens_d: f64,
    /// Significance level: `"***"` (p<0.001), `"**"` (p<0.01), `"*"` (p<0.05), `"ns"`.
    pub(crate) sig: &'static str,
    /// Human-readable effect magnitude.
    pub(crate) effect: &'static str,
}

/// Welch's t-test for unequal variances + Cohen's d effect size.
///
/// Compares two sample groups and reports statistical significance and practical
/// effect size. Uses normal approximation for p-value (valid for n > 30).
///
/// Returns `None` if either group has fewer than 2 samples or zero variance.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn welch_test(a: &[u64], b: &[u64]) -> Option<WelchResult> {
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
pub(crate) fn linear_regression(points: &[(u64, u64)]) -> Option<LinearFit> {
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

/// Detect the latency transition point (knee) in a sequence of latencies.
///
/// Uses a sliding window to find where the mean jumps significantly (>= 2x)
/// compared to the previous window. Returns the index of the first sample
/// in the elevated window, representing the pool exhaustion point.
///
/// Returns `None` if no clear transition is found.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn detect_latency_knee(latencies: &[u64], window: usize) -> Option<usize> {
    if latencies.len() < window * 2 {
        return None;
    }

    let win_mean = |start: usize| -> f64 {
        latencies[start..start + window]
            .iter()
            .map(|&v| v as f64)
            .sum::<f64>()
            / window as f64
    };

    let mut prev_mean = win_mean(0);
    for start in 1..=latencies.len() - window {
        let curr_mean = win_mean(start);
        if prev_mean > 0.0 && curr_mean >= prev_mean * 2.0 {
            return Some(start);
        }
        prev_mean = curr_mean;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn detect_latency_knee_clear_step() {
        // 50 fast samples at ~5us, then 50 slow at ~100us (20x jump).
        // Use window=3 so the sliding window averages transition sharply.
        let mut latencies: Vec<u64> = vec![5; 50];
        latencies.extend(vec![100; 50]);
        let knee = detect_latency_knee(&latencies, 3);
        assert!(knee.is_some());
        let k = knee.unwrap();
        // Knee should be near the transition point (index ~50)
        assert!((48..=52).contains(&k), "knee={k} expected near 50");
    }

    #[test]
    fn detect_latency_knee_no_transition() {
        // All uniform values — no transition
        let latencies = vec![10u64; 100];
        assert!(detect_latency_knee(&latencies, 5).is_none());
    }

    #[test]
    fn detect_latency_knee_gradual() {
        // Gradual increase — may not trigger 2x threshold
        let latencies: Vec<u64> = (1..=100).collect();
        // With window=5, first window avg=3, later windows grow slowly
        // At some point a window will be 2x the previous — find it or None
        let _result = detect_latency_knee(&latencies, 5);
        // Just verify it doesn't panic; gradual data may or may not trigger
    }

    #[test]
    fn detect_latency_knee_too_short() {
        let latencies = vec![10u64; 5];
        assert!(detect_latency_knee(&latencies, 5).is_none());
    }
}
