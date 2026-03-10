// GPU rendering workload simulation: app launch burst, app switch, game texture streaming.

use std::time::Instant;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::cmd::perf::compute_stats;
use crate::cmd::scenario::bulk_alloc;
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};

/// GPU scenario configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GpuConfig {
    pub buffer_count: usize,
    pub switch_count: u32,
    pub texture_size: u64,
    pub pool_size: usize,
    pub evict_pct: u32,
    pub evict_rounds: u32,
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            buffer_count: 50,
            switch_count: 10,
            texture_size: 1_048_576, // 1 MB
            pool_size: 100,
            evict_pct: 30,
            evict_rounds: 10,
        }
    }
}

/// Preset buffer sizes for app launch simulation (textures, render targets, etc.).
const APP_LAUNCH_SIZES: &[u64] = &[
    4096,       // small uniform/constant buffer
    16384,      // vertex buffer
    65536,      // index buffer
    262_144,    // small texture (256 KB)
    1_048_576,  // medium texture (1 MB)
    4_194_304,  // large texture (4 MB)
    16_777_216, // render target (16 MB)
];

/// Run all GPU scenario tests.
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &GpuConfig,
) -> (
    Vec<crate::runner::SubTestResult>,
    Option<Box<dyn std::error::Error>>,
) {
    let tests: Vec<(&str, nix::Result<()>)> = vec![
        ("app_launch", gpu_app_launch(backend, heap_name, config)),
        ("app_switch", gpu_app_switch(backend, heap_name, config)),
        ("game_texture", gpu_game_texture(backend, heap_name, config)),
    ];
    crate::runner::collect_test_results("gpu", &tests)
}

/// App launch: burst allocation of mixed-size buffers.
#[allow(clippy::cast_possible_truncation)]
fn gpu_app_launch<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &GpuConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut buffers = Vec::with_capacity(config.buffer_count);
    let mut latencies = Vec::with_capacity(config.buffer_count);

    let total_start = Instant::now();
    for i in 0..config.buffer_count {
        let size = APP_LAUNCH_SIZES[i % APP_LAUNCH_SIZES.len()];
        let start = Instant::now();
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        latencies.push(start.elapsed().as_micros() as u64);
        buffers.push(DmaBuf::new(backend, fd, size as usize));
    }
    let total_us = total_start.elapsed().as_micros() as u64;

    if let Some(stats) = compute_stats(&latencies) {
        tracing::info!(
            buffer_count = config.buffer_count,
            total_us,
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            max_us = stats.max_us,
            "app_launch"
        );
    }

    drop(buffers);
    Ok(())
}

/// App switch: release all buffers, reallocate for new app.
#[allow(clippy::cast_possible_truncation)]
fn gpu_app_switch<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &GpuConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let buf_size = 262_144u64; // 256 KB per buffer
    let mut switch_latencies = Vec::with_capacity(config.switch_count as usize);

    for _ in 0..config.switch_count {
        let start = Instant::now();

        // Allocate app buffers.
        let (buffers, _) = bulk_alloc(backend, &heap, buf_size, config.buffer_count)?;

        // Release all.
        drop(buffers);

        switch_latencies.push(start.elapsed().as_micros() as u64);
    }

    if let Some(stats) = compute_stats(&switch_latencies) {
        tracing::info!(
            switches = config.switch_count,
            buffers_per_switch = config.buffer_count,
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            "app_switch"
        );
    }

    Ok(())
}

/// Game texture streaming: pool with periodic evict/reload cycles.
#[allow(clippy::cast_possible_truncation)]
fn gpu_game_texture<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &GpuConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Allocate texture pool.
    let mut pool: Vec<Option<DmaBuf<'_, B>>> = Vec::with_capacity(config.pool_size);
    for _ in 0..config.pool_size {
        let fd = heap.alloc(
            config.texture_size,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        )?;
        pool.push(Some(DmaBuf::new(backend, fd, config.texture_size as usize)));
    }

    let evict_count = (config.pool_size as u64 * u64::from(config.evict_pct) / 100) as usize;
    let mut round_latencies = Vec::with_capacity(config.evict_rounds as usize);

    for round in 0..config.evict_rounds {
        let start = Instant::now();

        // Evict: drop every N-th texture (simulating LRU).
        let mut evicted = 0;
        for (i, slot) in pool.iter_mut().enumerate() {
            if evicted >= evict_count {
                break;
            }
            // Evict based on round offset for variety.
            if (i + round as usize).is_multiple_of(3) && slot.is_some() {
                slot.take();
                evicted += 1;
            }
        }

        // Reload: fill empty slots.
        for slot in &mut pool {
            if slot.is_none() {
                let fd = heap.alloc(
                    config.texture_size,
                    DMA_HEAP_ALLOC_FD_FLAGS,
                    DMA_HEAP_VALID_HEAP_FLAGS,
                )?;
                *slot = Some(DmaBuf::new(backend, fd, config.texture_size as usize));
            }
        }

        round_latencies.push(start.elapsed().as_micros() as u64);
    }

    if let Some(stats) = compute_stats(&round_latencies) {
        tracing::info!(
            pool_size = config.pool_size,
            evict_pct = config.evict_pct,
            rounds = config.evict_rounds,
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            "game_texture"
        );
    }

    drop(pool);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    fn test_config() -> GpuConfig {
        GpuConfig {
            buffer_count: 10,
            switch_count: 3,
            texture_size: 4096,
            pool_size: 10,
            evict_pct: 30,
            evict_rounds: 3,
        }
    }

    #[test]
    fn app_launch_runs() {
        let b = MockBackend::new();
        gpu_app_launch(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn app_switch_runs() {
        let b = MockBackend::new();
        gpu_app_switch(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn game_texture_runs() {
        let b = MockBackend::new();
        gpu_game_texture(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        let (results, err) = run(&b, "system", &test_config());
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
    }

    #[test]
    fn app_launch_no_leak() {
        let b = MockBackend::new();
        gpu_app_launch(&b, "system", &test_config()).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn app_switch_no_leak() {
        let b = MockBackend::new();
        gpu_app_switch(&b, "system", &test_config()).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn game_texture_no_leak() {
        let b = MockBackend::new();
        gpu_game_texture(&b, "system", &test_config()).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }
}
