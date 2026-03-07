// High-level wrapper for dma-buf file descriptor operations.

use std::os::unix::io::RawFd;

use crate::backend::DmaBufBackend;
use crate::ioctl::dma_buf::{
    DMA_BUF_SYNC_END, DMA_BUF_SYNC_START, DmaBufExportSyncFile, DmaBufImportSyncFile,
};

/// High-level wrapper for a `dma-buf` file descriptor.
#[derive(Debug)]
pub struct DmaBuf<'a, D: DmaBufBackend> {
    backend: &'a D,
    fd: RawFd,
    len: usize,
    mapped: Option<(*mut u8, usize)>,
}

impl<'a, D: DmaBufBackend> DmaBuf<'a, D> {
    /// Wrap a raw `dma-buf` fd with its known length.
    #[must_use]
    pub fn new(backend: &'a D, fd: RawFd, len: usize) -> Self {
        Self {
            backend,
            fd,
            len,
            mapped: None,
        }
    }

    /// Map the buffer into userspace. Returns a pointer to the mapped region.
    ///
    /// Idempotent: if already mapped, returns the existing pointer.
    ///
    /// # Errors
    /// - `EBADF` if fd is invalid
    /// - `EINVAL` if len exceeds buffer size
    pub fn mmap(&mut self) -> nix::Result<*mut u8> {
        if let Some((ptr, _)) = self.mapped {
            return Ok(ptr);
        }
        let ptr = self.backend.mmap(self.fd, self.len)?;
        tracing::debug!(fd = self.fd, len = self.len, "buffer mapped");
        self.mapped = Some((ptr, self.len));
        Ok(ptr)
    }

    /// Begin CPU access with the given direction flags.
    ///
    /// # Errors
    /// - `EINVAL` if flags are invalid
    /// - `EBADF` if fd is invalid
    pub fn sync_start(&self, flags: u64) -> nix::Result<()> {
        let combined = DMA_BUF_SYNC_START | flags;
        tracing::trace!(fd = self.fd, flags = combined, "sync start");
        self.backend.sync(self.fd, combined)
    }

    /// End CPU access with the given direction flags.
    ///
    /// # Errors
    /// - `EINVAL` if flags are invalid
    /// - `EBADF` if fd is invalid
    pub fn sync_end(&self, flags: u64) -> nix::Result<()> {
        let combined = DMA_BUF_SYNC_END | flags;
        tracing::trace!(fd = self.fd, flags = combined, "sync end");
        self.backend.sync(self.fd, combined)
    }

    /// Query the actual buffer size via `llseek(SEEK_END)`.
    ///
    /// Restores the file offset to 0 (`SEEK_SET`) after querying.
    ///
    /// # Errors
    /// - `EBADF` if fd is invalid
    pub fn llseek_size(&self) -> nix::Result<i64> {
        let size = self.backend.llseek(self.fd, 0, libc::SEEK_END)?;
        let _ = self.backend.llseek(self.fd, 0, libc::SEEK_SET);
        Ok(size)
    }

    /// Set a debug name on this `dma-buf`.
    ///
    /// # Errors
    /// - `ENAMETOOLONG` if name exceeds `DMA_BUF_NAME_LEN`
    /// - `EBADF` if fd is invalid
    pub fn set_name(&self, name: &str) -> nix::Result<()> {
        self.backend.set_name(self.fd, name)?;
        tracing::debug!(fd = self.fd, name, "name set");
        Ok(())
    }

    /// Export fences as a `sync_file`. Returns the `sync_file` fd.
    ///
    /// # Errors
    /// - `EINVAL` if flags are invalid
    /// - `EBADF` if fd is invalid
    pub fn export_sync_file(&self, flags: u32) -> nix::Result<RawFd> {
        let mut data = DmaBufExportSyncFile { flags, fd: -1 };
        self.backend.export_sync_file(self.fd, &mut data)?;
        Ok(data.fd as RawFd)
    }

    /// Import a `sync_file` into this `dma-buf`.
    ///
    /// # Errors
    /// - `EINVAL` if flags are invalid or `sync_fd` is not a valid `sync_file`
    /// - `EBADF` if fd is invalid
    pub fn import_sync_file(&self, flags: u32, sync_fd: RawFd) -> nix::Result<()> {
        let data = DmaBufImportSyncFile {
            flags,
            fd: sync_fd,
        };
        self.backend.import_sync_file(self.fd, data)
    }

