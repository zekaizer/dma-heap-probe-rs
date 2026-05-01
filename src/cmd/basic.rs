// Basic deterministic smoke tests: one-shot validation of each fundamental
// dma-heap/dma-buf operation. Sweeps `--sizes` at the suite level — each size
// runs the full suite once, then advances to the next size.
//
// Heavy/repetitive coverage lives elsewhere:
//   - sustained alloc/free loops → `aging`
//   - concurrent multi-thread access → `aging --threads` or `perf`
//   - latency distribution → `histogram`
//   - micro-op timing → `microbench`

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_RW, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::probe::align_to;
use crate::runner::{self, SubTestResult};

/// Number of buffers used in the zero-on-alloc test (lower than before:
/// basic only needs to confirm zeroing happens, not stress test it).
const ZEROED_TEST_COUNT: usize = 4;

/// Run the basic smoke suite per heap. Outer loop sweeps `sizes`; for each
/// size the inner suite runs once. Tests stay atomic (own alloc/close).
///
/// Each test that depends on mmap is auto-skipped on heaps without mmap
/// support (e.g. protected heaps).
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    heap_w: usize,
) -> (Vec<SubTestResult>, Option<anyhow::Error>) {
    tracing::debug!(heap = heap_name, ?sizes, "basic sequence");

    let caps = crate::probe::probe_heap(backend, heap_name);
    let mmap_ok = caps.can_mmap;
    let granularity = caps.alloc_granularity;

    // Build a flat list of (size-prefixed name, result, skipped) entries,
    // running each inner test once per size before advancing.
    let mut names: Vec<String> = Vec::new();
    let mut results: Vec<nix::Result<()>> = Vec::new();
    let mut skipped: Vec<bool> = Vec::new();

    for &size in sizes {
        let label = format_size_label(size);

        // alloc/close — always
        names.push(format!("{label}::alloc_close"));
        results.push(test_alloc_close(backend, heap_name, size));
        skipped.push(false);

        // write/read verify — needs mmap
        names.push(format!("{label}::write_read_verify"));
        if mmap_ok {
            results.push(test_write_read_verify(backend, heap_name, size));
            skipped.push(false);
        } else {
            results.push(Ok(()));
            skipped.push(true);
        }

        // zero-on-alloc security check — needs mmap
        names.push(format!("{label}::zero_on_alloc"));
        if mmap_ok {
            results.push(test_zero_on_alloc(backend, heap_name, size));
            skipped.push(false);
        } else {
            results.push(Ok(()));
            skipped.push(true);
        }

        // llseek size reporting
        names.push(format!("{label}::llseek_size"));
        results.push(test_llseek_size(backend, heap_name, size, granularity));
        skipped.push(false);

        // sync_file export
        names.push(format!("{label}::export_sync_file"));
        results.push(test_export_sync_file(backend, heap_name, size));
        skipped.push(false);

        // sync_file export+import roundtrip
        names.push(format!("{label}::import_sync_file"));
        results.push(test_import_sync_file(backend, heap_name, size));
        skipped.push(false);

        // dup outlives original close
        names.push(format!("{label}::dup_survives_close"));
        results.push(test_dup_survives_close(backend, heap_name, size));
        skipped.push(false);
    }

    let tests: Vec<(&str, nix::Result<()>, bool)> = names
        .iter()
        .zip(results)
        .zip(skipped)
        .map(|((name, res), skip)| (name.as_str(), res, skip))
        .collect();

    runner::collect_test_results("basic", heap_name, heap_w, &tests)
}

/// Format a byte size as a short label suitable for a test name prefix.
/// Examples: 4096 → "4K", 65536 → "64K", 1048576 → "1M", 1073741824 → "1G".
/// Falls back to the raw number for non-power-of-two values.
fn format_size_label(size: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = 1024 * 1024;
    const G: u64 = 1024 * 1024 * 1024;

    if size >= G && size.is_multiple_of(G) {
        format!("{}G", size / G)
    } else if size >= M && size.is_multiple_of(M) {
        format!("{}M", size / M)
    } else if size >= K && size.is_multiple_of(K) {
        format!("{}K", size / K)
    } else {
        format!("{size}B")
    }
}

// ── alloc_close ─────────────────────────────────────────────────────────────

/// Alloc → close (no mmap). Verifies the basic alloc path works on this heap.
#[allow(clippy::cast_possible_truncation)]
fn test_alloc_close<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
) -> nix::Result<()> {
    tracing::debug!(size, "alloc_close");
    let heap = DmaHeap::open(backend, heap_name)?;
    let buf_fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, buf_fd, size as usize);
    drop(buf);
    Ok(())
}

// ── write_read_verify ───────────────────────────────────────────────────────

