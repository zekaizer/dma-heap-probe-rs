// Camera workload simulation: preview buffer pool, burst capture,
// resolution switch, multi-stream.

use std::time::Instant;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::cmd::perf::compute_stats;
use crate::cmd::scenario::{BufferPool, bulk_alloc, check_thread_failures, fill_buffer};
use crate::heap::DmaHeap;

/// Camera scenario configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CameraConfig {
    pub width: u32,
    pub height: u32,
    pub format: CameraFormat,
    pub pool_size: usize,
    pub fps: u32,
    pub frames: u32,
    pub burst_format: CameraFormat,
    pub burst_count: u32,
    pub resolutions: Vec<(u32, u32)>,
    pub streams: Vec<StreamConfig>,
}

/// Camera pixel format with bytes-per-pixel multiplier.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraFormat {
    Nv12,
    Raw10,
    Raw16,
}

impl CameraFormat {
    /// Bytes per pixel (as a fraction: numerator per pixel).
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn buffer_size(self, width: u32, height: u32) -> u64 {
        let pixels = u64::from(width) * u64::from(height);
        match self {
            Self::Nv12 => pixels * 3 / 2,  // 1.5 bpp
            Self::Raw10 => pixels * 5 / 4, // 1.25 bpp
            Self::Raw16 => pixels * 2,     // 2.0 bpp
        }
    }
}

/// Configuration for a single camera stream.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub format: CameraFormat,
    pub pool_size: usize,
    pub frames: u32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            format: CameraFormat::Nv12,
            pool_size: 8,
            frames: 30,
        }
    }
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            format: CameraFormat::Nv12,
            pool_size: 8,
            fps: 30,
            frames: 100,
            burst_format: CameraFormat::Raw16,
            burst_count: 10,
            resolutions: vec![(1920, 1080), (4000, 3000), (1920, 1080)],
            streams: vec![
                StreamConfig {
                    width: 1920,
                    height: 1080,
                    format: CameraFormat::Nv12,
                    pool_size: 8,
                    frames: 30,
                },
                StreamConfig {
                    width: 1280,
                    height: 720,
                    format: CameraFormat::Nv12,
                    pool_size: 6,
                    frames: 30,
                },
                StreamConfig {
                    width: 640,
                    height: 480,
                    format: CameraFormat::Raw10,
                    pool_size: 4,
                    frames: 30,
                },
            ],
        }
    }
}

/// Run all camera scenario tests.
pub fn run<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    config: &CameraConfig,
) -> (Vec<crate::runner::SubTestResult>, Option<anyhow::Error>) {
    let preview_buf_size = config.format.buffer_size(config.width, config.height);
    let capture_buf_size = config.burst_format.buffer_size(4000, 3000);
    println!("scenario camera sequence:");
    println!("  heap: {heap_name}");
    println!(
        "  preview: {}x{} {:?} (buf_size: {preview_buf_size})",
        config.width, config.height, config.format
    );
    println!(
        "  pool_size: {}, fps: {}, frames: {}",
        config.pool_size, config.fps, config.frames
    );
    println!(
        "  burst: {:?} x {} (buf_size: {capture_buf_size})",
        config.burst_format, config.burst_count
    );
    println!("  resolutions: {:?}", config.resolutions);
    println!("  streams: {} concurrent", config.streams.len());
    for (i, s) in config.streams.iter().enumerate() {
        println!(
            "    stream {i}: {}x{} {:?} pool={} frames={}",
            s.width, s.height, s.format, s.pool_size, s.frames
        );
    }
    println!();
    println!(
        "  phase 1: preview       alloc {}-buffer pool, cycle {} frames with mmap+write",
        config.pool_size, config.frames
    );
    println!(
        "  phase 2: capture       burst alloc {} x high-res {:?} buffers, measure close latency",
        config.burst_count, config.burst_format
    );
    println!(
        "  phase 3: switch        {} resolution transitions, realloc pool each time",
        config.resolutions.len()
    );
    println!(
        "  phase 4: multi_stream  {} concurrent streams with independent buffer pools",
        config.streams.len()
    );
    println!("  cleanup: release all pools");
    println!();
    println!("scenario camera result legend:");
    println!("  buf_size        per-frame buffer size (bytes)");
    println!("  pool_size       number of buffers in rotation pool");
    println!("  capture_size    burst capture buffer size");
    println!("  close_us        time to release all burst buffers");
    println!("  p50/p95_us      per-frame or per-switch latency percentiles");
    println!("  max_us          worst-case alloc latency in burst");
    println!("  stream_id       concurrent stream thread index");
    println!();

    let tests: Vec<(&str, nix::Result<()>)> = vec![
        ("preview", camera_preview(backend, heap_name, config)),
        ("capture", camera_capture(backend, heap_name, config)),
        ("switch", camera_switch(backend, heap_name, config)),
        (
            "multi_stream",
            camera_multi_stream(backend, heap_name, config),
        ),
    ];
    crate::runner::collect_test_results("camera", &tests)
}

/// Preview buffer pool: allocate pool, cycle through buffers writing frames.
#[allow(clippy::cast_possible_truncation)]
fn camera_preview<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &CameraConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let buf_size = config.format.buffer_size(config.width, config.height);

    let mut pool = BufferPool::new(backend, &heap, buf_size, config.pool_size)?;
    let mut frame_latencies = Vec::with_capacity(config.frames as usize);

    for frame in 0..config.frames {
        let start = Instant::now();
        let buf = pool.next();
        let ptr = buf.mmap()?;
        fill_buffer(buf, ptr, buf.len(), (frame & 0xFF) as u8)?;
        frame_latencies.push(start.elapsed().as_micros() as u64);
    }

    if let Some(stats) = compute_stats(&frame_latencies) {
        println!(
            "scenario camera: preview buf_size={buf_size} pool_size={} p50_us={} p95_us={}",
            config.pool_size, stats.p50_us, stats.p95_us
        );
    }

    Ok(())
}

