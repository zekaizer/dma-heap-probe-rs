// NPU workload simulation: model load, inference loop, model switch,
// sustained inference, concurrent clients, pressure inference.

use std::time::Instant;

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::cmd::perf::compute_stats;
use crate::cmd::scenario::{bulk_alloc, check_thread_failures, fill_buffer, read_sync};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};

/// Default model size in bytes (512 MB).
const DEFAULT_MODEL_SIZE: u64 = 512 * 1024 * 1024;

/// Default chunk size for model loading (64 MB).
const DEFAULT_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// Default input buffer size (~600 KB, simulating 224x224x3xfp32).
const DEFAULT_INPUT_SIZE: u64 = 602_112;

/// Default output buffer size (4 KB, simulating 1000xfp32).
const DEFAULT_OUTPUT_SIZE: u64 = 4096;

/// Default inference iterations.
const DEFAULT_INFERENCE_ITERS: u32 = 100;

/// Compute (`chunk_count`, `actual_chunk_size`) for a given model/chunk config.
#[allow(clippy::cast_possible_truncation)]
fn chunk_params(model_size: u64, chunk_size: u64) -> (usize, u64) {
    if chunk_size > 0 {
        (model_size.div_ceil(chunk_size) as usize, chunk_size)
    } else {
        (1, model_size)
    }
}

/// NPU scenario configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct NpuConfig {
    pub model_size: u64,
    pub chunk_size: u64,
    pub input_size: u64,
    pub output_size: u64,
    pub iterations: u32,
    pub clients: u32,
}

impl Default for NpuConfig {
    fn default() -> Self {
        Self {
            model_size: DEFAULT_MODEL_SIZE,
            chunk_size: DEFAULT_CHUNK_SIZE,
            input_size: DEFAULT_INPUT_SIZE,
            output_size: DEFAULT_OUTPUT_SIZE,
            iterations: DEFAULT_INFERENCE_ITERS,
            clients: 4,
        }
    }
}

/// Run all NPU scenario tests.
pub fn run<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    config: &NpuConfig,
) -> (
    Vec<crate::runner::SubTestResult>,
    Option<Box<dyn std::error::Error>>,
) {
    let (chunk_count, _) = chunk_params(config.model_size, config.chunk_size);
    let iters_per_client = config.iterations / config.clients;
    println!("scenario npu sequence:");
    println!("  heap: {heap_name}");
    println!("  model_size: {}", config.model_size);
    println!("  chunk_size: {} ({chunk_count} chunks)", config.chunk_size);
    println!("  input_size: {}", config.input_size);
    println!("  output_size: {}", config.output_size);
    println!("  iterations: {}", config.iterations);
    println!("  clients: {}", config.clients);
    println!();
    println!(
        "  phase 1: model_load     bulk_alloc {chunk_count} weight chunks, mmap+write, probe 4K/64K/1M"
    );
    println!(
        "  phase 2: inference_loop  hold model + {} x (input alloc -> compute -> output read -> release)",
        config.iterations
    );
    println!(
        "  phase 3: model_switch   load A(half) -> load B(full) alongside -> release A -> load C(half)"
    );
    println!(
        "  phase 4: sustained      hold model + {} x alloc/free with periodic p50/p95/p99 snapshots",
        config.iterations
    );
    println!(
        "  phase 5: concurrent     {} threads x {} iters each, independent inference loops",
        config.clients, iters_per_client
    );
    println!("  cleanup: release all model buffers");
    println!();
    println!("scenario npu result legend:");
    println!("  chunks          number of model weight chunks loaded");
    println!("  total_alloc_us  total model load time");
    println!("  probe_us        small alloc latency while model is held");
    println!("  p50/p95/p99_us  per-iteration alloc latency percentiles");
    println!("  client_id       concurrent client thread index");
    println!("  iterations      iterations completed (per client or total)");
    println!();

    let tests: Vec<(&str, nix::Result<()>)> = vec![
        ("model_load", npu_model_load(backend, heap_name, config)),
        (
            "inference_loop",
            npu_inference_loop(backend, heap_name, config),
        ),
        ("model_switch", npu_model_switch(backend, heap_name, config)),
        ("sustained", npu_sustained(backend, heap_name, config)),
        ("concurrent", npu_concurrent(backend, heap_name, config)),
    ];
    crate::runner::collect_test_results("npu", &tests)
}

