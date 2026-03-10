// CLI subcommand definitions for dhp.

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};

/// dma-heap-probe (dhp) — Comprehensive DMA-Heap userspace test tool for Android 16+ (kernel 6.12+).
#[derive(Parser, Debug)]
#[command(
    name = "dhp",
    version,
    about = "dma-heap-probe (dhp) — Comprehensive DMA-Heap userspace test tool for Android 16+ (kernel 6.12+)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Heap name.
    #[arg(long, default_value = "system", global = true)]
    pub heap: String,

    /// Enable Perfetto atrace markers.
    #[arg(long, global = true)]
    pub trace: bool,

    /// Collect sysfs stats before/after.
    #[arg(long, global = true)]
    pub sysfs: bool,

    /// Collect buddyinfo/pagetypeinfo/meminfo/vmstat.
    #[arg(long, global = true)]
    pub procfs: bool,

    /// JSON result output path.
    #[arg(long, global = true)]
    pub output: Option<PathBuf>,

    /// JSON configuration file for scenario workload parameters.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Increase verbosity (-v=info, -vv=debug, -vvv=trace).
    #[arg(short = 'v', long, action = ArgAction::Count, global = true)]
    pub verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Basic tests (alloc, mmap, sync, llseek, zeroed, repeated).
    Basic {
        /// Allocation sizes, comma-separated (e.g. 4096,65536,1048576).
        #[arg(long, value_delimiter = ',', default_values_t = [4096, 65536, 1_048_576])]
        sizes: Vec<u64>,

        /// Repeat count for repeated alloc test.
        #[arg(long, default_value_t = 1024)]
        repeat: u32,
    },

    /// Sync-file export/import tests.
    SyncFile,

    /// Boundary condition tests (concurrent, dup, naming).
    Edge {
        /// Concurrent alloc threads.
        #[arg(long, default_value_t = 100)]
        threads: u32,
    },

    /// Performance measurement.
    Perf {
        /// Measurement sizes, comma-separated (default: 4096,65536,1048576).
        #[arg(long, value_delimiter = ',')]
        sizes: Option<Vec<u64>>,

        /// Iterations per measurement.
        #[arg(long, default_value_t = 100)]
        iterations: u32,

        /// Warmup iterations.
        #[arg(long, default_value_t = 10)]
        warmup: u32,
    },

    /// Memory pressure tests.
    Pressure {
        /// Allocation size for gradual exhaust (bytes).
        #[arg(long, default_value_t = 1_048_576)]
        alloc_size: u64,
    },

    /// Fragmentation analysis.
    Fragmentation {
        /// Allocation pattern type.
        #[arg(long, value_enum, default_value_t = FragPattern::Interleave)]
        pattern: FragPattern,
    },

    /// Pool/cache behavior tests.
    Pool,

    /// Negative tests (error paths, invalid input, races).
    Negative,

    /// Workload scenario simulations.
    Scenario {
        #[command(subcommand)]
        scenario: ScenarioCommand,
    },

    /// Aging tests (sustained alloc/free with periodic reporting).
    Aging {
        /// Allocation size in bytes (ignored in fuzz mode).
        #[arg(long, default_value_t = 4096)]
        size: u64,

        /// Worker thread count.
        #[arg(long, default_value_t = 1)]
        threads: u32,

        /// Max duration in seconds (omit for infinite).
        #[arg(long)]
        duration: Option<u64>,

        /// Max iterations (omit for no limit).
        #[arg(long)]
        iterations: Option<u64>,

        /// Report interval in seconds.
        #[arg(long, default_value_t = 30)]
        report_interval: u64,

        /// Comma-separated heap names (auto-discover `/dev/dma_heap/` if omitted).
        #[arg(long, value_delimiter = ',')]
        heaps: Option<Vec<String>>,

        /// Enable fuzz mode (random size, operation, timing).
        #[arg(long)]
        fuzz: bool,

        /// Max buffers held simultaneously (fuzz mode only).
        #[arg(long, default_value_t = 32)]
        max_hold: usize,

        /// Random seed for fuzz mode (auto if omitted).
        #[arg(long)]
        seed: Option<u64>,
    },

    /// Run all tests including scenarios.
    All,

    /// Display system DMA heap information and buffer status.
    Info {
        /// Show individual buffer list (requires debugfs access for full detail).
        #[arg(long)]
        detail: bool,
    },

    /// Standalone sysfs/procfs snapshot.
    SysfsDump,
}

