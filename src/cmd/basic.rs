// Basic deterministic tests: alloc, mmap, sync, llseek, zeroed, repeated,
// sync_file export/import, concurrent alloc, dup.

use std::sync::atomic::{AtomicUsize, Ordering};

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_RW, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};

/// Number of buffers used in the zeroed-page test.
const ZEROED_TEST_COUNT: usize = 16;

/// Default allocation size for edge tests (concurrent, dup).
const EDGE_ALLOC_SIZE: u64 = 4096;

/// Round `size` up to the nearest multiple of `granularity`.
fn align_to(size: u64, granularity: u64) -> u64 {
    size.next_multiple_of(granularity)
}

/// Run all basic deterministic tests. Executes all tests even if some fail;
/// returns sub-test results (and the first error, if any).
pub fn run<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    repeat: u32,
    threads: u32,
    heap_w: usize,
) -> (Vec<SubTestResult>, Option<anyhow::Error>) {
    tracing::debug!(heap = heap_name, ?sizes, repeat, threads, "basic sequence");

    let caps = crate::probe::probe_heap(backend, heap_name);

    // Heap not available — no tests to run.
    if !caps.can_alloc {
        return (
            vec![],
            Some(anyhow::anyhow!(
                "heap '{heap_name}' not available (open/alloc failed)"
            )),
        );
    }

    let mmap_ok = caps.can_mmap;
    let granularity = caps.alloc_granularity;

    let tests: [(&str, nix::Result<()>, bool); 8] = [
        (
            "alloc_and_map",
            if mmap_ok {
                test_alloc_and_map(backend, heap_name, sizes)
            } else {
                Ok(())
            },
            !mmap_ok,
        ),
        (
            "alloc_zeroed",
            if mmap_ok {
                test_alloc_zeroed(backend, heap_name, sizes)
            } else {
                Ok(())
            },
            !mmap_ok,
        ),
        (
            "repeated_alloc",
            if mmap_ok {
                test_repeated_alloc(backend, heap_name, sizes, repeat)
            } else {
                Ok(())
            },
            !mmap_ok,
        ),
        (
            "llseek_size",
            test_llseek_size(backend, heap_name, sizes, granularity),
            false,
        ),
        (
            "export_sync_file",
            test_export_sync_file(backend, heap_name),
            false,
        ),
        (
            "import_sync_file",
            test_import_sync_file(backend, heap_name),
            false,
        ),
        (
            "concurrent_alloc",
            if mmap_ok {
                test_concurrent_alloc(backend, heap_name, threads)
            } else {
                Ok(())
            },
            !mmap_ok,
        ),
        (
            "dup_fd",
            if mmap_ok {
                test_dup_fd(backend, heap_name)
            } else {
                Ok(())
            },
            !mmap_ok,
        ),
    ];

    runner::collect_test_results("basic", heap_name, heap_w, &tests)
}

/// Alloc → mmap → pattern write → read verify for each size.
#[allow(clippy::cast_possible_truncation)]
fn test_alloc_and_map<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        tracing::debug!(size, "alloc_and_map");
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

        // Read and verify
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
        // Drop handles munmap + close
    }

    Ok(())
}

/// Alloc 16 buffers, contaminate with 0xAA, close, reallocate, verify zeroed.
#[allow(clippy::cast_possible_truncation)]
fn test_alloc_zeroed<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        tracing::debug!(size, "alloc_zeroed");

        // Pass 1: contaminate
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
            // All bufs dropped here — munmap + close
        }

        // Pass 2: verify zero
        {
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
        }
    }

    Ok(())
}

/// Repeated alloc/mmap/close loop to verify stability and absence of leaks.
#[allow(clippy::cast_possible_truncation)]
fn test_repeated_alloc<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    repeat: u32,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        tracing::debug!(size, repeat, "repeated_alloc");

        for i in 0..repeat {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let mut buf = DmaBuf::new(backend, fd, size as usize);
            let ptr = buf.mmap()?;

            buf.sync_start(DMA_BUF_SYNC_WRITE)?;
            // Write iteration index into the first 4 bytes
            let tag = i.to_ne_bytes();
            let len = tag.len().min(size as usize);
            unsafe {
                std::ptr::copy_nonoverlapping(tag.as_ptr(), ptr, len);
            }
            buf.sync_end(DMA_BUF_SYNC_WRITE)?;
            // Drop handles munmap + close
        }
    }

    Ok(())
}

/// Verify that `llseek(SEEK_END)` returns the granularity-aligned buffer size.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn test_llseek_size<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    granularity: u64,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        tracing::debug!(size, granularity, "llseek_size");
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
    }

    Ok(())
}

