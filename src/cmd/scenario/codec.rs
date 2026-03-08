// Video codec workload simulation: DPB decode pool, adaptive streaming,
// transcode (concurrent decode + encode).

use std::time::Instant;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::cmd::perf::compute_stats;
use crate::cmd::scenario::{
    BufferPool, bulk_alloc, fill_buffer, nv12_size, read_sync,
};
use crate::heap::DmaHeap;

/// Codec scenario configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CodecConfig {
    pub width: u32,
    pub height: u32,
    pub dpb_size: usize,
    pub fps: u32,
    pub frames: u32,
    pub resolutions: Vec<(u32, u32)>,
}

impl Default for CodecConfig {
    fn default() -> Self {
        Self {
            width: 3840,
            height: 2160,
            dpb_size: 16,
            fps: 30,
            frames: 60,
            resolutions: vec![(1280, 720), (1920, 1080), (3840, 2160), (1920, 1080)],
        }
    }
}

/// Run all codec scenario tests.
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &CodecConfig,
) -> (Vec<crate::runner::SubTestResult>, Option<Box<dyn std::error::Error>>) {
    let tests: Vec<(&str, nix::Result<()>)> = vec![
        ("decode", codec_decode(backend, heap_name, config)),
        ("adaptive", codec_adaptive(backend, heap_name, config)),
        ("transcode", codec_transcode(backend, heap_name, config)),
    ];
    crate::runner::collect_test_results("codec", &tests)
}

/// DPB pool decode: allocate DPB, cycle through frames.
#[allow(clippy::cast_possible_truncation)]
fn codec_decode<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &CodecConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let frame_size = nv12_size(config.width, config.height);

    // Allocate DPB pool.
    let alloc_start = Instant::now();
    let mut pool = BufferPool::new(backend, &heap, frame_size, config.dpb_size)?;
    let dpb_alloc_us = alloc_start.elapsed().as_micros() as u64;

    // Decode loop: cycle through DPB.
    let mut frame_latencies = Vec::with_capacity(config.frames as usize);
    for frame in 0..config.frames {
        let start = Instant::now();
        let buf = pool.next();
        let ptr = buf.mmap()?;
        fill_buffer(buf, ptr, buf.len(), (frame & 0xFF) as u8)?;
        read_sync(buf)?;
        frame_latencies.push(start.elapsed().as_micros() as u64);
    }

    if let Some(stats) = compute_stats(&frame_latencies) {
        tracing::info!(
            width = config.width,
            height = config.height,
            dpb_size = config.dpb_size,
            fps = config.fps,
            frame_size,
            dpb_alloc_us,
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            "decode"
        );
    }

    Ok(())
}

/// Adaptive streaming: switch DPB resolution on the fly.
#[allow(clippy::cast_possible_truncation)]
fn codec_adaptive<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &CodecConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let mut switch_latencies = Vec::with_capacity(config.resolutions.len());

    for (i, &(w, h)) in config.resolutions.iter().enumerate() {
        let frame_size = nv12_size(w, h);
        let start = Instant::now();
        let (buffers, alloc_lats) = bulk_alloc(backend, &heap, frame_size, config.dpb_size)?;
        let total_us = start.elapsed().as_micros() as u64;
        switch_latencies.push(total_us);

        if let Some(stats) = compute_stats(&alloc_lats) {
            tracing::info!(
                phase = i + 1,
                width = w,
                height = h,
                frame_size,
                total_us,
                p50_us = stats.p50_us,
                "adaptive"
            );
        }

        drop(buffers);
    }

    if let Some(stats) = compute_stats(&switch_latencies) {
        tracing::info!(
            switches = config.resolutions.len(),
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            "adaptive_summary"
        );
    }

    Ok(())
}

/// Transcode: concurrent decoder DPB + encoder pools.
#[allow(clippy::cast_possible_truncation)]
fn codec_transcode<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    config: &CodecConfig,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let frame_size = nv12_size(config.width, config.height);

    // Decoder DPB pool.
    let mut dec_pool = BufferPool::new(backend, &heap, frame_size, config.dpb_size)?;

    // Encoder input/output pools (smaller: half resolution encode).
    let enc_size = nv12_size(config.width / 2, config.height / 2);
    let enc_pool_size = 4;
    let mut enc_pool = BufferPool::new(backend, &heap, enc_size, enc_pool_size)?;

    let mut latencies = Vec::with_capacity(config.frames as usize);

    for frame in 0..config.frames {
        let start = Instant::now();

        // Decode: write to DPB buffer.
        let dec_buf = dec_pool.next();
        let dec_ptr = dec_buf.mmap()?;
        fill_buffer(dec_buf, dec_ptr, dec_buf.len(), (frame & 0xFF) as u8)?;

        // Encode: write to encoder buffer.
        let enc_buf = enc_pool.next();
        let enc_ptr = enc_buf.mmap()?;
        fill_buffer(enc_buf, enc_ptr, enc_buf.len(), (frame & 0xFF) as u8)?;
        read_sync(enc_buf)?;

        latencies.push(start.elapsed().as_micros() as u64);
    }

    if let Some(stats) = compute_stats(&latencies) {
        tracing::info!(
            frames = config.frames,
            dec_frame_size = frame_size,
            enc_frame_size = enc_size,
            p50_us = stats.p50_us,
            p95_us = stats.p95_us,
            "transcode"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    fn test_config() -> CodecConfig {
        CodecConfig {
            width: 320,
            height: 240,
            dpb_size: 4,
            fps: 30,
            frames: 20,
            resolutions: vec![(160, 120), (320, 240), (160, 120)],
        }
    }

    #[test]
    fn nv12_size_calc() {
        assert_eq!(nv12_size(1920, 1080), 1920 * 1080 * 3 / 2);
    }

    #[test]
    fn decode_runs() {
        let b = MockBackend::new();
        codec_decode(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn adaptive_runs() {
        let b = MockBackend::new();
        codec_adaptive(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn transcode_runs() {
        let b = MockBackend::new();
        codec_transcode(&b, "system", &test_config()).unwrap();
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        let (results, err) = run(&b, "system", &test_config());
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
    }

    #[test]
    fn decode_no_leak() {
        let b = MockBackend::new();
        codec_decode(&b, "system", &test_config()).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn adaptive_no_leak() {
        let b = MockBackend::new();
        codec_adaptive(&b, "system", &test_config()).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }

    #[test]
    fn transcode_no_leak() {
        let b = MockBackend::new();
        codec_transcode(&b, "system", &test_config()).unwrap();
        assert_eq!(b.buffer_count(), 0);
    }
}
