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
use crate::stats::{
    LatencyStats, LinearFit, autocorrelation, compute_stats, convergence_index, detect_bimodal,
    detect_drift, detect_latency_knee, distribution_shape, latency_entropy, linear_regression,
    percentile_ci, warmup_sufficient, welch_test,
};

/// Sizes for order boundary sweep (around 64K boundary).
const ORDER_BOUNDARY_SIZES: &[u64] = &[
    4096, 8192, 16384, 32768, 49152, 61440, 65536, 69632, 131_072, 262_144, 524_288, 1_048_576,
    2_097_152, 4_194_304, 8_388_608,
];

/// Sizes for internal fragmentation measurement.
const FRAG_SIZES: &[u64] = &[1, 4095, 4097, 65535, 65537, 100_000];

/// Benchmark configuration shared across all measurement functions.
pub struct BenchConfig<'a> {
    /// Allocation sizes to measure.
    pub sizes: &'a [u64],
    /// Number of timed iterations per size.
    pub iterations: u32,
    /// Number of warmup iterations (not measured).
    pub warmup: u32,
    /// Column width for heap name formatting.
    pub heap_w: usize,
    /// Whether to drain the page pool before each measurement.
    pub pool_bypass: bool,
    /// Override pool drain count (None = auto-detect).
    pub drain_count: Option<u32>,
}

/// Format throughput as Kops/s (thousands of operations per second).
fn format_throughput(ops: u64) -> String {
    if ops >= 1000 {
        format!("{}", ops / 1000)
    } else {
        #[allow(clippy::cast_precision_loss)]
        let v = ops as f64 / 1000.0;
        format!("{v:.1}")
    }
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
    /// Pool depth estimate (present when `--pool-bypass` is active).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_estimate: Option<PoolEstimate>,
}

/// Latency stats for a specific allocation size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeStats {
    pub size: u64,
    pub latency: LatencyStats,
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
    cfg: &BenchConfig<'_>,
) -> (
    Vec<SubTestResult>,
    Option<anyhow::Error>,
    Option<PerfAnalysis>,
) {
    tracing::debug!(
        heap = heap_name,
        ?cfg.sizes,
        cfg.iterations,
        cfg.warmup,
        cfg.pool_bypass,
        "perf sequence"
    );

    let caps = crate::probe::probe_heap(backend, heap_name);

    // Run bench_alloc_only separately to capture PerfAnalysis.
    let alloc_result = bench_alloc_only(backend, heap_name, cfg);
    let analysis = alloc_result.as_ref().ok().cloned();

    let tests: Vec<(&str, nix::Result<()>, bool)> = vec![
        ("bench_alloc_only", alloc_result.map(|_| ()), false),
        (
            "bench_full_pipeline",
            if caps.can_mmap {
                bench_full_pipeline(backend, heap_name, cfg)
            } else {
                Ok(())
            },
            !caps.can_mmap,
        ),
        ("bench_close", bench_close(backend, heap_name, cfg), false),
        (
            "bench_order_boundary",
            bench_order_boundary(backend, heap_name, cfg),
            false,
        ),
        (
            "bench_internal_frag",
            bench_internal_frag(backend, heap_name, cfg.heap_w, caps.alloc_granularity),
            false,
        ),
        (
            "bench_pool_warmup",
            bench_pool_warmup(backend, heap_name, cfg.heap_w),
            false,
        ),
        (
            "bench_size_switch",
            bench_size_switch(backend, heap_name, cfg.heap_w),
            false,
        ),
    ];

    let (sub, err) = runner::collect_test_results("perf", heap_name, cfg.heap_w, &tests);
    (sub, err, analysis)
}

