// Mock backend for host testing.
//
// Simulates dma-heap allocation and dma-buf operations using in-memory
// buffers. Validates ioctl flags and errno paths without actual kernel calls.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};

use nix::errno::Errno;

use crate::ioctl::dma_buf::{
    DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ, DMA_BUF_SYNC_VALID_FLAGS_MASK, DMA_BUF_SYNC_WRITE,
    DmaBufExportSyncFile, DmaBufImportSyncFile,
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
    /// Shared buffer data (allows zero-copy dup).
    data: Arc<[u8]>,
    /// Current sync state.
    sync_state: SyncState,
}

/// Simulation configuration for injecting faults into mock operations.
///
/// All fields default to disabled (no fault injection).
#[derive(Debug, Clone, Default)]
pub struct SimConfig {
    /// Max active buffers before alloc returns `ENOMEM`.
    /// `None` = unlimited (default behavior).
    pub enomem_threshold: Option<usize>,

    /// Every Nth alloc returns `EIO` (non-ENOMEM error).
    /// `0` = disabled.
    pub fail_every_nth: u64,

    /// Every Nth mmap, flip the first byte to simulate data corruption.
    /// `0` = disabled. Useful for testing `WriteReadVerify` error detection.
    pub corrupt_every_nth: u64,
}

#[derive(Debug)]
struct MockState {
    buffers: HashMap<RawFd, BufferState>,
    heap_fds: HashSet<RawFd>,
    /// Tracks mock `sync_file` fds (from export).
    sync_file_fds: HashSet<RawFd>,
    next_fd: i32,
    /// Simulation configuration for fault injection.
    sim: Option<SimConfig>,
    /// Total alloc calls (for `fail_every_nth`).
    alloc_count: u64,
    /// Total mmap calls (for `corrupt_every_nth`).
    mmap_count: u64,
}

impl MockState {
    fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            heap_fds: HashSet::new(),
            sync_file_fds: HashSet::new(),
            next_fd: MOCK_FD_START,
            sim: None,
            alloc_count: 0,
            mmap_count: 0,
        }
    }

    fn alloc_fd(&mut self) -> RawFd {
        let fd = self.next_fd;
        self.next_fd += 1;
        fd
    }
}

