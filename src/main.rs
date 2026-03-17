#[allow(dead_code)]
mod backend;
mod cli;
mod cmd;
#[allow(dead_code)]
mod container;
#[allow(dead_code)]
mod dmabuf;
mod fmt;
#[allow(dead_code)]
mod heap;
#[allow(dead_code)]
mod ioctl;
#[allow(dead_code)]
mod log;
#[allow(dead_code)]
mod probe;
#[allow(dead_code)]
mod procfs;
mod runner;
#[allow(dead_code)]
mod sysfs;
mod trace;

use std::time::Instant;

use clap::Parser;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

use cli::{Cli, Command};

fn main() {
    let cli = Cli::parse();

    // Initialize log file (tee output) if --log is specified.
    if let Some(ref path) = cli.log
        && let Err(e) = log::init(path)
    {
        eprintln!("warning: failed to open log file: {e}");
    }

    let level = match cli.verbose {
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };
    init_tracing(level, cli.log_level);

    if cli.trace {
        crate::trace::init();
        tracing::info!("Perfetto atrace tracing enabled");
    }

    #[cfg(target_os = "android")]
    let backend = backend::real::RealBackend::new();
    #[cfg(not(target_os = "android"))]
    let backend = backend::mock::MockBackend::new_multi_heap_realistic();
    #[cfg(not(target_os = "android"))]
    tracing::warn!("running with mock backend (not Android) — DMA-BUF operations are simulated");

    #[cfg(not(target_os = "android"))]
    let heaps = if cli.heaps.is_some() {
        probe::discover_heaps(cli.heaps.as_deref())
    } else {
        backend.available_heaps()
    };
    #[cfg(target_os = "android")]
    let heaps = probe::discover_heaps(cli.heaps.as_deref());
    let heap_w = fmt::heap_width(&heaps);

    match dispatch_command(&cli, &backend, &heaps, heap_w) {
        Ok(Some(result)) => {
            if let Some(ref path) = cli.output
                && let Err(e) = result.write_json(path)
            {
                tracing::error!(error = %e, "failed to write JSON output");
            }
            log::flush();
            if !result.all_passed() {
                std::process::exit(1);
            }
        }
        Ok(None) => {
            log::flush();
        }
        Err(e) => {
            tracing::error!(error = %e, "command failed");
            log::flush();
            std::process::exit(1);
        }
    }
}

/// Initialize tracing subscriber with stderr layer and optional log file layer.
fn init_tracing(stderr_level: LevelFilter, log_level: cli::LogLevel) {
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(stderr_level);

    if let Some(file) = log::try_clone_file() {
        let file_level: LevelFilter = log_level.into();
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_filter(file_level);
        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(file_layer)
            .init();
    } else {
        tracing_subscriber::registry().with(stderr_layer).init();
    }
}

/// Validate that all specified heaps can be opened.
fn validate_heaps<B: backend::HeapBackend>(backend: &B, heaps: &[String]) -> anyhow::Result<()> {
    for heap in heaps {
        let _h = heap::DmaHeap::open(backend, heap)
            .map_err(|e| anyhow::anyhow!("heap '{heap}' not accessible: {e}"))?;
    }
    Ok(())
}

/// Dispatch a parsed command to the appropriate handler.
/// Returns `Ok(Some(RunResult))` for test commands, `Ok(None)` for info/side-effect
/// commands, or `Err` on info failure.
#[allow(clippy::too_many_lines)]
fn dispatch_command<
    B: backend::HeapBackend + backend::DmaBufBackend + backend::ContainerBackend + Send + Sync,