/// Benchmark alloc-only latency (ioctl call to fd return).
/// Returns `PerfAnalysis` with all computed diagnostics for JSON output.
#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
fn bench_alloc_only<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    cfg: &BenchConfig<'_>,
) -> nix::Result<PerfAnalysis> {
    let sizes = cfg.sizes;
    let iterations = cfg.iterations;
    let warmup = cfg.warmup;
    let heap_w = cfg.heap_w;
    let pool_bypass = cfg.pool_bypass;
    let drain_count = cfg.drain_count;
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut regression_points: Vec<(u64, u64)> = Vec::new();
    let mut last_samples: Vec<u64> = Vec::new();
    let mut all_stats: Vec<(u64, LatencyStats)> = Vec::new();
    let mut pool_est: Option<PoolEstimate> = None;

    for &size in sizes {
        // Pool bypass: estimate and log pool depth per size.
        let mut drainer: Option<PoolDrainer<'_, B>> = if pool_bypass {
            let est = estimate_pool_depth(backend, &heap, size, drain_count);
            tracing::info!(
                size,
                depth = est.depth_buffers,
                source = ?est.source,
                "pool bypass active"
            );
            let count = est.depth_buffers;
            pool_est = Some(est);
            Some(PoolDrainer::new(backend, &heap, size, count))
        } else {
            None
        };

        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        // Measure
        let mut samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let elapsed = with_pool_bypass(&mut drainer, || {
                let start = Instant::now();
                let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
                let e = start.elapsed().as_micros() as u64;
                let buf = DmaBuf::new(backend, fd, size as usize);
                drop(buf);
                Ok(e)
            })?;
            samples.push(elapsed);
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
            all_stats.push((size, stats.clone()));
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
    let (drift_info, ac_info, last_sorted) = if last_samples.is_empty() {
        (None, None, Vec::new())
    } else {
        let drift = detect_drift(&last_samples);
        let ac = autocorrelation(&last_samples);
        let mut sorted = last_samples.clone();
        sorted.sort_unstable();
        (drift, ac, sorted)
    };

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
        let base_tp = all_stats[0].1.throughput_ops;
        if base_tp > 0 {
            let mut scaling_rows: Vec<Vec<String>> = Vec::new();
            for &(size, ref stats) in &all_stats {
                #[allow(clippy::cast_precision_loss)]
                let efficiency = stats.throughput_ops as f64 / base_tp as f64 * 100.0;
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

    // Distribution shape of last measured size.
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

    // Drift detection on the last measured size — most sensitive to thermal/pressure effects.
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

    // Autocorrelation check on last measured size — detects non-independence.
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
    let stat_refs: Vec<&LatencyStats> = all_stats.iter().map(|(_, s)| s).collect();
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
    let size_stats: Vec<SizeStats> = all_stats
        .iter()
        .map(|(size, st)| SizeStats {
            size: *size,
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
        pool_estimate: pool_est,
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
    cfg: &BenchConfig<'_>,
) -> nix::Result<()> {
    let sizes = cfg.sizes;
    let iterations = cfg.iterations;
    let warmup = cfg.warmup;
    let heap_w = cfg.heap_w;
    let pool_bypass = cfg.pool_bypass;
    let drain_count = cfg.drain_count;
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut total_rows: Vec<Vec<String>> = Vec::new();

    for &size in sizes {
        let mut drainer: Option<PoolDrainer<'_, B>> = if pool_bypass {
            let est = estimate_pool_depth(backend, &heap, size, drain_count);
            Some(PoolDrainer::new(backend, &heap, size, est.depth_buffers))
        } else {
            None
        };
        let _ = &mut drainer; // suppress unused warning for non-bypass path

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
    cfg: &BenchConfig<'_>,
) -> nix::Result<()> {
    let sizes = cfg.sizes;
    let iterations = cfg.iterations;
    let warmup = cfg.warmup;
    let heap_w = cfg.heap_w;
    let pool_bypass = cfg.pool_bypass;
    let drain_count = cfg.drain_count;
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut ratio_points: Vec<(u64, u64, u64)> = Vec::new();

    for &size in sizes {
        let mut drainer: Option<PoolDrainer<'_, B>> = if pool_bypass {
            let est = estimate_pool_depth(backend, &heap, size, drain_count);
            Some(PoolDrainer::new(backend, &heap, size, est.depth_buffers))
        } else {
            None
        };

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
            if let Some(d) = drainer.as_mut() {
                try_drop_caches();
                d.drain()?;
            }
            let t0 = Instant::now();
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let t1 = Instant::now();
            let buf = DmaBuf::new(backend, fd, size as usize);
            let t2 = Instant::now();
            drop(buf);
            let t3 = Instant::now();
            alloc_samples.push(t1.duration_since(t0).as_micros() as u64);
            close_samples.push(t3.duration_since(t2).as_micros() as u64);
            if let Some(d) = drainer.as_mut() {
                d.release();
            }
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
    cfg: &BenchConfig<'_>,
) -> nix::Result<()> {
    let iterations = cfg.iterations;
    let warmup = cfg.warmup;
    let heap_w = cfg.heap_w;
    let pool_bypass = cfg.pool_bypass;
    let drain_count = cfg.drain_count;
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut sweep_points: Vec<(u64, u64)> = Vec::new();

    for &size in ORDER_BOUNDARY_SIZES {
        let mut drainer: Option<PoolDrainer<'_, B>> = if pool_bypass {
            let est = estimate_pool_depth(backend, &heap, size, drain_count);
            Some(PoolDrainer::new(backend, &heap, size, est.depth_buffers))
        } else {
            None
        };

        // Warmup
        for _ in 0..warmup {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }

        // Measure
        let mut samples = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let elapsed = with_pool_bypass(&mut drainer, || {
                let start = Instant::now();
                let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
                let e = start.elapsed().as_micros() as u64;
                let buf = DmaBuf::new(backend, fd, size as usize);
                drop(buf);
                Ok(e)
            })?;
            samples.push(elapsed);
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

// ── Pool bypass infrastructure ──

/// How the pool depth was estimated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PoolEstimateSource {
    /// Read from `/sys/kernel/dma_heap/total_pools_kb`.
    Sysfs,
    /// Detected via latency transition probing.
    Probed,
    /// Conservative fallback (sysfs unavailable, probing failed/skipped).
    Fallback,
}

/// Estimated pool depth for a given allocation size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolEstimate {
    /// Estimated pool depth in buffers of the requested size.
    pub depth_buffers: u32,
    /// Estimated pool size in bytes.
    pub pool_bytes: u64,
    /// How the estimate was obtained.
    pub source: PoolEstimateSource,
    /// Latency transition index (probing only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knee_index: Option<u32>,
    /// Fast-path average latency in microseconds (probing only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_avg_us: Option<u64>,
    /// Slow-path average latency in microseconds (probing only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slow_avg_us: Option<u64>,
}

/// Maximum number of probe allocations (safety limit).
const MAX_PROBE_ALLOCS: usize = 2048;

/// Minimum drain count regardless of estimation method.
const MIN_DRAIN_COUNT: u32 = 32;

/// Fallback drain count when all estimation methods fail.
const FALLBACK_DRAIN_COUNT: u32 = 256;

/// Fallback pool size cap in bytes (64 MB).
const FALLBACK_POOL_CAP: u64 = 64 * 1024 * 1024;

/// Probe pool depth by allocating without releasing until latency spikes.
///
/// Holds all allocated buffers simultaneously to drain the pool.  Once the
/// sliding-window average exceeds 3× the initial average, probing stops.
/// The transition point is detected via `detect_latency_knee`.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn probe_pool_depth<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap: &DmaHeap<'_, B>,
    size: u64,
) -> Option<PoolEstimate> {
    let mut held: Vec<DmaBuf<'_, B>> = Vec::new();
    let mut latencies: Vec<u64> = Vec::new();

    for i in 0..MAX_PROBE_ALLOCS {
        let start = Instant::now();
        let fd = heap
            .alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
            .ok()?;
        let elapsed = start.elapsed().as_micros() as u64;
        latencies.push(elapsed);
        held.push(DmaBuf::new(backend, fd, size as usize));

        // Early termination: recent 10 samples average >= 3× initial 10 samples.
        if i >= 20 {
            let early_avg: f64 = latencies[..10].iter().map(|&v| v as f64).sum::<f64>() / 10.0;
            let recent_start = i - 9;
            let recent_avg: f64 = latencies[recent_start..=i]
                .iter()
                .map(|&v| v as f64)
                .sum::<f64>()
                / 10.0;
            if early_avg > 0.0 && recent_avg >= early_avg * 3.0 {
                break;
            }
        }
    }

    drop(held);

    let knee = detect_latency_knee(&latencies, 10)?;

    // Compute fast/slow averages around the knee.
    let fast_avg = if knee > 0 {
        latencies[..knee].iter().sum::<u64>() / knee as u64
    } else {
        0
    };
    let slow_count = latencies.len() - knee;
    let slow_avg = if slow_count > 0 {
        latencies[knee..].iter().sum::<u64>() / slow_count as u64
    } else {
        0
    };

    // Add 20% margin to account for partial pool state.
    let depth = (knee as u32 * 6 / 5).max(MIN_DRAIN_COUNT);

    Some(PoolEstimate {
        depth_buffers: depth,
        pool_bytes: u64::from(depth) * size,
        source: PoolEstimateSource::Probed,
        knee_index: Some(knee as u32),
        fast_avg_us: Some(fast_avg),
        slow_avg_us: Some(slow_avg),
    })
}

/// Estimate pool depth using the 3-tier strategy: sysfs → probe → fallback.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn estimate_pool_depth<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap: &DmaHeap<'_, B>,
    size: u64,
    override_count: Option<u32>,
) -> PoolEstimate {
    // User override takes priority.
    if let Some(count) = override_count {
        let count = count.max(MIN_DRAIN_COUNT);
        return PoolEstimate {
            depth_buffers: count,
            pool_bytes: u64::from(count) * size,
            source: PoolEstimateSource::Fallback,
            knee_index: None,
            fast_avg_us: None,
            slow_avg_us: None,
        };
    }

    // Tier 1: sysfs.
    if let Some(pool_kb) = crate::sysfs::read_dma_heap_pool_kb() {
        let pool_bytes = pool_kb * 1024;
        // drain_count = pool_bytes / size * 1.2, minimum MIN_DRAIN_COUNT.
        let raw = pool_bytes / size;
        let count = (raw * 6 / 5).max(u64::from(MIN_DRAIN_COUNT)) as u32;
        tracing::info!(pool_kb, size, count, "pool estimate from sysfs");
        return PoolEstimate {
            depth_buffers: count,
            pool_bytes,
            source: PoolEstimateSource::Sysfs,
            knee_index: None,
            fast_avg_us: None,
            slow_avg_us: None,
        };
    }

    // Tier 2: latency transition probing.
    if let Some(est) = probe_pool_depth(backend, heap, size) {
        tracing::info!(
            depth = est.depth_buffers,
            knee = ?est.knee_index,
            fast = ?est.fast_avg_us,
            slow = ?est.slow_avg_us,
            "pool estimate from probing"
        );
        return est;
    }

    // Tier 3: conservative fallback.
    #[allow(clippy::cast_possible_truncation)]
    let cap_count = (FALLBACK_POOL_CAP / size) as u32;
    let count = FALLBACK_DRAIN_COUNT.min(cap_count).max(MIN_DRAIN_COUNT);
    tracing::info!(count, "pool estimate fallback");
    PoolEstimate {
        depth_buffers: count,
        pool_bytes: u64::from(count) * size,
        source: PoolEstimateSource::Fallback,
        knee_index: None,
        fast_avg_us: None,
        slow_avg_us: None,
    }
}

/// Manages draining and releasing pool buffers for bypass measurements.
///
/// On `drain()`, allocates `drain_count` buffers and holds them to exhaust the
/// heap page pool.  On `release()`, drops all held buffers so pages return to
/// the pool for the next cycle.
pub(crate) struct PoolDrainer<'a, B: HeapBackend + DmaBufBackend> {
    backend: &'a B,
    heap: &'a DmaHeap<'a, B>,
    size: u64,
    drain_count: u32,
    held: Vec<DmaBuf<'a, B>>,
}

