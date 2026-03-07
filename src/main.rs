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
mod procfs;
mod runner;
#[allow(dead_code)]
mod sysfs;
#[allow(dead_code)]
mod trace;

use clap::Parser;
use tracing_subscriber::filter::LevelFilter;

use cli::{Cli, Command};

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

    match cli.command {
        Command::Basic { sizes, repeat } => {
            if let Err(e) = cmd::basic::run(&backend, &cli.heap, &sizes, repeat) {
                tracing::error!(error = %e, "basic tests failed");
                std::process::exit(1);
            }
        }
        Command::SyncFile => {
            if let Err(e) = cmd::sync_file::run(&backend, &cli.heap) {
                tracing::error!(error = %e, "sync_file tests failed");
                std::process::exit(1);
            }
        }
        Command::Edge { threads } => {
            if let Err(e) = cmd::edge::run(&backend, &cli.heap, threads) {
                tracing::error!(error = %e, "edge tests failed");
                std::process::exit(1);
            }
        }
        Command::Negative => {
            if let Err(e) = cmd::negative::run(&backend, &cli.heap) {
                tracing::error!(error = %e, "negative tests failed");
                std::process::exit(1);
            }
        }
        Command::Perf {
            sizes,
            iterations,
            warmup,
        } => {
            if let Err(e) =
                cmd::perf::run(&backend, &cli.heap, sizes.as_deref(), iterations, warmup)
            {
                tracing::error!(error = %e, "perf tests failed");
                std::process::exit(1);
            }
        }
        Command::Pressure { alloc_size } => {
            if let Err(e) = cmd::pressure::run(&backend, &cli.heap, alloc_size) {
                tracing::error!(error = %e, "pressure tests failed");
                std::process::exit(1);
            }
        }
        Command::Fragmentation { pattern } => {
            if let Err(e) = cmd::fragmentation::run(&backend, &cli.heap, &pattern) {
                tracing::error!(error = %e, "fragmentation tests failed");
                std::process::exit(1);
            }
        }
        Command::Pool => {
            if let Err(e) = cmd::pool::run(&backend, &cli.heap) {
                tracing::error!(error = %e, "pool tests failed");
                std::process::exit(1);
            }
        }
        Command::All => {
            run_all(&backend, &cli);
        }
        Command::SysfsDump => {
            run_sysfs_dump();
        }
        Command::Scenario => {
            tracing::info!("scenario command requires subcommand (not yet wired)");
        }
    }
}

/// Run all test stages sequentially with result tracking.
fn run_all<B: backend::HeapBackend + backend::DmaBufBackend + Send + Sync>(backend: &B, cli: &Cli) {
    let mut results = runner::RunResult::new(&cli.heap);
    let heap = cli.heap.clone();

    runner::run_stage(&mut results, "basic", || {
        cmd::basic::run(backend, &heap, &[4096, 65536, 1_048_576], 1024)
    });
    runner::run_stage(&mut results, "sync_file", || {
        cmd::sync_file::run(backend, &heap)
    });
    runner::run_stage(&mut results, "edge", || cmd::edge::run(backend, &heap, 100));
    runner::run_stage(&mut results, "negative", || {
        cmd::negative::run(backend, &heap)
    });
    runner::run_stage(&mut results, "perf", || {
        cmd::perf::run(backend, &heap, None, 100, 10)
    });
    runner::run_stage(&mut results, "pressure", || {
        cmd::pressure::run(backend, &heap, 1_048_576)
    });
    runner::run_stage(&mut results, "fragmentation", || {
        cmd::fragmentation::run(backend, &heap, "interleave")
    });
    runner::run_stage(&mut results, "pool", || cmd::pool::run(backend, &heap));

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