>(
    cli: &Cli,
    backend: &B,
    heaps: &[String],
    heap_w: usize,
) -> anyhow::Result<Option<runner::RunResult>> {
    // Layer 1: validate global --heaps option before entering any subcommand.
    if !matches!(cli.command, Command::Info { .. }) {
        validate_heaps(backend, heaps)?;
    }

    tracing::info!(command = ?cli.command, ?heaps, "subcommand start");

    match &cli.command {
        Command::Basic {
            sizes,
            repeat,
            threads,
        } => {
            let start = Instant::now();
            let (sub, err) = run_per_heap(heaps, |h| {
                cmd::basic::run(backend, h, sizes, *repeat, *threads, heap_w)
            });
            Ok(Some(build_single_stage_result(
                "basic",
                heaps,
                &sub,
                err,
                start.elapsed(),
            )))
        }
        Command::Negative => {
            let start = Instant::now();
            let (sub, err) = run_per_heap(heaps, |h| cmd::negative::run(backend, h, heap_w));
            Ok(Some(build_single_stage_result(
                "negative",
                heaps,
                &sub,
                err,
                start.elapsed(),
            )))
        }
        Command::Perf {
            sizes,
            iterations,
            warmup,
        } => {
            let start = Instant::now();
            let (sub, err) = run_per_heap(heaps, |h| {
                cmd::perf::run(backend, h, sizes.as_deref(), *iterations, *warmup, heap_w)
            });
            Ok(Some(build_single_stage_result(
                "perf",
                heaps,
                &sub,
                err,
                start.elapsed(),
            )))
        }
        Command::Pressure {
            alloc_size,
            max_allocs,
        } => {
            let start = Instant::now();
            let is_pressure_worker = std::env::var(cmd::pressure::PRESSURE_WORKER_ENV).is_ok();
            let (sub, err) = if cfg!(target_os = "android") && !is_pressure_worker {
                // On Android, use subprocess to survive OOM kills.
                let limit =
                    max_allocs.unwrap_or_else(|| cmd::pressure::safe_exhaust_limit(*alloc_size));
                run_per_heap(heaps, |h| {
                    cmd::pressure::run_subprocess(h, *alloc_size, limit)
                })
            } else {
                // On host or as worker subprocess, run inline.
                run_per_heap(heaps, |h| {
                    cmd::pressure::run(backend, h, *alloc_size, *max_allocs, heap_w)
                })
            };
            Ok(Some(build_single_stage_result(
                "pressure",
                heaps,
                &sub,
                err,
                start.elapsed(),
            )))
        }
        Command::Info {
            detail,
            probe,
            dump,
            follow,
            interval,
        } => {
            if *dump {
                run_sysfs_dump();
            } else if *follow {
                let dur = std::time::Duration::from_secs(*interval);
                cmd::info::run_follow(dur, *detail, heaps);
            } else {
                let heap_filter: Option<Vec<&str>> = if *detail {
                    Some(heaps.iter().map(String::as_str).collect())
                } else {
                    None
                };
                cmd::info::run(
                    backend,
                    heaps,
                    *detail,
                    heap_filter.as_deref(),
                    cli.procfs,
                    *probe,
                    cli.output.as_ref(),
                )?;
            }
            Ok(None)
        }
        Command::Aging {
            size,
            threads,
            duration,
            iterations,
            report_interval,
            fuzz,
            max_hold,
            seed,
        } => {
            let dur = duration.map(std::time::Duration::from_secs);
            let interval = std::time::Duration::from_secs(*report_interval);
            let alloc_size = size
                .as_ref()
                .map(|s| cmd::aging::parse_size(s))
                .transpose()
                .map_err(|e| anyhow::anyhow!(e))?;
            let hold_limit =
                cmd::aging::parse_hold_limit(max_hold).map_err(|e| anyhow::anyhow!(e))?;
            let start = Instant::now();
            let (sub, err, _aging_result) = cmd::aging::run(
                backend,
                heaps,
                alloc_size,
                *threads,
                dur,
                *iterations,
                interval,
                *fuzz,
                hold_limit,
                *seed,
                heap_w,
            );
            Ok(Some(build_single_stage_result(
                "aging",
                heaps,
                &sub,
                err,
                start.elapsed(),
            )))
        }
        Command::Histogram {
            sizes,
            samples,
            warmup,
            mode,
            buckets,
        } => {
            let start = Instant::now();
            let (sub, err) = cmd::histogram::run(
                backend, heaps, sizes, *samples, *warmup, *mode, *buckets, heap_w,
            );
            Ok(Some(build_single_stage_result(
                "histogram",
                heaps,
                &sub,
                err,
                start.elapsed(),
            )))
        }
        Command::Container => {
            let start = Instant::now();
            let (sub, err) = cmd::container::run(backend, heaps, heap_w);
            Ok(Some(build_single_stage_result(
                "container",
                heaps,
                &sub,
                err,
                start.elapsed(),
            )))
        }
        Command::All => Ok(Some(run_all(backend, heaps))),
    }
}

/// Build a `RunResult` from a single stage's sub-test results.
#[allow(clippy::cast_possible_truncation)]
fn build_single_stage_result(
    stage_name: &str,
    heaps: &[String],
    sub_tests: &[runner::SubTestResult],
    err: Option<anyhow::Error>,
    duration: std::time::Duration,
) -> runner::RunResult {
    let details = Some(runner::sub_tests_to_details(sub_tests));
    let mut results = runner::RunResult::new(heaps);
    let duration_ms = duration.as_millis() as u64;
    let mapped = match err {
        Some(e) => Err(e),
        None => Ok(()),
    };
    results.record(stage_name, mapped, duration_ms, details);
    results
}

