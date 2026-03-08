// Stage 1 basic tests: alloc, mmap, sync, llseek, zeroed, repeated.

use std::error::Error;

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};

/// Number of buffers used in the zeroed-page test.
const ZEROED_TEST_COUNT: usize = 16;

/// Page size for alignment calculations.
const PAGE_SIZE: u64 = 4096;

/// Round `size` up to the nearest page boundary.
fn page_align(size: u64) -> u64 {
    size.next_multiple_of(PAGE_SIZE)
}

/// Run all stage 1 basic tests. Executes all tests even if some fail;
/// returns sub-test results (and the first error, if any).
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
    repeat: u32,
) -> (Vec<SubTestResult>, Option<Box<dyn Error>>) {
    let tests: [(&str, nix::Result<()>); 4] = [
        (
            "alloc_and_map",
            test_alloc_and_map(backend, heap_name, sizes),
        ),
        ("alloc_zeroed", test_alloc_zeroed(backend, heap_name, sizes)),
        (
            "repeated_alloc",
            test_repeated_alloc(backend, heap_name, sizes, repeat),
        ),
        ("llseek_size", test_llseek_size(backend, heap_name, sizes)),
    ];

    runner::collect_test_results("basic", &tests)
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
        let buf_fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
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
                let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
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
                let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
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
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
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

/// Verify that `llseek(SEEK_END)` returns the page-aligned buffer size.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn test_llseek_size<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    sizes: &[u64],
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for &size in sizes {
        tracing::debug!(size, "llseek_size");
        let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let buf = DmaBuf::new(backend, fd, size as usize);

        let reported = buf.llseek_size()?;
        let expected = page_align(size) as i64;

        if reported != expected {
            tracing::error!(size, reported, expected, "llseek size mismatch");
            return Err(Errno::EIO);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    // ── page_align helper ──

    #[test]
    fn page_align_zero() {
        assert_eq!(page_align(0), 0);
    }

    #[test]
    fn page_align_exact() {
        assert_eq!(page_align(4096), 4096);
        assert_eq!(page_align(8192), 8192);
    }

    #[test]
    fn page_align_rounds_up() {
        assert_eq!(page_align(1), 4096);
        assert_eq!(page_align(4097), 8192);
        assert_eq!(page_align(4095), 4096);
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
        test_llseek_size(&backend, "system", &[4096]).unwrap();
    }

    #[test]
    fn llseek_unaligned() {
        let backend = MockBackend::new();
        // 1 byte → 4096, 4097 → 8192
        test_llseek_size(&backend, "system", &[1, 4097]).unwrap();
    }

    // ── run() integration ──

    #[test]
    fn run_passes() {
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "system", &[4096, 65536], 10);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn run_bad_heap() {
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "", &[4096], 10);
        assert!(err.is_some());
        assert!(results.iter().any(|t| !t.passed));
    }
}
