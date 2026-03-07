// Common scenario patterns for workload simulations.

pub mod camera;
pub mod codec;
pub mod display;
pub mod gpu;
pub mod npu;

use std::time::Instant;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};

/// A fixed-size buffer pool that allocates N buffers and cycles through them.
///
/// Used by camera preview, display flip, codec DPB, etc.
pub struct BufferPool<'a, B: DmaBufBackend> {
    buffers: Vec<DmaBuf<'a, B>>,
    index: usize,
}

impl<'a, B: HeapBackend + DmaBufBackend> BufferPool<'a, B> {
    /// Allocate a pool of `count` buffers, each of `size` bytes.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(
        backend: &'a B,
        heap: &DmaHeap<'a, B>,
        size: u64,
        count: usize,
    ) -> nix::Result<Self> {
        let mut buffers = Vec::with_capacity(count);
        for _ in 0..count {
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            let mut buf = DmaBuf::new(backend, fd, size as usize);
            buf.mmap()?;
            buffers.push(buf);
        }
        Ok(Self { buffers, index: 0 })
    }

    /// Get the next buffer in round-robin order.
    pub fn next(&mut self) -> &mut DmaBuf<'a, B> {
        let len = self.buffers.len();
        let idx = self.index;
        self.index = (idx + 1) % len;
        &mut self.buffers[idx]
    }

    /// Return the number of buffers in the pool.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    /// Return whether the pool is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }
}

/// Bulk-allocate `count` buffers of `size` bytes, returning alloc latencies.
#[allow(clippy::cast_possible_truncation)]
pub fn bulk_alloc<'a, B: HeapBackend + DmaBufBackend>(
    backend: &'a B,
    heap: &DmaHeap<'a, B>,
    size: u64,
    count: usize,
) -> nix::Result<(Vec<DmaBuf<'a, B>>, Vec<u64>)> {
    let mut buffers = Vec::with_capacity(count);
    let mut latencies = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let elapsed = start.elapsed().as_micros() as u64;
        latencies.push(elapsed);
        buffers.push(DmaBuf::new(backend, fd, size as usize));
    }
    Ok((buffers, latencies))
}

/// Write a pattern to a buffer: `sync_start(WRITE)` → fill → `sync_end(WRITE)`.
///
/// # Safety
/// Caller must ensure `buf` has been `mmap`ped and `ptr` is valid for `len`.
pub fn fill_buffer<B: DmaBufBackend>(
    buf: &DmaBuf<'_, B>,
    ptr: *mut u8,
    len: usize,
    pattern: u8,
) -> nix::Result<()> {
    buf.sync_start(DMA_BUF_SYNC_WRITE)?;
    unsafe { std::ptr::write_bytes(ptr, pattern, len) };
    buf.sync_end(DMA_BUF_SYNC_WRITE)?;
    Ok(())
}

/// Read-sync a buffer (no actual read, just sync cycle).
pub fn read_sync<B: DmaBufBackend>(buf: &DmaBuf<'_, B>) -> nix::Result<()> {
    buf.sync_start(DMA_BUF_SYNC_READ)?;
    buf.sync_end(DMA_BUF_SYNC_READ)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn buffer_pool_round_robin() {
        let b = MockBackend::new();
        let heap = DmaHeap::open(&b, "system").unwrap();
        let mut pool = BufferPool::new(&b, &heap, 4096, 3).unwrap();
        assert_eq!(pool.len(), 3);
        assert!(!pool.is_empty());

        let fd0 = pool.next().fd();
        let fd1 = pool.next().fd();
        let fd2 = pool.next().fd();
        let fd0_again = pool.next().fd();
        assert_eq!(fd0, fd0_again);
        assert_ne!(fd0, fd1);
        assert_ne!(fd1, fd2);
    }

    #[test]
    fn bulk_alloc_returns_latencies() {
        let b = MockBackend::new();
        let heap = DmaHeap::open(&b, "system").unwrap();
        let (bufs, lats) = bulk_alloc(&b, &heap, 4096, 10).unwrap();
        assert_eq!(bufs.len(), 10);
        assert_eq!(lats.len(), 10);
    }

    #[test]
    fn fill_and_read_sync() {
        let b = MockBackend::new();
        let heap = DmaHeap::open(&b, "system").unwrap();
        let fd = heap
            .alloc(4096, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
            .unwrap();
        let mut buf = DmaBuf::new(&b, fd, 4096);
        let ptr = buf.mmap().unwrap();
        fill_buffer(&buf, ptr, 4096, 0xAA).unwrap();
        read_sync(&buf).unwrap();
    }

    #[test]
    fn buffer_pool_no_leak() {
        let b = MockBackend::new();
        {
            let heap = DmaHeap::open(&b, "system").unwrap();
            let pool = BufferPool::new(&b, &heap, 4096, 5).unwrap();
            assert_eq!(pool.len(), 5);
        }
        assert_eq!(b.buffer_count(), 0);
    }
}