/// Simulate NPU model loading: bulk alloc chunks, mmap, write pattern.
#[allow(clippy::cast_possible_truncation)]
fn npu_model_load<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &NpuConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let (chunk_count, actual_chunk) = chunk_params(config.model_size, config.chunk_size);

    let start = Instant::now();
    let (mut buffers, latencies) = bulk_alloc(backend, &heap, actual_chunk, chunk_count)?;
    let total_alloc_us = start.elapsed().as_micros() as u64;

    // mmap and write pattern to simulate model weight loading.
    for buf in &mut buffers {
        let ptr = buf.mmap()?;
        fill_buffer(buf, ptr, buf.len(), 0xCC)?;
    }

    if let Some(stats) = compute_stats(&latencies) {
        println!(
            "scenario npu: model_load chunks={chunk_count} chunk_size={actual_chunk} total_alloc_us={total_alloc_us} p50_us={} p95_us={}",
            stats.p50_us, stats.p95_us
        );
    }

    // Test additional small allocs while model is held.
    for &probe_size in &[4096u64, 65536, 1_048_576] {
        let probe_start = Instant::now();
        match heap.alloc(
            probe_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        ) {
            Ok(fd) => {
                let probe_us = probe_start.elapsed().as_micros() as u64;
                println!(
                    "scenario npu: model_load_probe_ok probe_size={probe_size} probe_us={probe_us}"
                );
                let _ = DmaBuf::new(backend, fd, probe_size as usize);
            }
            Err(Errno::ENOMEM) => {
                println!("scenario npu: model_load_probe_enomem probe_size={probe_size}");
            }
            Err(e) => return Err(e),
        }
    }

    drop(buffers);
    Ok(())
}

/// Simulate NPU inference loop: model held + repeated input/output alloc/free.
#[allow(clippy::cast_possible_truncation)]
fn npu_inference_loop<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &NpuConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Load model (keep alive).
    let (chunk_count, actual_chunk) = chunk_params(config.model_size, config.chunk_size);
    let (model_bufs, _) = bulk_alloc(backend, &heap, actual_chunk, chunk_count)?;

    // Inference loop.
    let mut iter_latencies = Vec::with_capacity(config.iterations as usize);

    for _ in 0..config.iterations {
        let iter_start = Instant::now();

        // Allocate input buffer.
        let in_fd = heap.alloc(
            config.input_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        )?;
        let mut in_buf = DmaBuf::new(backend, in_fd, config.input_size as usize);
        let in_ptr = in_buf.mmap()?;
        fill_buffer(&in_buf, in_ptr, in_buf.len(), 0xAA)?;

        // Allocate output buffer.
        let out_fd = heap.alloc(
            config.output_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        )?;
        let out_buf = DmaBuf::new(backend, out_fd, config.output_size as usize);

        // Simulate inference (no actual delay in test).
        read_sync(&out_buf)?;

        // Release per-iteration buffers.
        drop(in_buf);
        drop(out_buf);

        let iter_us = iter_start.elapsed().as_micros() as u64;
        iter_latencies.push(iter_us);
    }

    if let Some(stats) = compute_stats(&iter_latencies) {
        println!(
            "scenario npu: inference_loop iterations={} p50_us={} p95_us={} p99_us={}",
            config.iterations, stats.p50_us, stats.p95_us, stats.p99_us
        );
    }

    drop(model_bufs);
    Ok(())
}

/// Simulate model switching: load A → load B alongside → release A → load C.
#[allow(clippy::cast_possible_truncation)]
fn npu_model_switch<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &NpuConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    let small_size = config.model_size / 2;
    let (a_count, chunk) = chunk_params(small_size, config.chunk_size);

    // Phase 1: Load model A (half model size).
    let (model_a, _) = bulk_alloc(backend, &heap, chunk, a_count)?;
    println!("scenario npu: model_switch_phase1 chunks={a_count}");

    // Phase 2: Load model B (full model size) alongside A.
    let (b_count, _) = chunk_params(config.model_size, config.chunk_size);
    let start_b = Instant::now();
    let (model_b, b_lats) = bulk_alloc(backend, &heap, chunk, b_count)?;
    let b_total_us = start_b.elapsed().as_micros() as u64;
    if let Some(stats) = compute_stats(&b_lats) {
        println!(
            "scenario npu: model_switch_phase2 chunks={b_count} total_us={b_total_us} p50_us={}",
            stats.p50_us
        );
    }

    // Phase 3: Release model A.
    let release_start = Instant::now();
    drop(model_a);
    let release_us = release_start.elapsed().as_micros() as u64;
    println!("scenario npu: model_switch_phase3 release_us={release_us}");

    // Phase 4: Load model C (same size as A) into freed space.
    let start_c = Instant::now();
    let (model_c, c_lats) = bulk_alloc(backend, &heap, chunk, a_count)?;
    let c_total_us = start_c.elapsed().as_micros() as u64;
    if let Some(stats) = compute_stats(&c_lats) {
        println!(
            "scenario npu: model_switch_phase4 chunks={a_count} total_us={c_total_us} p50_us={}",
            stats.p50_us
        );
    }

    drop(model_b);
    drop(model_c);
    Ok(())
}

