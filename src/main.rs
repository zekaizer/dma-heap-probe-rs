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
        _ => {
            tracing::info!(command = ?cli.command, "not implemented");
        }
    }
}
