// High-level wrapper for dma-heap device operations.

use std::os::unix::io::RawFd;

use crate::backend::HeapBackend;
use crate::ioctl::dma_heap::DmaHeapAllocationData;

/// High-level wrapper for a `/dev/dma_heap/<name>` device.
#[derive(Debug)]
pub struct DmaHeap<'a, H: HeapBackend> {
    backend: &'a H,
    fd: RawFd,
}

impl<'a, H: HeapBackend> DmaHeap<'a, H> {
    /// Open a dma-heap device by name.
    ///
    /// # Errors
    /// - `ENOENT` if the heap does not exist
    /// - `EACCES` if permission is denied
    #[must_use = "returns a heap handle that should be used"]
    pub fn open(backend: &'a H, name: &str) -> nix::Result<Self> {
        let fd = backend.open(name)?;
        tracing::debug!(heap = name, fd, "heap opened");
        Ok(Self { backend, fd })
    }

    /// Allocate a `dma-buf` from this heap. Returns the buffer file descriptor.
    ///
    /// # Errors
    /// - `EINVAL` if `size` is 0, `fd_flags` or `heap_flags` are invalid
    /// - `ENOMEM` if allocation fails
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
    pub fn alloc(&self, size: u64, fd_flags: u32, heap_flags: u64) -> nix::Result<RawFd> {
        let mut data = DmaHeapAllocationData {
            len: size,
            fd: 0,
            fd_flags,
            heap_flags,
        };
        self.backend.alloc(self.fd, &mut data)?;
        let buf_fd = data.fd as RawFd;
        tracing::debug!(size, buf_fd, "buffer allocated");
        Ok(buf_fd)
    }

    /// Return the underlying heap file descriptor.
    #[must_use]
    pub fn fd(&self) -> RawFd {
        self.fd
    }
}

impl<H: HeapBackend> Drop for DmaHeap<'_, H> {
    fn drop(&mut self) {
        tracing::trace!(fd = self.fd, "heap closing");
        let _ = self.backend.close_heap(self.fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;
    use crate::ioctl::dma_heap::{DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};

    fn make_backend() -> MockBackend {
        MockBackend::new()
    }

    #[test]
    fn open_and_alloc() {
        let backend = make_backend();
        let heap = DmaHeap::open(&backend, "system").unwrap();
        let fd = heap
            .alloc(4096, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
            .unwrap();
        assert!(fd >= 1000); // mock fds start at 1000
    }

    #[test]
    fn open_nonexistent_heap() {
        let backend = make_backend();
        let result = DmaHeap::open(&backend, "");
        assert_eq!(result.unwrap_err(), nix::errno::Errno::ENOENT);
    }

    #[test]
    fn alloc_zero_size() {
        let backend = make_backend();
        let heap = DmaHeap::open(&backend, "system").unwrap();
        let result = heap.alloc(0, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS);
        assert_eq!(result.unwrap_err(), nix::errno::Errno::EINVAL);
    }

    #[test]
    fn alloc_multiple_returns_different_fds() {
        let backend = make_backend();
        let heap = DmaHeap::open(&backend, "system").unwrap();
        let fd1 = heap
            .alloc(4096, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
            .unwrap();
        let fd2 = heap
            .alloc(4096, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
            .unwrap();
        assert_ne!(fd1, fd2);
    }

    #[test]
    fn drop_closes_heap() {
        let backend = make_backend();
        let heap_fd;
        {
            let heap = DmaHeap::open(&backend, "system").unwrap();
            heap_fd = heap.fd();
        }
        // After drop, the heap fd should be invalid — attempting to alloc should fail.
        let mut data = DmaHeapAllocationData {
            len: 4096,
            fd: 0,
            fd_flags: DMA_HEAP_VALID_FD_FLAGS,
            heap_flags: DMA_HEAP_VALID_HEAP_FLAGS,
        };
        let result = backend.alloc(heap_fd, &mut data);
        assert_eq!(result.unwrap_err(), nix::errno::Errno::EBADF);
    }

    #[test]
    fn fd_accessor() {
        let backend = make_backend();
        let heap = DmaHeap::open(&backend, "system").unwrap();
        assert!(heap.fd() >= 1000);
    }
}
