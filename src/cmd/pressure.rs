// Memory pressure tests: gradual exhaust, recovery, concurrent pressure.
//
// On Android, exhaust tests run in a subprocess so that OOM killer takes
// out the child instead of the parent. The parent retries with a reduced
// allocation count (100% -> 90% -> 80% -> 70%) if the child is killed.

use std::time::Instant;

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};

/// Environment variable that signals we are running as a pressure worker subprocess.
pub const PRESSURE_WORKER_ENV: &str = "DHP_PRESSURE_WORKER";

/// Minimum retry ratio (70%).
const MIN_RETRY_RATIO: u32 = 70;

/// Ratio step for adaptive retry (10%).
const RETRY_STEP: u32 = 10;

/// Run all pressure tests. Returns sub-test results (and the first error, if any).
#[allow(clippy::cast_possible_truncation)]
pub fn run<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    alloc_size: u64,
    max_allocs_override: Option<usize>,
    heap_w: usize,
) -> (Vec<SubTestResult>, Option<anyhow::Error>) {
    let max_allocs = match max_allocs_override {
        Some(n) => n,
        None => safe_exhaust_limit(alloc_size),
    };

    tracing::debug!(
        heap = heap_name,
        alloc_size,
        max_allocs,
        "pressure sequence"
    );

    let tests: Vec<(&str, nix::Result<()>)> = vec![
        (
            "gradual_exhaust",
            test_gradual_exhaust(backend, heap_name, alloc_size, max_allocs),
        ),
        (
            "recovery",
            test_recovery(backend, heap_name, alloc_size, max_allocs),
        ),
        (
            "pressure_concurrent",
            test_pressure_concurrent(backend, heap_name, alloc_size),
        ),
    ];

    runner::collect_test_results("pressure", heap_name, heap_w, &tests)
}

/// Run pressure tests as a subprocess with adaptive OOM retry.
///
/// Spawns `dhp pressure` as a child process. If the child is killed by
/// OOM (exit code None / signal 9), reduce the allocation count by 10%
/// and retry (down to 70% of the original).
pub fn run_subprocess(
    heap_name: &str,
    alloc_size: u64,
    max_allocs: usize,
) -> (Vec<SubTestResult>, Option<anyhow::Error>) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return (
                Vec::new(),
                Some(anyhow::anyhow!("failed to get current exe: {e}")),
            );
        }
    };

    let mut ratio = 100u32;
    loop {
        let adjusted = max_allocs * ratio as usize / 100;
        tracing::debug!(
            adjusted,
            ratio,
            heap = heap_name,
            "spawning pressure worker"
        );

        let output = std::process::Command::new(&exe)
            .args([
                "pressure",
                "--heaps",
                heap_name,
                "--alloc-size",
                &alloc_size.to_string(),
                "--max-allocs",
                &adjusted.to_string(),
            ])
            .env(PRESSURE_WORKER_ENV, "1")
            .output();

        match output {
            Ok(out) => match out.status.code() {
                Some(0) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    return parse_worker_output(&stdout, ratio);
                }
                Some(code) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    tracing::warn!(code, %stderr, "pressure worker exited with error");
                    return (
                        vec![SubTestResult {
                            name: "pressure_subprocess".to_string(),
                            passed: false,
                            error: Some(format!("worker exit code {code}")),
                        }],
                        None,
                    );
                }
                None => {
                    // Killed by signal (likely OOM)
                    if ratio > MIN_RETRY_RATIO {
                        ratio -= RETRY_STEP;
                        tracing::warn!(ratio, "worker OOM killed, retrying at {ratio}%");
                        continue;
                    }
                    return (
                        vec![SubTestResult {
                            name: "pressure_subprocess".to_string(),
                            passed: false,
                            error: Some(format!(
                                "worker OOM killed even at {ratio}% ({adjusted} allocs)"
                            )),
                        }],
                        None,
                    );
                }
            },
            Err(e) => {
                return (
                    Vec::new(),
                    Some(anyhow::anyhow!("failed to spawn pressure worker: {e}")),
                );
            }
        }
    }
}

