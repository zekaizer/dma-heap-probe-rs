// Mock backend for host testing.
//
// Simulates dma-heap allocation and dma-buf operations using in-memory
// buffers. Validates ioctl flags and errno paths without actual kernel calls.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::sync::Mutex;

use nix::errno::Errno;

use crate::ioctl::dma_buf::{
    DMA_BUF_NAME_LEN, DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ, DMA_BUF_SYNC_VALID_FLAGS_MASK,
    DMA_BUF_SYNC_WRITE, DmaBufExportSyncFile, DmaBufImportSyncFile,
};
use crate::ioctl::dma_heap::{DMA_HEAP_VALID_FD_FLAGS, DmaHeapAllocationData};

use super::{DmaBufBackend, HeapBackend};

/// Page size used for alignment in mock allocations.
const PAGE_SIZE: u64 = 4096;

/// Maximum allocation size in mock (1 GiB).
const MAX_ALLOC_SIZE: u64 = 1024 * 1024 * 1024;

/// Starting fd number for mock (avoids collision with real OS fds).
const MOCK_FD_START: i32 = 1000;

#[derive(Debug)]
enum SyncState {
    None,
    Started { flags: u64 },
}

#[derive(Debug)]
struct BufferState {
    /// Pinned buffer data (won't move in memory).
    data: Pin<Box<[u8]>>,
    /// Page-aligned allocation size.
    alloc_size: u64,
    /// Debug name set via `SET_NAME`.
    name: Option<String>,
    /// Current sync state.
    sync_state: SyncState,
    /// Reference count (incremented by dup, decremented by close).
    ref_count: u32,
}

#[derive(Debug)]
struct MockState {
    buffers: HashMap<RawFd, BufferState>,
    heap_fds: HashSet<RawFd>,
    /// Tracks mock `sync_file` fds (from export).
    sync_file_fds: HashSet<RawFd>,
    next_fd: i32,
}

impl MockState {
    fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            heap_fds: HashSet::new(),
            sync_file_fds: HashSet::new(),
            next_fd: MOCK_FD_START,
        }
    }

    fn alloc_fd(&mut self) -> RawFd {
        let fd = self.next_fd;
        self.next_fd += 1;
        fd
    }
}

/// Mock backend that simulates dma-heap and dma-buf operations in memory.
///
/// Thread-safe via internal `Mutex`. All buffers are zero-initialized
/// (matching kernel security requirement).
#[derive(Debug)]
pub struct MockBackend {
    state: Mutex<MockState>,
}

impl MockBackend {
    /// Create a new mock backend with empty state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MockState::new()),
        }
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn page_align(size: u64) -> Option<u64> {
    size.checked_add(PAGE_SIZE - 1)
        .map(|v| v & !(PAGE_SIZE - 1))
}

impl HeapBackend for MockBackend {
    fn open(&self, name: &str) -> nix::Result<RawFd> {
        if name.is_empty() {
            return Err(Errno::ENOENT);
        }
        let mut state = self.state.lock().unwrap();
        let fd = state.alloc_fd();
        state.heap_fds.insert(fd);
        Ok(fd)
    }

    fn alloc(&self, heap_fd: RawFd, data: &mut DmaHeapAllocationData) -> nix::Result<()> {
        let mut state = self.state.lock().unwrap();

        // Validate heap fd
        if !state.heap_fds.contains(&heap_fd) {
            return Err(Errno::EBADF);
        }

        // Validate allocation parameters
        if data.len == 0 {
            return Err(Errno::EINVAL);
        }

        if data.heap_flags != 0 {
            return Err(Errno::EINVAL);
        }

        if data.fd_flags & !DMA_HEAP_VALID_FD_FLAGS != 0 {
            return Err(Errno::EINVAL);
        }

        // Check for overflow in page alignment and size limit
        let aligned_size = page_align(data.len).ok_or(Errno::EINVAL)?;
        if aligned_size > MAX_ALLOC_SIZE {
            return Err(Errno::ENOMEM);
        }

        // Allocate zero-filled buffer
        #[allow(clippy::cast_possible_truncation)]
        let buf = vec![0u8; aligned_size as usize];
        let pinned = Pin::new(buf.into_boxed_slice());

        let fd = state.alloc_fd();
        state.buffers.insert(
            fd,
            BufferState {
                data: pinned,
                alloc_size: aligned_size,
                name: None,
                sync_state: SyncState::None,
                ref_count: 1,
            },
        );

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            data.fd = fd as u32;
        }
        Ok(())
    }

    fn close_heap(&self, heap_fd: RawFd) -> nix::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.heap_fds.remove(&heap_fd) {
            Ok(())
        } else {
            Err(Errno::EBADF)
        }
    }
}