/// Run a command across multiple heaps, aggregating sub-test results.
fn run_per_heap<F>(heaps: &[String], f: F) -> (Vec<runner::SubTestResult>, Option<anyhow::Error>)
where
    F: Fn(&str) -> (Vec<runner::SubTestResult>, Option<anyhow::Error>),
{
    let mut all_sub = Vec::new();
    let mut first_err: Option<anyhow::Error> = None;
    for heap in heaps {
        let (sub, err) = f(heap);
        all_sub.extend(sub);
        if first_err.is_none() {
            first_err = err;
        }
    }
    (all_sub, first_err)
}

/// Run all test stages sequentially with result tracking.
fn run_all<
    B: backend::HeapBackend + backend::DmaBufBackend + backend::ContainerBackend + Send + Sync,
>(
    backend: &B,
    heaps: &[String],
) -> runner::RunResult {
    /// Helper to adapt `(Vec<SubTestResult>, Option<Error>)` to `run_stage` closure.
    fn stage_result(
        r: (Vec<runner::SubTestResult>, Option<anyhow::Error>),
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let (sub, err) = r;
        let details = Some(runner::sub_tests_to_details(&sub));
        match err {
            Some(e) => Err(e),
            None => Ok(details),
        }
    }

    let mut results = runner::RunResult::new(heaps);
    let heap_w = fmt::heap_width(heaps);

    for heap in heaps {
        runner::run_stage(&mut results, "basic", heap, heap_w, || {
            stage_result(cmd::basic::run(
                backend,
                heap,
                &[4096, 65536, 1_048_576],
                8,
                4,
                heap_w,
            ))
        });
        runner::run_stage(&mut results, "negative", heap, heap_w, || {
            stage_result(cmd::negative::run(backend, heap, heap_w))
        });
        runner::run_stage(&mut results, "perf", heap, heap_w, || {
            stage_result(cmd::perf::run(backend, heap, None, 10, 2, heap_w))
        });
        runner::run_stage(&mut results, "pressure", heap, heap_w, || {
            stage_result(cmd::pressure::run(backend, heap, 4096, None, heap_w))
        });
    }

    // Container tests run once across all heaps (not per-heap).
    runner::run_stage(&mut results, "container", "container", heap_w, || {
        stage_result(cmd::container::run(backend, heaps, heap_w))
    });

    tracing::info!(
        passed = results.total_passed,
        failed = results.total_failed,
        duration_ms = results.total_duration_ms,
        "all stages complete"
    );

    results
}

/// Standalone sysfs/procfs snapshot dump.
fn run_sysfs_dump() {
    match sysfs::snapshot() {
        Ok(snap) => {
            if let Ok(json) = serde_json::to_string_pretty(&snap) {
                tee_println!("{json}");
            }
        }
        Err(e) => tracing::warn!(error = %e, "sysfs snapshot unavailable"),
    }

    match procfs::read_meminfo() {
        Ok(info) => {
            if let Ok(json) = serde_json::to_string_pretty(&info) {
                tee_println!("{json}");
            }
        }
        Err(e) => tracing::warn!(error = %e, "meminfo unavailable"),
    }

    match procfs::read_vmstat() {
        Ok(stat) => {
            if let Ok(json) = serde_json::to_string_pretty(&stat) {
                tee_println!("{json}");
            }
        }
        Err(e) => tracing::warn!(error = %e, "vmstat unavailable"),
    }
}

#[cfg(test)]
mod fuzz_tests {
    use super::*;
    use proptest::prelude::*;

    // -----------------------------------------------------------------------
    // Strategy: global options
    // -----------------------------------------------------------------------

    fn arb_global_opts() -> impl Strategy<Value = Vec<String>> {
        (
            proptest::option::of("[a-z_]{1,12}"),
            proptest::bool::ANY,
            proptest::bool::ANY,
            proptest::bool::ANY,
            0..4u8,
        )
            .prop_map(|(heap, trace, sysfs, procfs, verbose)| {
                let mut args: Vec<String> = Vec::new();
                if let Some(h) = heap {
                    args.push("--heaps".into());
                    args.push(h);
                }
                if trace {
                    args.push("--trace".into());
                }
                if sysfs {
                    args.push("--sysfs".into());
                }
                if procfs {
                    args.push("--procfs".into());
                }
                match verbose {
                    1 => args.push("-v".into()),
                    2 => args.push("-vv".into()),
                    3 => args.push("-vvv".into()),
                    _ => {}
                }
                args
            })
    }

