// Micro-benchmark subcommand: isolated latency measurement of individual
// DMA heap/buf kernel operations under controlled environment.

use std::time::Instant;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{
    DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ, DMA_BUF_SYNC_RW, DMA_BUF_SYNC_START, DMA_BUF_SYNC_WRITE,
};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::SubTestResult;
use crate::stats::{self, LatencyStats};

/// Operations available for micro-benchmarking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Alloc,
    Mmap,
    Munmap,
    SyncStartW,
    SyncEndW,
    SyncStartR,
    SyncEndR,
    SyncStartRw,
    SyncEndRw,
    Close,
    Dup,
    Llseek,
    ExportSyncFile,
    ImportSyncFile,
    Pipeline,
}

impl Op {
    /// Parse comma-separated operation names into a list of ops.
    pub fn parse_list(s: &str) -> Result<Vec<Self>, String> {
        s.split(',')
            .map(|tok| {
                let tok = tok.trim();
                match tok {
                    "alloc" => Ok(Self::Alloc),
                    "mmap" => Ok(Self::Mmap),
                    "munmap" => Ok(Self::Munmap),
                    "sync_start_w" => Ok(Self::SyncStartW),
                    "sync_end_w" => Ok(Self::SyncEndW),
                    "sync_start_r" => Ok(Self::SyncStartR),
                    "sync_end_r" => Ok(Self::SyncEndR),
                    "sync_start_rw" => Ok(Self::SyncStartRw),
                    "sync_end_rw" => Ok(Self::SyncEndRw),
                    "close" => Ok(Self::Close),
                    "dup" => Ok(Self::Dup),
                    "llseek" => Ok(Self::Llseek),
                    "export_sync_file" => Ok(Self::ExportSyncFile),
                    "import_sync_file" => Ok(Self::ImportSyncFile),
                    "pipeline" => Ok(Self::Pipeline),
                    other => Err(format!("unknown op: {other}")),
                }
            })
            .collect()
    }

    fn name(self) -> &'static str {
        match self {
            Self::Alloc => "alloc",
            Self::Mmap => "mmap",
            Self::Munmap => "munmap",
            Self::SyncStartW => "sync_start_w",
            Self::SyncEndW => "sync_end_w",
            Self::SyncStartR => "sync_start_r",
            Self::SyncEndR => "sync_end_r",
            Self::SyncStartRw => "sync_start_rw",
            Self::SyncEndRw => "sync_end_rw",
            Self::Close => "close",
            Self::Dup => "dup",
            Self::Llseek => "llseek",
            Self::ExportSyncFile => "export_sync_file",
            Self::ImportSyncFile => "import_sync_file",
            Self::Pipeline => "pipeline",
        }
    }

    /// All ops in default execution order.
    pub fn all() -> Vec<Self> {
        vec![
            Self::Alloc,
            Self::Mmap,
            Self::Munmap,
            Self::SyncStartW,
            Self::SyncEndW,
            Self::SyncStartR,
            Self::SyncEndR,
            Self::SyncStartRw,
            Self::SyncEndRw,
            Self::Close,
            Self::Dup,
            Self::Llseek,
            Self::ExportSyncFile,
            Self::ImportSyncFile,
            Self::Pipeline,
        ]
    }
}

/// Micro-benchmark configuration for a single heap's benchmark run.
/// Environment control settings live at the dispatch layer and are not
/// passed through here.
pub struct MicrobenchConfig {
    pub ops: Vec<Op>,
    pub sizes: Vec<u64>,
    pub iterations: u32,
    pub warmup: u32,
    pub heap_w: usize,
}

/// Per-heap benchmark results: op name → size → stats.
pub type HeapBenchmarks =
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, LatencyStats>>;

