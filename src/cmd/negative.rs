// Negative tests: error paths, invalid input, resource leaks, concurrency.
//
// Systematically verifies that the kernel returns correct errno values
// and does not crash or panic on invalid operations.

use std::sync::atomic::{AtomicUsize, Ordering};

use nix::errno::Errno;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::DMA_BUF_SYNC_READ;
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};
use crate::testing::{expect_errno, expect_errno_one_of};

/// Allocation size for negative tests.
const NEG_ALLOC_SIZE: u64 = 4096;

/// Run all negative tests. Executes all tests even if some fail;
/// returns sub-test results (and the first error, if any).
pub fn run<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
    heap_w: usize,
) -> (Vec<SubTestResult>, Option<anyhow::Error>) {
    tracing::debug!(heap = heap_name, "negative sequence");

    let caps = crate::probe::probe_heap(backend, heap_name);

    let tests: Vec<(&str, nix::Result<()>, bool)> = vec![
        // Layer 1: Heap device access
        ("neg_open_nonexistent", neg_open_nonexistent(backend), false),
        // Layer 2: Alloc ioctl
        (
            "neg_alloc_zero_size",
            neg_alloc_zero_size(backend, heap_name),
            false,
        ),
        (
            "neg_alloc_overflow_size",
            neg_alloc_overflow_size(backend, heap_name),
            false,
        ),
        (
            "neg_alloc_invalid_fd_flags",
            neg_alloc_invalid_fd_flags(backend, heap_name),
            false,
        ),
        (
            "neg_alloc_invalid_heap_flags",
            neg_alloc_invalid_heap_flags(backend, heap_name),
            false,
        ),
        (
            "neg_alloc_on_closed_heap",
            neg_alloc_on_closed_heap(backend, heap_name),
            false,
        ),
        // Layer 3: dma-buf fd ops
        (
            "neg_sync_invalid_flags",
            neg_sync_invalid_flags(backend, heap_name),
            false,
        ),
        (
            "neg_sync_on_closed_fd",
            neg_sync_on_closed_fd(backend, heap_name),
            false,
        ),
        (
            "neg_llseek_invalid",
            neg_llseek_invalid(backend, heap_name),
            false,
        ),
        // Layer 4: mmap (skip if heap does not support mmap)
        (
            "neg_mmap_beyond_size",
            if caps.can_mmap {
                neg_mmap_beyond_size(backend, heap_name)
            } else {
                Ok(())
            },
            !caps.can_mmap,
        ),
        // Layer 5: sync_file
        (
            "neg_export_sync_file_invalid_flags",
            neg_export_sync_file_invalid_flags(backend, heap_name),
            false,
        ),
        (
            "neg_import_sync_file_bad_fd",
            neg_import_sync_file_bad_fd(backend, heap_name),
            false,
        ),
        // Layer 6: Resource leaks
        (
            "neg_rapid_alloc_close_no_leak",
            neg_rapid_alloc_close_no_leak(backend, heap_name),
            false,
        ),
        // Layer 7: Concurrency
        (
            "neg_concurrent_close_same_fd",
            neg_concurrent_close_same_fd(backend, heap_name),
            false,
        ),
    ];

    runner::collect_test_results("negative", heap_name, heap_w, &tests)
}

// ── Layer 1: Heap device access ──

/// Open a nonexistent heap → expect `ENOENT`.
fn neg_open_nonexistent<B: HeapBackend + DmaBufBackend>(backend: &B) -> nix::Result<()> {
    let result = DmaHeap::open(backend, "");
    expect_errno(result, Errno::ENOENT, "open empty name")
}

// ── Layer 2: Alloc ioctl ──

/// Alloc with `len`=0 → expect `EINVAL`.
fn neg_alloc_zero_size<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let result = heap.alloc(0, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS);
    expect_errno(result, Errno::EINVAL, "alloc zero size")
}

/// Alloc with `u64::MAX` → expect `EINVAL` or `ENOMEM`.
fn neg_alloc_overflow_size<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let result = heap.alloc(u64::MAX, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS);
    expect_errno_one_of(result, &[Errno::EINVAL, Errno::ENOMEM], "alloc overflow")
}

/// Alloc with invalid `fd_flags` (`O_APPEND`) → expect `EINVAL`.
fn neg_alloc_invalid_fd_flags<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    #[allow(clippy::cast_sign_loss)]
    let result = heap.alloc(
        NEG_ALLOC_SIZE,
        libc::O_APPEND as u32,
        DMA_HEAP_VALID_HEAP_FLAGS,
    );
    expect_errno(result, Errno::EINVAL, "alloc invalid fd_flags")
}

/// Alloc with `heap_flags` != 0 → expect `EINVAL`.
fn neg_alloc_invalid_heap_flags<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let result = heap.alloc(NEG_ALLOC_SIZE, DMA_HEAP_ALLOC_FD_FLAGS, 1);
    expect_errno(result, Errno::EINVAL, "alloc invalid heap_flags")
}

