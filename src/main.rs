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

use cli::{Cli, Command, ScenarioCommand};

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
            let (_results, err) = cmd::basic::run(&backend, &cli.heap, &sizes, repeat);
            if let Some(e) = err {
                tracing::error!(error = %e, "basic tests failed");
                std::process::exit(1);
            }
        }
        Command::SyncFile => {
            let (_results, err) = cmd::sync_file::run(&backend, &cli.heap);
            if let Some(e) = err {
                tracing::error!(error = %e, "sync_file tests failed");
                std::process::exit(1);
            }
        }
        Command::Edge { threads } => {
            let (_results, err) = cmd::edge::run(&backend, &cli.heap, threads);
            if let Some(e) = err {
                tracing::error!(error = %e, "edge tests failed");
                std::process::exit(1);
            }
        }
        Command::Negative => {
            let (_results, err) = cmd::negative::run(&backend, &cli.heap);
            if let Some(e) = err {
                tracing::error!(error = %e, "negative tests failed");
                std::process::exit(1);
            }
        }
        Command::Perf {
            sizes,
            iterations,
            warmup,
        } => {
            let (_results, err) =
                cmd::perf::run(&backend, &cli.heap, sizes.as_deref(), iterations, warmup);
            if let Some(e) = err {
                tracing::error!(error = %e, "perf tests failed");
                std::process::exit(1);
            }
        }
        Command::Pressure { alloc_size } => {
            let (_results, err) = cmd::pressure::run(&backend, &cli.heap, alloc_size);
            if let Some(e) = err {
                tracing::error!(error = %e, "pressure tests failed");
                std::process::exit(1);
            }
        }
        Command::Fragmentation { pattern } => {
            let (_results, err) =
                cmd::fragmentation::run(&backend, &cli.heap, pattern.as_str());
            if let Some(e) = err {
                tracing::error!(error = %e, "fragmentation tests failed");
                std::process::exit(1);
            }
        }
        Command::Pool => {
            let (_results, err) = cmd::pool::run(&backend, &cli.heap);
            if let Some(e) = err {
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
        Command::Scenario { ref scenario } => {
            run_scenario(&backend, &cli.heap, scenario);
        }
    }
}

/// Dispatch a scenario subcommand.
fn run_scenario<B: backend::HeapBackend + backend::DmaBufBackend + Send + Sync>(
    backend: &B,
    heap: &str,
    scenario: &ScenarioCommand,
) {
    use cmd::scenario::{camera, codec, display, gpu, npu, pipeline};

    let (_results, err) = match *scenario {
        ScenarioCommand::Npu {
            iterations,
            clients,
        } => npu::run(
            backend,
            heap,
            &npu::NpuConfig {
                iterations,
                clients,
                ..Default::default()
            },
        ),
        ScenarioCommand::Camera {
            width,
            height,
            frames,
        } => camera::run(
            backend,
            heap,
            &camera::CameraConfig {
                width,
                height,
                frames,
                ..Default::default()
            },
        ),
        ScenarioCommand::Display {
            width,
            height,
            frames,
        } => display::run(
            backend,
            heap,
            &display::DisplayConfig {
                width,
                height,
                frames,
                ..Default::default()
            },
        ),
        ScenarioCommand::Codec {
            width,
            height,
            frames,
        } => codec::run(
            backend,
            heap,
            &codec::CodecConfig {
                width,
                height,
                frames,
                ..Default::default()
            },
        ),
        ScenarioCommand::Gpu {
            buffer_count,
            texture_size,
        } => gpu::run(
            backend,
            heap,
            &gpu::GpuConfig {
                buffer_count,
                texture_size,
                ..Default::default()
            },
        ),
        ScenarioCommand::Pipeline { frames } => pipeline::run(
            backend,
            heap,
            &pipeline::PipelineConfig {
                frames,
                ..Default::default()
            },
        ),
        ScenarioCommand::All => run_all_scenarios(backend, heap),
    };

    if let Some(e) = err {
        tracing::error!(error = %e, "scenario tests failed");
        std::process::exit(1);
    }
}

/// Run all scenario simulations with default configs.
fn run_all_scenarios<B: backend::HeapBackend + backend::DmaBufBackend + Send + Sync>(
    backend: &B,
    heap: &str,
) -> (Vec<runner::SubTestResult>, Option<Box<dyn std::error::Error>>) {
    use cmd::scenario::{camera, codec, display, gpu, npu, pipeline};

    let mut all_results = Vec::new();

    for (sub, err) in [
        npu::run(backend, heap, &npu::NpuConfig::default()),
        camera::run(backend, heap, &camera::CameraConfig::default()),
        display::run(backend, heap, &display::DisplayConfig::default()),
        codec::run(backend, heap, &codec::CodecConfig::default()),
        gpu::run(backend, heap, &gpu::GpuConfig::default()),
        pipeline::run(backend, heap, &pipeline::PipelineConfig::default()),
    ] {
        all_results.extend(sub);
        if err.is_some() {
            return (all_results, err);
        }
    }

    (all_results, None)
}

/// Run all test stages sequentially with result tracking.
fn run_all<B: backend::HeapBackend + backend::DmaBufBackend + Send + Sync>(backend: &B, cli: &Cli) {
    let mut results = runner::RunResult::new(&cli.heap);
    let heap = cli.heap.clone();

    runner::run_stage(&mut results, "basic", || {
        let (_sub, err) = cmd::basic::run(backend, &heap, &[4096, 65536, 1_048_576], 1024);
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "sync_file", || {
        let (_sub, err) = cmd::sync_file::run(backend, &heap);
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "edge", || {
        let (_sub, err) = cmd::edge::run(backend, &heap, 100);
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "negative", || {
        let (_sub, err) = cmd::negative::run(backend, &heap);
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "perf", || {
        let (_sub, err) = cmd::perf::run(backend, &heap, None, 100, 10);
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "pressure", || {
        let (_sub, err) = cmd::pressure::run(backend, &heap, 1_048_576);
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "fragmentation", || {
        let (_sub, err) = cmd::fragmentation::run(backend, &heap, "interleave");
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "pool", || {
        let (_sub, err) = cmd::pool::run(backend, &heap);
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "scenario_npu", || {
        let (_sub, err) =
            cmd::scenario::npu::run(backend, &heap, &cmd::scenario::npu::NpuConfig::default());
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "scenario_camera", || {
        let (_sub, err) = cmd::scenario::camera::run(
            backend,
            &heap,
            &cmd::scenario::camera::CameraConfig::default(),
        );
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "scenario_display", || {
        let (_sub, err) = cmd::scenario::display::run(
            backend,
            &heap,
            &cmd::scenario::display::DisplayConfig::default(),
        );
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "scenario_codec", || {
        let (_sub, err) = cmd::scenario::codec::run(
            backend,
            &heap,
            &cmd::scenario::codec::CodecConfig::default(),
        );
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "scenario_gpu", || {
        let (_sub, err) =
            cmd::scenario::gpu::run(backend, &heap, &cmd::scenario::gpu::GpuConfig::default());
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });
    runner::run_stage(&mut results, "scenario_pipeline", || {
        let (_sub, err) = cmd::scenario::pipeline::run(
            backend,
            &heap,
            &cmd::scenario::pipeline::PipelineConfig::default(),
        );
        match err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    });

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