/// Run micro-benchmarks for a single heap. Environment setup and snapshot
/// capture are caller's responsibility (typically done once per session).
#[allow(clippy::too_many_lines)]
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    cfg: &MicrobenchConfig,
) -> (Vec<SubTestResult>, Option<anyhow::Error>, HeapBenchmarks) {
    let caps = crate::probe::probe_heap(backend, heap_name);

    let mut sub_tests = Vec::new();
    let mut first_err: Option<anyhow::Error> = None;
    let mut benchmarks: HeapBenchmarks = std::collections::BTreeMap::new();

    for &op in &cfg.ops {
        // Skip ops that require capabilities the heap doesn't support.
        let skip = match op {
            Op::Mmap
            | Op::Munmap
            | Op::SyncStartW
            | Op::SyncEndW
            | Op::SyncStartR
            | Op::SyncEndR
            | Op::SyncStartRw
            | Op::SyncEndRw
            | Op::Pipeline => !caps.can_mmap,
            Op::Dup => !caps.can_dup,
            Op::Llseek => !caps.can_llseek,
            Op::ExportSyncFile | Op::ImportSyncFile => !caps.can_sync_file,
            Op::Alloc | Op::Close => false,
        };

        if skip {
            sub_tests.push(SubTestResult {
                name: format!("microbench::{}", op.name()),
                passed: true,
                skipped: true,
                error: None,
            });
            continue;
        }

        let result = run_op(backend, heap_name, op, cfg);
        match result {
            Ok(size_stats) => {
                // Print table.
                let mut rows: Vec<Vec<String>> = Vec::new();
                let mut op_map = std::collections::BTreeMap::new();
                for (size, st) in &size_stats {
                    rows.push(vec![
                        human_size(*size),
                        st.avg_us.to_string(),
                        st.stddev_us.to_string(),
                        st.p50_us.to_string(),
                        st.p95_us.to_string(),
                        st.p99_us.to_string(),
                        st.p99_9_us.to_string(),
                        st.throughput_ops.to_string(),
                    ]);
                    op_map.insert(size.to_string(), st.clone());
                }
                crate::fmt::print_table(
                    heap_name,
                    cfg.heap_w,
                    &format!("microbench::{}", op.name()),
                    Some("(us)"),
                    &["size", "avg", "sd", "p50", "p95", "p99", "p99.9", "ops/s"],
                    &rows,
                );
                benchmarks.insert(op.name().to_string(), op_map);
                sub_tests.push(SubTestResult {
                    name: format!("microbench::{}", op.name()),
                    passed: true,
                    skipped: false,
                    error: None,
                });
            }
            Err(e) => {
                let msg = format!("{e}");
                sub_tests.push(SubTestResult {
                    name: format!("microbench::{}", op.name()),
                    passed: false,
                    skipped: false,
                    error: Some(msg.clone()),
                });
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }

    (sub_tests, first_err, benchmarks)
}

// ---------------------------------------------------------------------------
// Individual operation benchmarks
// ---------------------------------------------------------------------------

/// Run a single operation benchmark across all configured sizes.
fn run_op<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    op: Op,
    cfg: &MicrobenchConfig,
) -> anyhow::Result<Vec<(u64, LatencyStats)>> {
    let mut results = Vec::new();
    for &size in &cfg.sizes {
        let samples = match op {
            Op::Alloc => bench_alloc(backend, heap_name, size, cfg.warmup, cfg.iterations)?,
            Op::Mmap => bench_mmap(backend, heap_name, size, cfg.warmup, cfg.iterations)?,
            Op::Munmap => bench_munmap(backend, heap_name, size, cfg.warmup, cfg.iterations)?,
            Op::SyncStartW => bench_sync(
                backend,
                heap_name,
                size,
                DMA_BUF_SYNC_START | DMA_BUF_SYNC_WRITE,
                DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE,
                cfg.warmup,
                cfg.iterations,
            )?,
            Op::SyncEndW => bench_sync(
                backend,
                heap_name,
                size,
                DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE,
                DMA_BUF_SYNC_START | DMA_BUF_SYNC_WRITE,
                cfg.warmup,
                cfg.iterations,
            )?,
            Op::SyncStartR => bench_sync(
                backend,
                heap_name,
                size,
                DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ,
                DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ,
                cfg.warmup,
                cfg.iterations,
            )?,
            Op::SyncEndR => bench_sync(
                backend,
                heap_name,
                size,
                DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ,
                DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ,
                cfg.warmup,
                cfg.iterations,
            )?,
            Op::SyncStartRw => bench_sync(
                backend,
                heap_name,
                size,
                DMA_BUF_SYNC_START | DMA_BUF_SYNC_RW,
                DMA_BUF_SYNC_END | DMA_BUF_SYNC_RW,
                cfg.warmup,
                cfg.iterations,
            )?,
            Op::SyncEndRw => bench_sync(
                backend,
                heap_name,
                size,
                DMA_BUF_SYNC_END | DMA_BUF_SYNC_RW,
                DMA_BUF_SYNC_START | DMA_BUF_SYNC_RW,
                cfg.warmup,
                cfg.iterations,
            )?,
            Op::Close => bench_close(backend, heap_name, size, cfg.warmup, cfg.iterations)?,
            Op::Dup => bench_dup(backend, heap_name, size, cfg.warmup, cfg.iterations)?,
            Op::Llseek => bench_llseek(backend, heap_name, size, cfg.warmup, cfg.iterations)?,
            Op::ExportSyncFile => {
                bench_export_sync_file(backend, heap_name, size, cfg.warmup, cfg.iterations)?
            }
            Op::ImportSyncFile => {
                bench_import_sync_file(backend, heap_name, size, cfg.warmup, cfg.iterations)?
            }
            Op::Pipeline => bench_pipeline(backend, heap_name, size, cfg.warmup, cfg.iterations)?,
        };
        if let Some(st) = stats::compute_stats(&samples) {
            results.push((size, st));
        }
    }
    Ok(results)
}

// -- Alloc: time DMA_HEAP_IOCTL_ALLOC only --

#[allow(clippy::cast_possible_truncation)]
fn bench_alloc<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    warmup: u32,
    iterations: u32,
) -> anyhow::Result<Vec<u64>> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Warmup
    for _ in 0..warmup {
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        backend.close(fd)?;
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let t0 = Instant::now();
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let elapsed = t0.elapsed().as_micros() as u64;
        samples.push(elapsed);
        backend.close(fd)?;
    }
    Ok(samples)
}

