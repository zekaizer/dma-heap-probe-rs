// Samsung dma-buf container tests: merge, mask, cross-heap, negative paths.

use nix::errno::Errno;

use crate::backend::{ContainerBackend, DmaBufBackend, HeapBackend};
use crate::container::DmaBufContainer;
use crate::heap::DmaHeap;
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

    // Use the first heap for single-heap tests, all heaps for cross-heap.
    let primary_heap = heaps.first().map_or("system", String::as_str);

    let mut tests: Vec<(&str, nix::Result<()>)> = vec![
        // Positive: merge & lifecycle
        ("merge_two_bufs", merge_two_bufs(backend, primary_heap)),
        ("merge_single_buf", merge_single_buf(backend, primary_heap)),
        (
            "merge_close_lifecycle",
            merge_close_lifecycle(backend, primary_heap),
        ),
        (
            "merge_flatten_container",
            merge_flatten_container(backend, primary_heap),
        ),
        // Positive: mask operations
        (
            "set_get_mask_roundtrip",
            set_get_mask_roundtrip(backend, primary_heap),
        ),
        ("mask_zero", mask_zero(backend, primary_heap)),
        ("mask_all_bits", mask_all_bits(backend, primary_heap)),
        (
            "mask_partial_bits",
            mask_partial_bits(backend, primary_heap),
        ),
        // Negative: merge
        ("neg_merge_zero_count", neg_merge_zero_count(backend)),
        (
            "neg_merge_over_max",
            neg_merge_over_max(backend, primary_heap),
        ),
        ("neg_merge_invalid_fd", neg_merge_invalid_fd(backend)),
        (
            "neg_merge_closed_buf",
            neg_merge_closed_buf(backend, primary_heap),
        ),
        // Negative: container fd ops
        (
            "neg_mmap_container_fd",
            neg_mmap_container_fd(backend, primary_heap),
        ),
        (
            "neg_sync_container_fd",
            neg_sync_container_fd(backend, primary_heap),
        ),
        (
            "neg_llseek_container_fd",
            neg_llseek_container_fd(backend, primary_heap),
        ),
        // Negative: mask
        (
            "neg_set_mask_overflow",
            neg_set_mask_overflow(backend, primary_heap),
        ),
        (
            "neg_set_mask_bad_container",
            neg_set_mask_bad_container(backend),
        ),
        (
            "neg_get_mask_bad_container",
            neg_get_mask_bad_container(backend),
        ),
        (
            "neg_ops_after_close",
            neg_ops_after_close(backend, primary_heap),
        ),
    ];

    // Cross-heap merge (only if multiple heaps available).
    if heaps.len() >= 2 {
        tests.push((
            "cross_heap_merge",
            cross_heap_merge(backend, &heaps[0], &heaps[1]),
        ));
    }

    runner::collect_test_results("container", "container", heap_w, &tests)
}

// ── Helpers ──

fn alloc_one<'a, B: HeapBackend + DmaBufBackend>(
    backend: &'a B,
    heap_name: &str,
) -> nix::Result<(DmaHeap<'a, B>, i32)> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let buf_fd = heap.alloc(
        ALLOC_SIZE,
        DMA_HEAP_ALLOC_FD_FLAGS,
        DMA_HEAP_VALID_HEAP_FLAGS,
    )?;
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
    let (_h, fd1) = alloc_one(backend, heap_name)?;
    let (_h2, fd2) = alloc_one(backend, heap_name)?;

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
    let (_h, fd1) = alloc_one(backend, heap_name)?;

    let cfd = container.merge(&[fd1])?;
    container.close(cfd)?;
    Ok(())
}

fn merge_close_lifecycle<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_one(backend, heap_name)?;

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
    let (_h1, fd1) = alloc_one(backend, heap_name)?;
    let (_h2, fd2) = alloc_one(backend, heap_name)?;
    let (_h3, fd3) = alloc_one(backend, heap_name)?;

    // Create inner container [fd1, fd2].
    let inner = container.merge(&[fd1, fd2])?;
    // Merge inner + fd3 → should flatten to 3 buffers.
    let outer = container.merge(&[inner, fd3])?;

    // Verify count=3 by testing mask bits.
    container.set_mask(outer, 0b111)?;
    let mask = container.get_mask(outer)?;
    if mask != 0b111 {
        tracing::error!(expected = 0b111, got = mask, "flatten mask mismatch");
        return Err(Errno::EIO);
    }

    // Bit 3 should be invalid (only 3 buffers).
    expect_errno(
        container.set_mask(outer, 0b1000),
        Errno::EINVAL,
        "flatten overflow bit",
    )?;

    container.close(outer)?;
    container.close(inner)?;
    Ok(())
}

// ── Positive: mask operations ──

fn set_get_mask_roundtrip<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h1, fd1) = alloc_one(backend, heap_name)?;
    let (_h2, fd2) = alloc_one(backend, heap_name)?;
    let (_h3, fd3) = alloc_one(backend, heap_name)?;

    let cfd = container.merge(&[fd1, fd2, fd3])?;

    for mask in [0b001, 0b010, 0b100, 0b011, 0b101, 0b110, 0b111] {
        container.set_mask(cfd, mask)?;
        let got = container.get_mask(cfd)?;
        if got != mask {
            tracing::error!(expected = mask, got, "mask roundtrip mismatch");
            return Err(Errno::EIO);
        }
    }

    container.close(cfd)?;
    Ok(())
}

