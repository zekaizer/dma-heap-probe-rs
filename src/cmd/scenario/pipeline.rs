// Composite pipeline simulations: multiple subsystems sharing dma-heap concurrently.

use std::error::Error;
use std::time::Instant;

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::cmd::perf::compute_stats;
use crate::cmd::scenario::{BufferPool, fill_buffer, read_sync};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_heap::{DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};

/// Pipeline scenario configuration.
pub struct PipelineConfig {
    pub frames: u32,
    /// Heap name overrides per role. Falls back to default heap.
    pub workloads: Vec<(String, String)>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            frames: 30,
            workloads: Vec::new(),
        }
    }
}

impl PipelineConfig {
    /// Get the heap name for a given role, falling back to `default_heap`.
    fn heap_for_role<'a>(&'a self, role: &str, default_heap: &'a str) -> &'a str {
        self.workloads
            .iter()
            .find(|(r, _)| r == role)
            .map_or(default_heap, |(_, h)| h.as_str())
    }
}

/// NV12 buffer size.
fn nv12_size(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * 3 / 2
}

/// ARGB8888 buffer size.
fn argb_size(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * 4
}

/// Run all pipeline scenario tests.
pub fn run<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    config: &PipelineConfig,
) -> Result<(), Box<dyn Error>> {
    let tests: Vec<(&str, nix::Result<()>)> = vec![
        (
            "camera_preview",
            pipeline_camera_preview(backend, heap_name, config),
        ),
        (
            "video_call",
            pipeline_video_call(backend, heap_name, config),
        ),
        ("ai_camera", pipeline_ai_camera(backend, heap_name, config)),
        ("heavy", pipeline_heavy(backend, heap_name, config)),
    ];

    let mut first_error: Option<(&str, nix::Error)> = None;

    for (name, result) in &tests {
        match result {
            Ok(()) => tracing::info!(name, "PASS"),
            Err(e) => {
                tracing::error!(name, error = %e, "FAIL");
                if first_error.is_none() {
                    first_error = Some((name, *e));
                }
            }
        }
    }

    match first_error {
        None => Ok(()),
        Some((name, e)) => Err(format!("pipeline scenario '{name}' failed: {e}").into()),
    }
}

/// Camera preview + display flip + GPU composition.
#[allow(clippy::cast_possible_truncation)]
fn pipeline_camera_preview<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    config: &PipelineConfig,
) -> nix::Result<()> {
    let cam_heap = config.heap_for_role("camera", heap_name);
    let disp_heap = config.heap_for_role("display", heap_name);
    let gpu_heap = config.heap_for_role("gpu", heap_name);

    let fail_count = std::sync::atomic::AtomicUsize::new(0);
    let fail_ref = &fail_count;
    let frames = config.frames;

    std::thread::scope(|s| {
        // Camera thread: 1080p NV12 pool.
        s.spawn(move || {
            run_pool_worker(
                backend,
                cam_heap,
                nv12_size(1920, 1080),
                4,
                frames,
                fail_ref,
            );
        });
        // Display thread: 1080p ARGB flip.
        s.spawn(move || {
            run_pool_worker(
                backend,
                disp_heap,
                argb_size(1080, 1920),
                3,
                frames,
                fail_ref,
            );
        });
        // GPU thread: composition buffers.
        s.spawn(move || {
            run_pool_worker(
                backend,
                gpu_heap,
                argb_size(1080, 1920),
                2,
                frames,
                fail_ref,
            );
        });
    });

    check_failures(&fail_count)
}

/// Video call: camera + encode + decode + display.
#[allow(clippy::cast_possible_truncation)]
fn pipeline_video_call<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    config: &PipelineConfig,
) -> nix::Result<()> {
    let cam_heap = config.heap_for_role("camera", heap_name);
    let codec_heap = config.heap_for_role("codec", heap_name);
    let disp_heap = config.heap_for_role("display", heap_name);

    let fail_count = std::sync::atomic::AtomicUsize::new(0);
    let fail_ref = &fail_count;
    let frames = config.frames;

    std::thread::scope(|s| {
        // Camera: 720p NV12.
        s.spawn(move || {
            run_pool_worker(backend, cam_heap, nv12_size(1280, 720), 4, frames, fail_ref);
        });
        // Encoder: 720p NV12.
        s.spawn(move || {
            run_pool_worker(
                backend,
                codec_heap,
                nv12_size(1280, 720),
                4,
                frames,
                fail_ref,
            );
        });
        // Decoder: 720p NV12.
        s.spawn(move || {
            run_pool_worker(
                backend,
                codec_heap,
                nv12_size(1280, 720),
                8,
                frames,
                fail_ref,
            );
        });
        // Display: 720p ARGB.
        s.spawn(move || {
            run_pool_worker(
                backend,
                disp_heap,
                argb_size(720, 1280),
                3,
                frames,
                fail_ref,
            );
        });
    });

    check_failures(&fail_count)
}

/// AI camera: camera preview + NPU inference + display.
#[allow(clippy::cast_possible_truncation)]
fn pipeline_ai_camera<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    config: &PipelineConfig,
) -> nix::Result<()> {
    let cam_heap = config.heap_for_role("camera", heap_name);
    let npu_heap = config.heap_for_role("npu", heap_name);
    let disp_heap = config.heap_for_role("display", heap_name);

    let fail_count = std::sync::atomic::AtomicUsize::new(0);
    let fail_ref = &fail_count;
    let frames = config.frames;

    std::thread::scope(|s| {
        // Camera: 1080p NV12.
        s.spawn(move || {
            run_pool_worker(
                backend,
                cam_heap,
                nv12_size(1920, 1080),
                4,
                frames,
                fail_ref,
            );
        });
        // NPU: per-frame input/output alloc/free.
        s.spawn(move || {
            run_alloc_free_worker(backend, npu_heap, 602_112, frames, fail_ref);
        });
        // Display: 1080p ARGB.
        s.spawn(move || {
            run_pool_worker(
                backend,
                disp_heap,
                argb_size(1080, 1920),
                3,
                frames,
                fail_ref,
            );
        });
    });

    check_failures(&fail_count)
}