// -- Mmap: time mmap() on an existing dmabuf fd --

#[allow(clippy::cast_possible_truncation)]
fn bench_mmap<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    warmup: u32,
    iterations: u32,
) -> anyhow::Result<Vec<u64>> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let len = size as usize;

    // Warmup
    for _ in 0..warmup {
        let ptr = backend.mmap(fd, len)?;
        backend.munmap(ptr, len)?;
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let t0 = Instant::now();
        let ptr = backend.mmap(fd, len)?;
        let elapsed = t0.elapsed().as_micros() as u64;
        samples.push(elapsed);
        backend.munmap(ptr, len)?;
    }
    backend.close(fd)?;
    Ok(samples)
}

// -- Munmap: time munmap() on an existing mapping --

#[allow(clippy::cast_possible_truncation)]
fn bench_munmap<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    warmup: u32,
    iterations: u32,
) -> anyhow::Result<Vec<u64>> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let len = size as usize;

    // Warmup
    for _ in 0..warmup {
        let ptr = backend.mmap(fd, len)?;
        backend.munmap(ptr, len)?;
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let ptr = backend.mmap(fd, len)?;
        let t0 = Instant::now();
        backend.munmap(ptr, len)?;
        let elapsed = t0.elapsed().as_micros() as u64;
        samples.push(elapsed);
    }
    backend.close(fd)?;
    Ok(samples)
}

// -- Sync: time a single sync ioctl (start or end, any direction) --

#[allow(clippy::cast_possible_truncation)]
fn bench_sync<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    measure_flags: u64,
    cleanup_flags: u64,
    warmup: u32,
    iterations: u32,
) -> anyhow::Result<Vec<u64>> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let mut buf = DmaBuf::new(backend, fd, size as usize);
    let _ptr = buf.mmap()?;

    // Warmup
    for _ in 0..warmup {
        backend.sync(fd, measure_flags)?;
        backend.sync(fd, cleanup_flags)?;
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        // Ensure we're in the correct state before measuring.
        backend.sync(fd, cleanup_flags)?;
        let t0 = Instant::now();
        backend.sync(fd, measure_flags)?;
        let elapsed = t0.elapsed().as_micros() as u64;
        samples.push(elapsed);
    }
    drop(buf);
    Ok(samples)
}

// -- Close: time close() of a dmabuf fd (requires alloc per iteration) --

#[allow(clippy::cast_possible_truncation)]
fn bench_close<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    warmup: u32,
    iterations: u32,
) -> anyhow::Result<Vec<u64>> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Warmup
    for _ in 0..warmup {
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        backend.close(fd)?;
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let t0 = Instant::now();
        backend.close(fd)?;
        let elapsed = t0.elapsed().as_micros() as u64;
        samples.push(elapsed);
    }
    Ok(samples)
}

