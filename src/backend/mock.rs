// Mock backend for host testing.
//
// Simulates dma-heap allocation and dma-buf operations using in-memory
// buffers. Validates ioctl flags and errno paths without actual kernel calls.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};

use nix::errno::Errno;
use rand::Rng;

use crate::ioctl::dma_buf::{
    DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ, DMA_BUF_SYNC_VALID_FLAGS_MASK, DMA_BUF_SYNC_WRITE,
    DmaBufExportSyncFile, DmaBufImportSyncFile,
};
use crate::ioctl::dma_heap::{DMA_HEAP_VALID_FD_FLAGS, DmaHeapAllocationData};

use crate::ioctl::dmabuf_container::MAX_BUFCON_SRC_BUFS;

use super::{ContainerBackend, DmaBufBackend, HeapBackend};

/// Page size used for alignment in mock allocations.
const PAGE_SIZE: u64 = 4096;

/// Maximum allocation size in mock (1 GiB).
const MAX_ALLOC_SIZE: u64 = 1024 * 1024 * 1024;

/// Starting fd number for mock (avoids collision with real OS fds).
const MOCK_FD_START: i32 = 1000;

/// Anonymous mmap-backed buffer (demand-paged, zero-filled by OS).
///
/// Uses `libc::mmap(MAP_ANONYMOUS)` so physical pages are only allocated
/// on first access, avoiding the malloc+zero-fill overhead of `Vec<u8>`.
struct MmapBacking {
    ptr: *mut u8,
    len: usize,
}

impl MmapBacking {
    fn new(len: usize) -> nix::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(Errno::ENOMEM);
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
        })
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for MmapBacking {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

impl std::fmt::Debug for MmapBacking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MmapBacking")
            .field("ptr", &self.ptr)
            .field("len", &self.len)
            .finish()
    }
}

// SAFETY: The mmap'd region is process-wide and the pointer is stable.
unsafe impl Send for MmapBacking {}
unsafe impl Sync for MmapBacking {}

#[derive(Debug)]
enum SyncState {
    None,
    Started { flags: u64 },
}

#[derive(Debug)]
struct BufferState {
    /// Shared buffer data (allows zero-copy dup).
    data: Arc<MmapBacking>,
    /// Current sync state (shared across dup'd fds).
    sync_state: Arc<Mutex<SyncState>>,
    /// Heap name that allocated this buffer (for per-heap behavior).
    heap_name: Option<String>,
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

    /// Simulated latency per 4K page (nanoseconds). `0` = disabled.
    pub latency_ns_per_4k: u64,
}

/// Per-heap simulation profile for multi-heap mock testing.
///
/// Controls latency characteristics, cache behavior, and CPU access
/// restrictions for individual heaps in strict (multi-heap) mode.
#[derive(Debug, Clone)]
pub struct HeapProfile {
    /// Latency multiplier relative to base (1.0 = normal, >1.0 = slower).
    pub latency_multiplier: f64,
    /// Whether this heap uses cached memory (affects sync latency ratio).
    pub cached: bool,
    /// Whether CPU access (mmap) is allowed on buffers from this heap.
    /// `false` = mmap returns `EACCES`, sync is no-op (Samsung secure heap behavior).
    pub cpu_access: bool,
    /// Per-heap fault injection config (overrides global `SimConfig`).
    pub sim_override: Option<SimConfig>,
}

impl Default for HeapProfile {
    fn default() -> Self {
        Self {
            latency_multiplier: 1.0,
            cached: true,
            cpu_access: true,
            sim_override: None,
        }
    }
}

impl HeapProfile {
    /// Profile for `system` heap (CMA-backed, cached).
    #[must_use]
    pub fn system() -> Self {
        Self::default()
    }

    /// Profile for `system-uncached` heap (write-combine, uncached).
    /// Higher alloc latency (~1.8x), lower sync cost (no cache flush).
    #[must_use]
    pub fn system_uncached() -> Self {
        Self {
            latency_multiplier: 1.8,
            cached: false,
            cpu_access: true,
            sim_override: None,
        }
    }