/// Heavy: all subsystems simultaneously.
#[allow(clippy::cast_possible_truncation)]
fn pipeline_heavy<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    config: &PipelineConfig,
) -> nix::Result<()> {
    let cam_heap = config.heap_for_role("camera", heap_name);
    let codec_heap = config.heap_for_role("codec", heap_name);
    let gpu_heap = config.heap_for_role("gpu", heap_name);
    let npu_heap = config.heap_for_role("npu", heap_name);
    let disp_heap = config.heap_for_role("display", heap_name);

    let fail_count = std::sync::atomic::AtomicUsize::new(0);
    let fail_ref = &fail_count;
    let frames = config.frames;

    std::thread::scope(|s| {
        s.spawn(move || {
            run_pool_worker(
                backend,
                cam_heap,
                nv12_size(1920, 1080),
                4,
                frames,
                fail_ref,
            );
        });
        s.spawn(move || {
            run_pool_worker(
                backend,
                codec_heap,
                nv12_size(1920, 1080),
                8,
                frames,
                fail_ref,
            );
        });
        s.spawn(move || {
            run_pool_worker(
                backend,
                gpu_heap,
                argb_size(1080, 1920),
                3,
                frames,
                fail_ref,
            );
        });
        s.spawn(move || {
            run_alloc_free_worker(backend, npu_heap, 602_112, frames, fail_ref);
        });
        s.spawn(move || {
            run_pool_worker(
                backend,
                disp_heap,
                argb_size(1080, 1920),
                3,
                frames,
                fail_ref,
            );
        });
    });

    check_failures(&fail_count)
}

/// Worker: allocate a buffer pool and cycle through it.
#[allow(clippy::cast_possible_truncation)]
fn run_pool_worker<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    buf_size: u64,
    pool_size: usize,
    frames: u32,
    fail_count: &std::sync::atomic::AtomicUsize,
) {
    let Ok(heap) = DmaHeap::open(backend, heap_name) else {
        fail_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    };
    let Ok(mut pool) = BufferPool::new(backend, &heap, buf_size, pool_size) else {
        fail_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    };

    let mut latencies = Vec::with_capacity(frames as usize);
    for frame in 0..frames {
        let start = Instant::now();
        let buf = pool.next();
        if let Ok(ptr) = buf.mmap() {
            let _ = fill_buffer(buf, ptr, buf.len(), (frame & 0xFF) as u8);
        }
        latencies.push(start.elapsed().as_micros() as u64);
    }

    if let Some(stats) = compute_stats(&latencies) {
        tracing::debug!(
            heap = heap_name,
            buf_size,
            pool_size,
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            "pool_worker"
        );
    }
}

/// Worker: per-frame alloc/free (no pool).
#[allow(clippy::cast_possible_truncation)]
fn run_alloc_free_worker<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    buf_size: u64,
    frames: u32,
    fail_count: &std::sync::atomic::AtomicUsize,
) {
    let Ok(heap) = DmaHeap::open(backend, heap_name) else {
        fail_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    };

    let mut latencies = Vec::with_capacity(frames as usize);
    for _ in 0..frames {
        let start = Instant::now();
        match heap.alloc(buf_size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS) {
            Ok(fd) => {
                let buf = DmaBuf::new(backend, fd, buf_size as usize);
                let _ = read_sync(&buf);
                drop(buf);
            }
            Err(Errno::ENOMEM) => break,
            Err(_) => {
                fail_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
        latencies.push(start.elapsed().as_micros() as u64);
    }

    if let Some(stats) = compute_stats(&latencies) {
        tracing::debug!(
            heap = heap_name,
            buf_size,
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            "alloc_free_worker"
        );
    }
}

/// Check if any workers reported failures.
fn check_failures(fail_count: &std::sync::atomic::AtomicUsize) -> nix::Result<()> {
    let failures = fail_count.load(std::sync::atomic::Ordering::Relaxed);
    if failures > 0 {
        tracing::error!(failures, "pipeline worker failures");
        return Err(Errno::EIO);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    fn test_config() -> PipelineConfig {
        PipelineConfig {
            frames: 10,
            workloads: Vec::new(),
        }
    }

    #[test]
    fn camera_preview_runs() {
        let b = MockBackend::new();
        pipeline_camera_preview(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn video_call_runs() {
        let b = MockBackend::new();
        pipeline_video_call(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn ai_camera_runs() {
        let b = MockBackend::new();
        pipeline_ai_camera(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn heavy_runs() {
        let b = MockBackend::new();
        pipeline_heavy(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        run(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn heap_for_role_default() {
        let cfg = test_config();
        assert_eq!(cfg.heap_for_role("camera", "system"), "system");
    }

    #[test]
    fn heap_for_role_override() {
        let cfg = PipelineConfig {
            frames: 10,
            workloads: vec![("camera".into(), "camera_heap".into())],
        };
        assert_eq!(cfg.heap_for_role("camera", "system"), "camera_heap");
        assert_eq!(cfg.heap_for_role("display", "system"), "system");
    }

    #[test]
    fn heavy_no_leak() {
        let b = MockBackend::new();
        pipeline_heavy(&b, "system", &test_config()).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }
}