/// Alloc on a closed heap fd → expect `EBADF`.
fn neg_alloc_on_closed_heap<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let heap_fd = heap.fd();
    drop(heap); // closes heap fd
    // Direct backend call since DmaHeap is dropped.
    let mut data = crate::ioctl::dma_heap::DmaHeapAllocationData {
        len: NEG_ALLOC_SIZE,
        fd: 0,
        fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
        heap_flags: DMA_HEAP_VALID_HEAP_FLAGS,
    };
    let result = backend.alloc(heap_fd, &mut data);
    expect_errno(result, Errno::EBADF, "alloc on closed heap")
}

// ── Layer 3: dma-buf fd ops ──

/// Sync with invalid flags → expect `EINVAL`.
#[allow(clippy::cast_possible_truncation)]
fn neg_sync_invalid_flags<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(
        NEG_ALLOC_SIZE,
        DMA_HEAP_ALLOC_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;
    let buf = DmaBuf::new(backend, fd, NEG_ALLOC_SIZE as usize);

    // Zero flags
    let result = buf.sync_start(0);
    expect_errno(result, Errno::EINVAL, "sync zero flags")?;

    // Invalid bits (bit 3)
    let result = buf.sync_start(0x8 | DMA_BUF_SYNC_READ);
    expect_errno(result, Errno::EINVAL, "sync invalid bits")?;

    Ok(())
}

/// Sync on a closed dma-buf fd → expect `EBADF`.
#[allow(clippy::cast_possible_truncation)]
fn neg_sync_on_closed_fd<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(
        NEG_ALLOC_SIZE,
        DMA_HEAP_ALLOC_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;
    let buf = DmaBuf::new(backend, fd, NEG_ALLOC_SIZE as usize);
    let raw_fd = buf.fd();
    drop(buf);

    // Direct backend sync on closed fd.
    let result = backend.sync(raw_fd, DMA_BUF_SYNC_READ);
    expect_errno(result, Errno::EBADF, "sync on closed fd")
}

/// `llseek` with invalid whence/offset → expect `EINVAL`.
#[allow(clippy::cast_possible_truncation)]
fn neg_llseek_invalid<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(
        NEG_ALLOC_SIZE,
        DMA_HEAP_ALLOC_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;
    let buf = DmaBuf::new(backend, fd, NEG_ALLOC_SIZE as usize);

    // SEEK_CUR not supported
    let result = backend.llseek(buf.fd(), 0, libc::SEEK_CUR);
    expect_errno(result, Errno::EINVAL, "llseek SEEK_CUR")?;

    // Non-zero offset with SEEK_SET
    let result = backend.llseek(buf.fd(), 1, libc::SEEK_SET);
    expect_errno(result, Errno::EINVAL, "llseek nonzero SEEK_SET")?;

    // Non-zero offset with SEEK_END
    let result = backend.llseek(buf.fd(), 1, libc::SEEK_END);
    expect_errno(result, Errno::EINVAL, "llseek nonzero SEEK_END")?;

    Ok(())
}

// ── Layer 4: mmap ──

/// `mmap` with length exceeding buffer size → expect `EINVAL`.
#[allow(clippy::cast_possible_truncation)]
fn neg_mmap_beyond_size<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(
        NEG_ALLOC_SIZE,
        DMA_HEAP_ALLOC_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;
    let mut buf = DmaBuf::new(backend, fd, (NEG_ALLOC_SIZE * 2) as usize);

    // DmaBuf.mmap uses self.len which we set to 2x actual size
    let result = buf.mmap();
    expect_errno(result, Errno::EINVAL, "mmap beyond size")
}

// ── Layer 5: sync_file ──

/// `export_sync_file` with flags=0 → expect `EINVAL`.
#[allow(clippy::cast_possible_truncation)]
fn neg_export_sync_file_invalid_flags<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(
        NEG_ALLOC_SIZE,
        DMA_HEAP_ALLOC_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;
    let buf = DmaBuf::new(backend, fd, NEG_ALLOC_SIZE as usize);

    let result = buf.export_sync_file(0);
    expect_errno(result, Errno::EINVAL, "export_sync_file flags=0")
}

/// `import_sync_file` with invalid `sync_file` fd → expect `EINVAL`.
#[allow(clippy::cast_possible_truncation)]
fn neg_import_sync_file_bad_fd<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(
        NEG_ALLOC_SIZE,
        DMA_HEAP_ALLOC_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;
    let buf = DmaBuf::new(backend, fd, NEG_ALLOC_SIZE as usize);

    #[allow(clippy::cast_possible_truncation)]
    let result = buf.import_sync_file(DMA_BUF_SYNC_READ as u32, 9999);
    expect_errno(result, Errno::EINVAL, "import bad sync_file fd")
}