impl DmaBufBackend for MockBackend {
    fn mmap(&self, fd: RawFd, len: usize) -> nix::Result<*mut u8> {
        let state = self.state.lock().unwrap();
        let buf = state.buffers.get(&fd).ok_or(Errno::EBADF)?;

        if len as u64 > buf.alloc_size {
            return Err(Errno::EINVAL);
        }

        // Return raw pointer to the pinned buffer.
        // Safe because Pin guarantees the buffer won't move, and the buffer
        // lives as long as the fd is open.
        let ptr = buf.data.as_ptr().cast_mut();
        Ok(ptr)
    }

    fn munmap(&self, _addr: *mut u8, _len: usize) -> nix::Result<()> {
        // Mock: no-op. Real munmap is handled by nix::sys::mman::munmap.
        Ok(())
    }

    fn sync(&self, fd: RawFd, flags: u64) -> nix::Result<()> {
        let mut state = self.state.lock().unwrap();
        let buf = state.buffers.get_mut(&fd).ok_or(Errno::EBADF)?;

        // Validate flags: only valid bits allowed
        if flags & !DMA_BUF_SYNC_VALID_FLAGS_MASK != 0 {
            return Err(Errno::EINVAL);
        }

        // Must specify at least READ or WRITE
        if flags & (DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE) == 0 {
            return Err(Errno::EINVAL);
        }

        if flags & DMA_BUF_SYNC_END != 0 {
            // END
            buf.sync_state = SyncState::None;
        } else {
            // START
            buf.sync_state = SyncState::Started {
                flags: flags & (DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE),
            };
        }

        Ok(())
    }

