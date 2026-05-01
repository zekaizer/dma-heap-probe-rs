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

    /// Log file path (tee all output to file).
    #[arg(long, global = true)]
    pub log: Option<PathBuf>,

    /// Log file tracing verbosity (default: trace). Only effective with `--log`.
    #[arg(long, global = true, default_value_t = LogLevel::Trace)]
    pub log_level: LogLevel,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Basic deterministic smoke tests (alloc, mmap, sync, llseek, zeroed,
    /// `sync_file`, dup). Sweeps `--sizes` at the suite level: each size runs
    /// the full suite once. Heavy/repetitive coverage is in `aging` /
    /// `histogram` / `microbench`.
    Basic {
        /// Allocation sizes, comma-separated (e.g. 4096,65536,1048576).
        #[arg(long, value_delimiter = ',', default_values_t = [4096, 65536, 1_048_576])]
        sizes: Vec<u64>,
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

        /// Drain heap page pool before each measurement to bypass pool fast-path.
        #[arg(long)]
        pool_bypass: bool,

        /// Explicit drain buffer count for pool bypass (auto-estimated if omitted).
        #[arg(long)]
        drain_count: Option<u32>,
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

    /// Negative tests (error paths, invalid input, races).
    Negative,

    /// Aging tests (sustained alloc/free with periodic reporting).
    Aging {
        /// Allocation size, e.g. 4096, 64K, 1M (default: 4096; fuzz mode: max size cap).
        #[arg(long)]
        size: Option<String>,

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

        /// Max hold limit: count (e.g. 1000) or size (e.g. 512M, 1GiB). 0 = disable.
        #[arg(long, default_value = "32")]
        max_hold: String,

        /// Normal (non-fuzz) mode only: microseconds to sleep after each
        /// immediate close(fd) to let a deferred dma-buf workqueue drain
        /// before the next alloc. 0 = no sleep (default).
        ///
        /// Needed on Android ACK android15-6.6+: `dma_buf_stats_setup`
        /// takes `get_dma_buf()` and defers `kobject_init_and_add` to a
        /// workqueue (`sysfs_add_workfn`). Until that work runs, file
        /// refcount stays >0, `__fput`/heap `.release` is NOT invoked by
        /// close(fd). A tight aging loop builds a backlog that can
        /// exhaust heap pools before release catches up. A small sleep
        /// (50-200us) lets the workqueue keep pace.
        ///
        /// Applies only to the non-hold iter path (see --max-hold). Not
        /// applied to fuzz mode or hold-pool eviction/drain.
        #[arg(long, default_value_t = 0)]
        close_settle_us: u64,

        /// Random seed for fuzz mode (auto if omitted).
        #[arg(long)]
        seed: Option<u64>,
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

        /// Drain heap page pool before each measurement to bypass pool fast-path.
        #[arg(long)]
        pool_bypass: bool,

        /// Explicit drain buffer count for pool bypass (auto-estimated if omitted).
        #[arg(long)]
        drain_count: Option<u32>,
    },

    /// Micro-benchmark individual DMA heap/buf operations with environment control.
    Microbench {
        /// Operations to benchmark (comma-separated, default: all).
        /// Available: `alloc`, `mmap`, `munmap`, `sync_start_w`, `sync_end_w`,
        /// `sync_start_r`, `sync_end_r`, `sync_start_rw`, `sync_end_rw`,
        /// `close`, `dup`, `llseek`, `export_sync_file`, `import_sync_file`, `pipeline`.
        #[arg(long)]
        ops: Option<String>,

        /// Allocation sizes, comma-separated (default: 4096,65536,1048576).
        #[arg(long, value_delimiter = ',', default_values_t = [4096, 65536, 1_048_576])]
        sizes: Vec<u64>,

        /// Measurement iterations per (op, size).
        #[arg(long, default_value_t = 1000)]
        iterations: u32,

        /// Warmup iterations (excluded from measurement).
        #[arg(long, default_value_t = 100)]
        warmup: u32,

        /// CPU core to pin (default: auto-detect fastest core).
        #[arg(long)]
        cpu: Option<u32>,

        /// Disable environment control (CPU freq/affinity/priority).
        #[arg(long)]
        no_env_control: bool,
    },

    /// Samsung dma-buf container tests (merge, mask, cross-heap).
    Container,

    /// Run all test stages (basic, negative, perf, pressure, container).
    All,

    /// Display system DMA heap information and buffer status.
    Info {
        /// Show individual buffer list (requires debugfs access for full detail).
        #[arg(long)]
        detail: bool,

        /// Probe heap capabilities (alloc, mmap, sync, dup, granularity).
        #[arg(long)]
        probe: bool,

        /// Dump raw sysfs/procfs snapshot (JSON).
        #[arg(long)]
        dump: bool,

        /// Continuous monitoring mode (periodic compact status line).
        #[arg(long)]
        follow: bool,

        /// Monitoring interval in seconds (used with `--follow`).
        #[arg(long, default_value_t = 5)]
        interval: u64,
    },
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

/// Log file tracing verbosity level.
#[derive(ValueEnum, Debug, Clone, Copy, Default)]
pub enum LogLevel {
    /// Only errors.
    Error,
    /// Warnings and errors.
    Warn,
    /// Informational messages.
    Info,
    /// Debug-level messages.
    Debug,
    /// All trace-level messages.
    #[default]
    Trace,
}

impl From<LogLevel> for tracing_subscriber::filter::LevelFilter {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Error => Self::ERROR,
            LogLevel::Warn => Self::WARN,
            LogLevel::Info => Self::INFO,
            LogLevel::Debug => Self::DEBUG,
            LogLevel::Trace => Self::TRACE,
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error => write!(f, "error"),
            Self::Warn => write!(f, "warn"),
            Self::Info => write!(f, "info"),
            Self::Debug => write!(f, "debug"),
            Self::Trace => write!(f, "trace"),
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
            Command::Basic { sizes } => {
                assert_eq!(sizes, vec![4096, 65536, 1_048_576]);
            }
            _ => panic!("expected Basic"),
        }
    }

    #[test]
    fn basic_custom_sizes() {
        let cli = parse(&["dhp", "basic", "--sizes", "1024,2048"]);
        match cli.command {
            Command::Basic { sizes } => {
                assert_eq!(sizes, vec![1024, 2048]);
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
            "--log",
            "/tmp/dhp.log",
            "--log-level",
            "debug",
            "-vv",
        ]);
        assert_eq!(cli.heaps, Some(vec!["system".to_string()]));
        assert!(cli.trace);
        assert!(cli.sysfs);
        assert!(cli.procfs);
        assert_eq!(cli.output, Some(PathBuf::from("/tmp/out.json")));
        assert_eq!(cli.log, Some(PathBuf::from("/tmp/dhp.log")));
        assert!(matches!(cli.log_level, LogLevel::Debug));
        assert_eq!(cli.verbose, 2);
    }

    #[test]
    fn info_defaults() {
        let cli = parse(&["dhp", "info"]);
        match cli.command {
            Command::Info {
                detail,
                probe,
                dump,
                follow,
                interval,
            } => {
                assert!(!detail);
                assert!(!probe);
                assert!(!dump);
                assert!(!follow);
                assert_eq!(interval, 5);
            }
            _ => panic!("expected Info"),
        }
    }

    #[test]
    fn info_with_detail() {
        let cli = parse(&["dhp", "info", "--detail"]);
        match cli.command {
            Command::Info { detail, .. } => assert!(detail),
            _ => panic!("expected Info"),
        }
    }

    #[test]
    fn info_with_dump() {
        let cli = parse(&["dhp", "info", "--dump"]);
        match cli.command {
            Command::Info { dump, .. } => assert!(dump),
            _ => panic!("expected Info"),
        }
    }

    #[test]
    fn info_with_follow() {
        let cli = parse(&["dhp", "info", "--follow", "--interval", "2"]);
        match cli.command {
            Command::Info {
                follow, interval, ..
            } => {
                assert!(follow);
                assert_eq!(interval, 2);
            }
            _ => panic!("expected Info"),
        }
    }

    #[test]
    fn perf_pool_bypass_defaults() {
        let cli = parse(&["dhp", "perf"]);
        match cli.command {
            Command::Perf {
                pool_bypass,
                drain_count,
                ..
            } => {
                assert!(!pool_bypass);
                assert!(drain_count.is_none());
            }
            _ => panic!("expected Perf"),
        }
    }

    #[test]
    fn perf_pool_bypass_enabled() {
        let cli = parse(&["dhp", "perf", "--pool-bypass", "--drain-count", "512"]);
        match cli.command {
            Command::Perf {
                pool_bypass,
                drain_count,
                ..
            } => {
                assert!(pool_bypass);
                assert_eq!(drain_count, Some(512));
            }
            _ => panic!("expected Perf"),
        }
    }

    #[test]
    fn histogram_pool_bypass() {
        let cli = parse(&["dhp", "histogram", "--pool-bypass"]);
        match cli.command {
            Command::Histogram {
                pool_bypass,
                drain_count,
                ..
            } => {
                assert!(pool_bypass);
                assert!(drain_count.is_none());
            }
            _ => panic!("expected Histogram"),
        }
    }

    #[test]
    fn stub_commands() {
        for cmd in &[
            "perf",
            "pressure",
            "negative",
            "aging",
            "histogram",
            "microbench",
            "container",
            "all",
            "info",
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

    #[test]
    fn microbench_defaults() {
        let cli = parse(&["dhp", "microbench"]);
        match cli.command {
            Command::Microbench {
                ops,
                sizes,
                iterations,
                warmup,
                cpu,
                no_env_control,
            } => {
                assert!(ops.is_none());
                assert_eq!(sizes, vec![4096, 65536, 1_048_576]);
                assert_eq!(iterations, 1000);
                assert_eq!(warmup, 100);
                assert!(cpu.is_none());
                assert!(!no_env_control);
            }
            _ => panic!("expected Microbench"),
        }
    }

    #[test]
    fn microbench_custom_ops() {
        let cli = parse(&[
            "dhp",
            "microbench",
            "--ops",
            "alloc,mmap,close",
            "--sizes",
            "4096",
            "--iterations",
            "50",
            "--warmup",
            "5",
            "--cpu",
            "7",
            "--no-env-control",
        ]);
        match cli.command {
            Command::Microbench {
                ops,
                sizes,
                iterations,
                warmup,
                cpu,
                no_env_control,
            } => {
                assert_eq!(ops, Some("alloc,mmap,close".to_string()));
                assert_eq!(sizes, vec![4096]);
                assert_eq!(iterations, 50);
                assert_eq!(warmup, 5);
                assert_eq!(cpu, Some(7));
                assert!(no_env_control);
            }
            _ => panic!("expected Microbench"),
        }
    }

    #[test]
    fn log_option() {
        let cli = parse(&["dhp", "basic", "--log", "/tmp/dhp.log"]);
        assert_eq!(cli.log, Some(PathBuf::from("/tmp/dhp.log")));
    }

    #[test]
    fn log_level_default() {
        let cli = parse(&["dhp", "basic"]);
        assert!(matches!(cli.log_level, LogLevel::Trace));
    }

    #[test]
    fn log_level_custom() {
        let cli = parse(&["dhp", "basic", "--log-level", "debug"]);
        assert!(matches!(cli.log_level, LogLevel::Debug));
    }
}
