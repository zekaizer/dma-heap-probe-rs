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

use std::path::PathBuf;
use std::time::Instant;

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
            let start = Instant::now();
            let (sub, err) = cmd::basic::run(&backend, &cli.heap, &sizes, repeat);
            handle_cmd_output("basic", &cli.heap, cli.output.as_ref(), &sub, err, start.elapsed());
        }
        Command::SyncFile => {
            let start = Instant::now();
            let (sub, err) = cmd::sync_file::run(&backend, &cli.heap);
            handle_cmd_output("sync_file", &cli.heap, cli.output.as_ref(), &sub, err, start.elapsed());
        }
        Command::Edge { threads } => {
            let start = Instant::now();
            let (sub, err) = cmd::edge::run(&backend, &cli.heap, threads);
            handle_cmd_output("edge", &cli.heap, cli.output.as_ref(), &sub, err, start.elapsed());
        }
        Command::Negative => {
            let start = Instant::now();
            let (sub, err) = cmd::negative::run(&backend, &cli.heap);
            handle_cmd_output("negative", &cli.heap, cli.output.as_ref(), &sub, err, start.elapsed());
        }
        Command::Perf {
            sizes,
            iterations,
            warmup,
        } => {
            let start = Instant::now();
            let (sub, err) =
                cmd::perf::run(&backend, &cli.heap, sizes.as_deref(), iterations, warmup);
            handle_cmd_output("perf", &cli.heap, cli.output.as_ref(), &sub, err, start.elapsed());
        }
        Command::Pressure { alloc_size } => {
            let start = Instant::now();
            let (sub, err) = cmd::pressure::run(&backend, &cli.heap, alloc_size);
            handle_cmd_output("pressure", &cli.heap, cli.output.as_ref(), &sub, err, start.elapsed());
        }
        Command::Fragmentation { pattern } => {
            let start = Instant::now();
            let (sub, err) =
                cmd::fragmentation::run(&backend, &cli.heap, pattern.as_str());
            handle_cmd_output("fragmentation", &cli.heap, cli.output.as_ref(), &sub, err, start.elapsed());
        }
        Command::Pool => {
            let start = Instant::now();
            let (sub, err) = cmd::pool::run(&backend, &cli.heap);
            handle_cmd_output("pool", &cli.heap, cli.output.as_ref(), &sub, err, start.elapsed());
        }
        Command::All => {
            run_all(&backend, &cli);
        }
        Command::SysfsDump => {
            run_sysfs_dump();
        }
        Command::Scenario { ref scenario } => {
            run_scenario(&backend, &cli, scenario);
        }
    }
}

/// Dispatch a scenario subcommand.
fn run_scenario<B: backend::HeapBackend + backend::DmaBufBackend + Send + Sync>(
    backend: &B,
    cli: &Cli,
    scenario: &ScenarioCommand,
) {
    use cmd::scenario::{camera, codec, display, gpu, npu, pipeline};

    let stage_name = match *scenario {
        ScenarioCommand::Npu { .. } => "scenario_npu",
        ScenarioCommand::Camera { .. } => "scenario_camera",
        ScenarioCommand::Display { .. } => "scenario_display",
        ScenarioCommand::Codec { .. } => "scenario_codec",
        ScenarioCommand::Gpu { .. } => "scenario_gpu",
        ScenarioCommand::Pipeline { .. } => "scenario_pipeline",
        ScenarioCommand::All => "scenario_all",
    };

    let start = Instant::now();
    let (sub, err) = match *scenario {
        ScenarioCommand::Npu {
            iterations,
            clients,
        } => npu::run(
            backend,
            &cli.heap,
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
            &cli.heap,
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
            &cli.heap,
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
            &cli.heap,
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
            &cli.heap,
            &gpu::GpuConfig {
                buffer_count,
                texture_size,
                ..Default::default()
            },
        ),
        ScenarioCommand::Pipeline { frames } => pipeline::run(
            backend,
            &cli.heap,
            &pipeline::PipelineConfig {
                frames,
                ..Default::default()
            },
        ),
        ScenarioCommand::All => run_all_scenarios(backend, &cli.heap),
    };

    handle_cmd_output(stage_name, &cli.heap, cli.output.as_ref(), &sub, err, start.elapsed());
}