    fn llseek(&self, fd: RawFd, offset: i64, whence: i32) -> nix::Result<i64> {
        let state = self.state.lock().unwrap();
        let buf = state.buffers.get(&fd).ok_or(Errno::EBADF)?;

        match whence {
            libc::SEEK_END => {
                if offset != 0 {
                    return Err(Errno::EINVAL);
                }
                #[allow(clippy::cast_possible_wrap)]
                Ok(buf.alloc_size as i64)
            }
            libc::SEEK_SET => {
                if offset != 0 {
                    return Err(Errno::EINVAL);
                }
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    fn set_name(&self, fd: RawFd, name: &str) -> nix::Result<()> {
        let mut state = self.state.lock().unwrap();
        let buf = state.buffers.get_mut(&fd).ok_or(Errno::EBADF)?;

        if name.len() > DMA_BUF_NAME_LEN {
            return Err(Errno::ENAMETOOLONG);
        }

        buf.name = Some(name.to_owned());
        Ok(())
    }

    fn export_sync_file(&self, fd: RawFd, data: &mut DmaBufExportSyncFile) -> nix::Result<()> {
        let mut state = self.state.lock().unwrap();

        if !state.buffers.contains_key(&fd) {
            return Err(Errno::EBADF);
        }

        // Flags must have at least READ or WRITE
        let flags_u64 = u64::from(data.flags);
        if flags_u64 & (DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE) == 0 {
            return Err(Errno::EINVAL);
        }

        // Return a mock sync_file fd
        let sync_fd = state.alloc_fd();
        state.sync_file_fds.insert(sync_fd);
        data.fd = sync_fd;
        Ok(())
    }

    fn import_sync_file(&self, fd: RawFd, data: DmaBufImportSyncFile) -> nix::Result<()> {
        let state = self.state.lock().unwrap();

        if !state.buffers.contains_key(&fd) {
            return Err(Errno::EBADF);
        }

        // Flags must have at least READ or WRITE
        let flags_u64 = u64::from(data.flags);
        if flags_u64 & (DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE) == 0 {
            return Err(Errno::EINVAL);
        }

        // Validate the sync_file fd
        if !state.sync_file_fds.contains(&data.fd) {
            return Err(Errno::EINVAL);
        }

        Ok(())
    }

    fn dup(&self, fd: RawFd) -> nix::Result<RawFd> {
        let mut state = self.state.lock().unwrap();
        let buf = state.buffers.get_mut(&fd).ok_or(Errno::EBADF)?;
        buf.ref_count += 1;

        // Return a new fd that maps to the same buffer.
        // For simplicity, we increment ref_count and return a new fd number,
        // but we need to create a new entry pointing to the same data.
        // Instead, we use the ref_count on the original and track the alias.
        buf.ref_count -= 1; // undo, we'll use a different approach

        // Clone the buffer entry with a new fd (shared ref semantics via separate entries).
        let new_fd = state.alloc_fd();
        let original = state.buffers.get(&fd).ok_or(Errno::EBADF)?;

        // For mock purposes, create an independent copy of buffer metadata
        // but share the same data pointer (Pin guarantees no move).
        // Since we can't share Pin<Box<[u8]>>, we create a new buffer with the
        // same size. The dup semantics in mock are simplified: the dup'd fd
        // gets its own buffer copy for data isolation in tests.
        let size = original.alloc_size;
        let data_copy = original.data.to_vec();
        let name = original.name.clone();

        state.buffers.insert(
            new_fd,
            BufferState {
                data: Pin::new(data_copy.into_boxed_slice()),
                alloc_size: size,
                name,
                sync_state: SyncState::None,
                ref_count: 1,
            },
        );

        Ok(new_fd)
    }

    fn close(&self, fd: RawFd) -> nix::Result<()> {
        let mut state = self.state.lock().unwrap();

        // Check buffers first, then sync_file fds
        if state.buffers.remove(&fd).is_some() {
            return Ok(());
        }
        if state.sync_file_fds.remove(&fd) {
            return Ok(());
        }

        Err(Errno::EBADF)
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::ioctl::dma_buf::{DMA_BUF_SYNC_RW, DMA_BUF_SYNC_START};

    fn setup() -> MockBackend {
        MockBackend::new()
    }

    #[allow(clippy::cast_possible_truncation)]
    fn open_and_alloc(backend: &MockBackend, size: u64) -> (RawFd, RawFd) {
        let heap_fd = backend.open("system").unwrap();
        let mut data = DmaHeapAllocationData {
            len: size,
            fd_flags: libc::O_CLOEXEC as u32,
            ..Default::default()
        };
        backend.alloc(heap_fd, &mut data).unwrap();
        #[allow(clippy::cast_possible_wrap)]
        let buf_fd = data.fd as i32;
        (heap_fd, buf_fd)
    }

    // ── Heap alloc tests ──

    #[test]
    fn alloc_basic() {
        let b = setup();
        let (heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        assert!(buf_fd >= MOCK_FD_START);
        b.close(buf_fd).unwrap();
        b.close_heap(heap_fd).unwrap();
    }

    #[test]
    fn alloc_zeroed() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        let ptr = b.mmap(buf_fd, 4096).unwrap();
        let slice = unsafe { std::slice::from_raw_parts(ptr, 4096) };
        assert!(slice.iter().all(|&byte| byte == 0));
    }

    #[test]
    fn alloc_zero_size() {
        let b = setup();
        let heap_fd = b.open("system").unwrap();
        let mut data = DmaHeapAllocationData {
            len: 0,
            fd_flags: libc::O_CLOEXEC as u32,
            ..Default::default()
        };
        assert_eq!(b.alloc(heap_fd, &mut data), Err(Errno::EINVAL));
    }

    #[test]
    fn alloc_invalid_fd_flags() {
        let b = setup();
        let heap_fd = b.open("system").unwrap();
        let mut data = DmaHeapAllocationData {
            len: 4096,
            fd_flags: libc::O_APPEND as u32,
            ..Default::default()
        };
        assert_eq!(b.alloc(heap_fd, &mut data), Err(Errno::EINVAL));
    }

    #[test]
    fn alloc_invalid_heap_flags() {
        let b = setup();
        let heap_fd = b.open("system").unwrap();
        let mut data = DmaHeapAllocationData {
            len: 4096,
            fd_flags: libc::O_CLOEXEC as u32,
            heap_flags: 1,
            ..Default::default()
        };
        assert_eq!(b.alloc(heap_fd, &mut data), Err(Errno::EINVAL));
    }

    #[test]
    fn alloc_on_closed_heap() {
        let b = setup();
        let heap_fd = b.open("system").unwrap();
        b.close_heap(heap_fd).unwrap();
        let mut data = DmaHeapAllocationData {
            len: 4096,
            fd_flags: libc::O_CLOEXEC as u32,
            ..Default::default()
        };
        assert_eq!(b.alloc(heap_fd, &mut data), Err(Errno::EBADF));
    }

    #[test]
    fn alloc_page_aligns() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 1); // 1 byte -> aligned to 4096
        let size = b.llseek(buf_fd, 0, libc::SEEK_END).unwrap();
        assert_eq!(size, 4096);
    }

    #[test]
    fn alloc_enomem_huge_size() {
        let b = setup();
        let heap_fd = b.open("system").unwrap();
        let mut data = DmaHeapAllocationData {
            len: MAX_ALLOC_SIZE + 1,
            fd_flags: libc::O_CLOEXEC as u32,
            ..Default::default()
        };
        assert_eq!(b.alloc(heap_fd, &mut data), Err(Errno::ENOMEM));
    }

    // ── Sync tests ──

    #[test]
    fn sync_valid_flags() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);

        // START + READ
        b.sync(buf_fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ)
            .unwrap();
        b.sync(buf_fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ)
            .unwrap();

        // START + WRITE
        b.sync(buf_fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_WRITE)
            .unwrap();
        b.sync(buf_fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_WRITE)
            .unwrap();

        // START + RW
        b.sync(buf_fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_RW)
            .unwrap();
        b.sync(buf_fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_RW).unwrap();
    }

