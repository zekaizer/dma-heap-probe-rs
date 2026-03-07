// Real backend implementation for Android targets.
//
// Wraps actual ioctl/mmap/close syscalls via the nix crate.
// Only compiled when target_os = "android".

use std::ffi::CString;
use std::num::NonZeroUsize;
use std::os::unix::io::RawFd;

use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::mman::{self, MapFlags, ProtFlags};
use nix::sys::stat::Mode;
use nix::unistd;

use crate::ioctl::dma_buf::{
    self, DMA_BUF_NAME_LEN, DmaBufExportSyncFile, DmaBufImportSyncFile, DmaBufSync,
};
use crate::ioctl::dma_heap::{self, DmaHeapAllocationData};

use super::{DmaBufBackend, HeapBackend};

/// Real heap backend using `/dev/dma_heap/` device nodes.
pub struct RealHeapBackend;

/// Real dma-buf backend using ioctl/mmap syscalls.
pub struct RealDmaBufBackend;

impl HeapBackend for RealHeapBackend {
    fn open(&self, name: &str) -> nix::Result<RawFd> {
        let path = format!("/dev/dma_heap/{name}");
        let cpath = CString::new(path).map_err(|_| Errno::EINVAL)?;
        nix::fcntl::open(
            cpath.as_c_str(),
            OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
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

impl DmaBufBackend for RealDmaBufBackend {
    fn mmap(&self, fd: RawFd, len: usize) -> nix::Result<*mut u8> {
        let len = NonZeroUsize::new(len).ok_or(Errno::EINVAL)?;
        // SAFETY: fd is a valid dma-buf fd, mapping is shared read/write.
        let ptr = unsafe {
            mman::mmap(
                None,
                len,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                Some(fd),
                0,
            )?
        };
        Ok(ptr.cast::<u8>())
    }

    fn munmap(&self, addr: *mut u8, len: usize) -> nix::Result<()> {
        let len = NonZeroUsize::new(len).ok_or(Errno::EINVAL)?;
        // SAFETY: addr was returned by a previous mmap call with the given len.
        unsafe { mman::munmap(std::ptr::NonNull::new(addr).ok_or(Errno::EINVAL)?, len) }
    }

    fn sync(&self, fd: RawFd, flags: u64) -> nix::Result<()> {
        let sync_data = DmaBufSync { flags };
        // SAFETY: fd is a valid dma-buf fd, sync_data has valid flags.
        unsafe { dma_buf::dma_buf_ioctl_sync(fd, &sync_data) }?;
        Ok(())
    }

    fn llseek(&self, fd: RawFd, offset: i64, whence: i32) -> nix::Result<i64> {
        let w = match whence {
            libc::SEEK_SET => unistd::Whence::SeekSet,
            libc::SEEK_END => unistd::Whence::SeekEnd,
            _ => return Err(Errno::EINVAL),
        };
        unistd::lseek(fd, offset, w).map(Into::into)
    }

    fn set_name(&self, fd: RawFd, name: &str) -> nix::Result<()> {
        if name.len() > DMA_BUF_NAME_LEN {
            return Err(Errno::ENAMETOOLONG);
        }
        let cname = CString::new(name).map_err(|_| Errno::EINVAL)?;
        // SAFETY: fd is a valid dma-buf fd, cname is a valid C string.
        unsafe { dma_buf::dma_buf_set_name(fd, cname.as_ptr()) }?;
        Ok(())
    }

    fn export_sync_file(&self, fd: RawFd, data: &mut DmaBufExportSyncFile) -> nix::Result<()> {
        // SAFETY: fd is a valid dma-buf fd, data is properly initialized.
        unsafe { dma_buf::dma_buf_ioctl_export_sync_file(fd, data) }?;
        Ok(())
    }

    fn import_sync_file(&self, fd: RawFd, data: DmaBufImportSyncFile) -> nix::Result<()> {
        let mut data = data;
        // SAFETY: fd is a valid dma-buf fd, data contains a valid sync_file fd.
        unsafe { dma_buf::dma_buf_ioctl_import_sync_file(fd, &mut data) }?;
        Ok(())
    }

    fn dup(&self, fd: RawFd) -> nix::Result<RawFd> {
        unistd::dup(fd)
    }

    fn close(&self, fd: RawFd) -> nix::Result<()> {
        unistd::close(fd)
    }
}