/// Alloc → mmap → pattern write → read verify. Folds mmap+sync+write into a
/// single check.
#[allow(clippy::cast_possible_truncation)]
fn test_write_read_verify<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
) -> nix::Result<()> {
    tracing::debug!(size, "write_read_verify");
    let heap = DmaHeap::open(backend, heap_name)?;
    let buf_fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let mut buf = DmaBuf::new(backend, buf_fd, size as usize);
    let ptr = buf.mmap()?;

    // Write pattern
    buf.sync_start(DMA_BUF_SYNC_WRITE)?;
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr, size as usize) };
    for (i, byte) in slice.iter_mut().enumerate() {
        *byte = (i % 256) as u8;
    }
    buf.sync_end(DMA_BUF_SYNC_WRITE)?;

    // Read back and verify
    buf.sync_start(DMA_BUF_SYNC_READ)?;
    let slice = unsafe { std::slice::from_raw_parts(ptr, size as usize) };
    for (i, &byte) in slice.iter().enumerate() {
        if byte != (i % 256) as u8 {
            tracing::error!(
                size,
                offset = i,
                expected = (i % 256),
                got = byte,
                "data mismatch"
            );
            return Err(Errno::EIO);
        }
    }
    buf.sync_end(DMA_BUF_SYNC_READ)?;
    Ok(())
}

// ── zero_on_alloc ───────────────────────────────────────────────────────────

/// Contaminate a small set of buffers, free, reallocate, and verify the new
/// buffers come back zeroed. Security-relevant smoke check.
#[allow(clippy::cast_possible_truncation)]
fn test_zero_on_alloc<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
) -> nix::Result<()> {
    tracing::debug!(size, "zero_on_alloc");
    let heap = DmaHeap::open(backend, heap_name)?;

    // Pass 1: contaminate and free
    {
        let mut bufs = Vec::with_capacity(ZEROED_TEST_COUNT);
        for _ in 0..ZEROED_TEST_COUNT {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let mut buf = DmaBuf::new(backend, fd, size as usize);
            let ptr = buf.mmap()?;
            buf.sync_start(DMA_BUF_SYNC_WRITE)?;
            let slice = unsafe { std::slice::from_raw_parts_mut(ptr, size as usize) };
            slice.fill(0xAA);
            buf.sync_end(DMA_BUF_SYNC_WRITE)?;
            bufs.push(buf);
        }
    }

    // Pass 2: realloc and verify zero
    for _ in 0..ZEROED_TEST_COUNT {
        let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let mut buf = DmaBuf::new(backend, fd, size as usize);
        let ptr = buf.mmap()?;
        buf.sync_start(DMA_BUF_SYNC_READ)?;
        let slice = unsafe { std::slice::from_raw_parts(ptr, size as usize) };
        if let Some(pos) = slice.iter().position(|&b| b != 0) {
            tracing::error!(
                size,
                offset = pos,
                got = slice[pos],
                "non-zero byte in fresh buffer"
            );
            return Err(Errno::EIO);
        }
        buf.sync_end(DMA_BUF_SYNC_READ)?;
    }
    Ok(())
}

// ── llseek_size ─────────────────────────────────────────────────────────────

/// Verify `llseek(SEEK_END)` returns the granularity-aligned buffer size.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn test_llseek_size<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
    granularity: u64,
) -> nix::Result<()> {
    tracing::debug!(size, granularity, "llseek_size");
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, fd, size as usize);

    let reported = buf.llseek_size()?;
    let expected = align_to(size, granularity) as i64;

    if reported != expected {
        tracing::error!(
            size,
            reported,
            expected,
            granularity,
            "llseek size mismatch"
        );
        return Err(Errno::EIO);
    }
    Ok(())
}

// ── sync_file export ────────────────────────────────────────────────────────

/// Export `sync_file` once with READ flags. Verifies the returned fd is valid.
#[allow(clippy::cast_possible_truncation)]
fn test_export_sync_file<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
) -> nix::Result<()> {
    tracing::debug!(size, "export_sync_file");
    let heap = DmaHeap::open(backend, heap_name)?;
    let buf_fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, buf_fd, size as usize);

    let sync_fd = buf.export_sync_file(DMA_BUF_SYNC_RW as u32)?;
    if sync_fd < 0 {
        tracing::error!(sync_fd, "invalid sync_file fd");
        return Err(Errno::EIO);
    }
    backend.close(sync_fd)?;
    Ok(())
}

// ── sync_file import roundtrip ──────────────────────────────────────────────

/// Export and re-import a `sync_file`. Verifies the full roundtrip.
#[allow(clippy::cast_possible_truncation)]
fn test_import_sync_file<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
) -> nix::Result<()> {
    tracing::debug!(size, "import_sync_file");
    let heap = DmaHeap::open(backend, heap_name)?;
    let buf_fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, buf_fd, size as usize);

    let sync_fd = buf.export_sync_file(DMA_BUF_SYNC_READ as u32)?;
    buf.import_sync_file(DMA_BUF_SYNC_READ as u32, sync_fd)?;
    backend.close(sync_fd)?;
    Ok(())
}

// ── dup outlives original ───────────────────────────────────────────────────