/// Validate sync direction flags (used by sync, export, import).
/// Returns `Err(EINVAL)` if no READ/WRITE bit is set.
fn validate_sync_direction(flags: u64) -> nix::Result<()> {
    if flags & (DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE) == 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
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

    /// Create a mock backend with simulation/fault injection config.
    #[must_use]
    pub fn with_sim(sim: SimConfig) -> Self {
        let mut mock_state = MockState::new();
        mock_state.sim = Some(sim);
        Self {
            state: Mutex::new(mock_state),
        }
    }

    /// Return the number of active buffer file descriptors.
    ///
    /// Test-only utility for leak detection in repeated alloc/close loops.
    #[must_use]
    pub fn buffer_count(&self) -> usize {
        self.state.lock().unwrap().buffers.len()
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn page_align(size: u64) -> Option<u64> {
    size.checked_next_multiple_of(PAGE_SIZE)
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

        // Simulation: fault injection
        if let Some(sim) = state.sim.clone() {
            state.alloc_count += 1;
            if sim.fail_every_nth > 0 && state.alloc_count.is_multiple_of(sim.fail_every_nth) {
                return Err(Errno::EIO);
            }
            if let Some(threshold) = sim.enomem_threshold
                && state.buffers.len() >= threshold
            {
                return Err(Errno::ENOMEM);
            }
        }

        // Allocate zero-filled buffer
        #[allow(clippy::cast_possible_truncation)]
        let buf: Arc<[u8]> = vec![0u8; aligned_size as usize].into();

        let fd = state.alloc_fd();
        state.buffers.insert(
            fd,
            BufferState {
                data: buf,
                sync_state: SyncState::None,
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
        let mut state = self.state.lock().unwrap();

        // Extract corruption config before borrowing buffers.
        let corrupt_nth = state.sim.as_ref().map_or(0, |s| s.corrupt_every_nth);

        let buf = state.buffers.get(&fd).ok_or(Errno::EBADF)?;

        if len > buf.data.len() {
            return Err(Errno::EINVAL);
        }

        // Return raw pointer to the Arc buffer.
        // Safe for mock: the Arc keeps data alive as long as any fd references it,
        // and buffer data is immovable once allocated.
        let ptr = buf.data.as_ptr().cast_mut();

        // Simulation: data corruption injection
        if corrupt_nth > 0 {
            state.mmap_count += 1;
            if state.mmap_count.is_multiple_of(corrupt_nth) && len > 0 {
                // SAFETY: ptr is valid for len bytes and we only flip byte 0.
                unsafe {
                    *ptr ^= 0xFF;
                }
            }
        }

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
        validate_sync_direction(flags)?;

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
                Ok(buf.data.len() as i64)
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

    fn export_sync_file(&self, fd: RawFd, data: &mut DmaBufExportSyncFile) -> nix::Result<()> {
        let mut state = self.state.lock().unwrap();

        if !state.buffers.contains_key(&fd) {
            return Err(Errno::EBADF);
        }

        // Flags must have at least READ or WRITE
        validate_sync_direction(u64::from(data.flags))?;

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
        validate_sync_direction(u64::from(data.flags))?;

        // Validate the sync_file fd
        if !state.sync_file_fds.contains(&data.fd) {
            return Err(Errno::EINVAL);
        }

        Ok(())
    }

    fn dup(&self, fd: RawFd) -> nix::Result<RawFd> {
        let mut state = self.state.lock().unwrap();

        let original = state.buffers.get(&fd).ok_or(Errno::EBADF)?;
        let new_buf = BufferState {
            data: Arc::clone(&original.data),
            sync_state: SyncState::None,
        };

        let new_fd = state.alloc_fd();
        state.buffers.insert(new_fd, new_buf);
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
    use crate::ioctl::dma_heap::DMA_HEAP_ALLOC_FD_FLAGS;

    fn setup() -> MockBackend {
        MockBackend::new()
    }

    #[allow(clippy::cast_possible_truncation)]
    fn open_and_alloc(backend: &MockBackend, size: u64) -> (RawFd, RawFd) {
        let heap_fd = backend.open("system").unwrap();
        let mut data = DmaHeapAllocationData {
            len: size,
            fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
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
            fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
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
            fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
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
            fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
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
            fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
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

    #[test]
    fn dup_shares_data() {
        let b = setup();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);

        // Write through original
        let ptr = b.mmap(buf_fd, 4096).unwrap();
        unsafe { *ptr = 0xAB };

        // Dup and verify shared data
        let dup_fd = b.dup(buf_fd).unwrap();
        let dup_ptr = b.mmap(dup_fd, 4096).unwrap();
        assert_eq!(unsafe { *dup_ptr }, 0xAB);
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

    // ── SimConfig fault injection tests ──

    #[test]
    fn sim_enomem_triggers() {
        let b = MockBackend::with_sim(SimConfig {
            enomem_threshold: Some(3),
            ..Default::default()
        });
        let heap_fd = b.open("system").unwrap();
        // First 3 allocs succeed (threshold=3 means fail when buffers.len() >= 3)
        for _ in 0..3 {
            let mut data = DmaHeapAllocationData {
                len: 4096,
                fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
                ..Default::default()
            };
            b.alloc(heap_fd, &mut data).unwrap();
        }
        // 4th alloc should fail with ENOMEM
        let mut data = DmaHeapAllocationData {
            len: 4096,
            fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
            ..Default::default()
        };
        assert_eq!(b.alloc(heap_fd, &mut data), Err(Errno::ENOMEM));
    }

    #[test]
    fn sim_fail_every_nth() {
        let b = MockBackend::with_sim(SimConfig {
            fail_every_nth: 3,
            ..Default::default()
        });
        let heap_fd = b.open("system").unwrap();
        let mut results = Vec::new();
        for _ in 0..6 {
            let mut data = DmaHeapAllocationData {
                len: 4096,
                fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
                ..Default::default()
            };
            results.push(b.alloc(heap_fd, &mut data));
        }
        // Every 3rd alloc (index 2, 5) returns EIO
        assert!(results[0].is_ok());
        assert!(results[1].is_ok());
        assert_eq!(results[2], Err(Errno::EIO));
        assert!(results[3].is_ok());
        assert!(results[4].is_ok());
        assert_eq!(results[5], Err(Errno::EIO));
    }

    #[test]
    fn sim_corruption_detected() {
        let b = MockBackend::with_sim(SimConfig {
            corrupt_every_nth: 1,
            ..Default::default()
        });
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        // Buffer starts zeroed, mmap with corrupt_every_nth=1 flips byte 0
        let ptr = b.mmap(buf_fd, 4096).unwrap();
        let first_byte = unsafe { *ptr };
        assert_eq!(first_byte, 0xFF, "first byte should be flipped from 0x00");
    }

    #[test]
    fn sim_no_corruption_without_config() {
        let b = MockBackend::new();
        let (_heap_fd, buf_fd) = open_and_alloc(&b, 4096);
        let ptr = b.mmap(buf_fd, 4096).unwrap();
        let first_byte = unsafe { *ptr };
        assert_eq!(first_byte, 0x00, "no corruption without SimConfig");
    }

    #[test]
    fn sim_enomem_recovers_after_free() {
        let b = MockBackend::with_sim(SimConfig {
            enomem_threshold: Some(2),
            ..Default::default()
        });
        let heap_fd = b.open("system").unwrap();
        // Alloc 2 buffers
        let mut fds = Vec::new();
        for _ in 0..2 {
            let mut data = DmaHeapAllocationData {
                len: 4096,
                fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
                ..Default::default()
            };
            b.alloc(heap_fd, &mut data).unwrap();
            #[allow(clippy::cast_possible_wrap)]
            fds.push(data.fd as i32);
        }
        // Next alloc fails
        let mut data = DmaHeapAllocationData {
            len: 4096,
            fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
            ..Default::default()
        };
        assert_eq!(b.alloc(heap_fd, &mut data), Err(Errno::ENOMEM));
        // Free one buffer
        b.close(fds[0]).unwrap();
        // Now alloc should succeed again
        let mut data2 = DmaHeapAllocationData {
            len: 4096,
            fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
            ..Default::default()
        };
        b.alloc(heap_fd, &mut data2).unwrap();
    }
}
