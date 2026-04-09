// High-level wrapper for Samsung dma-buf container operations.

use std::os::unix::io::RawFd;

use crate::backend::ContainerBackend;
use crate::trace;

/// High-level wrapper for `/dev/dmabuf_container` device operations.
///
/// Opens the container device on creation and closes it on drop.
/// Individual container fds (from merge) must be closed explicitly
/// or via the caller's lifecycle management.
#[derive(Debug)]
pub struct DmaBufContainer<'a, C: ContainerBackend> {
    backend: &'a C,
    device_fd: RawFd,
}

impl<'a, C: ContainerBackend> DmaBufContainer<'a, C> {
    /// Open the `/dev/dmabuf_container` device.
    ///
    /// # Errors
    /// - `ENOENT` if the device does not exist (non-Samsung)
    /// - `EACCES` if permission is denied
    pub fn open(backend: &'a C) -> nix::Result<Self> {
        let device_fd = backend.open_container_device()?;
        tracing::debug!(device_fd, "container device opened");
        Ok(Self { backend, device_fd })
    }

    /// Merge dma-buf fds into a container. Returns the new container fd.
    ///
    /// # Errors
    /// - `EINVAL` if `buf_fds` is empty or exceeds `MAX_BUFCON_SRC_BUFS`
    /// - `EBADF` if any buffer fd is invalid
    pub fn merge(&self, buf_fds: &[RawFd]) -> nix::Result<RawFd> {
        if trace::enabled() {
            trace::begin(&format!("container_merge_{}", buf_fds.len()));
        }
        let container_fd = self.backend.merge(self.device_fd, buf_fds)?;
        if trace::enabled() {
            trace::end();
        }
        tracing::debug!(container_fd, count = buf_fds.len(), "container merged");
        Ok(container_fd)
    }

    /// Set the active buffer mask on a container.
    ///
    /// # Errors
    /// - `EINVAL` if mask has bits beyond the buffer count
    /// - `EBADF` if `container_fd` is invalid
    pub fn set_mask(&self, container_fd: RawFd, mask: u64) -> nix::Result<()> {
        if trace::enabled() {
            trace::begin(&format!("container_set_mask_{container_fd:#x}"));
        }
        self.backend.set_mask(self.device_fd, container_fd, mask)?;
        if trace::enabled() {
            trace::end();
        }
        tracing::debug!(container_fd, mask, "mask set");
        Ok(())
    }

    /// Get the current buffer mask from a container.
    ///
    /// # Errors
    /// - `EBADF` if `container_fd` is invalid
    pub fn get_mask(&self, container_fd: RawFd) -> nix::Result<u64> {
        if trace::enabled() {
            trace::begin(&format!("container_get_mask_{container_fd:#x}"));
        }
        let mask = self.backend.get_mask(self.device_fd, container_fd)?;
        if trace::enabled() {
            trace::end();
        }
        tracing::debug!(container_fd, mask, "mask read");
        Ok(mask)
    }

    /// Close a container fd.
    ///
    /// # Errors
    /// - `EBADF` if `container_fd` is invalid
    pub fn close(&self, container_fd: RawFd) -> nix::Result<()> {
        if trace::enabled() {
            trace::instant(&format!("container_close_{container_fd}"));
        }
        self.backend.close_container(container_fd)
    }

    /// Return the underlying device fd.
    #[must_use]
    pub fn device_fd(&self) -> RawFd {
        self.device_fd
    }
}