    /// Profile for `restricted` (secure) heap.
    /// Alloc succeeds but mmap returns `EACCES`, sync is no-op.
    /// Matches Samsung kernel `DMA_HEAP_FLAG_PROTECTED` behavior.
    #[must_use]
    pub fn restricted() -> Self {
        Self {
            latency_multiplier: 1.0,
            cached: false,
            cpu_access: false,
            sim_override: None,
        }
    }
}

/// State for a container fd (created by MERGE).
#[derive(Debug, Clone)]
struct ContainerState {
    /// Buffer fds contained in this container.
    buffer_fds: Vec<RawFd>,
    /// Active buffer mask (0 = all unmasked on creation).
    mask: u64,
}

#[derive(Debug)]
struct MockState {
    buffers: HashMap<RawFd, BufferState>,
    heap_fds: HashMap<RawFd, String>,
    /// Tracks mock `sync_file` fds (from export).
    sync_file_fds: HashSet<RawFd>,
    /// Container fds created by MERGE.
    container_fds: HashMap<RawFd, ContainerState>,
    /// Container device fds (`/dev/dmabuf_container`).
    container_device_fds: HashSet<RawFd>,
    next_fd: i32,
    /// Simulation configuration for fault injection.
    sim: Option<SimConfig>,
    /// Per-heap configs. `None` = permissive mode (any name accepted).
    /// `Some` = strict mode (only registered heaps accepted).
    heap_configs: Option<HashMap<String, HeapProfile>>,
    /// Per-heap alloc counts (for per-heap `fail_every_nth`).
    heap_alloc_counts: HashMap<String, u64>,
    /// Total alloc calls (for `fail_every_nth` in permissive mode).
    alloc_count: u64,
    /// Total mmap calls (for `corrupt_every_nth`).
    mmap_count: u64,
}

impl MockState {
    fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            heap_fds: HashMap::new(),
            sync_file_fds: HashSet::new(),
            container_fds: HashMap::new(),
            container_device_fds: HashSet::new(),
            next_fd: MOCK_FD_START,
            sim: None,
            heap_configs: None,
            heap_alloc_counts: HashMap::new(),
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