/// Parse stdout lines from the worker process into `SubTestResult`s.
fn parse_worker_output(stdout: &str, ratio: u32) -> (Vec<SubTestResult>, Option<anyhow::Error>) {
    let mut results = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.contains("[PASS]") {
            let name = extract_test_name(trimmed);
            results.push(SubTestResult {
                name,
                passed: true,
                error: if ratio < 100 {
                    Some(format!("at {ratio}% capacity"))
                } else {
                    None
                },
            });
        } else if trimmed.contains("[FAIL]") {
            let name = extract_test_name(trimmed);
            results.push(SubTestResult {
                name,
                passed: false,
                error: Some(trimmed.to_string()),
            });
        }
    }
    (results, None)
}

/// Extract test name from a `[heap] [PASS/FAIL] stage::test_name ...` line.
fn extract_test_name(line: &str) -> String {
    // Format: "[heap] [PASS] pressure::test_name ..."
    if let Some(pos) = line.find("::") {
        let after = &line[pos + 2..];
        after
            .split_whitespace()
            .next()
            .unwrap_or("unknown")
            .to_string()
    } else {
        "unknown".to_string()
    }
}

/// Absolute upper bound on exhaust allocations.
const MAX_EXHAUST_ALLOCS: usize = 10_000;

/// Conservative fallback when `/proc/meminfo` is unavailable (e.g. macOS).
const DEFAULT_EXHAUST_LIMIT: usize = 500;

/// Fraction of available memory to use for exhaust testing (1/4 = 25%).
const MEM_USAGE_FRACTION: u64 = 4;

/// Calculate a safe exhaust allocation limit based on available memory.
#[allow(clippy::cast_possible_truncation)]
pub fn safe_exhaust_limit(alloc_size: u64) -> usize {
    if let Ok(meminfo) = crate::procfs::read_meminfo() {
        let available_bytes = meminfo.mem_available_kb * 1024;
        let usable = available_bytes / MEM_USAGE_FRACTION;
        let limit = (usable / alloc_size) as usize;
        let clamped = limit.clamp(1, MAX_EXHAUST_ALLOCS);
        tracing::debug!(
            mem_available_kb = meminfo.mem_available_kb,
            alloc_size,
            limit = clamped,
            "dynamic exhaust limit"
        );
        return clamped;
    }
    tracing::debug!(limit = DEFAULT_EXHAUST_LIMIT, "using default exhaust limit");
    DEFAULT_EXHAUST_LIMIT
}

/// Allocate fixed-size buffers until `ENOMEM`. Track latency and total allocated.
#[allow(clippy::cast_possible_truncation)]
fn test_gradual_exhaust<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    alloc_size: u64,
    max_allocs: usize,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut buffers: Vec<DmaBuf<'_, B>> = Vec::new();
    let mut latencies_us: Vec<u64> = Vec::new();

    loop {
        if buffers.len() >= max_allocs {
            tracing::warn!(
                count = buffers.len(),
                "exhaust limit reached without ENOMEM"
            );
            break;
        }
        let start = Instant::now();
        match heap.alloc(
            alloc_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        ) {
            Ok(fd) => {
                let elapsed = start.elapsed().as_micros() as u64;
                latencies_us.push(elapsed);
                buffers.push(DmaBuf::new(backend, fd, alloc_size as usize));
            }
            Err(Errno::ENOMEM) => break,
            Err(e) => return Err(e),
        }
    }

    let total_bytes = buffers.len() as u64 * alloc_size;
    let avg_latency = if latencies_us.is_empty() {
        0
    } else {
        latencies_us.iter().sum::<u64>() / latencies_us.len() as u64
    };

    println!(
        "[{heap_name}] pressure::gradual_exhaust count={} total_mb={} avg_latency_us={avg_latency}",
        buffers.len(),
        total_bytes / (1024 * 1024),
    );

    // Clean up all buffers (Drop handles close).
    drop(buffers);
    Ok(())
}