impl<'a, B: HeapBackend + DmaBufBackend> PoolDrainer<'a, B> {
    /// Create a new drainer (does not allocate until `drain()` is called).
    pub fn new(backend: &'a B, heap: &'a DmaHeap<'a, B>, size: u64, drain_count: u32) -> Self {
        Self {
            backend,
            heap,
            size,
            drain_count,
            #[allow(clippy::cast_possible_truncation)]
            held: Vec::with_capacity(drain_count as usize),
        }
    }

    /// Allocate `drain_count` buffers to exhaust the pool.
    pub fn drain(&mut self) -> nix::Result<()> {
        for _ in 0..self.drain_count {
            let fd = self.heap.alloc(
                self.size,
                DMA_HEAP_ALLOC_FD_FLAGS,
                DMA_HEAP_VALID_HEAP_FLAGS,
            )?;
            #[allow(clippy::cast_possible_truncation)]
            self.held
                .push(DmaBuf::new(self.backend, fd, self.size as usize));
        }
        Ok(())
    }

    /// Release all held drain buffers (pages return to pool).
    pub fn release(&mut self) {
        self.held.clear();
    }

    /// Number of currently held drain buffers.
    #[cfg(test)]
    pub fn held_count(&self) -> usize {
        self.held.len()
    }
}

