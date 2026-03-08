// Stage 2 edge tests: concurrent alloc, dup fd, set_name.

use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};

/// Allocation size used for edge tests.
const EDGE_ALLOC_SIZE: u64 = 4096;

/// Run all stage 2 edge tests. Executes all tests even if some fail;
/// returns sub-test results (and the first error, if any).
pub fn run<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    threads: u32,
) -> (Vec<SubTestResult>, Option<Box<dyn Error>>) {
    let tests: [(&str, nix::Result<()>); 3] = [
        (
            "concurrent_alloc",
            test_concurrent_alloc(backend, heap_name, threads),
        ),
        ("dup_fd", test_dup_fd(backend, heap_name)),
        ("set_name", test_set_name(backend, heap_name)),
    ];

    runner::collect_test_results("edge", &tests)
}

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
                        DMA_HEAP_VALID_FD_FLAGS,
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
        DMA_HEAP_VALID_FD_FLAGS,
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

/// Set a debug name on a dma-buf and verify it succeeds.
fn test_set_name<B: HeapBackend + DmaBufBackend>(backend: &B, heap_name: &str) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(
        EDGE_ALLOC_SIZE,
        DMA_HEAP_VALID_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;
    #[allow(clippy::cast_possible_truncation)]
    let buf = DmaBuf::new(backend, fd, EDGE_ALLOC_SIZE as usize);

    // Short name
    buf.set_name("test_buffer")?;
    tracing::debug!("set short name ok");

    // Max length name (DMA_BUF_NAME_LEN = 32)
    let max_name = "a".repeat(32);
    buf.set_name(&max_name)?;
    tracing::debug!("set max length name ok");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

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

    #[test]
    fn dup_survives_original_close() {
        let backend = MockBackend::new();
        test_dup_fd(&backend, "system").unwrap();
    }

    #[test]
    fn set_name_valid() {
        let backend = MockBackend::new();
        test_set_name(&backend, "system").unwrap();
    }

    #[test]
    fn run_passes() {
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "system", 10);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn run_bad_heap() {
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "", 10);
        assert!(err.is_some());
        assert!(results.iter().any(|t| !t.passed));
    }
}
