// Basic deterministic tests: alloc, mmap, sync, llseek, zeroed, repeated,
// sync_file export/import, concurrent alloc, dup.

use std::sync::Mutex;

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

use crate::probe::align_to;

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
    let mmap_ok = caps.can_mmap;
    let granularity = caps.alloc_granularity;

    let tests: [(&str, nix::Result<()>, bool); 9] = [
        // Always runs (no mmap needed)
        (
            "alloc_close",
            test_alloc_close(backend, heap_name, sizes),
            false,
        ),
        (
            "write_read_verify",
            if mmap_ok {
                test_write_read_verify(backend, heap_name, sizes)
            } else {
                Ok(())
            },
            !mmap_ok,
        ),
        (
            "zero_on_alloc",
            if mmap_ok {
                test_zero_on_alloc(backend, heap_name, sizes)
            } else {
                Ok(())
            },
            !mmap_ok,
        ),
        // alloc/free stability — no mmap needed
        (
            "alloc_free_loop",
            test_alloc_free_loop(backend, heap_name, sizes, repeat),
            false,
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
            "concurrent_write_verify",
            if mmap_ok {
                test_concurrent_write_verify(backend, heap_name, threads)
            } else {
                Ok(())
            },
            !mmap_ok,
        ),
        (
            "dup_survives_close",
            test_dup_survives_close(backend, heap_name),
            false,
        ),
    ];

    runner::collect_test_results("basic", heap_name, heap_w, &tests)
}

// ── alloc_close ─────────────────────────────────────────────────────────────

/// Alloc → close for each size (no mmap). Verifies basic alloc path on all heaps.
#[allow(clippy::cast_possible_truncation)]
fn test_alloc_close<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        tracing::debug!(size, "alloc_close");
        let buf_fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let buf = DmaBuf::new(backend, buf_fd, size as usize);
        drop(buf);
    }

    Ok(())
}

// ── write_read_verify ───────────────────────────────────────────────────────

/// Alloc → mmap → pattern write → read verify for each size.
#[allow(clippy::cast_possible_truncation)]
fn test_write_read_verify<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        tracing::debug!(size, "write_read_verify");
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
    }

    Ok(())
}

// ── zero_on_alloc ───────────────────────────────────────────────────────────

/// Alloc 16 buffers, contaminate with 0xAA, close, reallocate, verify zeroed.
#[allow(clippy::cast_possible_truncation)]
fn test_zero_on_alloc<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        tracing::debug!(size, "zero_on_alloc");

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

// ── alloc_free_loop ─────────────────────────────────────────────────────────

/// Repeated alloc/close loop to verify stability and absence of leaks.
#[allow(clippy::cast_possible_truncation)]
fn test_alloc_free_loop<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    repeat: u32,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        tracing::debug!(size, repeat, "alloc_free_loop");

        for _ in 0..repeat {
            let fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let buf = DmaBuf::new(backend, fd, size as usize);
            drop(buf);
        }
    }

    Ok(())
}

// ── llseek_size ─────────────────────────────────────────────────────────────

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
        backend.close(sync_fd)?;
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

    backend.close(sync_fd)?;

    Ok(())
}

// ── Edge / boundary tests ───────────────────────────────────────────────────

/// Concurrent alloc → mmap → sync → write → verify → close from N threads.
#[allow(clippy::cast_possible_truncation)]
fn test_concurrent_write_verify<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    threads: u32,
) -> nix::Result<()> {
    let first_err: Mutex<Option<(u32, Errno)>> = Mutex::new(None);
    let err_ref = &first_err;

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

                if let Err(e) = result {
                    let mut guard = err_ref.lock().unwrap();
                    if guard.is_none() {
                        *guard = Some((tid, e));
                    }
                }
            });
        }
    });

    if let Some((tid, errno)) = first_err.into_inner().unwrap() {
        tracing::error!(%errno, tid, threads, "concurrent alloc failure");
        return Err(errno);
    }

    tracing::debug!(threads, "all concurrent threads passed");
    Ok(())
}