/// Fragmentation allocation pattern.
#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum FragPattern {
    /// Interleave: free every other buffer.
    Interleave,
    /// Sequential: free first half of buffers.
    Sequential,
}

impl FragPattern {
    /// Return the pattern name as a string slice.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interleave => "interleave",
            Self::Sequential => "sequential",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum ScenarioCommand {
    /// NPU inference workload simulation.
    Npu {
        /// Inference iterations (default: 100).
        #[arg(long)]
        iterations: Option<u32>,

        /// Concurrent client threads (default: 4).
        #[arg(long)]
        clients: Option<u32>,
    },

    /// Camera capture/preview workload simulation.
    Camera {
        /// Capture width (default: 1920).
        #[arg(long)]
        width: Option<u32>,

        /// Capture height (default: 1080).
        #[arg(long)]
        height: Option<u32>,

        /// Number of frames to simulate (default: 100).
        #[arg(long)]
        frames: Option<u32>,
    },

    /// Display compositor workload simulation.
    Display {
        /// Display width (default: 1440).
        #[arg(long)]
        width: Option<u32>,

        /// Display height (default: 3200).
        #[arg(long)]
        height: Option<u32>,

        /// Number of frames to simulate (default: 120).
        #[arg(long)]
        frames: Option<u32>,
    },

    /// Video codec (decoder DPB) workload simulation.
    Codec {
        /// Video width (default: 3840).
        #[arg(long)]
        width: Option<u32>,

        /// Video height (default: 2160).
        #[arg(long)]
        height: Option<u32>,

        /// Number of frames to simulate (default: 60).
        #[arg(long)]
        frames: Option<u32>,
    },

    /// GPU texture/render workload simulation.
    Gpu {
        /// Number of buffers (default: 50).
        #[arg(long)]
        buffer_count: Option<usize>,

        /// Texture buffer size in bytes (default: 1048576).
        #[arg(long)]
        texture_size: Option<u64>,
    },

    /// Combined camera+display+codec+npu pipeline simulation.
    Pipeline {
        /// Number of frames to simulate (default: 30).
        #[arg(long)]
        frames: Option<u32>,
    },

    /// Dump default scenario configuration as JSON.
    DumpConfig,

    /// Run all scenario simulations.
    All,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("parse failed")
    }

    #[test]
    fn basic_defaults() {
        let cli = parse(&["dhp", "basic"]);
        match cli.command {
            Command::Basic { sizes, repeat } => {
                assert_eq!(sizes, vec![4096, 65536, 1_048_576]);
                assert_eq!(repeat, 1024);
            }
            _ => panic!("expected Basic"),
        }
    }

    #[test]
    fn basic_custom_sizes() {
        let cli = parse(&["dhp", "basic", "--sizes", "1024,2048"]);
        match cli.command {
            Command::Basic { sizes, .. } => {
                assert_eq!(sizes, vec![1024, 2048]);
            }
            _ => panic!("expected Basic"),
        }
    }

    #[test]
    fn basic_custom_repeat() {
        let cli = parse(&["dhp", "basic", "--repeat", "10"]);
        match cli.command {
            Command::Basic { repeat, .. } => {
                assert_eq!(repeat, 10);
            }
            _ => panic!("expected Basic"),
        }
    }

    #[test]
    fn global_heap_option() {
        let cli = parse(&["dhp", "basic", "--heap", "my_heap"]);
        assert_eq!(cli.heap, "my_heap");
    }