    #[test]
    fn sync_invalid_flags() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);

        // No READ/WRITE bit
        assert_eq!(b.sync(buf_fd, DMA_BUF_SYNC_START), Err(Errno::EINVAL));

        // Invalid bits set (bit 3 = 0x8)
        assert_eq!(b.sync(buf_fd, 0x8 | DMA_BUF_SYNC_READ), Err(Errno::EINVAL));

        // Zero flags
        assert_eq!(b.sync(buf_fd, 0), Err(Errno::EINVAL));
    }

    #[test]
    fn sync_on_bad_fd() {
        let b = setup();
        assert_eq!(
            b.sync(9999, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ),
            Err(Errno::EBADF)
        );
    }

    // ── mmap tests ──

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn mmap_and_access() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);

        let ptr = b.mmap(buf_fd, 4096).unwrap();
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, 4096) };

        // Write pattern
        for (i, byte) in slice.iter_mut().enumerate() {
            *byte = (i & 0xFF) as u8;
        }

        // Read back
        let read_slice = unsafe { std::slice::from_raw_parts(ptr, 4096) };
        for (i, &byte) in read_slice.iter().enumerate() {
            assert_eq!(byte, (i & 0xFF) as u8);
        }
    }

    #[test]
    fn mmap_beyond_size() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        assert_eq!(b.mmap(buf_fd, 8192), Err(Errno::EINVAL));
    }

    // ── llseek tests ──

    #[test]
    fn llseek_size() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 65536);
        let size = b.llseek(buf_fd, 0, libc::SEEK_END).unwrap();
        assert_eq!(size, 65536);

        // SEEK_SET resets to 0
        let pos = b.llseek(buf_fd, 0, libc::SEEK_SET).unwrap();
        assert_eq!(pos, 0);
    }

    #[test]
    fn llseek_invalid_whence() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        assert_eq!(b.llseek(buf_fd, 0, libc::SEEK_CUR), Err(Errno::EINVAL));
    }

    #[test]
    fn llseek_nonzero_offset() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        assert_eq!(b.llseek(buf_fd, 1, libc::SEEK_SET), Err(Errno::EINVAL));
        assert_eq!(b.llseek(buf_fd, 1, libc::SEEK_END), Err(Errno::EINVAL));
    }

    // ── set_name tests ──

    #[test]
    fn set_name_valid() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        b.set_name(buf_fd, "test_buffer").unwrap();
    }

    #[test]
    fn set_name_max_length() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        let name = "a".repeat(DMA_BUF_NAME_LEN);
        b.set_name(buf_fd, &name).unwrap();
    }

    #[test]
    fn set_name_too_long() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        let name = "a".repeat(DMA_BUF_NAME_LEN + 1);
        assert_eq!(b.set_name(buf_fd, &name), Err(Errno::ENAMETOOLONG));
    }

    // ── close / lifecycle tests ──

    #[test]
    fn close_then_access() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        b.close(buf_fd).unwrap();
        assert_eq!(b.mmap(buf_fd, 4096), Err(Errno::EBADF));
        assert_eq!(
            b.sync(buf_fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ),
            Err(Errno::EBADF)
        );
        assert_eq!(b.llseek(buf_fd, 0, libc::SEEK_END), Err(Errno::EBADF));
    }

    #[test]
    fn close_bad_fd() {
        let b = setup();
        assert_eq!(b.close(9999), Err(Errno::EBADF));
    }

    #[test]
    fn dup_and_close() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        let dup_fd = b.dup(buf_fd).unwrap();
        assert_ne!(buf_fd, dup_fd);

        // Close original, dup should still work
        b.close(buf_fd).unwrap();
        let size = b.llseek(dup_fd, 0, libc::SEEK_END).unwrap();
        assert_eq!(size, 4096);

        b.close(dup_fd).unwrap();
    }

    // ── export/import sync_file tests ──

    #[test]
    fn export_sync_file() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        let mut data = DmaBufExportSyncFile {
            flags: DMA_BUF_SYNC_READ as u32,
            fd: -1,
        };
        b.export_sync_file(buf_fd, &mut data).unwrap();
        assert!(data.fd >= MOCK_FD_START);
        b.close(data.fd).unwrap();
    }

    #[test]
    fn export_sync_file_invalid_flags() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        let mut data = DmaBufExportSyncFile { flags: 0, fd: -1 };
        assert_eq!(b.export_sync_file(buf_fd, &mut data), Err(Errno::EINVAL));
    }

    #[test]
    fn import_sync_file() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);

        // First export to get a valid sync_file fd
        let mut export_data = DmaBufExportSyncFile {
            flags: DMA_BUF_SYNC_WRITE as u32,
            fd: -1,
        };
        b.export_sync_file(buf_fd, &mut export_data).unwrap();

        // Import it back
        let import_data = DmaBufImportSyncFile {
            flags: DMA_BUF_SYNC_WRITE as u32,
            fd: export_data.fd,
        };
        b.import_sync_file(buf_fd, import_data).unwrap();
    }

    #[test]
    fn import_sync_file_invalid_flags() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);

        let mut export_data = DmaBufExportSyncFile {
            flags: DMA_BUF_SYNC_READ as u32,
            fd: -1,
        };
        b.export_sync_file(buf_fd, &mut export_data).unwrap();

        let import_data = DmaBufImportSyncFile {
            flags: 0,
            fd: export_data.fd,
        };
        assert_eq!(b.import_sync_file(buf_fd, import_data), Err(Errno::EINVAL));
    }

    #[test]
    fn import_sync_file_bad_sync_fd() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        let import_data = DmaBufImportSyncFile {
            flags: DMA_BUF_SYNC_READ as u32,
            fd: 9999,
        };
        assert_eq!(b.import_sync_file(buf_fd, import_data), Err(Errno::EINVAL));
    }

    #[test]
    fn open_empty_name() {
        let b = setup();
        assert_eq!(b.open(""), Err(Errno::ENOENT));
    }

    #[test]
    fn close_heap_bad_fd() {
        let b = setup();
        assert_eq!(b.close_heap(9999), Err(Errno::EBADF));
    }
}
