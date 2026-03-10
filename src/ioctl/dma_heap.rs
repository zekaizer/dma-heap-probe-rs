// DMA-Heap ioctl definitions
// Based on linux/uapi/linux/dma-heap.h

/// Metadata passed from userspace for allocations.
///
/// Matches kernel `struct dma_heap_allocation_data` exactly.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct DmaHeapAllocationData {
    /// Size of the allocation (input).
    pub len: u64,
    /// File descriptor for the allocated dma-buf (output).
    pub fd: u32,
    /// File descriptor flags: `O_CLOEXEC | O_ACCMODE` only.
    pub fd_flags: u32,
    /// Heap flags (currently must be 0).
    pub heap_flags: u64,
}

/// Valid `fd_flags` validation mask: `O_CLOEXEC | O_ACCMODE`. For alloc defaults, use
/// [`DMA_HEAP_ALLOC_FD_FLAGS`].
pub const DMA_HEAP_VALID_FD_FLAGS: u32 = (libc::O_CLOEXEC | libc::O_ACCMODE) as u32;

/// Default `fd_flags` for allocation: read-write + close-on-exec.
pub const DMA_HEAP_ALLOC_FD_FLAGS: u32 = (libc::O_RDWR | libc::O_CLOEXEC) as u32;

/// Valid `heap_flags` mask (currently none).
pub const DMA_HEAP_VALID_HEAP_FLAGS: u64 = 0;

const DMA_HEAP_IOC_MAGIC: u8 = b'H';

nix::ioctl_readwrite!(
    /// `DMA_HEAP_IOCTL_ALLOC` — allocate memory from a dma-heap.
    dma_heap_ioctl_alloc,
    DMA_HEAP_IOC_MAGIC,
    0x0,
    DmaHeapAllocationData
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    #[test]
    fn allocation_data_layout() {
        assert_eq!(mem::size_of::<DmaHeapAllocationData>(), 24);
        assert_eq!(mem::align_of::<DmaHeapAllocationData>(), 8);
    }

    #[test]
    fn allocation_data_field_offsets() {
        assert_eq!(mem::offset_of!(DmaHeapAllocationData, len), 0);
        assert_eq!(mem::offset_of!(DmaHeapAllocationData, fd), 8);
        assert_eq!(mem::offset_of!(DmaHeapAllocationData, fd_flags), 12);
        assert_eq!(mem::offset_of!(DmaHeapAllocationData, heap_flags), 16);
    }

    #[test]
    fn ioctl_request_code() {
        let expected = nix::request_code_readwrite!(
            DMA_HEAP_IOC_MAGIC,
            0x0,
            mem::size_of::<DmaHeapAllocationData>()
        );
        // Verify the generated ioctl function uses the correct request code.
        // The nix macro generates a function; we compare the manual code.
        assert_eq!(expected, nix::request_code_readwrite!(b'H', 0x0, 24));
    }

    #[test]
    fn valid_fd_flags_includes_cloexec() {
        assert_ne!(DMA_HEAP_VALID_FD_FLAGS & libc::O_CLOEXEC as u32, 0);
    }

    #[test]
    fn valid_heap_flags_is_zero() {
        assert_eq!(DMA_HEAP_VALID_HEAP_FLAGS, 0);
    }

    #[test]
    fn alloc_fd_flags_is_rdwr_cloexec() {
        assert_eq!(
            DMA_HEAP_ALLOC_FD_FLAGS,
            (libc::O_RDWR | libc::O_CLOEXEC) as u32
        );
        // Ensure O_RDWR (not O_ACCMODE) is the access mode
        assert_eq!(
            DMA_HEAP_ALLOC_FD_FLAGS & libc::O_ACCMODE as u32,
            libc::O_RDWR as u32,
        );
    }

    #[test]
    fn default_is_zeroed() {
        let data = DmaHeapAllocationData::default();
        assert_eq!(data.len, 0);
        assert_eq!(data.fd, 0);
        assert_eq!(data.fd_flags, 0);
        assert_eq!(data.heap_flags, 0);
    }
}