fn mask_zero<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_one(backend, heap_name)?;
    let cfd = container.merge(&[fd1])?;

    container.set_mask(cfd, 0)?;
    let mask = container.get_mask(cfd)?;
    if mask != 0 {
        return Err(Errno::EIO);
    }
    container.close(cfd)?;
    Ok(())
}

fn mask_all_bits<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h1, fd1) = alloc_one(backend, heap_name)?;
    let (_h2, fd2) = alloc_one(backend, heap_name)?;
    let (_h3, fd3) = alloc_one(backend, heap_name)?;
    let (_h4, fd4) = alloc_one(backend, heap_name)?;

    let cfd = container.merge(&[fd1, fd2, fd3, fd4])?;
    let all = (1u64 << 4) - 1; // 0b1111
    container.set_mask(cfd, all)?;
    let got = container.get_mask(cfd)?;
    if got != all {
        return Err(Errno::EIO);
    }
    container.close(cfd)?;
    Ok(())
}

fn mask_partial_bits<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h1, fd1) = alloc_one(backend, heap_name)?;
    let (_h2, fd2) = alloc_one(backend, heap_name)?;
    let (_h3, fd3) = alloc_one(backend, heap_name)?;
    let (_h4, fd4) = alloc_one(backend, heap_name)?;

    let cfd = container.merge(&[fd1, fd2, fd3, fd4])?;

    // Set only odd-indexed buffers: bit 1 and bit 3 → 0b1010.
    container.set_mask(cfd, 0b1010)?;
    let got = container.get_mask(cfd)?;
    if got != 0b1010 {
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
    let (_ha, fda) = alloc_one(backend, heap_a)?;
    let (_hb, fdb) = alloc_one(backend, heap_b)?;

    let cfd = container.merge(&[fda, fdb])?;
    container.set_mask(cfd, 0b11)?;
    let mask = container.get_mask(cfd)?;
    if mask != 0b11 {
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
    let (_h, fd1) = alloc_one(backend, heap_name)?;
    backend.close(fd1)?;
    expect_errno(container.merge(&[fd1]), Errno::EBADF, "merge closed buf")
}

// ── Negative: container fd ops ──

fn neg_mmap_container_fd<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_one(backend, heap_name)?;
    let cfd = container.merge(&[fd1])?;

    let result = backend.mmap(cfd, 4096);
    container.close(cfd)?;
    expect_errno(result, Errno::EACCES, "mmap container fd")
}

fn neg_sync_container_fd<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_START};

    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_one(backend, heap_name)?;
    let cfd = container.merge(&[fd1])?;

    let result = backend.sync(cfd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ);
    container.close(cfd)?;
    expect_errno(result, Errno::EINVAL, "sync container fd")
}

fn neg_llseek_container_fd<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_one(backend, heap_name)?;
    let cfd = container.merge(&[fd1])?;

    let result = backend.llseek(cfd, 0, libc::SEEK_END);
    container.close(cfd)?;
    expect_errno(result, Errno::EINVAL, "llseek container fd")
}

// ── Negative: mask ──

fn neg_set_mask_overflow<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h1, fd1) = alloc_one(backend, heap_name)?;
    let (_h2, fd2) = alloc_one(backend, heap_name)?;
    let cfd = container.merge(&[fd1, fd2])?;

    // count=2, valid bits = 0b11, bit 2 (0b100) should fail.
    let result = container.set_mask(cfd, 0b100);
    container.close(cfd)?;
    expect_errno(result, Errno::EINVAL, "set_mask overflow")
}

fn neg_set_mask_bad_container<B: ContainerBackend>(backend: &B) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    expect_errno(
        container.set_mask(9999, 0),
        Errno::EBADF,
        "set_mask bad container",
    )
}

fn neg_get_mask_bad_container<B: ContainerBackend>(backend: &B) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    expect_errno(
        container.get_mask(9999),
        Errno::EBADF,
        "get_mask bad container",
    )
}

fn neg_ops_after_close<B: HeapBackend + DmaBufBackend + ContainerBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let container = DmaBufContainer::open(backend)?;
    let (_h, fd1) = alloc_one(backend, heap_name)?;
    let cfd = container.merge(&[fd1])?;
    container.close(cfd)?;

    expect_errno(
        container.set_mask(cfd, 0),
        Errno::EBADF,
        "set_mask after close",
    )?;
    expect_errno(
        container.get_mask(cfd),
        Errno::EBADF,
        "get_mask after close",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn run_all_container_tests() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string(), "reserved".to_string()];
        let (results, err) = run(&b, &heaps, 8);
        assert!(err.is_none(), "unexpected error: {err:?}");
        assert!(
            results.iter().all(|r| r.passed),
            "failed tests: {:?}",
            results.iter().filter(|r| !r.passed).collect::<Vec<_>>()
        );
        // 19 base + 1 cross-heap = 20 tests.
        assert_eq!(results.len(), 20);
    }

    #[test]
    fn run_single_heap_no_cross() {
        let b = MockBackend::new();
        let heaps = vec!["system".to_string()];
        let (results, err) = run(&b, &heaps, 6);
        assert!(err.is_none());
        // 19 tests (no cross-heap).
        assert_eq!(results.len(), 19);
        assert!(results.iter().all(|r| r.passed));
    }
}