// -- Dup: time dup() on an existing fd --

#[allow(clippy::cast_possible_truncation)]
fn bench_dup<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    warmup: u32,
    iterations: u32,
) -> anyhow::Result<Vec<u64>> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;

    // Warmup
    for _ in 0..warmup {
        let dup_fd = backend.dup(fd)?;
        backend.close(dup_fd)?;
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let t0 = Instant::now();
        let dup_fd = backend.dup(fd)?;
        let elapsed = t0.elapsed().as_micros() as u64;
        samples.push(elapsed);
        backend.close(dup_fd)?;
    }
    backend.close(fd)?;
    Ok(samples)
}

// -- Llseek: time lseek(SEEK_END) on an existing fd --

#[allow(clippy::cast_possible_truncation)]
fn bench_llseek<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    warmup: u32,
    iterations: u32,
) -> anyhow::Result<Vec<u64>> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;

    // Warmup
    for _ in 0..warmup {
        let _ = backend.llseek(fd, 0, libc::SEEK_END)?;
        let _ = backend.llseek(fd, 0, libc::SEEK_SET)?;
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let t0 = Instant::now();
        let _ = backend.llseek(fd, 0, libc::SEEK_END)?;
        let elapsed = t0.elapsed().as_micros() as u64;
        samples.push(elapsed);
        let _ = backend.llseek(fd, 0, libc::SEEK_SET)?;
    }
    backend.close(fd)?;
    Ok(samples)
}

// -- Export sync_file: time DMA_BUF_IOCTL_EXPORT_SYNC_FILE --

#[allow(clippy::cast_possible_truncation)]
fn bench_export_sync_file<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    warmup: u32,
    iterations: u32,
) -> anyhow::Result<Vec<u64>> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, fd, size as usize);

    // Warmup
    for _ in 0..warmup {
        let sync_fd = buf.export_sync_file(DMA_BUF_SYNC_READ as u32)?;
        backend.close(sync_fd)?;
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let t0 = Instant::now();
        let sync_fd = buf.export_sync_file(DMA_BUF_SYNC_READ as u32)?;
        let elapsed = t0.elapsed().as_micros() as u64;
        samples.push(elapsed);
        backend.close(sync_fd)?;
    }
    drop(buf);
    Ok(samples)
}

// -- Import sync_file: time DMA_BUF_IOCTL_IMPORT_SYNC_FILE --

#[allow(clippy::cast_possible_truncation)]
fn bench_import_sync_file<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    warmup: u32,
    iterations: u32,
) -> anyhow::Result<Vec<u64>> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, fd, size as usize);

    // Warmup
    for _ in 0..warmup {
        let sync_fd = buf.export_sync_file(DMA_BUF_SYNC_READ as u32)?;
        buf.import_sync_file(DMA_BUF_SYNC_READ as u32, sync_fd)?;
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let sync_fd = buf.export_sync_file(DMA_BUF_SYNC_READ as u32)?;
        let t0 = Instant::now();
        buf.import_sync_file(DMA_BUF_SYNC_READ as u32, sync_fd)?;
        let elapsed = t0.elapsed().as_micros() as u64;
        samples.push(elapsed);
    }
    drop(buf);
    Ok(samples)
}

// -- Pipeline: time full alloc→mmap→sync_w→write→sync_r→close --