/// Run all scenario simulations with default configs.
fn run_all_scenarios<B: backend::HeapBackend + backend::DmaBufBackend + Send + Sync>(
    backend: &B,
    heap: &str,
) -> (Vec<runner::SubTestResult>, Option<Box<dyn std::error::Error>>) {
    use cmd::scenario::{camera, codec, display, gpu, npu, pipeline};

    let mut all_results = Vec::with_capacity(36);

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
    /// Helper to adapt `(Vec<SubTestResult>, Option<Error>)` to `run_stage` closure.
    fn stage_result(
        r: (Vec<runner::SubTestResult>, Option<Box<dyn std::error::Error>>),
    ) -> Result<Option<serde_json::Value>, Box<dyn std::error::Error>> {
        let (sub, err) = r;
        let details = Some(runner::sub_tests_to_details(&sub));
        match err {
            Some(e) => Err(e),
            None => Ok(details),
        }
    }

    let mut results = runner::RunResult::new(&cli.heap);
    let heap = cli.heap.clone();

    runner::run_stage(&mut results, "basic", || {
        stage_result(cmd::basic::run(backend, &heap, &[4096, 65536, 1_048_576], 1024))
    });
    runner::run_stage(&mut results, "sync_file", || {
        stage_result(cmd::sync_file::run(backend, &heap))
    });
    runner::run_stage(&mut results, "edge", || {
        stage_result(cmd::edge::run(backend, &heap, 100))
    });
    runner::run_stage(&mut results, "negative", || {
        stage_result(cmd::negative::run(backend, &heap))
    });
    runner::run_stage(&mut results, "perf", || {
        stage_result(cmd::perf::run(backend, &heap, None, 100, 10))
    });
    runner::run_stage(&mut results, "pressure", || {
        stage_result(cmd::pressure::run(backend, &heap, 1_048_576))
    });
    runner::run_stage(&mut results, "fragmentation", || {
        stage_result(cmd::fragmentation::run(backend, &heap, "interleave"))
    });
    runner::run_stage(&mut results, "pool", || {
        stage_result(cmd::pool::run(backend, &heap))
    });
    runner::run_stage(&mut results, "scenario_npu", || {
        stage_result(cmd::scenario::npu::run(backend, &heap, &cmd::scenario::npu::NpuConfig::default()))
    });
    runner::run_stage(&mut results, "scenario_camera", || {
        stage_result(cmd::scenario::camera::run(
            backend, &heap, &cmd::scenario::camera::CameraConfig::default(),
        ))
    });
    runner::run_stage(&mut results, "scenario_display", || {
        stage_result(cmd::scenario::display::run(
            backend, &heap, &cmd::scenario::display::DisplayConfig::default(),
        ))
    });
    runner::run_stage(&mut results, "scenario_codec", || {
        stage_result(cmd::scenario::codec::run(
            backend, &heap, &cmd::scenario::codec::CodecConfig::default(),
        ))
    });
    runner::run_stage(&mut results, "scenario_gpu", || {
        stage_result(cmd::scenario::gpu::run(backend, &heap, &cmd::scenario::gpu::GpuConfig::default()))
    });
    runner::run_stage(&mut results, "scenario_pipeline", || {
        stage_result(cmd::scenario::pipeline::run(
            backend, &heap, &cmd::scenario::pipeline::PipelineConfig::default(),
        ))
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

/// Handle a single subcommand's output: write JSON if --output, exit(1) on failure.
fn handle_cmd_output(
    stage_name: &str,
    heap: &str,
    output: Option<&PathBuf>,
    sub_tests: &[runner::SubTestResult],
    err: Option<Box<dyn std::error::Error>>,
    duration: std::time::Duration,
) {
    let has_failure = err.is_some();

    if let Some(output_path) = output {
        let details = Some(runner::sub_tests_to_details(sub_tests));
        let mut results = runner::RunResult::new(heap);
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