// ── Layer 6: Resource leaks ──

/// Rapid alloc→close loop, verify no fd/buffer leak.
#[allow(clippy::cast_possible_truncation)]
fn neg_rapid_alloc_close_no_leak<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    for _ in 0..1000 {
        let fd = heap.alloc(
            NEG_ALLOC_SIZE,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        )?;
        let buf = DmaBuf::new(backend, fd, NEG_ALLOC_SIZE as usize);
        drop(buf); // close
    }

    // If we get here without ENOMEM or fd exhaustion, no leak.
    tracing::debug!("rapid alloc/close: 1000 iterations ok");
    Ok(())
}

// ── Layer 7: Concurrency ──

/// Two threads close the same fd — one succeeds, one gets `EBADF`.
#[allow(clippy::cast_possible_truncation)]
fn neg_concurrent_close_same_fd<B: HeapBackend + DmaBufBackend + Send + Sync>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let fd = heap.alloc(
        NEG_ALLOC_SIZE,
        DMA_HEAP_ALLOC_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;

    let ok_count = AtomicUsize::new(0);
    let ebadf_count = AtomicUsize::new(0);
    let ok_ref = &ok_count;
    let ebadf_ref = &ebadf_count;

    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(move || {
                match backend.close(fd) {
                    Ok(()) => ok_ref.fetch_add(1, Ordering::Relaxed),
                    Err(Errno::EBADF) => ebadf_ref.fetch_add(1, Ordering::Relaxed),
                    Err(e) => {
                        tracing::error!(error = %e, "unexpected errno on double close");
                        0 // unexpected
                    }
                };
            });
        }
    });

    let oks = ok_count.load(Ordering::Relaxed);
    let ebadfs = ebadf_count.load(Ordering::Relaxed);
    tracing::debug!(oks, ebadfs, "concurrent close results");

    // One should succeed, one should get EBADF.
    if oks + ebadfs != 2 || oks == 0 {
        tracing::error!(oks, ebadfs, "unexpected concurrent close outcome");
        return Err(Errno::EIO);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    // ── Layer 1 ──

    #[test]
    fn open_nonexistent() {
        let b = MockBackend::new();
        neg_open_nonexistent(&b).unwrap();
    }

    // ── Layer 2 ──

    #[test]
    fn alloc_zero_size() {
        let b = MockBackend::new();
        neg_alloc_zero_size(&b, "system").unwrap();
    }

    #[test]
    fn alloc_overflow_size() {
        let b = MockBackend::new();
        neg_alloc_overflow_size(&b, "system").unwrap();
    }

    #[test]
    fn alloc_invalid_fd_flags() {
        let b = MockBackend::new();
        neg_alloc_invalid_fd_flags(&b, "system").unwrap();
    }

    #[test]
    fn alloc_invalid_heap_flags() {
        let b = MockBackend::new();
        neg_alloc_invalid_heap_flags(&b, "system").unwrap();
    }

    #[test]
    fn alloc_on_closed_heap() {
        let b = MockBackend::new();
        neg_alloc_on_closed_heap(&b, "system").unwrap();
    }

    // ── Layer 3 ──

    #[test]
    fn sync_invalid_flags() {
        let b = MockBackend::new();
        neg_sync_invalid_flags(&b, "system").unwrap();
    }

    #[test]
    fn sync_on_closed_fd() {
        let b = MockBackend::new();
        neg_sync_on_closed_fd(&b, "system").unwrap();
    }

    #[test]
    fn llseek_invalid() {
        let b = MockBackend::new();
        neg_llseek_invalid(&b, "system").unwrap();
    }

    // ── Layer 4 ──

    #[test]
    fn mmap_beyond_size() {
        let b = MockBackend::new();
        neg_mmap_beyond_size(&b, "system").unwrap();
    }

    // ── Layer 5 ──

    #[test]
    fn export_sync_file_invalid_flags() {
        let b = MockBackend::new();
        neg_export_sync_file_invalid_flags(&b, "system").unwrap();
    }

    #[test]
    fn import_sync_file_bad_fd() {
        let b = MockBackend::new();
        neg_import_sync_file_bad_fd(&b, "system").unwrap();
    }

    // ── Layer 6 ──

    #[test]
    fn rapid_alloc_close_no_leak() {
        let b = MockBackend::new();
        neg_rapid_alloc_close_no_leak(&b, "system").unwrap();
    }

    // ── Layer 7 ──

    #[test]
    fn concurrent_close_same_fd() {
        let b = MockBackend::new();
        neg_concurrent_close_same_fd(&b, "system").unwrap();
    }

    // ── Integration ──

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        let (results, err) = run(&b, "system", 6);
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
        assert_eq!(results.len(), 14);
    }
}