    /// Create a mock backend with realistic latency simulation.
    ///
    /// Uses 1000ns per 4K page with size-proportional delays on alloc, mmap,
    /// sync, and close. Suitable for production use on non-Android hosts.
    #[must_use]
    pub fn new_realistic() -> Self {
        Self::with_sim(SimConfig {
            latency_ns_per_4k: 1000,
            ..Default::default()
        })
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

    /// Multi-heap mock with default profiles (system + system-uncached + restricted).
    /// Strict mode: only registered heaps are accepted by `open()`.
    #[must_use]
    pub fn new_multi_heap() -> Self {
        let mut configs = HashMap::new();
        configs.insert("system".to_string(), HeapProfile::system());
        configs.insert(
            "system-uncached".to_string(),
            HeapProfile::system_uncached(),
        );
        configs.insert("restricted".to_string(), HeapProfile::restricted());
        Self::with_heaps(configs)
    }

    /// Multi-heap mock with custom per-heap profiles. Strict mode.
    #[must_use]
    pub fn with_heaps(heaps: HashMap<String, HeapProfile>) -> Self {
        let mut state = MockState::new();
        state.heap_configs = Some(heaps);
        Self {
            state: Mutex::new(state),
        }
    }

    /// Multi-heap mock with default profiles and realistic latency simulation.
    #[must_use]
    pub fn new_multi_heap_realistic() -> Self {
        let mut configs = HashMap::new();
        configs.insert("system".to_string(), HeapProfile::system());
        configs.insert(
            "system-uncached".to_string(),
            HeapProfile::system_uncached(),
        );
        configs.insert("restricted".to_string(), HeapProfile::restricted());
        let mut state = MockState::new();
        state.heap_configs = Some(configs);
        state.sim = Some(SimConfig {
            latency_ns_per_4k: 1000,
            ..Default::default()
        });
        Self {
            state: Mutex::new(state),
        }
    }

    /// List available heap names (sorted).
    /// Returns registered heaps in strict mode, `["system"]` in permissive mode.
    #[must_use]
    pub fn available_heaps(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        match &state.heap_configs {
            Some(configs) => {
                let mut names: Vec<String> = configs.keys().cloned().collect();
                names.sort();
                names
            }
            None => vec!["system".to_string()],
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

/// Simulate size-proportional latency with +/-30% jitter.
fn sim_delay(size: u64, ns_per_4k: u64, ratio_num: u64, ratio_den: u64) {
    if ns_per_4k == 0 {
        return;
    }
    let pages = size.div_ceil(4096);
    let base_ns = pages * ns_per_4k * ratio_num / ratio_den;
    // +/-30% jitter
    let jitter = rand::rng().random_range(700u64..=1300);
    let ns = base_ns * jitter / 1000;
    if ns > 0 {
        std::thread::sleep(std::time::Duration::from_nanos(ns));
    }
}

impl HeapBackend for MockBackend {
    fn open(&self, name: &str) -> nix::Result<RawFd> {
        if name.is_empty() {
            return Err(Errno::ENOENT);
        }
        let mut state = self.state.lock().unwrap();
        let fd = state.alloc_fd();
        state.heap_fds.insert(fd, name.to_string());
        Ok(fd)
    }

    fn alloc(&self, heap_fd: RawFd, data: &mut DmaHeapAllocationData) -> nix::Result<()> {
        let aligned_size;
        let ns_per_4k;
        {
            let mut state = self.state.lock().unwrap();

            // Validate heap fd
            if !state.heap_fds.contains_key(&heap_fd) {
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
            aligned_size = page_align(data.len).ok_or(Errno::EINVAL)?;
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

            // Allocate demand-paged mmap buffer (zero-filled by OS)
            #[allow(clippy::cast_possible_truncation)]
            let buf = Arc::new(MmapBacking::new(aligned_size as usize)?);

            let fd = state.alloc_fd();
            let heap_name = state.heap_fds.get(&heap_fd).cloned();
            state.buffers.insert(
                fd,
                BufferState {
                    data: buf,
                    sync_state: Arc::new(Mutex::new(SyncState::None)),
                    heap_name,
                },
            );

            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                data.fd = fd as u32;
            }

            ns_per_4k = state.sim.as_ref().map_or(0, |s| s.latency_ns_per_4k);
        }
        sim_delay(aligned_size, ns_per_4k, 1, 1); // alloc: 1.0x
        Ok(())
    }

    fn close_heap(&self, heap_fd: RawFd) -> nix::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.heap_fds.remove(&heap_fd).is_some() {
            Ok(())
        } else {
            Err(Errno::EBADF)
        }
    }
}

impl DmaBufBackend for MockBackend {
    fn mmap(&self, fd: RawFd, len: usize) -> nix::Result<*mut u8> {
        let ptr;
        let ns_per_4k;
        {
            let mut state = self.state.lock().unwrap();

            // Container fds cannot be mmap'd (kernel returns EACCES).
            if state.container_fds.contains_key(&fd) {
                return Err(Errno::EACCES);
            }

            // Extract corruption config before borrowing buffers.
            let corrupt_nth = state.sim.as_ref().map_or(0, |s| s.corrupt_every_nth);

            let buf = state.buffers.get(&fd).ok_or(Errno::EBADF)?;

            if len > buf.data.len() {
                return Err(Errno::EINVAL);
            }

            // Return raw pointer to the mmap'd buffer.
            // Safe for mock: the Arc keeps data alive as long as any fd references it,
            // and the mmap region is stable once allocated.
            ptr = buf.data.as_ptr();

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

            ns_per_4k = state.sim.as_ref().map_or(0, |s| s.latency_ns_per_4k);
        }
        sim_delay(len as u64, ns_per_4k, 1, 5); // mmap: 0.2x
        Ok(ptr)
    }

    fn munmap(&self, _addr: *mut u8, _len: usize) -> nix::Result<()> {
        // Mock: no-op. Real munmap is handled by nix::sys::mman::munmap.
        Ok(())
    }

    fn sync(&self, fd: RawFd, flags: u64) -> nix::Result<()> {
        let buf_size;
        let ns_per_4k;
        {
            let state = self.state.lock().unwrap();

            // Container fds cannot be synced.
            if state.container_fds.contains_key(&fd) {
                return Err(Errno::EINVAL);
            }

            let buf = state.buffers.get(&fd).ok_or(Errno::EBADF)?;

            // Validate flags: only valid bits allowed
            if flags & !DMA_BUF_SYNC_VALID_FLAGS_MASK != 0 {
                return Err(Errno::EINVAL);
            }

            // Must specify at least READ or WRITE
            validate_sync_direction(flags)?;

            buf_size = buf.data.len() as u64;

            let mut sync = buf.sync_state.lock().unwrap();
            if flags & DMA_BUF_SYNC_END != 0 {
                // END
                *sync = SyncState::None;
            } else {
                // START
                *sync = SyncState::Started {
                    flags: flags & (DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE),
                };
            }

            ns_per_4k = state.sim.as_ref().map_or(0, |s| s.latency_ns_per_4k);
        }
        sim_delay(buf_size, ns_per_4k, 1, 2); // sync: 0.5x
        Ok(())
    }

    fn llseek(&self, fd: RawFd, offset: i64, whence: i32) -> nix::Result<i64> {
        let state = self.state.lock().unwrap();

        // Container fds do not support llseek.
        if state.container_fds.contains_key(&fd) {
            return Err(Errno::EINVAL);
        }

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
            sync_state: Arc::clone(&original.sync_state),
            heap_name: original.heap_name.clone(),
        };

        let new_fd = state.alloc_fd();
        state.buffers.insert(new_fd, new_buf);
        Ok(new_fd)
    }

    fn close(&self, fd: RawFd) -> nix::Result<()> {
        let mut state = self.state.lock().unwrap();

        // Check buffers first, then sync_file fds
        if let Some(removed) = state.buffers.remove(&fd) {
            let size = removed.data.len() as u64;
            let ns_per_4k = state.sim.as_ref().map_or(0, |s| s.latency_ns_per_4k);
            drop(state);
            sim_delay(size, ns_per_4k, 3, 10); // free: 0.3x
            return Ok(());
        }
        if state.sync_file_fds.remove(&fd) {
            return Ok(());
        }

        Err(Errno::EBADF)
    }
}

impl ContainerBackend for MockBackend {
    fn open_container_device(&self) -> nix::Result<RawFd> {
        let mut state = self.state.lock().unwrap();
        let fd = state.alloc_fd();
        state.container_device_fds.insert(fd);
        Ok(fd)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn merge(&self, device_fd: RawFd, buf_fds: &[RawFd]) -> nix::Result<RawFd> {
        let mut state = self.state.lock().unwrap();

        // Validate device fd.
        if !state.container_device_fds.contains(&device_fd) {
            return Err(Errno::EBADF);
        }

        // Validate count range (kernel: 1..=MAX_BUFCON_SRC_BUFS).
        if buf_fds.is_empty() || buf_fds.len() > MAX_BUFCON_SRC_BUFS {
            return Err(Errno::EINVAL);
        }

        // Flatten: resolve each fd — if it's a container, extract its buffers.
        let mut resolved = Vec::new();
        for &fd in buf_fds {
            if let Some(container) = state.container_fds.get(&fd) {
                resolved.extend_from_slice(&container.buffer_fds);
            } else if state.buffers.contains_key(&fd) {
                resolved.push(fd);
            } else {
                return Err(Errno::EBADF);
            }
        }

        // Check total count after flattening.
        if resolved.len() > crate::ioctl::dmabuf_container::MAX_BUFCON_BUFS {
            return Err(Errno::EINVAL);
        }

        let container_fd = state.alloc_fd();
        state.container_fds.insert(
            container_fd,
            ContainerState {
                buffer_fds: resolved,
                mask: 0,
            },
        );

        Ok(container_fd)
    }

    fn set_mask(&self, device_fd: RawFd, container_fd: RawFd, mask: u64) -> nix::Result<()> {
        let state = self.state.lock().unwrap();

        if !state.container_device_fds.contains(&device_fd) {
            return Err(Errno::EBADF);
        }

        let container = state.container_fds.get(&container_fd).ok_or(Errno::EBADF)?;

        // Validate mask: bits beyond count are invalid.
        let count = container.buffer_fds.len();
        if count < 64 && mask & !((1u64 << count) - 1) != 0 {
            return Err(Errno::EINVAL);
        }

        // Drop immutable borrow and re-acquire mutably.
        drop(state);
        let mut state = self.state.lock().unwrap();
        state
            .container_fds
            .get_mut(&container_fd)
            .ok_or(Errno::EBADF)?
            .mask = mask;

        Ok(())
    }

    fn get_mask(&self, device_fd: RawFd, container_fd: RawFd) -> nix::Result<u64> {
        let state = self.state.lock().unwrap();

        if !state.container_device_fds.contains(&device_fd) {
            return Err(Errno::EBADF);
        }

        let container = state.container_fds.get(&container_fd).ok_or(Errno::EBADF)?;
        Ok(container.mask)
    }

    fn close_container(&self, fd: RawFd) -> nix::Result<()> {
        let mut state = self.state.lock().unwrap();

        if state.container_fds.remove(&fd).is_some() {
            return Ok(());
        }
        if state.container_device_fds.remove(&fd) {
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

    // ── Container backend tests ──

    #[test]
    fn container_open_device() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        assert!(dev_fd >= MOCK_FD_START);
        b.close_container(dev_fd).unwrap();
    }

    #[test]
    fn container_merge_two() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let (_, fd1) = open_and_alloc(&b, 4096);
        let (_, fd2) = open_and_alloc(&b, 8192);

        let cfd = b.merge(dev_fd, &[fd1, fd2]).unwrap();
        assert!(cfd >= MOCK_FD_START);

        // Default mask is 0.
        assert_eq!(b.get_mask(dev_fd, cfd).unwrap(), 0);

        b.close_container(cfd).unwrap();
        b.close_container(dev_fd).unwrap();
    }

    #[test]
    fn container_merge_single() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let (_, fd1) = open_and_alloc(&b, 4096);

        let cfd = b.merge(dev_fd, &[fd1]).unwrap();
        assert!(cfd >= MOCK_FD_START);
        b.close_container(cfd).unwrap();
    }

    #[test]
    fn container_merge_empty_fails() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        assert_eq!(b.merge(dev_fd, &[]), Err(Errno::EINVAL));
    }

    #[test]
    fn container_merge_over_max_fails() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let heap_fd = b.open("system").unwrap();

        // Allocate MAX_BUFCON_SRC_BUFS + 1 = 64 buffers.
        let mut fds = Vec::new();
        for _ in 0..=MAX_BUFCON_SRC_BUFS {
            let mut data = DmaHeapAllocationData {
                len: 4096,
                fd_flags: DMA_HEAP_ALLOC_FD_FLAGS,
                ..Default::default()
            };
            b.alloc(heap_fd, &mut data).unwrap();
            #[allow(clippy::cast_possible_wrap)]
            fds.push(data.fd as i32);
        }
        assert_eq!(fds.len(), 64);
        assert_eq!(b.merge(dev_fd, &fds), Err(Errno::EINVAL));
    }

