// Real backend implementation for Android targets.
//
// Wraps actual ioctl/mmap/close syscalls via the nix crate.
// Only compiled when target_os = "android".

use std::ffi::CString;
use std::num::NonZeroUsize;
use std::os::fd::{BorrowedFd, IntoRawFd};
use std::os::unix::io::RawFd;

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::mman::{self, MapFlags, ProtFlags};
use nix::sys::stat::Mode;
use nix::unistd;

use crate::ioctl::dma_buf::{self, DmaBufExportSyncFile, DmaBufImportSyncFile, DmaBufSync};
use crate::ioctl::dma_heap::{self, DmaHeapAllocationData};

use super::{DmaBufBackend, HeapBackend};

/// Real backend using `/dev/dma_heap/` device nodes and ioctl/mmap syscalls.
pub struct RealBackend;

impl RealBackend {
    /// Create a new real backend instance.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl HeapBackend for RealBackend {
    fn open(&self, name: &str) -> nix::Result<RawFd> {
        let path = format!("/dev/dma_heap/{name}");
        let cpath = CString::new(path).map_err(|_| Errno::EINVAL)?;
        let owned_fd = nix::fcntl::open(
            cpath.as_c_str(),
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        Ok(owned_fd.into_raw_fd())
    }

    fn alloc(&self, heap_fd: RawFd, data: &mut DmaHeapAllocationData) -> nix::Result<()> {
        // SAFETY: heap_fd is a valid dma-heap device fd, data is properly initialized.
        unsafe { dma_heap::dma_heap_ioctl_alloc(heap_fd, data) }?;
        Ok(())
    }

    fn close_heap(&self, heap_fd: RawFd) -> nix::Result<()> {
        unistd::close(heap_fd)
    }
}

impl DmaBufBackend for RealBackend {
    fn mmap(&self, fd: RawFd, len: usize) -> nix::Result<*mut u8> {
        let len = NonZeroUsize::new(len).ok_or(Errno::EINVAL)?;
        // SAFETY: fd is a valid dma-buf fd, mapping is shared read/write.
        let ptr = unsafe {
            let borrowed = BorrowedFd::borrow_raw(fd);
            mman::mmap(
                None,
                len,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                borrowed,
                0,
            )?
        };
        Ok(ptr.as_ptr().cast::<u8>())
    }

    fn munmap(&self, addr: *mut u8, len: usize) -> nix::Result<()> {
        let nn = std::ptr::NonNull::new(addr.cast::<std::ffi::c_void>()).ok_or(Errno::EINVAL)?;
        // SAFETY: addr was returned by a previous mmap call with the given len.
        unsafe { mman::munmap(nn, len) }
    }

    fn sync(&self, fd: RawFd, flags: u64) -> nix::Result<()> {
        let sync_data = DmaBufSync { flags };
        // SAFETY: fd is a valid dma-buf fd, sync_data has valid flags.
        unsafe { dma_buf::dma_buf_ioctl_sync(fd, &raw const sync_data) }?;
        Ok(())
    }

    fn llseek(&self, fd: RawFd, offset: i64, whence: i32) -> nix::Result<i64> {
        let w = match whence {
            libc::SEEK_SET => unistd::Whence::SeekSet,
            libc::SEEK_END => unistd::Whence::SeekEnd,
            _ => return Err(Errno::EINVAL),
        };
        // SAFETY: fd is a valid dma-buf fd.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        unistd::lseek(borrowed, offset, w)
    }

    fn export_sync_file(&self, fd: RawFd, data: &mut DmaBufExportSyncFile) -> nix::Result<()> {
        // SAFETY: fd is a valid dma-buf fd, data is properly initialized.
        unsafe { dma_buf::dma_buf_ioctl_export_sync_file(fd, data) }?;
        Ok(())
    }

    fn import_sync_file(&self, fd: RawFd, data: DmaBufImportSyncFile) -> nix::Result<()> {
        // SAFETY: fd is a valid dma-buf fd, data contains a valid sync_file fd.
        unsafe { dma_buf::dma_buf_ioctl_import_sync_file(fd, &raw const data) }?;
        Ok(())
    }

    fn dup(&self, fd: RawFd) -> nix::Result<RawFd> {
        // SAFETY: fd is a valid dma-buf fd.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let owned = unistd::dup(borrowed)?;
        Ok(owned.into_raw_fd())
    }

    fn close(&self, fd: RawFd) -> nix::Result<()> {
        unistd::close(fd)
    }
}