#[allow(clippy::cast_possible_truncation)]
fn bench_pipeline<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    warmup: u32,
    iterations: u32,
) -> anyhow::Result<Vec<u64>> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Warmup
    for _ in 0..warmup {
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let mut buf = DmaBuf::new(backend, fd, size as usize);
        let ptr = buf.mmap()?;
        buf.sync_start(DMA_BUF_SYNC_WRITE)?;
        unsafe { std::ptr::write_bytes(ptr, 0xAA, size as usize) };
        buf.sync_end(DMA_BUF_SYNC_WRITE)?;
        buf.sync_start(DMA_BUF_SYNC_READ)?;
        buf.sync_end(DMA_BUF_SYNC_READ)?;
        drop(buf);
    }

    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let t0 = Instant::now();
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let mut buf = DmaBuf::new(backend, fd, size as usize);
        let ptr = buf.mmap()?;
        buf.sync_start(DMA_BUF_SYNC_WRITE)?;
        unsafe { std::ptr::write_bytes(ptr, 0xAA, size as usize) };
        buf.sync_end(DMA_BUF_SYNC_WRITE)?;
        buf.sync_start(DMA_BUF_SYNC_READ)?;
        buf.sync_end(DMA_BUF_SYNC_READ)?;
        drop(buf);
        let elapsed = t0.elapsed().as_micros() as u64;
        samples.push(elapsed);
    }
    Ok(samples)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn human_size(bytes: u64) -> String {
    if bytes >= 1_048_576 && bytes.is_multiple_of(1_048_576) {
        format!("{}M", bytes / 1_048_576)
    } else if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{}K", bytes / 1024)
    } else {
        bytes.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn op_parse_all() {
        let ops = Op::parse_list("alloc,mmap,close").unwrap();
        assert_eq!(ops, vec![Op::Alloc, Op::Mmap, Op::Close]);
    }

    #[test]
    fn op_parse_unknown() {
        assert!(Op::parse_list("alloc,bogus").is_err());
    }

    #[test]
    fn op_parse_sync_rw() {
        let ops = Op::parse_list("sync_start_rw,sync_end_rw").unwrap();
        assert_eq!(ops, vec![Op::SyncStartRw, Op::SyncEndRw]);
    }

    #[test]
    fn bench_alloc_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_alloc(&backend, "system", 4096, 2, 10).unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn bench_mmap_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_mmap(&backend, "system", 4096, 2, 10).unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn bench_munmap_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_munmap(&backend, "system", 4096, 2, 10).unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn bench_sync_write_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_sync(
            &backend,
            "system",
            4096,
            DMA_BUF_SYNC_START | DMA_BUF_SYNC_WRITE,
            DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE,
            2,
            10,
        )
        .unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn bench_sync_rw_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_sync(
            &backend,
            "system",
            4096,
            DMA_BUF_SYNC_START | DMA_BUF_SYNC_RW,
            DMA_BUF_SYNC_END | DMA_BUF_SYNC_RW,
            2,
            10,
        )
        .unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn bench_close_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_close(&backend, "system", 4096, 2, 10).unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn bench_dup_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_dup(&backend, "system", 4096, 2, 10).unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn bench_llseek_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_llseek(&backend, "system", 4096, 2, 10).unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn bench_export_sync_file_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_export_sync_file(&backend, "system", 4096, 2, 10).unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn bench_import_sync_file_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_import_sync_file(&backend, "system", 4096, 2, 10).unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn bench_pipeline_runs() {
        let backend = MockBackend::new_realistic();
        let samples = bench_pipeline(&backend, "system", 4096, 2, 10).unwrap();
        assert_eq!(samples.len(), 10);
    }

    #[test]
    fn run_full_microbench() {
        let backend = MockBackend::new_realistic();
        let cfg = MicrobenchConfig {
            ops: vec![Op::Alloc, Op::Close, Op::Pipeline],
            sizes: vec![4096],
            iterations: 5,
            warmup: 1,
            heap_w: 8,
        };
        let (sub, err, benchmarks) = run(&backend, "system", &cfg);
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert_eq!(sub.len(), 3);
        assert!(sub.iter().all(|s| s.passed));
        assert!(benchmarks.contains_key("alloc"));
        assert!(benchmarks.contains_key("close"));
        assert!(benchmarks.contains_key("pipeline"));
    }

    #[test]
    fn human_size_formatting() {
        assert_eq!(human_size(4096), "4K");
        assert_eq!(human_size(65536), "64K");
        assert_eq!(human_size(1_048_576), "1M");
        assert_eq!(human_size(4097), "4097");
    }

    #[test]
    fn benchmarks_serialize() {
        let backend = MockBackend::new_realistic();
        let cfg = MicrobenchConfig {
            ops: vec![Op::Alloc],
            sizes: vec![4096],
            iterations: 5,
            warmup: 1,
            heap_w: 8,
        };
        let (_, _, benchmarks) = run(&backend, "system", &cfg);
        let json = serde_json::to_value(&benchmarks).unwrap();
        assert!(json["alloc"]["4096"]["avg_us"].is_u64());
    }
}
