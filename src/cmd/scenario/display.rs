// Display workload simulation: buffer flip, rotation switch, multi-layer.

use std::time::Instant;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::cmd::perf::compute_stats;
use crate::cmd::scenario::{BufferPool, bulk_alloc, fill_buffer, report_results};
use crate::heap::DmaHeap;

/// ARGB8888 bytes per pixel.
const BPP_ARGB8888: u64 = 4;

/// Display scenario configuration.
pub struct DisplayConfig {
    pub width: u32,
    pub height: u32,
    pub buffers: usize,
    pub fps: u32,
    pub frames: u32,
    pub rotation_cycles: u32,
    pub layers: usize,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            width: 1440,
            height: 3200,
            buffers: 3,
            fps: 60,
            frames: 120,
            rotation_cycles: 20,
            layers: 6,
        }
    }
}

/// Compute framebuffer size for ARGB8888.
fn framebuffer_size(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * BPP_ARGB8888
}

/// Run all display scenario tests.
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &DisplayConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let tests: Vec<(&str, nix::Result<()>)> = vec![
        ("flip", display_flip(backend, heap_name, config)),
        ("rotation", display_rotation(backend, heap_name, config)),
        (
            "multi_layer",
            display_multi_layer(backend, heap_name, config),
        ),
    ];
    report_results("display", &tests)
}

/// Double/triple buffer flip: allocate pool, cycle with frame writes.
#[allow(clippy::cast_possible_truncation)]
fn display_flip<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &DisplayConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let buf_size = framebuffer_size(config.width, config.height);

    let mut pool = BufferPool::new(backend, &heap, buf_size, config.buffers)?;
    let mut frame_latencies = Vec::with_capacity(config.frames as usize);

    for frame in 0..config.frames {
        let start = Instant::now();
        let buf = pool.next();
        let ptr = buf.mmap()?;
        fill_buffer(buf, ptr, buf.len(), (frame & 0xFF) as u8)?;
        frame_latencies.push(start.elapsed().as_micros() as u64);
    }

    if let Some(stats) = compute_stats(&frame_latencies) {
        tracing::info!(
            width = config.width,
            height = config.height,
            buf_size,
            buffers = config.buffers,
            frames = config.frames,
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            "flip"
        );
    }

    Ok(())
}

/// Rotation: alternate portrait/landscape pool alloc/free.
#[allow(clippy::cast_possible_truncation)]
fn display_rotation<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &DisplayConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut cycle_latencies = Vec::with_capacity(config.rotation_cycles as usize);

    for cycle in 0..config.rotation_cycles {
        let (w, h) = if cycle % 2 == 0 {
            (config.width, config.height) // portrait
        } else {
            (config.height, config.width) // landscape
        };
        let buf_size = framebuffer_size(w, h);

        let start = Instant::now();
        let (buffers, _) = bulk_alloc(backend, &heap, buf_size, config.buffers)?;
        drop(buffers);
        cycle_latencies.push(start.elapsed().as_micros() as u64);
    }

    if let Some(stats) = compute_stats(&cycle_latencies) {
        tracing::info!(
            cycles = config.rotation_cycles,
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            "rotation"
        );
    }

    Ok(())
}

/// Multi-layer composition: different-sized layers allocated together.
#[allow(clippy::cast_possible_truncation)]
fn display_multi_layer<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &DisplayConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Layer sizes: full screen, status bar, nav bar, and smaller overlays.
    let layer_sizes: Vec<u64> = (0..config.layers)
        .map(|i| {
            if i == 0 {
                // Full screen layer.
                framebuffer_size(config.width, config.height)
            } else {
                // Smaller overlay layers (1/4 to 1/16 of full screen).
                let divisor = 1u32 << i.min(4);
                framebuffer_size(config.width, config.height / divisor)
            }
        })
        .collect();

    // Allocate all layers.
    let start = Instant::now();
    let mut alloc_latencies = Vec::with_capacity(config.layers);
    let mut layer_bufs = Vec::with_capacity(config.layers);

    for &size in &layer_sizes {
        let alloc_start = Instant::now();
        let (mut bufs, _) = bulk_alloc(backend, &heap, size, 1)?;
        alloc_latencies.push(alloc_start.elapsed().as_micros() as u64);
        layer_bufs.append(&mut bufs);
    }
    let total_alloc_us = start.elapsed().as_micros() as u64;

    tracing::info!(layers = config.layers, total_alloc_us, "multi_layer");

    if let Some(stats) = compute_stats(&alloc_latencies) {
        tracing::info!(
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            "multi_layer_per_layer"
        );
    }

    drop(layer_bufs);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    fn test_config() -> DisplayConfig {
        DisplayConfig {
            width: 320,
            height: 480,
            buffers: 3,
            fps: 60,
            frames: 20,
            rotation_cycles: 4,
            layers: 4,
        }
    }

    #[test]
    fn framebuffer_size_calc() {
        assert_eq!(framebuffer_size(1440, 3200), 1440 * 3200 * 4);
    }

    #[test]
    fn flip_runs() {
        let b = MockBackend::new();
        display_flip(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn rotation_runs() {
        let b = MockBackend::new();
        display_rotation(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn multi_layer_runs() {
        let b = MockBackend::new();
        display_multi_layer(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        run(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn flip_no_leak() {
        let b = MockBackend::new();
        display_flip(&b, "system", &test_config()).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn rotation_no_leak() {
        let b = MockBackend::new();
        display_rotation(&b, "system", &test_config()).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn multi_layer_no_leak() {
        let b = MockBackend::new();
        display_multi_layer(&b, "system", &test_config()).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }
}