impl<C: ContainerBackend> Drop for DmaBufContainer<'_, C> {
    fn drop(&mut self) {
        tracing::trace!(device_fd = self.device_fd, "container device closing");
        let _ = self.backend.close_container(self.device_fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;
    use crate::backend::{DmaBufBackend, HeapBackend};
    use crate::ioctl::dma_heap::DMA_HEAP_ALLOC_FD_FLAGS;

    fn alloc_buf(backend: &MockBackend, size: u64) -> RawFd {
        let heap_fd = backend.open("system").unwrap();
        let mut data = crate::ioctl::dma_heap::DmaHeapAllocationData {
            len: size,
            fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
            ..Default::default()
        };
        backend.alloc(heap_fd, &mut data).unwrap();
        backend.close_heap(heap_fd).unwrap();
        #[allow(clippy::cast_possible_wrap)]
        let fd = data.fd as RawFd;
        fd
    }

    #[test]
    fn open_and_drop() {
        let b = MockBackend::new();
        let container = DmaBufContainer::open(&b).unwrap();
        assert!(container.device_fd() >= 1000);
        // Drop closes device fd.
    }

    #[test]
    fn merge_and_close() {
        let b = MockBackend::new();
        let container = DmaBufContainer::open(&b).unwrap();
        let fd1 = alloc_buf(&b, 4096);
        let fd2 = alloc_buf(&b, 4096);

        let cfd = container.merge(&[fd1, fd2]).unwrap();
        assert!(cfd >= 1000);
        container.close(cfd).unwrap();
        b.close(fd1).unwrap();
        b.close(fd2).unwrap();
    }

    #[test]
    fn mask_roundtrip() {
        let b = MockBackend::new();
        let container = DmaBufContainer::open(&b).unwrap();
        let fd1 = alloc_buf(&b, 4096);
        let fd2 = alloc_buf(&b, 4096);
        let fd3 = alloc_buf(&b, 4096);

        let cfd = container.merge(&[fd1, fd2, fd3]).unwrap();

        container.set_mask(cfd, 0b101).unwrap();
        assert_eq!(container.get_mask(cfd).unwrap(), 0b101);

        container.close(cfd).unwrap();
        b.close(fd1).unwrap();
        b.close(fd2).unwrap();
        b.close(fd3).unwrap();
    }

    #[test]
    fn default_mask_is_zero() {
        let b = MockBackend::new();
        let container = DmaBufContainer::open(&b).unwrap();
        let fd1 = alloc_buf(&b, 4096);

        let cfd = container.merge(&[fd1]).unwrap();
        assert_eq!(container.get_mask(cfd).unwrap(), 0);

        container.close(cfd).unwrap();
        b.close(fd1).unwrap();
    }

    #[test]
    fn merge_empty_fails() {
        let b = MockBackend::new();
        let container = DmaBufContainer::open(&b).unwrap();
        assert_eq!(container.merge(&[]).unwrap_err(), nix::errno::Errno::EINVAL);
    }

    #[test]
    fn close_invalid_fails() {
        let b = MockBackend::new();
        let container = DmaBufContainer::open(&b).unwrap();
        assert_eq!(container.close(9999).unwrap_err(), nix::errno::Errno::EBADF);
    }

    #[test]
    fn merge_and_llseek() {
        let b = MockBackend::new();
        let container = DmaBufContainer::open(&b).unwrap();
        let fd1 = alloc_buf(&b, 4096);
        let fd2 = alloc_buf(&b, 8192);
        let cfd = container.merge(&[fd1, fd2]).unwrap();

        // Merged fd is a normal dma-buf; llseek returns combined size.
        let size = b.llseek(cfd, 0, libc::SEEK_END).unwrap();
        assert_eq!(size, 4096 + 8192);

        container.close(cfd).unwrap();
        b.close(fd1).unwrap();
        b.close(fd2).unwrap();
    }

    #[test]
    fn source_bufs_valid_after_container_close() {
        let b = MockBackend::new();
        let container = DmaBufContainer::open(&b).unwrap();
        let fd1 = alloc_buf(&b, 4096);
        let cfd = container.merge(&[fd1]).unwrap();

        container.close(cfd).unwrap();
        // Source buffer should still work.
        let size = b.llseek(fd1, 0, libc::SEEK_END).unwrap();
        assert_eq!(size, 4096);
        b.close(fd1).unwrap();
    }

    #[test]
    fn set_mask_overflow_fails() {
        let b = MockBackend::new();
        let container = DmaBufContainer::open(&b).unwrap();
        let fd1 = alloc_buf(&b, 4096);
        let cfd = container.merge(&[fd1]).unwrap();

        // count=1, so only bit 0 valid. Bit 1 should fail.
        assert_eq!(
            container.set_mask(cfd, 0b10).unwrap_err(),
            nix::errno::Errno::EINVAL
        );

        container.close(cfd).unwrap();
        b.close(fd1).unwrap();
    }
}