    // -----------------------------------------------------------------------
    // Strategy: valid subcommands with small parameters
    // -----------------------------------------------------------------------

    fn arb_subcommand() -> impl Strategy<Value = Vec<String>> {
        // Parameters kept minimal — proptest verifies crash-freedom, not functionality.
        // Dedicated unit tests in each cmd module cover the actual logic.
        // perf uses --iterations 1 --warmup 0 because bench_order_boundary allocates up to 8 MB.
        prop_oneof![
            (1..4u32).prop_map(|r| vec![
                "basic".into(),
                "--sizes".into(),
                "4096".into(),
                "--repeat".into(),
                r.to_string(),
            ]),
            (1..4u32).prop_map(|t| vec![
                "basic".into(),
                "--sizes".into(),
                "4096".into(),
                "--threads".into(),
                t.to_string(),
            ]),
            Just(vec!["negative".into()]),
            Just(vec![
                "perf".into(),
                "--sizes".into(),
                "4096".into(),
                "--iterations".into(),
                "1".into(),
                "--warmup".into(),
                "0".into(),
            ]),
            Just(vec![
                "pressure".into(),
                "--alloc-size".into(),
                "4096".into(),
                "--max-allocs".into(),
                "10".into(),
            ]),
            Just(vec!["info".into()]),
            Just(vec!["container".into()]),
            (1..3u64).prop_map(|i| vec!["aging".into(), "--iterations".into(), i.to_string()]),
            (1..3u64).prop_map(|i| vec![
                "aging".into(),
                "--fuzz".into(),
                "--iterations".into(),
                i.to_string(),
                "--seed".into(),
                "42".into(),
            ]),
        ]
    }

    // -----------------------------------------------------------------------
    // Strategy: invalid arguments
    // -----------------------------------------------------------------------

    fn arb_invalid_args() -> impl Strategy<Value = Vec<String>> {
        prop_oneof![
            "[a-z]{1,8}".prop_map(|s| vec![s]),
            Just(vec![
                "basic".into(),
                "--sizes".into(),
                "not_a_number".into()
            ]),
            Just(vec!["basic".into(), "--threads".into(), "abc".into()]),
            "[a-z]{1,8}".prop_map(|s| vec![format!("--{s}"), "basic".into()]),
        ]
    }

    // -----------------------------------------------------------------------
    // Proptest: valid combinations must not crash
    // -----------------------------------------------------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn valid_combinations_do_not_crash(
            global in arb_global_opts(),
            subcmd in arb_subcommand(),
        ) {
            let mut args = vec!["dhp".to_string()];
            args.extend(global);
            args.extend(subcmd);
            if let Ok(cli) = Cli::try_parse_from(&args) {
                let backend = backend::mock::MockBackend::new();
                let heaps = probe::discover_heaps(cli.heaps.as_deref());
                let heap_w = fmt::heap_width(&heaps);
                // Must not panic — dispatch returns normally.
                let _ = dispatch_command(&cli, &backend, &heaps, heap_w);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Proptest: dispatch with valid combinations produces valid JSON
    // -----------------------------------------------------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn output_produces_valid_json(
            global in arb_global_opts(),
            subcmd in arb_subcommand(),
        ) {
            let mut args = vec!["dhp".to_string()];
            args.extend(global);
            args.extend(subcmd);
            if let Ok(cli) = Cli::try_parse_from(&args) {
                let backend = backend::mock::MockBackend::new();
                let heaps = probe::discover_heaps(cli.heaps.as_deref());
                let heap_w = fmt::heap_width(&heaps);
                if let Ok(Some(result)) = dispatch_command(&cli, &backend, &heaps, heap_w) {
                    let json = serde_json::to_value(&result).expect("serialize RunResult");
                    prop_assert!(json["heaps"].is_array());
                    prop_assert!(json["stages"].is_array());
                    prop_assert!(json["total_passed"].is_u64());
                    prop_assert!(json["total_failed"].is_u64());
                    prop_assert!(json["total_duration_ms"].is_u64());
                }
                // Info → Ok(None) → skip JSON validation (tested in cli_smoke.rs)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Proptest: invalid arguments are rejected by clap
    // -----------------------------------------------------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn invalid_args_exit_gracefully(args in arb_invalid_args()) {
            let mut full_args = vec!["dhp".to_string()];
            full_args.extend(args);
            // Must be rejected by clap — try_parse_from returns Err.
            prop_assert!(Cli::try_parse_from(&full_args).is_err());
        }
    }
}