/// After exhausting allocations, close 50% and verify recovery.
#[allow(clippy::cast_possible_truncation)]
fn test_recovery<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    alloc_size: u64,
    max_allocs: usize,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut buffers: Vec<DmaBuf<'_, B>> = Vec::new();

    // Exhaust until ENOMEM (with safety limit).
    loop {
        if buffers.len() >= max_allocs {
            break;
        }
        match heap.alloc(
            alloc_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        ) {
            Ok(fd) => buffers.push(DmaBuf::new(backend, fd, alloc_size as usize)),
            Err(Errno::ENOMEM) => break,
            Err(e) => return Err(e),
        }
    }

    let total_before = buffers.len();
    if total_before == 0 {
        tracing::warn!("no buffers allocated before ENOMEM");
        return Ok(());
    }

    // Close 50% of buffers (every other one).
    let release_count = total_before / 2;
    for _ in 0..release_count {
        buffers.pop();
    }

    // Attempt reallocation after release.
    let mut recovered = 0u32;
    let mut recovery_latencies: Vec<u64> = Vec::new();

    for _ in 0..release_count {
        let start = Instant::now();
        match heap.alloc(
            alloc_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        ) {
            Ok(fd) => {
                let elapsed = start.elapsed().as_micros() as u64;
                recovery_latencies.push(elapsed);
                buffers.push(DmaBuf::new(backend, fd, alloc_size as usize));
                recovered += 1;
            }
            Err(Errno::ENOMEM) => break,
            Err(e) => return Err(e),
        }
    }

    let avg_recovery = if recovery_latencies.is_empty() {
        0
    } else {
        recovery_latencies.iter().sum::<u64>() / recovery_latencies.len() as u64
    };

    println!(
        "[{heap_name}] pressure::recovery total_before={total_before} released={release_count} recovered={recovered} avg_recovery_us={avg_recovery}",
    );

    drop(buffers);
    Ok(())
}

/// Concurrent alloc under memory pressure from worker threads.
#[allow(clippy::cast_possible_truncation)]
fn test_pressure_concurrent<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    alloc_size: u64,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let worker_count = 4u32;
    let allocs_per_worker = 50u32;

    let fail_count = std::sync::atomic::AtomicUsize::new(0);
    let fail_ref = &fail_count;
    let heap_ref = &heap;

    std::thread::scope(|s| {
        for worker_id in 0..worker_count {
            s.spawn(move || {
                let mut bufs: Vec<DmaBuf<'_, B>> = Vec::new();
                for _ in 0..allocs_per_worker {
                    match heap_ref.alloc(
                        alloc_size,
                        DMA_HEAP_ALLOC_FD_FLAGS,
                        DMA_HEAP_VALID_HEAP_FLAGS,
                    ) {
                        Ok(fd) => {
                            bufs.push(DmaBuf::new(backend, fd, alloc_size as usize));
                        }
                        Err(Errno::ENOMEM) => break,
                        Err(_) => {
                            fail_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
                tracing::debug!(worker_id, allocated = bufs.len(), "worker done");
                drop(bufs);
            });
        }
    });

    let failures = fail_count.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "[{heap_name}] pressure::pressure_concurrent workers={worker_count} allocs_per_worker={allocs_per_worker} unexpected_failures={failures}",
    );

    if failures > 0 {
        return Err(Errno::EIO);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn gradual_exhaust_hits_limit() {
        // Use small alloc size + conservative limit to avoid host OOM.
        let b = MockBackend::new();
        test_gradual_exhaust(&b, "system", 4096, 500).unwrap();
    }

    #[test]
    fn recovery_after_exhaust() {
        let b = MockBackend::new();
        test_recovery(&b, "system", 4096, 500).unwrap();
    }

    #[test]
    fn pressure_concurrent_runs() {
        let b = MockBackend::new();
        test_pressure_concurrent(&b, "system", 4096).unwrap();
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        let (results, err) = run(&b, "system", 4096, None, 6);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert_eq!(results.len(), 3);
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn exhaust_count() {
        let b = MockBackend::new();
        let heap = DmaHeap::open(&b, "system").unwrap();
        let alloc_size: u64 = 4096;
        let mut count = 0u32;
        let mut bufs = Vec::new();
        for _ in 0..100 {
            match heap.alloc(
                alloc_size,
                DMA_HEAP_ALLOC_FD_FLAGS,
                DMA_HEAP_VALID_HEAP_FLAGS,
            ) {
                Ok(fd) => {
                    count += 1;
                    bufs.push(DmaBuf::new(&b, fd, alloc_size as usize));
                }
                Err(Errno::ENOMEM) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(count > 0, "should allocate at least some buffers");
        drop(bufs);
    }

    #[test]
    fn recovery_no_leak() {
        let b = MockBackend::new();
        test_recovery(&b, "system", 4096, 500).unwrap();
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }
}
