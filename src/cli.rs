// CLI subcommand definitions for dhp.

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};

/// DMA-Heap probe tool for Android 16+ (kernel 6.12+).
#[derive(Parser, Debug)]
#[command(name = "dhp", version, about)]
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

    /// Increase verbosity (-v=info, -vv=debug, -vvv=trace).
    #[arg(short = 'v', long, action = ArgAction::Count, global = true)]
    pub verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Stage 1 basic tests (alloc, mmap, sync, llseek, zeroed, repeated).
    Basic {
        /// Allocation sizes (comma-separated).
        #[arg(long, value_delimiter = ',', default_values_t = [4096, 65536, 1_048_576])]
        sizes: Vec<u64>,

        /// Repeat count for `test_repeated_alloc`.
        #[arg(long, default_value_t = 1024)]
        repeat: u32,
    },

    /// Stage 2 `sync_file` export/import tests.
    SyncFile,

    /// Stage 2 boundary conditions (concurrent, dup, `set_name`).
    Edge {
        /// Concurrent alloc threads.
        #[arg(long, default_value_t = 100)]
        threads: u32,
    },

    /// Stage 3 performance measurement.
    Perf {
        /// Measurement sizes (comma-separated).
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

    /// Fragmentation observation.
    Fragmentation {
        /// Pattern type (interleave / sequential).
        #[arg(long, default_value = "interleave")]
        pattern: String,
    },

    /// Pool/cache behavior tests.
    Pool,

    /// Negative tests (error paths, invalid input, races).
    Negative,

    /// Workload scenario simulations.
    Scenario,

    /// Run all tests sequentially.
    All,

    /// Standalone sysfs/procfs snapshot.
    SysfsDump,
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
            "dhp", "basic", "--trace", "--sysfs", "--procfs", "--output", "/tmp/out.json", "-vv",
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
    fn stub_commands() {
        for cmd in &[
            "perf",
            "pressure",
            "fragmentation",
            "pool",
            "negative",
            "scenario",
            "all",
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