// ── Sync-file tests ─────────────────────────────────────────────────────────

/// Export `sync_file` with each valid flag combination and verify returned fd.
#[allow(clippy::cast_possible_truncation)]
fn test_export_sync_file<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let size: u64 = 4096;
    let buf_fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, buf_fd, size as usize);

    for &flags in &[DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE, DMA_BUF_SYNC_RW] {
        let sync_fd = buf.export_sync_file(flags as u32)?;
        tracing::debug!(flags, sync_fd, "exported sync_file");
        if sync_fd < 0 {
            tracing::error!(flags, sync_fd, "invalid sync_file fd");
            return Err(Errno::EIO);
        }
    }

    Ok(())
}

/// Export then import a `sync_file` to verify the full roundtrip.
#[allow(clippy::cast_possible_truncation)]
fn test_import_sync_file<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let size: u64 = 4096;
    let buf_fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, buf_fd, size as usize);

    let sync_fd = buf.export_sync_file(DMA_BUF_SYNC_READ as u32)?;
    tracing::debug!(sync_fd, "exported for import test");

    buf.import_sync_file(DMA_BUF_SYNC_READ as u32, sync_fd)?;
    tracing::debug!(sync_fd, "imported sync_file");

    Ok(())
}

// ── Edge / boundary tests ───────────────────────────────────────────────────

