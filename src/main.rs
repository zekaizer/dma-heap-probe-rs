#[allow(dead_code)]
mod backend;
mod cli;
mod cmd;
#[allow(dead_code)]
mod dmabuf;
#[allow(dead_code)]
mod heap;
#[allow(dead_code)]
mod ioctl;
#[allow(dead_code)]
mod probe;
#[allow(dead_code)]
mod procfs;
mod runner;
#[allow(dead_code)]
mod sysfs;
#[allow(dead_code)]
mod trace;

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use tracing_subscriber::filter::LevelFilter;

use cli::{Cli, Command};

#[allow(clippy::too_many_lines)]
fn main() {
    let cli = Cli::parse();

    let level = match cli.verbose {
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };
    tracing_subscriber::fmt().with_max_level(level).init();

    #[cfg(target_os = "android")]
    let backend = backend::real::RealBackend::new();
    #[cfg(not(target_os = "android"))]
    let backend = backend::mock::MockBackend::new();

    let heaps = probe::discover_heaps(cli.heaps.as_deref());

    match cli.command {
        Command::Basic {
            sizes,
            repeat,
            threads,
        } => {
            let start = Instant::now();
            let (sub, err) = run_per_heap(&heaps, |h| {
                cmd::basic::run(&backend, h, &sizes, repeat, threads)
            });
            handle_cmd_output(
                "basic",
                &heaps,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Negative => {
            let start = Instant::now();
            let (sub, err) = run_per_heap(&heaps, |h| cmd::negative::run(&backend, h));
            handle_cmd_output(
                "negative",
                &heaps,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Perf {
            sizes,
            iterations,
            warmup,
        } => {
            let start = Instant::now();
            let (sub, err) = run_per_heap(&heaps, |h| {
                cmd::perf::run(&backend, h, sizes.as_deref(), iterations, warmup)
            });
            handle_cmd_output(
                "perf",
                &heaps,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Pressure {
            alloc_size,
            max_allocs,
        } => {
            let start = Instant::now();
            let (sub, err) = run_per_heap(&heaps, |h| {
                cmd::pressure::run(&backend, h, alloc_size, max_allocs)
            });
            handle_cmd_output(
                "pressure",
                &heaps,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Pool => {
            let start = Instant::now();
            let (sub, err) = run_per_heap(&heaps, |h| cmd::pool::run(&backend, h));
            handle_cmd_output(
                "pool",
                &heaps,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Info {
            detail,
            dump,
            follow,
            interval,
        } => {
            if dump {
                run_sysfs_dump();
            } else if follow {
                let dur = std::time::Duration::from_secs(interval);
                cmd::info::run_follow(dur, detail, &heaps);
            } else {
                let heap_filter: Option<Vec<&str>> = if detail {
                    Some(heaps.iter().map(String::as_str).collect())
                } else {
                    None
                };
                if let Err(e) = cmd::info::run(
                    detail,
                    heap_filter.as_deref(),
                    cli.procfs,
                    cli.output.as_ref(),
                ) {
                    tracing::error!(error = %e, "info command failed");
                    std::process::exit(1);
                }
            }
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
            trend_fail,
            leak_threshold_mb,
            max_error_rate,
        } => {
            let dur = duration.map(std::time::Duration::from_secs);
            let interval = std::time::Duration::from_secs(report_interval);
            let thresholds = cmd::aging::AgingThresholds {
                trend_fail,
                leak_threshold_mb,
                max_error_rate,
            };
            let start = Instant::now();
            let (sub, err, _aging_result) = cmd::aging::run(
                &backend,
                &heaps,
                size,
                threads,
                dur,
                iterations,
                interval,
                fuzz,
                max_hold,
                seed,
                &thresholds,
            );
            handle_cmd_output(
                "aging",
                &heaps,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Histogram {
            sizes,
            samples,
            warmup,
            mode,
            buckets,
        } => {
            let start = Instant::now();
            let (sub, err) =
                cmd::histogram::run(&backend, &heaps, &sizes, samples, warmup, mode, buckets);
            handle_cmd_output(
                "histogram",
                &heaps,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::All => {
            run_all(&backend, &cli, &heaps);
        }
    }
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
fn run_all<B: backend::HeapBackend + backend::DmaBufBackend + Send + Sync>(
    backend: &B,
    cli: &Cli,
    heaps: &[String],
) {
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

    for heap in heaps {
        runner::run_stage(&mut results, "basic", heap, || {
            stage_result(cmd::basic::run(
                backend,
                heap,
                &[4096, 65536, 1_048_576],
                8,
                4,
            ))
        });
        runner::run_stage(&mut results, "negative", heap, || {
            stage_result(cmd::negative::run(backend, heap))
        });
        runner::run_stage(&mut results, "perf", heap, || {
            stage_result(cmd::perf::run(backend, heap, None, 10, 2))
        });
        runner::run_stage(&mut results, "pressure", heap, || {
            stage_result(cmd::pressure::run(backend, heap, 4096, None))
        });
        runner::run_stage(&mut results, "pool", heap, || {
            stage_result(cmd::pool::run(backend, heap))
        });
    }

    tracing::info!(
        passed = results.total_passed,
        failed = results.total_failed,
        duration_ms = results.total_duration_ms,
        "all stages complete"
    );

    if let Some(ref output_path) = cli.output
        && let Err(e) = results.write_json(output_path)
    {
        tracing::error!(error = %e, "failed to write JSON output");
    }

    if !results.all_passed() {
        std::process::exit(1);
    }
}

/// Handle a single subcommand's output: write JSON if --output, exit(1) on failure.
fn handle_cmd_output(
    stage_name: &str,
    heaps: &[String],
    output: Option<&PathBuf>,
    sub_tests: &[runner::SubTestResult],
    err: Option<anyhow::Error>,
    duration: std::time::Duration,
) {
    let has_failure = err.is_some();

    if let Some(output_path) = output {
        let details = Some(runner::sub_tests_to_details(sub_tests));
        let mut results = runner::RunResult::new(heaps);
        #[allow(clippy::cast_possible_truncation)]
        let duration_ms = duration.as_millis() as u64;
        let mapped = match err {
            Some(e) => Err(e),
            None => Ok(()),
        };
        results.record(stage_name, mapped, duration_ms, details);
        if let Err(e) = results.write_json(output_path) {
            tracing::error!(error = %e, "failed to write JSON output");
        }
    } else if let Some(e) = err {
        tracing::error!(error = %e, "{stage_name} tests failed");
    }

    if has_failure {
        std::process::exit(1);
    }
}

/// Standalone sysfs/procfs snapshot dump.
fn run_sysfs_dump() {
    match sysfs::snapshot() {
        Ok(snap) => {
            if let Ok(json) = serde_json::to_string_pretty(&snap) {
                println!("{json}");
            }
        }
        Err(e) => tracing::warn!(error = %e, "sysfs snapshot unavailable"),
    }

    match procfs::read_meminfo() {
        Ok(info) => {
            if let Ok(json) = serde_json::to_string_pretty(&info) {
                println!("{json}");
            }
        }
        Err(e) => tracing::warn!(error = %e, "meminfo unavailable"),
    }

    match procfs::read_vmstat() {
        Ok(stat) => {
            if let Ok(json) = serde_json::to_string_pretty(&stat) {
                println!("{json}");
            }
        }
        Err(e) => tracing::warn!(error = %e, "vmstat unavailable"),
    }
}