    #[test]
    fn container_merge_invalid_fd() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        assert_eq!(b.merge(dev_fd, &[9999]), Err(Errno::EBADF));
    }

    #[test]
    fn container_merge_bad_device_fd() {
        let b = setup();
        let (_, fd1) = open_and_alloc(&b, 4096);
        assert_eq!(b.merge(9999, &[fd1]), Err(Errno::EBADF));
    }

    #[test]
    fn container_set_get_mask_roundtrip() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let (_, fd1) = open_and_alloc(&b, 4096);
        let (_, fd2) = open_and_alloc(&b, 4096);
        let (_, fd3) = open_and_alloc(&b, 4096);

        let cfd = b.merge(dev_fd, &[fd1, fd2, fd3]).unwrap();

        // Set mask = 0b101 (buffers 0 and 2 active).
        b.set_mask(dev_fd, cfd, 0b101).unwrap();
        assert_eq!(b.get_mask(dev_fd, cfd).unwrap(), 0b101);

        // Set mask = 0b111 (all active).
        b.set_mask(dev_fd, cfd, 0b111).unwrap();
        assert_eq!(b.get_mask(dev_fd, cfd).unwrap(), 0b111);

        // Set mask = 0 (all unmasked).
        b.set_mask(dev_fd, cfd, 0).unwrap();
        assert_eq!(b.get_mask(dev_fd, cfd).unwrap(), 0);
    }

    #[test]
    fn container_set_mask_overflow_fails() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let (_, fd1) = open_and_alloc(&b, 4096);
        let (_, fd2) = open_and_alloc(&b, 4096);

        let cfd = b.merge(dev_fd, &[fd1, fd2]).unwrap();
        // count=2 → valid bits are 0b11. Setting bit 2 (0b100) should fail.
        assert_eq!(b.set_mask(dev_fd, cfd, 0b100), Err(Errno::EINVAL));
    }

    #[test]
    fn container_set_mask_bad_container() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        assert_eq!(b.set_mask(dev_fd, 9999, 0), Err(Errno::EBADF));
    }

    #[test]
    fn container_get_mask_bad_container() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        assert_eq!(b.get_mask(dev_fd, 9999), Err(Errno::EBADF));
    }

    #[test]
    fn container_close_then_ops_fail() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let (_, fd1) = open_and_alloc(&b, 4096);
        let cfd = b.merge(dev_fd, &[fd1]).unwrap();

        b.close_container(cfd).unwrap();
        assert_eq!(b.set_mask(dev_fd, cfd, 0), Err(Errno::EBADF));
        assert_eq!(b.get_mask(dev_fd, cfd), Err(Errno::EBADF));
    }

    #[test]
    fn container_close_preserves_source_bufs() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let (_, fd1) = open_and_alloc(&b, 4096);
        let cfd = b.merge(dev_fd, &[fd1]).unwrap();

        b.close_container(cfd).unwrap();
        // Source buffer should still be usable.
        let size = b.llseek(fd1, 0, libc::SEEK_END).unwrap();
        assert_eq!(size, 4096);
    }

    #[test]
    fn container_close_bad_fd() {
        let b = setup();
        assert_eq!(b.close_container(9999), Err(Errno::EBADF));
    }

    #[test]
    fn container_mmap_fails() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let (_, fd1) = open_and_alloc(&b, 4096);
        let cfd = b.merge(dev_fd, &[fd1]).unwrap();

        assert_eq!(b.mmap(cfd, 4096), Err(Errno::EACCES));
    }

    #[test]
    fn container_sync_fails() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let (_, fd1) = open_and_alloc(&b, 4096);
        let cfd = b.merge(dev_fd, &[fd1]).unwrap();

        assert_eq!(
            b.sync(cfd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ),
            Err(Errno::EINVAL)
        );
    }

    #[test]
    fn container_llseek_fails() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let (_, fd1) = open_and_alloc(&b, 4096);
        let cfd = b.merge(dev_fd, &[fd1]).unwrap();

        assert_eq!(b.llseek(cfd, 0, libc::SEEK_END), Err(Errno::EINVAL));
    }

    #[test]
    fn container_flatten_nested() {
        let b = setup();
        let dev_fd = b.open_container_device().unwrap();
        let (_, fd1) = open_and_alloc(&b, 4096);
        let (_, fd2) = open_and_alloc(&b, 4096);
        let (_, fd3) = open_and_alloc(&b, 4096);

        // Create inner container [fd1, fd2].
        let inner = b.merge(dev_fd, &[fd1, fd2]).unwrap();

        // Merge inner container + fd3 → should flatten to [fd1, fd2, fd3].
        let outer = b.merge(dev_fd, &[inner, fd3]).unwrap();

        // Mask with 3 bits should be valid (count=3 after flatten).
        b.set_mask(dev_fd, outer, 0b111).unwrap();
        assert_eq!(b.get_mask(dev_fd, outer).unwrap(), 0b111);

        // Bit 3 (0b1000) should be invalid.
        assert_eq!(b.set_mask(dev_fd, outer, 0b1000), Err(Errno::EINVAL));
    }
}