/// Sustained inference: repeated inference loop with periodic stats.
#[allow(clippy::cast_possible_truncation)]
fn npu_sustained<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &NpuConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Load model.
    let (chunk_count, chunk) = chunk_params(config.model_size, config.chunk_size);
    let (model_bufs, _) = bulk_alloc(backend, &heap, chunk, chunk_count)?;

    let snapshot_interval = 50u32;
    let mut window: Vec<u64> = Vec::new();

    for i in 0..config.iterations {
        let iter_start = Instant::now();

        let in_fd = heap.alloc(
            config.input_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        )?;
        let in_buf = DmaBuf::new(backend, in_fd, config.input_size as usize);
        let out_fd = heap.alloc(
            config.output_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        )?;
        let out_buf = DmaBuf::new(backend, out_fd, config.output_size as usize);

        drop(in_buf);
        drop(out_buf);

        let iter_us = iter_start.elapsed().as_micros() as u64;
        window.push(iter_us);

        if (i + 1) % snapshot_interval == 0 {
            if let Some(stats) = compute_stats(&window) {
                println!(
                    "scenario npu: sustained_snapshot iteration={} window_size={} p50_us={} p95_us={} p99_us={}",
                    i + 1,
                    window.len(),
                    stats.p50_us,
                    stats.p95_us,
                    stats.p99_us
                );
            }
            window.clear();
        }
    }

    drop(model_bufs);
    Ok(())
}

/// Concurrent NPU clients: N threads each run independent inference loops.
#[allow(clippy::cast_possible_truncation)]
fn npu_concurrent<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    config: &NpuConfig,
) -> nix::Result<()> {
    let fail_count = std::sync::atomic::AtomicUsize::new(0);
    let fail_ref = &fail_count;

    std::thread::scope(|s| {
        for client_id in 0..config.clients {
            s.spawn(move || {
                let Ok(heap) = DmaHeap::open(backend, heap_name) else {
                    fail_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return;
                };

                // Each client runs a small inference loop.
                let iters = config.iterations / config.clients;
                let mut latencies = Vec::with_capacity(iters as usize);

                for _ in 0..iters {
                    let start = Instant::now();
                    let in_fd = match heap.alloc(
                        config.input_size,
                        DMA_HEAP_ALLOC_FD_FLAGS,
                        DMA_HEAP_VALID_HEAP_FLAGS,
                    ) {
                        Ok(fd) => fd,
                        Err(Errno::ENOMEM) => break,
                        Err(_) => {
                            fail_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                    };
                    let in_buf = DmaBuf::new(backend, in_fd, config.input_size as usize);
                    drop(in_buf);
                    latencies.push(start.elapsed().as_micros() as u64);
                }

                if let Some(stats) = compute_stats(&latencies) {
                    println!(
                        "scenario npu: concurrent_client client_id={client_id} iterations={} p50_us={} p95_us={}",
                        latencies.len(), stats.p50_us, stats.p95_us
                    );
                }
            });
        }
    });

    check_thread_failures(&fail_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    /// Small config that fits in mock's 1 GiB limit without OOM.
    fn test_config() -> NpuConfig {
        NpuConfig {
            model_size: 4096 * 4, // 16 KB
            chunk_size: 4096,     // 4 KB chunks
            input_size: 4096,
            output_size: 4096,
            iterations: 20,
            clients: 2,
        }
    }

    #[test]
    fn model_load_runs() {
        let b = MockBackend::new();
        let cfg = test_config();
        npu_model_load(&b, "system", &cfg).unwrap();
    }

    #[test]
    fn inference_loop_runs() {
        let b = MockBackend::new();
        let cfg = test_config();
        npu_inference_loop(&b, "system", &cfg).unwrap();
    }

    #[test]
    fn model_switch_runs() {
        let b = MockBackend::new();
        let cfg = test_config();
        npu_model_switch(&b, "system", &cfg).unwrap();
    }

    #[test]
    fn sustained_runs() {
        let b = MockBackend::new();
        let cfg = test_config();
        npu_sustained(&b, "system", &cfg).unwrap();
    }

    #[test]
    fn concurrent_runs() {
        let b = MockBackend::new();
        let cfg = test_config();
        npu_concurrent(&b, "system", &cfg).unwrap();
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        let cfg = test_config();
        let (results, err) = run(&b, "system", &cfg);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
    }

    #[test]
    fn model_load_no_leak() {
        let b = MockBackend::new();
        let cfg = test_config();
        npu_model_load(&b, "system", &cfg).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn inference_loop_no_leak() {
        let b = MockBackend::new();
        let cfg = test_config();
        npu_inference_loop(&b, "system", &cfg).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn model_switch_no_leak() {
        let b = MockBackend::new();
        let cfg = test_config();
        npu_model_switch(&b, "system", &cfg).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }
}