/// Dup a dma-buf fd, close the original, verify the dup still works via llseek.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn test_dup_survives_close<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(
        EDGE_ALLOC_SIZE,
        DMA_HEAP_ALLOC_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;
    let buf = DmaBuf::new(backend, fd, EDGE_ALLOC_SIZE as usize);

    // Query actual kernel-allocated size before dup
    let orig_size = buf.llseek_size()?;

    // Dup then drop original
    let dup_buf = buf.dup()?;
    drop(buf);

    // Verify dup is still usable via llseek and matches the original size
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

    // ── align_to helper ──

    #[test]
    fn align_to_zero_size() {
        assert_eq!(align_to(0, 4096), 0);
    }

    #[test]
    fn align_to_zero_granularity() {
        // Zero granularity must not panic; returns size unchanged.
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

    #[test]
    fn align_to_large_granularity() {
        assert_eq!(align_to(1, 65536), 65536);
        assert_eq!(align_to(65536, 65536), 65536);
        assert_eq!(align_to(65537, 65536), 131_072);
    }

    // ── test_alloc_close ──

    #[test]
    fn alloc_close_basic() {
        let backend = MockBackend::new();
        test_alloc_close(&backend, "system", &[4096, 65536]).unwrap();
    }

    // ── test_write_read_verify ──

    #[test]
    fn write_read_verify_single_page() {
        let backend = MockBackend::new();
        test_write_read_verify(&backend, "system", &[4096]).unwrap();
    }

    #[test]
    fn write_read_verify_multiple_sizes() {
        let backend = MockBackend::new();
        test_write_read_verify(&backend, "system", &[4096, 65536, 1_048_576]).unwrap();
    }

    // ── test_zero_on_alloc ──

    #[test]
    fn zero_on_alloc_single() {
        let backend = MockBackend::new();
        test_zero_on_alloc(&backend, "system", &[4096]).unwrap();
    }

    #[test]
    fn zero_on_alloc_large() {
        let backend = MockBackend::new();
        test_zero_on_alloc(&backend, "system", &[1_048_576]).unwrap();
    }

    // ── test_alloc_free_loop ──

    #[test]
    fn alloc_free_loop_no_leak() {
        let backend = MockBackend::new();
        let initial = backend.buffer_count();
        test_alloc_free_loop(&backend, "system", &[4096], 100).unwrap();
        assert_eq!(backend.buffer_count(), initial, "buffer leak detected");
    }

    #[test]
    fn alloc_free_loop_default_count() {
        let backend = MockBackend::new();
        test_alloc_free_loop(&backend, "system", &[4096], 1024).unwrap();
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

    // ── test_concurrent_write_verify ──

    #[test]
    fn concurrent_basic() {
        let backend = MockBackend::new();
        test_concurrent_write_verify(&backend, "system", 10).unwrap();
    }

    #[test]
    fn concurrent_no_leak() {
        let backend = MockBackend::new();
        let initial = backend.buffer_count();
        test_concurrent_write_verify(&backend, "system", 20).unwrap();
        assert_eq!(backend.buffer_count(), initial, "buffer leak detected");
    }

    // ── test_dup_survives_close ──

    #[test]
    fn dup_survives_original_close() {
        let backend = MockBackend::new();
        test_dup_survives_close(&backend, "system").unwrap();
    }

    // ── run() integration ──

    #[test]
    fn run_passes() {
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "system", &[4096, 65536], 10, 10, 6);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert!(results.iter().all(|t| !t.skipped));
        assert_eq!(results.len(), 9);
    }

    #[test]
    fn run_bad_heap() {
        // Bad heap validation is handled at the framework level (Layer 1).
        // When called directly, individual tests fail with proper errors.
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "", &[4096], 10, 10, 6);
        assert!(err.is_some());
        assert!(results.iter().any(|r| !r.passed));
    }
}