/// Burst capture: allocate high-res buffers in rapid succession.
#[allow(clippy::cast_possible_truncation)]
fn camera_capture<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &CameraConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let capture_size = config.burst_format.buffer_size(4000, 3000);

    let (buffers, latencies) =
        bulk_alloc(backend, &heap, capture_size, config.burst_count as usize)?;

    if let Some(stats) = compute_stats(&latencies) {
        println!(
            "scenario camera: capture capture_size={capture_size} burst_count={} p50_us={} p95_us={} max_us={}",
            config.burst_count, stats.p50_us, stats.p95_us, stats.max_us
        );
    }

    // Measure close latency.
    let close_start = Instant::now();
    drop(buffers);
    let close_us = close_start.elapsed().as_micros() as u64;
    println!("scenario camera: capture_release close_us={close_us}");

    Ok(())
}

/// Resolution switch: alloc pool at res A → free → alloc at res B → repeat.
#[allow(clippy::cast_possible_truncation)]
fn camera_switch<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &CameraConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for (i, &(w, h)) in config.resolutions.iter().enumerate() {
        let buf_size = config.format.buffer_size(w, h);
        let start = Instant::now();
        let (buffers, latencies) = bulk_alloc(backend, &heap, buf_size, config.pool_size)?;
        let alloc_us = start.elapsed().as_micros() as u64;

        if let Some(stats) = compute_stats(&latencies) {
            println!(
                "scenario camera: switch phase={} width={w} height={h} buf_size={buf_size} alloc_us={alloc_us} p50_us={}",
                i + 1,
                stats.p50_us
            );
        }

        drop(buffers);
    }

    Ok(())
}

/// Multi-stream: run concurrent camera streams with independent pools.
#[allow(clippy::cast_possible_truncation)]
fn camera_multi_stream<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    config: &CameraConfig,
) -> nix::Result<()> {
    let fail_count = std::sync::atomic::AtomicUsize::new(0);
    let fail_ref = &fail_count;
    let streams = &config.streams;

    std::thread::scope(|s| {
        for (stream_id, stream) in streams.iter().enumerate() {
            s.spawn(move || {
                let Ok(heap) = DmaHeap::open(backend, heap_name) else {
                    fail_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return;
                };
                let buf_size = stream.format.buffer_size(stream.width, stream.height);

                let Ok(mut pool) = BufferPool::new(backend, &heap, buf_size, stream.pool_size)
                else {
                    fail_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return;
                };

                let mut latencies = Vec::with_capacity(stream.frames as usize);
                for frame in 0..stream.frames {
                    let start = Instant::now();
                    let buf = pool.next();
                    if let Ok(ptr) = buf.mmap() {
                        let _ = fill_buffer(buf, ptr, buf.len(), (frame & 0xFF) as u8);
                    }
                    latencies.push(start.elapsed().as_micros() as u64);
                }

                if let Some(stats) = compute_stats(&latencies) {
                    println!(
                        "scenario camera: multi_stream stream_id={stream_id} frames={} p50_us={} p95_us={}",
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

    fn test_config() -> CameraConfig {
        CameraConfig {
            width: 320,
            height: 240,
            format: CameraFormat::Nv12,
            pool_size: 4,
            fps: 30,
            frames: 20,
            burst_format: CameraFormat::Raw10,
            burst_count: 5,
            resolutions: vec![(320, 240), (640, 480), (320, 240)],
            streams: vec![
                StreamConfig {
                    width: 320,
                    height: 240,
                    format: CameraFormat::Nv12,
                    pool_size: 3,
                    frames: 10,
                },
                StreamConfig {
                    width: 160,
                    height: 120,
                    format: CameraFormat::Nv12,
                    pool_size: 2,
                    frames: 10,
                },
            ],
        }
    }

    #[test]
    fn buffer_size_nv12() {
        assert_eq!(CameraFormat::Nv12.buffer_size(1920, 1080), 3_110_400);
    }

    #[test]
    fn buffer_size_raw10() {
        assert_eq!(CameraFormat::Raw10.buffer_size(4000, 3000), 15_000_000);
    }

    #[test]
    fn buffer_size_raw16() {
        assert_eq!(CameraFormat::Raw16.buffer_size(4000, 3000), 24_000_000);
    }

    #[test]
    fn preview_runs() {
        let b = MockBackend::new();
        let cfg = test_config();
        camera_preview(&b, "system", &cfg).unwrap();
    }

    #[test]
    fn capture_runs() {
        let b = MockBackend::new();
        let cfg = test_config();
        camera_capture(&b, "system", &cfg).unwrap();
    }

    #[test]
    fn switch_runs() {
        let b = MockBackend::new();
        let cfg = test_config();
        camera_switch(&b, "system", &cfg).unwrap();
    }

    #[test]
    fn multi_stream_runs() {
        let b = MockBackend::new();
        let cfg = test_config();
        camera_multi_stream(&b, "system", &cfg).unwrap();
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
    fn preview_no_leak() {
        let b = MockBackend::new();
        let cfg = test_config();
        camera_preview(&b, "system", &cfg).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn capture_no_leak() {
        let b = MockBackend::new();
        let cfg = test_config();
        camera_capture(&b, "system", &cfg).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }
}
