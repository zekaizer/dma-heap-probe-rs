// Backend abstraction for dma-heap and dma-buf operations.
//
// Real implementation (ioctl/mmap) is used on Android targets.
// Mock implementation is used for host testing.

use std::os::unix::io::RawFd;

use crate::ioctl::dma_buf::{DmaBufExportSyncFile, DmaBufImportSyncFile};
use crate::ioctl::dma_heap::DmaHeapAllocationData;

/// Backend for dma-heap device operations.
pub trait HeapBackend {
    /// Open a dma-heap device by name. Returns a heap file descriptor.
    ///
    /// # Errors
    /// - `ENOENT` if the heap does not exist
    /// - `EACCES` if permission is denied
    fn open(&self, name: &str) -> nix::Result<RawFd>;

    /// Allocate a dma-buf from the heap. Populates `data.fd` on success.
    ///
    /// # Errors
    /// - `EINVAL` if len is 0, `fd_flags` or `heap_flags` are invalid
    /// - `ENOMEM` if allocation fails
    /// - `EBADF` if `heap_fd` is invalid
    fn alloc(&self, heap_fd: RawFd, data: &mut DmaHeapAllocationData) -> nix::Result<()>;

    /// Close a heap device file descriptor.
    ///
    /// # Errors
    /// - `EBADF` if `heap_fd` is invalid
    fn close_heap(&self, heap_fd: RawFd) -> nix::Result<()>;
}

/// Backend for dma-buf file descriptor operations.
pub trait DmaBufBackend {
    /// Map the dma-buf into userspace. Returns a pointer to the mapped region.
    ///
    /// # Errors
    /// - `EBADF` if fd is invalid
    /// - `EINVAL` if len exceeds buffer size
    ///
    /// # Safety
    /// The returned pointer is valid until `munmap` is called.
    fn mmap(&self, fd: RawFd, len: usize) -> nix::Result<*mut u8>;

    /// Unmap a previously mapped dma-buf region.
    ///
    /// # Errors
    /// - `EINVAL` if addr/len are invalid
    fn munmap(&self, addr: *mut u8, len: usize) -> nix::Result<()>;

    /// Perform a `DMA_BUF_IOCTL_SYNC` with the given flags.
    ///
    /// # Errors
    /// - `EINVAL` if flags are invalid
    /// - `EBADF` if fd is invalid
    fn sync(&self, fd: RawFd, flags: u64) -> nix::Result<()>;

    /// Seek on the dma-buf fd. Used with `SEEK_END` to query buffer size.
    ///
    /// # Errors
    /// - `EINVAL` if whence/offset combination is invalid
    /// - `EBADF` if fd is invalid
    fn llseek(&self, fd: RawFd, offset: i64, whence: i32) -> nix::Result<i64>;

    /// Export fences as a `sync_file` via `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`.
    /// Populates `data.fd` with the `sync_file` descriptor on success.
    ///
    /// # Errors
    /// - `EINVAL` if flags are invalid
    /// - `EBADF` if fd is invalid
    fn export_sync_file(&self, fd: RawFd, data: &mut DmaBufExportSyncFile) -> nix::Result<()>;

    /// Import a `sync_file` into the dma-buf via `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`.
    ///
    /// # Errors
    /// - `EINVAL` if flags are invalid or `sync_file` fd is not a valid `sync_file`
    /// - `EBADF` if fd is invalid
    fn import_sync_file(&self, fd: RawFd, data: DmaBufImportSyncFile) -> nix::Result<()>;

    /// Duplicate a dma-buf file descriptor.
    ///
    /// # Errors
    /// - `EBADF` if fd is invalid
    fn dup(&self, fd: RawFd) -> nix::Result<RawFd>;

    /// Close a dma-buf file descriptor.
    ///
    /// # Errors
    /// - `EBADF` if fd is invalid
    fn close(&self, fd: RawFd) -> nix::Result<()>;
}

#[cfg(target_os = "android")]
pub mod real;

#[cfg(any(test, not(target_os = "android")))]
pub mod mock;