/// Dup a dma-buf fd, close the original, verify the dup still works via llseek.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn test_dup_survives_close<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    size: u64,
) -> nix::Result<()> {
    tracing::debug!(size, "dup_survives_close");
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, fd, size as usize);

    let orig_size = buf.llseek_size()?;
    let dup_buf = buf.dup()?;
    drop(buf);

    let dup_size = dup_buf.llseek_size()?;
    if dup_size != orig_size {
        tracing::error!(
            expected = orig_size,
            got = dup_size,
            "dup llseek size mismatch"
        );
        return Err(Errno::EIO);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    // ── format_size_label ──

    #[test]
    fn label_kib() {
        assert_eq!(format_size_label(4096), "4K");
        assert_eq!(format_size_label(65536), "64K");
    }

    #[test]
    fn label_mib() {
        assert_eq!(format_size_label(1_048_576), "1M");
        assert_eq!(format_size_label(8 * 1_048_576), "8M");
    }

    #[test]
    fn label_gib() {
        assert_eq!(format_size_label(1_073_741_824), "1G");
    }

    #[test]
    fn label_bytes() {
        assert_eq!(format_size_label(1), "1B");
        assert_eq!(format_size_label(4097), "4097B");
    }

    // ── align_to helper (kept here for backward-compat coverage) ──

    #[test]
    fn align_to_zero_size() {
        assert_eq!(align_to(0, 4096), 0);
    }

    #[test]
    fn align_to_zero_granularity() {
        assert_eq!(align_to(4096, 0), 4096);
        assert_eq!(align_to(0, 0), 0);
        assert_eq!(align_to(1, 0), 1);
    }

    #[test]
    fn align_to_exact() {
        assert_eq!(align_to(4096, 4096), 4096);
        assert_eq!(align_to(8192, 4096), 8192);
    }

    #[test]
    fn align_to_rounds_up() {
        assert_eq!(align_to(1, 4096), 4096);
        assert_eq!(align_to(4097, 4096), 8192);
        assert_eq!(align_to(4095, 4096), 4096);
    }

    // ── per-test smoke ──

    #[test]
    fn alloc_close_basic() {
        let backend = MockBackend::new();
        test_alloc_close(&backend, "system", 4096).unwrap();
    }

    #[test]
    fn write_read_verify_single_page() {
        let backend = MockBackend::new();
        test_write_read_verify(&backend, "system", 4096).unwrap();
    }

    #[test]
    fn zero_on_alloc_single() {
        let backend = MockBackend::new();
        test_zero_on_alloc(&backend, "system", 4096).unwrap();
    }

    #[test]
    fn llseek_aligned() {
        let backend = MockBackend::new();
        test_llseek_size(&backend, "system", 4096, 4096).unwrap();
    }

    #[test]
    fn llseek_unaligned() {
        let backend = MockBackend::new();
        test_llseek_size(&backend, "system", 1, 4096).unwrap();
        test_llseek_size(&backend, "system", 4097, 4096).unwrap();
    }

    #[test]
    fn export_returns_valid_fd() {
        let backend = MockBackend::new();
        test_export_sync_file(&backend, "system", 4096).unwrap();
    }

    #[test]
    fn import_after_export() {
        let backend = MockBackend::new();
        test_import_sync_file(&backend, "system", 4096).unwrap();
    }

    #[test]
    fn dup_survives_original_close() {
        let backend = MockBackend::new();
        test_dup_survives_close(&backend, "system", 4096).unwrap();
    }

    // ── run() integration ──

    #[test]
    fn run_passes() {
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "system", &[4096, 65536], 6);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert!(results.iter().all(|t| !t.skipped));
        // 7 tests × 2 sizes
        assert_eq!(results.len(), 14);
    }

    #[test]
    fn run_size_label_in_names() {
        let backend = MockBackend::new();
        let (results, _) = run(&backend, "system", &[4096, 65536], 6);
        assert!(results.iter().any(|r| r.name.starts_with("4K::")));
        assert!(results.iter().any(|r| r.name.starts_with("64K::")));
    }

    #[test]
    fn run_bad_heap() {
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "", &[4096], 6);
        assert!(err.is_some());
        assert!(results.iter().any(|r| !r.passed));
    }

    #[test]
    fn run_restricted_heap_skips_mmap_tests() {
        use crate::backend::mock::HeapProfile;
        use std::collections::HashMap;

        let mut configs = HashMap::new();
        configs.insert("restricted".to_string(), HeapProfile::restricted());
        let backend = MockBackend::with_heaps(configs);

        let (results, err) = run(&backend, "restricted", &[4096], 10);
        assert!(err.is_none(), "restricted heap run should not error");

        // mmap-dependent tests must be skipped
        for name in &["write_read_verify", "zero_on_alloc"] {
            let r = results.iter().find(|r| r.name.ends_with(name)).unwrap();
            assert!(r.skipped, "{name} should be skipped on restricted heap");
            assert!(r.passed, "{name} should still pass (skipped = ok)");
        }

        // Non-mmap tests must run (not skipped)
        for name in &[
            "alloc_close",
            "llseek_size",
            "export_sync_file",
            "import_sync_file",
            "dup_survives_close",
        ] {
            let r = results.iter().find(|r| r.name.ends_with(name)).unwrap();
            assert!(!r.skipped, "{name} should run on restricted heap");
            assert!(r.passed, "{name} should pass on restricted heap");
        }
    }
}
