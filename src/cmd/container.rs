// Samsung dma-buf container tests: merge, dma-buf ops, cross-heap, negative paths.

use nix::errno::Errno;

use crate::backend::{ContainerBackend, DmaBufBackend, HeapBackend};
use crate::container::DmaBufContainer;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ, DMA_BUF_SYNC_START};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::ioctl::dmabuf_container::MAX_BUFCON_SRC_BUFS;
use crate::runner::{self, SubTestResult};

/// Allocation size for container tests.
const ALLOC_SIZE: u64 = 4096;

/// Run all container tests. Uses all heaps for cross-heap merge tests.
pub fn run<B: HeapBackend + DmaBufBackend + ContainerBackend + Send + Sync>(
    backend: &B,
    heaps: &[String],
    heap_w: usize,
) -> (Vec<SubTestResult>, Option<anyhow::Error>) {
    tracing::debug!(?heaps, "container test sequence");

    // Use "system" if available, else first heap. Restricted heaps can't merge.
    let primary_heap = heaps
        .iter()
        .find(|h| h.as_str() == "system")
        .or_else(|| heaps.first())
        .map_or("system", String::as_str);

    let mut tests: Vec<(&str, nix::Result<()>, bool)> = vec![
        // Positive: merge & lifecycle
        (
            "merge_two_bufs",
            merge_two_bufs(backend, primary_heap),
            false,
        ),
        (
            "merge_single_buf",
            merge_single_buf(backend, primary_heap),
            false,
        ),
        (
            "merge_close_lifecycle",
            merge_close_lifecycle(backend, primary_heap),
            false,
        ),
        (
            "merge_flatten_container",
            merge_flatten_container(backend, primary_heap),
            false,
        ),
        // Positive: merged fd as normal dma-buf
        (
            "merge_mmap_read_write",
            merge_mmap_read_write(backend, primary_heap),
            false,
        ),
        (
            "merge_llseek_combined_size",
            merge_llseek_combined_size(backend, primary_heap),
            false,
        ),
        ("merge_sync", merge_sync(backend, primary_heap), false),
        // Positive: mask (deprecated, basic roundtrip only)
        (
            "set_get_mask_roundtrip",
            set_get_mask_roundtrip(backend, primary_heap),
            false,
        ),
        // Negative: merge
        ("neg_merge_zero_count", neg_merge_zero_count(backend), false),
        (
            "neg_merge_over_max",
            neg_merge_over_max(backend, primary_heap),
            false,
        ),
        ("neg_merge_invalid_fd", neg_merge_invalid_fd(backend), false),
        (
            "neg_merge_closed_buf",
            neg_merge_closed_buf(backend, primary_heap),
            false,
        ),
        // Negative: ops after close
        (
            "neg_ops_after_close",
            neg_ops_after_close(backend, primary_heap),
            false,
        ),
    ];

    // Cross-heap merge (only if 2+ mergeable heaps available).
    // Heaps without CPU access (e.g. restricted/secure) cannot be merged.
    let mergeable: Vec<&str> = heaps
        .iter()
        .filter(|h| crate::probe::probe_heap(backend, h).can_mmap)
        .map(String::as_str)
        .collect();
    if mergeable.len() >= 2 {
        tests.push((
            "cross_heap_merge",
            cross_heap_merge(backend, mergeable[0], mergeable[1]),
            false,
        ));
    }

    runner::collect_test_results("container", "container", heap_w, &tests)
}

// ── Helpers ──

fn alloc_buf<'a, B: HeapBackend + DmaBufBackend>(
    backend: &'a B,
    heap_name: &str,
    size: u64,
) -> nix::Result<(DmaHeap<'a, B>, i32)> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let buf_fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    Ok((heap, buf_fd))
}

#[allow(clippy::needless_pass_by_value)]
fn expect_errno<T>(result: nix::Result<T>, expected: Errno, context: &str) -> nix::Result<()> {
    match result {
        Err(e) if e == expected => Ok(()),
        Err(e) => {
            tracing::error!(context, expected = %expected, got = %e, "wrong errno");
            Err(Errno::EIO)
        }
        Ok(_) => {
            tracing::error!(context, expected = %expected, "expected error, got Ok");
            Err(Errno::EIO)
        }
    }
}

// ── Positive: merge & lifecycle ──

fn merge_two_bufs<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;
    let (_h2, fd2) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;

    let cfd = container.merge(&[fd1, fd2])?;
    assert!(cfd >= 0);
    container.close(cfd)?;
    Ok(())
}

fn merge_single_buf<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;

    let cfd = container.merge(&[fd1])?;
    container.close(cfd)?;
    Ok(())
}

fn merge_close_lifecycle<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;

    let cfd = container.merge(&[fd1])?;
    container.close(cfd)?;

    // Source buffer should still be usable after container close.
    let size = backend.llseek(fd1, 0, libc::SEEK_END)?;
    #[allow(clippy::cast_possible_wrap)]
    if size != ALLOC_SIZE as i64 {
        tracing::error!(
            expected = ALLOC_SIZE,
            got = size,
            "source buf size mismatch"
        );
        return Err(Errno::EIO);
    }
    Ok(())
}

fn merge_flatten_container<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h1, fd1) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;
    let (_h2, fd2) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;
    let (_h3, fd3) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;

    // Create inner container [fd1, fd2].
    let inner = container.merge(&[fd1, fd2])?;
    // Merge inner + fd3 → should flatten to 3 buffers.
    let outer = container.merge(&[inner, fd3])?;

    // Verify combined size = 3 * ALLOC_SIZE via llseek.
    let size = backend.llseek(outer, 0, libc::SEEK_END)?;
    #[allow(clippy::cast_possible_wrap)]
    let expected = (ALLOC_SIZE * 3) as i64;
    if size != expected {
        tracing::error!(expected, got = size, "flatten size mismatch");
        return Err(Errno::EIO);
    }

    container.close(outer)?;
    container.close(inner)?;
    Ok(())
}

