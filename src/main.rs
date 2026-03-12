#[allow(dead_code)]
mod backend;
mod cli;
mod cmd;
mod config;
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

use cli::{Cli, Command, ScenarioCommand};

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

    match cli.command {
        Command::Basic { sizes, repeat } => {
            let start = Instant::now();
            let (sub, err) = cmd::basic::run(&backend, &cli.heap, &sizes, repeat);
            handle_cmd_output(
                "basic",
                &cli.heap,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::SyncFile => {
            let start = Instant::now();
            let (sub, err) = cmd::sync_file::run(&backend, &cli.heap);
            handle_cmd_output(
                "sync_file",
                &cli.heap,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Edge { threads } => {
            let start = Instant::now();
            let (sub, err) = cmd::edge::run(&backend, &cli.heap, threads);
            handle_cmd_output(
                "edge",
                &cli.heap,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Negative => {
            let start = Instant::now();
            let (sub, err) = cmd::negative::run(&backend, &cli.heap);
            handle_cmd_output(
                "negative",
                &cli.heap,
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
            let (sub, err) =
                cmd::perf::run(&backend, &cli.heap, sizes.as_deref(), iterations, warmup);
            handle_cmd_output(
                "perf",
                &cli.heap,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Pressure { alloc_size } => {
            let start = Instant::now();
            let (sub, err) = cmd::pressure::run(&backend, &cli.heap, alloc_size);
            handle_cmd_output(
                "pressure",
                &cli.heap,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Fragmentation { pattern } => {
            let start = Instant::now();
            let (sub, err) = cmd::fragmentation::run(&backend, &cli.heap, pattern.as_str());
            handle_cmd_output(
                "fragmentation",
                &cli.heap,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Pool => {
            let start = Instant::now();
            let (sub, err) = cmd::pool::run(&backend, &cli.heap);
            handle_cmd_output(
                "pool",
                &cli.heap,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::Info { detail } => {
            let heap_filter = if detail {
                Some(cli.heap.as_str())
            } else {
                None
            };
            if let Err(e) = cmd::info::run(detail, heap_filter, cli.procfs, cli.output.as_ref()) {
                tracing::error!(error = %e, "info command failed");
                std::process::exit(1);
            }
        }
        Command::Aging {
            size,
            threads,
            duration,
            iterations,
            report_interval,
            heaps,
            fuzz,
            max_hold,
            seed,
            trend_fail,
            leak_threshold_mb,
            max_error_rate,
        } => {
            let heap_names = probe::discover_heaps(heaps.as_deref());
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
                &heap_names,
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
                &cli.heap,
                cli.output.as_ref(),
                &sub,
                err,
                start.elapsed(),
            );
        }
        Command::All => {
            let json_cfg = load_scenario_config(&cli);
            run_all(&backend, &cli, json_cfg.as_ref());
        }
        Command::SysfsDump => {
            run_sysfs_dump();
        }
        Command::Scenario {
            scenario: ScenarioCommand::DumpConfig,
        } => {
            println!("{}", config::dump_default_config());
        }
        Command::Scenario { ref scenario } => {
            let json_cfg = load_scenario_config(&cli);
            run_scenario(&backend, &cli, scenario, json_cfg.as_ref());
        }
    }
}

/// Load scenario config from JSON file if --config is provided.
fn load_scenario_config(cli: &Cli) -> Option<config::ScenarioConfigs> {
    cli.config.as_ref().map(|p| {
        config::load_config(p).unwrap_or_else(|e| {
            tracing::error!(error = %e, path = %p.display(), "failed to load config");
            std::process::exit(1);
        })
    })
}

/// Dispatch a scenario subcommand.
fn run_scenario<B: backend::HeapBackend + backend::DmaBufBackend + Send + Sync>(
    backend: &B,
    cli: &Cli,
    scenario: &ScenarioCommand,
    json: Option<&config::ScenarioConfigs>,
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
        ScenarioCommand::DumpConfig => unreachable!(),
    };

    let start = Instant::now();
    let (sub, err) = match *scenario {
        ScenarioCommand::Npu {
            iterations,
            clients,
        } => {
            let cfg = config::resolve_npu(json.and_then(|c| c.npu.as_ref()), iterations, clients);
            npu::run(backend, &cli.heap, &cfg)
        }
        ScenarioCommand::Camera {
            width,
            height,
            frames,
        } => {
            let cfg =
                config::resolve_camera(json.and_then(|c| c.camera.as_ref()), width, height, frames);
            camera::run(backend, &cli.heap, &cfg)
        }
        ScenarioCommand::Display {
            width,
            height,
            frames,
        } => {
            let cfg = config::resolve_display(
                json.and_then(|c| c.display.as_ref()),
                width,
                height,
                frames,
            );
            display::run(backend, &cli.heap, &cfg)
        }
        ScenarioCommand::Codec {
            width,
            height,
            frames,
        } => {
            let cfg =
                config::resolve_codec(json.and_then(|c| c.codec.as_ref()), width, height, frames);
            codec::run(backend, &cli.heap, &cfg)
        }
        ScenarioCommand::Gpu {
            buffer_count,
            texture_size,
        } => {
            let cfg = config::resolve_gpu(
                json.and_then(|c| c.gpu.as_ref()),
                buffer_count,
                texture_size,
            );
            gpu::run(backend, &cli.heap, &cfg)
        }
        ScenarioCommand::Pipeline { frames } => {
            let cfg = config::resolve_pipeline(json.and_then(|c| c.pipeline.as_ref()), frames);
            pipeline::run(backend, &cli.heap, &cfg)
        }
        ScenarioCommand::All => run_all_scenarios(backend, &cli.heap, json),
        ScenarioCommand::DumpConfig => unreachable!(),
    };

    handle_cmd_output(
        stage_name,
        &cli.heap,
        cli.output.as_ref(),
        &sub,
        err,
        start.elapsed(),
    );
}

/// Run all scenario simulations with optional JSON config.
fn run_all_scenarios<B: backend::HeapBackend + backend::DmaBufBackend + Send + Sync>(
    backend: &B,
    heap: &str,
    json: Option<&config::ScenarioConfigs>,
) -> (
    Vec<runner::SubTestResult>,
    Option<Box<dyn std::error::Error>>,
) {
    use cmd::scenario::{camera, codec, display, gpu, npu, pipeline};

    let cfgs = config::resolve_all_defaults(json);

    let mut all_results = Vec::with_capacity(36);

    macro_rules! run_scenario {
        ($run:expr) => {{
            let (sub, err) = $run;
            all_results.extend(sub);
            if err.is_some() {
                return (all_results, err);
            }
        }};
    }

    run_scenario!(npu::run(backend, heap, &cfgs.npu));
    run_scenario!(camera::run(backend, heap, &cfgs.camera));
    run_scenario!(display::run(backend, heap, &cfgs.display));
    run_scenario!(codec::run(backend, heap, &cfgs.codec));
    run_scenario!(gpu::run(backend, heap, &cfgs.gpu));
    run_scenario!(pipeline::run(backend, heap, &cfgs.pipeline));

    (all_results, None)
}

/// Run all test stages sequentially with result tracking.
fn run_all<B: backend::HeapBackend + backend::DmaBufBackend + Send + Sync>(
    backend: &B,
    cli: &Cli,
    json: Option<&config::ScenarioConfigs>,
) {
    /// Helper to adapt `(Vec<SubTestResult>, Option<Error>)` to `run_stage` closure.
    fn stage_result(
        r: (
            Vec<runner::SubTestResult>,
            Option<Box<dyn std::error::Error>>,
        ),
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
        stage_result(cmd::basic::run(
            backend,
            &heap,
            &[4096, 65536, 1_048_576],
            8,
        ))
    });
    runner::run_stage(&mut results, "sync_file", || {
        stage_result(cmd::sync_file::run(backend, &heap))
    });
    runner::run_stage(&mut results, "edge", || {
        stage_result(cmd::edge::run(backend, &heap, 4))
    });
    runner::run_stage(&mut results, "negative", || {
        stage_result(cmd::negative::run(backend, &heap))
    });
    runner::run_stage(&mut results, "perf", || {
        stage_result(cmd::perf::run(backend, &heap, None, 10, 2))
    });
    runner::run_stage(&mut results, "pressure", || {
        stage_result(cmd::pressure::run(backend, &heap, 4096))
    });
    runner::run_stage(&mut results, "fragmentation", || {
        stage_result(cmd::fragmentation::run(backend, &heap, "interleave"))
    });
    runner::run_stage(&mut results, "pool", || {
        stage_result(cmd::pool::run(backend, &heap))
    });

    let cfgs = config::resolve_all_defaults(json);

    runner::run_stage(&mut results, "scenario_npu", || {
        stage_result(cmd::scenario::npu::run(backend, &heap, &cfgs.npu))
    });
    runner::run_stage(&mut results, "scenario_camera", || {
        stage_result(cmd::scenario::camera::run(backend, &heap, &cfgs.camera))
    });
    runner::run_stage(&mut results, "scenario_display", || {
        stage_result(cmd::scenario::display::run(backend, &heap, &cfgs.display))
    });
    runner::run_stage(&mut results, "scenario_codec", || {
        stage_result(cmd::scenario::codec::run(backend, &heap, &cfgs.codec))
    });
    runner::run_stage(&mut results, "scenario_gpu", || {
        stage_result(cmd::scenario::gpu::run(backend, &heap, &cfgs.gpu))
    });
    runner::run_stage(&mut results, "scenario_pipeline", || {
        stage_result(cmd::scenario::pipeline::run(backend, &heap, &cfgs.pipeline))
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
