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

    /// Heap names, comma-separated (auto-discovers `/dev/dma_heap/` if omitted).
    #[arg(long, value_delimiter = ',', global = true)]
    pub heaps: Option<Vec<String>>,

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

    /// Increase verbosity (-v=info, -vv=debug, -vvv=trace).
    #[arg(short = 'v', long, action = ArgAction::Count, global = true)]
    pub verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Basic deterministic tests (alloc, mmap, sync, llseek, zeroed, repeated, `sync_file`).
    Basic {
        /// Allocation sizes, comma-separated (e.g. 4096,65536,1048576).
        #[arg(long, value_delimiter = ',', default_values_t = [4096, 65536, 1_048_576])]
        sizes: Vec<u64>,

        /// Repeat count for repeated alloc test.
        #[arg(long, default_value_t = 1024)]
        repeat: u32,
    },

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

        /// Max allocation count for exhaust tests (overrides auto-detection).
        #[arg(long)]
        max_allocs: Option<usize>,
    },

    /// Pool/cache behavior tests.
    Pool,

    /// Negative tests (error paths, invalid input, races).
    Negative,

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

        /// Enable fuzz mode (random size, operation, timing).
        #[arg(long)]
        fuzz: bool,

        /// Max buffers held simultaneously (0 = disable hold pool).
        #[arg(long, default_value_t = 32)]
        max_hold: usize,

        /// Random seed for fuzz mode (auto if omitted).
        #[arg(long)]
        seed: Option<u64>,

        /// Latency trend ratio threshold for failure.
        #[arg(long, default_value_t = 10.0)]
        trend_fail: f64,

        /// Memory leak threshold in MB (0 = disabled).
        #[arg(long, default_value_t = 0)]
        leak_threshold_mb: i64,

        /// Maximum non-ENOMEM error rate (fraction, e.g. 0.01 = 1%).
        #[arg(long, default_value_t = 0.01)]
        max_error_rate: f64,
    },

    /// Latency histogram analysis (per-heap, per-size distribution).
    Histogram {
        /// Allocation sizes, comma-separated (default: 4096,65536,1048576).
        #[arg(long, value_delimiter = ',', default_values_t = [4096, 65536, 1_048_576])]
        sizes: Vec<u64>,

        /// Number of samples per (heap, size) combination.
        #[arg(long, default_value_t = 10_000)]
        samples: u32,

        /// Warmup iterations (excluded from analysis).
        #[arg(long, default_value_t = 500)]
        warmup: u32,

        /// Measurement mode.
        #[arg(long, value_enum, default_value_t = HistMode::AllocOnly)]
        mode: HistMode,

        /// Number of histogram buckets (0 = auto via Sturges' rule).
        #[arg(long, default_value_t = 0)]
        buckets: usize,
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

/// Histogram measurement mode.
#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum HistMode {
    /// Measure alloc ioctl only.
    AllocOnly,
    /// Measure full pipeline (alloc + mmap + sync + fill + sync + close).
    FullPipeline,
    /// Measure close/release path only.
    CloseOnly,
}

impl HistMode {
    /// Return the mode name as a string slice.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllocOnly => "alloc-only",
            Self::FullPipeline => "full-pipeline",
            Self::CloseOnly => "close-only",
        }
    }
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
    fn global_heaps_option() {
        let cli = parse(&["dhp", "basic", "--heaps", "my_heap"]);
        assert_eq!(cli.heaps, Some(vec!["my_heap".to_string()]));
    }

    #[test]
    fn global_heaps_multi() {
        let cli = parse(&["dhp", "basic", "--heaps", "system,reserved"]);
        assert_eq!(
            cli.heaps,
            Some(vec!["system".to_string(), "reserved".to_string()])
        );
    }

    #[test]
    fn global_heaps_omitted() {
        let cli = parse(&["dhp", "basic"]);
        assert!(cli.heaps.is_none());
    }

    #[test]
    fn global_options_all() {
        let cli = parse(&[
            "dhp",
            "basic",
            "--heaps",
            "system",
            "--trace",
            "--sysfs",
            "--procfs",
            "--output",
            "/tmp/out.json",
            "-vv",
        ]);
        assert_eq!(cli.heaps, Some(vec!["system".to_string()]));
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
            "pool",
            "negative",
            "aging",
            "histogram",
            "all",
            "info",
            "sysfs-dump",
        ] {
            let cli = Cli::try_parse_from(["dhp", cmd]);
            assert!(cli.is_ok(), "failed to parse: {cmd}");
        }
    }

    #[test]
    fn verbose_count() {
        let cli = parse(&["dhp", "basic", "-vvv"]);
        assert_eq!(cli.verbose, 3);
    }
}