// ── Positive: merged fd as normal dma-buf ──

fn merge_mmap_read_write<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h1, fd1) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;
    let (_h2, fd2) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;

    // Write distinct patterns to source buffers.
    #[allow(clippy::cast_possible_truncation)]
    let alloc_usize = ALLOC_SIZE as usize;
    let ptr1 = backend.mmap(fd1, alloc_usize)?;
    unsafe { std::ptr::write_bytes(ptr1, 0xAA, alloc_usize) };
    let ptr2 = backend.mmap(fd2, alloc_usize)?;
    unsafe { std::ptr::write_bytes(ptr2, 0xBB, alloc_usize) };

    let cfd = container.merge(&[fd1, fd2])?;
    let merged_ptr = backend.mmap(cfd, alloc_usize * 2)?;

    // Verify concatenated data in sg_table order.
    unsafe {
        if *merged_ptr != 0xAA || *merged_ptr.add(alloc_usize) != 0xBB {
            tracing::error!("merged data mismatch");
            return Err(Errno::EIO);
        }
    }

    container.close(cfd)?;
    Ok(())
}

fn merge_llseek_combined_size<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h1, fd1) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;
    let (_h2, fd2) = alloc_buf(backend, heap_name, 8192)?;

    let cfd = container.merge(&[fd1, fd2])?;
    let size = backend.llseek(cfd, 0, libc::SEEK_END)?;
    if size != 4096 + 8192 {
        tracing::error!(expected = 4096 + 8192, got = size, "combined size mismatch");
        return Err(Errno::EIO);
    }

    container.close(cfd)?;
    Ok(())
}

fn merge_sync<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;
    let cfd = container.merge(&[fd1])?;

    backend.sync(cfd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ)?;
    backend.sync(cfd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ)?;

    container.close(cfd)?;
    Ok(())
}

// ── Positive: mask (deprecated, basic roundtrip) ──

fn set_get_mask_roundtrip<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h1, fd1) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;
    let (_h2, fd2) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;
    let (_h3, fd3) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;

    let cfd = container.merge(&[fd1, fd2, fd3])?;

    container.set_mask(cfd, 0b101)?;
    let got = container.get_mask(cfd)?;
    if got != 0b101 {
        tracing::error!(expected = 0b101, got, "mask roundtrip mismatch");
        return Err(Errno::EIO);
    }

    container.close(cfd)?;
    Ok(())
}

// ── Positive: cross-heap ──

fn cross_heap_merge<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_a: &str,
    heap_b: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_ha, fda) = alloc_buf(backend, heap_a, ALLOC_SIZE)?;
    let (_hb, fdb) = alloc_buf(backend, heap_b, ALLOC_SIZE)?;

    let cfd = container.merge(&[fda, fdb])?;
    let size = backend.llseek(cfd, 0, libc::SEEK_END)?;
    #[allow(clippy::cast_possible_wrap)]
    let expected = (ALLOC_SIZE * 2) as i64;
    if size != expected {
        tracing::error!(expected, got = size, "cross-heap size mismatch");
        return Err(Errno::EIO);
    }
    container.close(cfd)?;
    Ok(())
}

// ── Negative: merge ──

fn neg_merge_zero_count<B: ContainerBackend>(backend: &B) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    expect_errno(container.merge(&[]), Errno::EINVAL, "merge zero count")
}

fn neg_merge_over_max<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let heap = DmaHeap::open(backend, heap_name)?;

    let mut fds = Vec::with_capacity(MAX_BUFCON_SRC_BUFS + 1);
    for _ in 0..=MAX_BUFCON_SRC_BUFS {
        let fd = heap.alloc(
            ALLOC_SIZE,
            DMA_HEAP_ALLOC_FD_FLAGS,
            DMA_HEAP_VALID_HEAP_FLAGS,
        )?;
        fds.push(fd);
    }
    expect_errno(container.merge(&fds), Errno::EINVAL, "merge over max")
}

fn neg_merge_invalid_fd<B: ContainerBackend>(backend: &B) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    expect_errno(container.merge(&[9999]), Errno::EBADF, "merge invalid fd")
}

fn neg_merge_closed_buf<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;
    backend.close(fd1)?;
    expect_errno(container.merge(&[fd1]), Errno::EBADF, "merge closed buf")
}

// ── Negative: ops after close ──

fn neg_ops_after_close<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_buf(backend, heap_name, ALLOC_SIZE)?;
    let cfd = container.merge(&[fd1])?;
    container.close(cfd)?;

    expect_errno(
        backend.llseek(cfd, 0, libc::SEEK_END),
        Errno::EBADF,
        "llseek after close",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn run_all_container_tests() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string(), "system-uncached".to_string()];
        let (results, err) = run(&b, &heaps, 14);
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(
            results.iter().all(|r| r.passed),
            "failed tests: {:?}",
            results.iter().filter(|r| !r.passed).collect::<Vec<_>>()
        );
        // 13 base + 1 cross-heap = 14 tests.
        assert_eq!(results.len(), 14);
    }

    #[test]
    fn run_single_heap_no_cross() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string()];
        let (results, err) = run(&b, &heaps, 6);
        assert!(err.is_none());
        // 13 tests (no cross-heap).
        assert_eq!(results.len(), 13);
        assert!(results.iter().all(|r| r.passed));
    }
}
