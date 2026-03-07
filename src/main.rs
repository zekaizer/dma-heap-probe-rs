// Infrastructure modules — used by subsequent phases (cmd/, runner, etc.)
#[allow(dead_code)]
mod backend;
mod cli;
#[allow(dead_code)]
mod dmabuf;
#[allow(dead_code)]
mod heap;
#[allow(dead_code)]
mod ioctl;

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

    match cli.command {
        Command::Basic { ref sizes, repeat } => {
            tracing::info!(heap = %cli.heap, ?sizes, repeat, "basic: not implemented");
        }
        Command::SyncFile => {
            tracing::info!(heap = %cli.heap, "sync-file: not implemented");
        }
        Command::Edge { threads } => {
            tracing::info!(heap = %cli.heap, threads, "edge: not implemented");
        }
        _ => {
            tracing::info!(heap = %cli.heap, command = ?cli.command, "not implemented");
        }
    }
}