    #[test]
    fn global_options_all() {
        let cli = parse(&[
            "dhp",
            "basic",
            "--trace",
            "--sysfs",
            "--procfs",
            "--output",
            "/tmp/out.json",
            "-vv",
        ]);
        assert!(cli.trace);
        assert!(cli.sysfs);
        assert!(cli.procfs);
        assert_eq!(cli.output, Some(PathBuf::from("/tmp/out.json")));
        assert_eq!(cli.verbose, 2);
    }

    #[test]
    fn edge_defaults() {
        let cli = parse(&["dhp", "edge"]);
        match cli.command {
            Command::Edge { threads } => assert_eq!(threads, 100),
            _ => panic!("expected Edge"),
        }
    }

    #[test]
    fn edge_custom_threads() {
        let cli = parse(&["dhp", "edge", "--threads", "50"]);
        match cli.command {
            Command::Edge { threads } => assert_eq!(threads, 50),
            _ => panic!("expected Edge"),
        }
    }

    #[test]
    fn sync_file_command() {
        let cli = parse(&["dhp", "sync-file"]);
        assert!(matches!(cli.command, Command::SyncFile));
    }

    #[test]
    fn info_defaults() {
        let cli = parse(&["dhp", "info"]);
        match cli.command {
            Command::Info { detail } => assert!(!detail),
            _ => panic!("expected Info"),
        }
    }

    #[test]
    fn info_with_detail() {
        let cli = parse(&["dhp", "info", "--detail"]);
        match cli.command {
            Command::Info { detail } => assert!(detail),
            _ => panic!("expected Info"),
        }
    }

    #[test]
    fn stub_commands() {
        for cmd in &[
            "perf",
            "pressure",
            "fragmentation",
            "pool",
            "negative",
            "aging",
            "all",
            "info",
            "sysfs-dump",
        ] {
            let cli = Cli::try_parse_from(["dhp", cmd]);
            assert!(cli.is_ok(), "failed to parse: {cmd}");
        }
    }

    #[test]
    fn scenario_subcommands() {
        for sub in &[
            "npu",
            "camera",
            "display",
            "codec",
            "gpu",
            "pipeline",
            "dump-config",
            "all",
        ] {
            let cli = Cli::try_parse_from(["dhp", "scenario", sub]);
            assert!(cli.is_ok(), "failed to parse: scenario {sub}");
        }
    }

    #[test]
    fn scenario_npu_custom_args() {
        let cli = parse(&[
            "dhp",
            "scenario",
            "npu",
            "--iterations",
            "50",
            "--clients",
            "2",
        ]);
        match cli.command {
            Command::Scenario {
                scenario:
                    ScenarioCommand::Npu {
                        iterations,
                        clients,
                    },
            } => {
                assert_eq!(iterations, Some(50));
                assert_eq!(clients, Some(2));
            }
            _ => panic!("expected Scenario::Npu"),
        }
    }

    #[test]
    fn scenario_npu_defaults_none() {
        let cli = parse(&["dhp", "scenario", "npu"]);
        match cli.command {
            Command::Scenario {
                scenario:
                    ScenarioCommand::Npu {
                        iterations,
                        clients,
                    },
            } => {
                assert!(iterations.is_none());
                assert!(clients.is_none());
            }
            _ => panic!("expected Scenario::Npu"),
        }
    }

    #[test]
    fn config_option() {
        let cli = parse(&["dhp", "--config", "/tmp/cfg.json", "scenario", "npu"]);
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/cfg.json")));
    }

    #[test]
    fn dump_config_subcommand() {
        let cli = parse(&["dhp", "scenario", "dump-config"]);
        assert!(matches!(
            cli.command,
            Command::Scenario {
                scenario: ScenarioCommand::DumpConfig
            }
        ));
    }

    #[test]
    fn scenario_requires_subcommand() {
        let result = Cli::try_parse_from(["dhp", "scenario"]);
        assert!(result.is_err());
    }

    #[test]
    fn verbose_count() {
        let cli = parse(&["dhp", "basic", "-vvv"]);
        assert_eq!(cli.verbose, 3);
    }
}