    /// Duplicate this `dma-buf`, returning a new wrapper sharing the same backend.
    ///
    /// The new `DmaBuf` is unmapped regardless of the original's state.
    ///
    /// # Errors
    /// - `EBADF` if fd is invalid
    pub fn dup(&self) -> nix::Result<DmaBuf<'a, D>> {
        let new_fd = self.backend.dup(self.fd)?;
        tracing::debug!(old_fd = self.fd, new_fd, "buffer duped");
        Ok(DmaBuf {
            backend: self.backend,
            fd: new_fd,
            len: self.len,
            mapped: None,
        })
    }

    /// Return the underlying file descriptor.
    #[must_use]
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Return the buffer length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Return whether the buffer has zero length.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<D: DmaBufBackend> Drop for DmaBuf<'_, D> {
    fn drop(&mut self) {
        if let Some((addr, len)) = self.mapped.take() {
            tracing::trace!(fd = self.fd, "unmapping buffer");
            let _ = self.backend.munmap(addr, len);
        }
        tracing::trace!(fd = self.fd, "buffer closing");
        let _ = self.backend.close(self.fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;
    use crate::backend::HeapBackend;
    use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_RW};
    use crate::ioctl::dma_heap::{DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};

    fn alloc_buf(backend: &MockBackend, size: usize) -> RawFd {
        let heap_fd = backend.open("system").unwrap();
        let mut data = crate::ioctl::dma_heap::DmaHeapAllocationData {
            len: size as u64,
            fd: 0,
            fd_flags: DMA_HEAP_VALID_FD_FLAGS,
            heap_flags: DMA_HEAP_VALID_HEAP_FLAGS,
        };
        backend.alloc(heap_fd, &mut data).unwrap();
        #[allow(clippy::cast_possible_wrap)]
        let buf_fd = data.fd as RawFd;
        buf_fd
    }

    #[test]
    fn new_and_drop() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        {
            let _buf = DmaBuf::new(&backend, fd, 4096);
        }
        // After drop, fd should be closed — sync should fail with EBADF.
        let result = backend.sync(fd, DMA_BUF_SYNC_READ);
        assert_eq!(result.unwrap_err(), nix::errno::Errno::EBADF);
    }

    #[test]
    fn mmap_and_access() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        let mut buf = DmaBuf::new(&backend, fd, 4096);
        let ptr = buf.mmap().unwrap();
        // Write and read back through the mapped pointer.
        unsafe {
            *ptr = 0xAB;
            assert_eq!(*ptr, 0xAB);
        }
    }

    #[test]
    fn mmap_idempotent() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        let mut buf = DmaBuf::new(&backend, fd, 4096);
        let ptr1 = buf.mmap().unwrap();
        let ptr2 = buf.mmap().unwrap();
        assert_eq!(ptr1, ptr2);
    }

    #[test]
    fn sync_start_end() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        let buf = DmaBuf::new(&backend, fd, 4096);
        buf.sync_start(DMA_BUF_SYNC_READ).unwrap();
        buf.sync_end(DMA_BUF_SYNC_READ).unwrap();
    }

    #[test]
    fn sync_start_end_rw() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        let buf = DmaBuf::new(&backend, fd, 4096);
        buf.sync_start(DMA_BUF_SYNC_RW).unwrap();
        buf.sync_end(DMA_BUF_SYNC_RW).unwrap();
    }

    #[test]
    fn llseek_size_matches() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        let buf = DmaBuf::new(&backend, fd, 4096);
        let size = buf.llseek_size().unwrap();
        assert_eq!(size, 4096);
    }

    #[test]
    fn set_name_ok() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        let buf = DmaBuf::new(&backend, fd, 4096);
        buf.set_name("test_buffer").unwrap();
    }

    #[test]
    fn export_import_sync_file() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        let buf = DmaBuf::new(&backend, fd, 4096);
        #[allow(clippy::cast_possible_truncation)]
        let flags = DMA_BUF_SYNC_READ as u32;
        let sync_fd = buf.export_sync_file(flags).unwrap();
        assert!(sync_fd >= 1000);
        buf.import_sync_file(flags, sync_fd).unwrap();
    }

    #[test]
    fn dup_returns_different_fd() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        let buf = DmaBuf::new(&backend, fd, 4096);
        let dup_buf = buf.dup().unwrap();
        assert_ne!(buf.fd(), dup_buf.fd());
        assert_eq!(buf.len(), dup_buf.len());
    }

    #[test]
    fn dup_independent_close() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        let buf = DmaBuf::new(&backend, fd, 4096);
        let dup_buf = buf.dup().unwrap();
        let dup_fd = dup_buf.fd();
        // Drop original — dup should still work.
        drop(buf);
        let size = dup_buf.llseek_size().unwrap();
        assert_eq!(size, 4096);
        drop(dup_buf);
        // Now dup fd should be closed too.
        let result = backend.sync(dup_fd, DMA_BUF_SYNC_READ);
        assert_eq!(result.unwrap_err(), nix::errno::Errno::EBADF);
    }

    #[test]
    fn drop_unmaps_buffer() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        {
            let mut buf = DmaBuf::new(&backend, fd, 4096);
            let _ = buf.mmap().unwrap();
            // Drop triggers munmap then close.
        }
        // fd should be closed after drop.
        let result = backend.sync(fd, DMA_BUF_SYNC_READ);
        assert_eq!(result.unwrap_err(), nix::errno::Errno::EBADF);
    }

    #[test]
    fn drop_without_mmap() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        {
            let _buf = DmaBuf::new(&backend, fd, 4096);
        }
        // Should close cleanly without munmap.
        let result = backend.sync(fd, DMA_BUF_SYNC_READ);
        assert_eq!(result.unwrap_err(), nix::errno::Errno::EBADF);
    }

    #[test]
    fn len_and_is_empty() {
        let backend = MockBackend::new();
        let fd = alloc_buf(&backend, 4096);
        let buf = DmaBuf::new(&backend, fd, 4096);
        assert_eq!(buf.len(), 4096);
        assert!(!buf.is_empty());
    }
}