/// Try to drop OS page caches (best-effort, requires root).
fn try_drop_caches() {
    let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
}

/// Run a single benchmark iteration with optional pool bypass.
///
/// If `drainer` is `Some`, drains the pool before the measurement and releases
/// after. The closure `f` performs the actual timed operation.
fn with_pool_bypass<B, F, T>(drainer: &mut Option<PoolDrainer<'_, B>>, f: F) -> nix::Result<T>
where
    B: HeapBackend + DmaBufBackend,
    F: FnOnce() -> nix::Result<T>,
{
    if let Some(d) = drainer.as_mut() {
        try_drop_caches();
        d.drain()?;
    }
    let result = f();
    if let Some(d) = drainer.as_mut() {
        d.release();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    // ── compute_stats tests ──
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
    #[test]
    fn format_throughput_kops() {
        assert_eq!(format_throughput(100_000), "100");
        assert_eq!(format_throughput(23810), "23");
        assert_eq!(format_throughput(1000), "1");
        assert_eq!(format_throughput(500), "0.5");
        assert_eq!(format_throughput(150), "0.1");
        assert_eq!(format_throughput(50), "0.1");
        assert_eq!(format_throughput(0), "0.0");
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

    // ── bench function tests ──

    fn test_cfg(sizes: &[u64]) -> BenchConfig<'_> {
        BenchConfig {
            sizes,
            iterations: 10,
            warmup: 2,
            heap_w: 6,
            pool_bypass: false,
            drain_count: None,
        }
    }

    #[test]
    fn alloc_only_runs() {
        let b = MockBackend::new();
        bench_alloc_only(&b, "system", &test_cfg(&[4096])).unwrap();
    }

    #[test]
    fn full_pipeline_runs() {
        let b = MockBackend::new();
        bench_full_pipeline(&b, "system", &test_cfg(&[4096])).unwrap();
    }

    #[test]
    fn close_runs() {
        let b = MockBackend::new();
        bench_close(&b, "system", &test_cfg(&[4096])).unwrap();
    }

    #[test]
    fn order_boundary_runs() {
        let b = MockBackend::new();
        let cfg = BenchConfig {
            iterations: 5,
            warmup: 1,
            ..test_cfg(&[])
        };
        bench_order_boundary(&b, "system", &cfg).unwrap();
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
        let cfg = BenchConfig {
            iterations: 5,
            warmup: 1,
            ..test_cfg(&[4096])
        };
        let (results, err, analysis) = run(&b, "system", &cfg);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert_eq!(results.len(), 7);
        let a = analysis.unwrap();
        assert_eq!(a.stats.len(), 1);
        assert_eq!(a.stats[0].size, 4096);
        assert!(a.quality_score > 0);
    }

    // ── Pool bypass tests ──

    #[test]
    fn estimate_pool_depth_override() {
        let b = MockBackend::new();
        let heap = DmaHeap::open(&b, "system").unwrap();
        let est = estimate_pool_depth(&b, &heap, 4096, Some(100));
        // Override < MIN_DRAIN_COUNT gets clamped
        assert!(est.depth_buffers >= 32);
        assert_eq!(est.source, PoolEstimateSource::Fallback);
    }

    #[test]
    fn estimate_pool_depth_fallback() {
        // Mock backend has no sysfs, probing won't show latency transition
        let b = MockBackend::new();
        let heap = DmaHeap::open(&b, "system").unwrap();
        let est = estimate_pool_depth(&b, &heap, 4096, None);
        // Should fall through to probing or fallback
        assert!(est.depth_buffers >= 32);
        assert!(
            est.source == PoolEstimateSource::Probed || est.source == PoolEstimateSource::Fallback
        );
    }

    #[test]
    fn pool_drainer_drain_and_release() {
        let b = MockBackend::new();
        let heap = DmaHeap::open(&b, "system").unwrap();
        let mut drainer = PoolDrainer::new(&b, &heap, 4096, 10);
        assert_eq!(drainer.held_count(), 0);
        drainer.drain().unwrap();
        assert_eq!(drainer.held_count(), 10);
        drainer.release();
        assert_eq!(drainer.held_count(), 0);
    }

    #[test]
    fn pool_drainer_no_leak() {
        let b = MockBackend::new();
        let heap = DmaHeap::open(&b, "system").unwrap();
        let before = b.buffer_count();
        for _ in 0..5 {
            let mut drainer = PoolDrainer::new(&b, &heap, 4096, 8);
            drainer.drain().unwrap();
            assert_eq!(drainer.held_count(), 8);
            drainer.release();
        }
        // All buffers should be freed
        assert_eq!(b.buffer_count(), before);
    }

    #[test]
    fn run_with_pool_bypass() {
        let b = MockBackend::new();
        let cfg = BenchConfig {
            iterations: 3,
            warmup: 1,
            pool_bypass: true,
            drain_count: Some(32),
            ..test_cfg(&[4096])
        };
        let (results, err, analysis) = run(&b, "system", &cfg);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        let a = analysis.unwrap();
        assert!(a.pool_estimate.is_some());
        let est = a.pool_estimate.unwrap();
        assert_eq!(est.depth_buffers, 32);
    }
}