/// Concurrent alloc → mmap → sync → write → verify → close from N threads.
#[allow(clippy::cast_possible_truncation)]
fn test_concurrent_alloc<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    threads: u32,
) -> nix::Result<()> {
    let fail_count = AtomicUsize::new(0);
    let fail_ref = &fail_count;

    std::thread::scope(|s| {
        for tid in 0..threads {
            s.spawn(move || {
                let result = (|| -> nix::Result<()> {
                    let heap = DmaHeap::open(backend, heap_name)?;
                    let fd = heap.alloc(
                        EDGE_ALLOC_SIZE,
                        DMA_HEAP_ALLOC_FD_FLAGS,
                        DMA_HEAP_VALID_HEAP_FLAGS,
                    )?;
                    let mut buf = DmaBuf::new(backend, fd, EDGE_ALLOC_SIZE as usize);
                    let ptr = buf.mmap()?;

                    // Write thread-unique pattern
                    buf.sync_start(DMA_BUF_SYNC_WRITE)?;
                    let pattern = (tid % 256) as u8;
                    let slice =
                        unsafe { std::slice::from_raw_parts_mut(ptr, EDGE_ALLOC_SIZE as usize) };
                    slice.fill(pattern);
                    buf.sync_end(DMA_BUF_SYNC_WRITE)?;

                    // Read and verify
                    buf.sync_start(DMA_BUF_SYNC_READ)?;
                    let slice =
                        unsafe { std::slice::from_raw_parts(ptr, EDGE_ALLOC_SIZE as usize) };
                    if let Some(pos) = slice.iter().position(|&b| b != pattern) {
                        tracing::error!(tid, pos, expected = pattern, got = slice[pos], "mismatch");
                        return Err(Errno::EIO);
                    }
                    buf.sync_end(DMA_BUF_SYNC_READ)?;

                    Ok(())
                })();

                if result.is_err() {
                    fail_ref.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    let failures = fail_count.load(Ordering::Relaxed);
    if failures > 0 {
        tracing::error!(failures, threads, "concurrent alloc failures");
        return Err(Errno::EIO);
    }

    tracing::debug!(threads, "all concurrent threads passed");
    Ok(())
}

/// Dup a dma-buf fd, close the original, and verify the dup still works.
#[allow(clippy::cast_possible_truncation)]
fn test_dup_fd<B: HeapBackend + DmaBufBackend>(backend: &B, heap_name: &str) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(
        EDGE_ALLOC_SIZE,
        DMA_HEAP_ALLOC_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;
    let mut buf = DmaBuf::new(backend, fd, EDGE_ALLOC_SIZE as usize);

    // Write pattern via original
    let ptr = buf.mmap()?;
    buf.sync_start(DMA_BUF_SYNC_WRITE)?;
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr, EDGE_ALLOC_SIZE as usize) };
    slice.fill(0xBB);
    buf.sync_end(DMA_BUF_SYNC_WRITE)?;

    // Dup then drop original
    let mut dup_buf = buf.dup()?;
    drop(buf);

    // Verify data via dup
    let dup_ptr = dup_buf.mmap()?;
    dup_buf.sync_start(DMA_BUF_SYNC_READ)?;
    let dup_slice = unsafe { std::slice::from_raw_parts(dup_ptr, EDGE_ALLOC_SIZE as usize) };
    if let Some(pos) = dup_slice.iter().position(|&b| b != 0xBB) {
        tracing::error!(
            pos,
            expected = 0xBB,
            got = dup_slice[pos],
            "dup data mismatch"
        );
        return Err(Errno::EIO);
    }
    dup_buf.sync_end(DMA_BUF_SYNC_READ)?;

    tracing::debug!("dup_fd passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    // ── align_to helper ──

    #[test]
    fn align_to_zero() {
        assert_eq!(align_to(0, 4096), 0);
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

    #[test]
    fn align_to_large_granularity() {
        assert_eq!(align_to(1, 65536), 65536);
        assert_eq!(align_to(65536, 65536), 65536);
        assert_eq!(align_to(65537, 65536), 131_072);
    }

    // ── test_alloc_and_map ──

    #[test]
    fn alloc_and_map_single_page() {
        let backend = MockBackend::new();
        test_alloc_and_map(&backend, "system", &[4096]).unwrap();
    }

    #[test]
    fn alloc_and_map_multiple_sizes() {
        let backend = MockBackend::new();
        test_alloc_and_map(&backend, "system", &[4096, 65536, 1_048_576]).unwrap();
    }

    // ── test_alloc_zeroed ──

    #[test]
    fn alloc_zeroed_single() {
        let backend = MockBackend::new();
        test_alloc_zeroed(&backend, "system", &[4096]).unwrap();
    }

    #[test]
    fn alloc_zeroed_large() {
        let backend = MockBackend::new();
        test_alloc_zeroed(&backend, "system", &[1_048_576]).unwrap();
    }

    // ── test_repeated_alloc ──

    #[test]
    fn repeated_alloc_no_leak() {
        let backend = MockBackend::new();
        let initial = backend.buffer_count();
        test_repeated_alloc(&backend, "system", &[4096], 100).unwrap();
        assert_eq!(backend.buffer_count(), initial, "buffer leak detected");
    }

    #[test]
    fn repeated_alloc_default_count() {
        let backend = MockBackend::new();
        test_repeated_alloc(&backend, "system", &[4096], 1024).unwrap();
    }

    // ── test_llseek_size ──

    #[test]
    fn llseek_aligned() {
        let backend = MockBackend::new();
        test_llseek_size(&backend, "system", &[4096], 4096).unwrap();
    }

    #[test]
    fn llseek_unaligned() {
        let backend = MockBackend::new();
        // 1 byte → 4096, 4097 → 8192
        test_llseek_size(&backend, "system", &[1, 4097], 4096).unwrap();
    }

    // ── test_export_sync_file ──

    #[test]
    fn export_returns_valid_fd() {
        let backend = MockBackend::new();
        test_export_sync_file(&backend, "system").unwrap();
    }

    #[test]
    fn import_after_export() {
        let backend = MockBackend::new();
        test_import_sync_file(&backend, "system").unwrap();
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn export_multiple_sizes() {
        let backend = MockBackend::new();
        let heap = DmaHeap::open(&backend, "system").unwrap();

        for size in [4096_u64, 65536, 1_048_576] {
            let fd = heap
                .alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
                .unwrap();
            let buf = DmaBuf::new(&backend, fd, size as usize);
            let sync_fd = buf.export_sync_file(DMA_BUF_SYNC_RW as u32).unwrap();
            assert!(sync_fd >= 0);
        }
    }

    // ── test_concurrent_alloc ──

    #[test]
    fn concurrent_basic() {
        let backend = MockBackend::new();
        test_concurrent_alloc(&backend, "system", 10).unwrap();
    }

    #[test]
    fn concurrent_no_leak() {
        let backend = MockBackend::new();
        let initial = backend.buffer_count();
        test_concurrent_alloc(&backend, "system", 20).unwrap();
        assert_eq!(backend.buffer_count(), initial, "buffer leak detected");
    }

    // ── test_dup_fd ──

    #[test]
    fn dup_survives_original_close() {
        let backend = MockBackend::new();
        test_dup_fd(&backend, "system").unwrap();
    }

    // ── run() integration ──

    #[test]
    fn run_passes() {
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "system", &[4096, 65536], 10, 10, 6);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert!(results.iter().all(|t| !t.skipped));
        assert_eq!(results.len(), 8);
    }

    #[test]
    fn run_bad_heap() {
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "", &[4096], 10, 10, 6);
        assert!(err.is_some());
        // Heap not available → empty results (no fake PASS).
        assert!(results.is_empty());
    }
}
