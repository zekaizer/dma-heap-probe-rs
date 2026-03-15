// Samsung dma-buf container ioctl definitions.
// Based on Samsung kernel include/linux/dma-buf-container.h

/// Merge multiple dma-buf fds into a container.
///
/// Matches Samsung kernel `struct dma_buf_merge` exactly (aarch64).
/// The `dma_bufs` field is encoded as `u64` instead of a raw pointer
/// to keep the struct `Copy`/`Default`/`Send`/`Sync`-friendly.
/// On aarch64, `u64` and `*const i32` are both 8 bytes.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct DmaBufMerge {
    /// Pointer to the fd array, encoded as `u64`. Cast via `as_ptr() as u64`.
    pub dma_bufs: u64,
    /// Number of fds in the array (1..=`MAX_BUFCON_SRC_BUFS`).
    pub count: i32,
    /// Output: the resulting container fd.
    pub dmabuf_container: i32,
    /// Reserved, must be zero.
    pub reserved: [u32; 2],
}

/// Mask for selecting active buffers in a container.
///
/// Matches Samsung kernel `struct dma_buf_mask` exactly.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct DmaBufMask {
    /// The container fd to operate on.
    pub dmabuf_container: i32,
    /// Reserved, must be zero.
    pub reserved: u32,
    /// 64-bit bitmask as two `u32`s: `mask[0]` = lower 32, `mask[1]` = upper 32.
    pub mask: [u32; 2],
}

/// Maximum number of buffers in a container (inclusive).
pub const MAX_BUFCON_BUFS: usize = 64;

/// Maximum number of source buffers for a single MERGE call.
pub const MAX_BUFCON_SRC_BUFS: usize = MAX_BUFCON_BUFS - 1;

const DMABUF_CONTAINER_BASE: u8 = b'D';

nix::ioctl_readwrite!(
    /// `DMABUF_CONTAINER_IOCTL_MERGE` — merge dma-buf fds into a container.
    dmabuf_container_merge,
    DMABUF_CONTAINER_BASE,
    1,
    DmaBufMerge
);

nix::ioctl_write_ptr!(
    /// `DMABUF_CONTAINER_IOCTL_SET_MASK` — set active buffer mask.
    dmabuf_container_set_mask,
    DMABUF_CONTAINER_BASE,
    3,
    DmaBufMask
);

nix::ioctl_read!(
    /// `DMABUF_CONTAINER_IOCTL_GET_MASK` — get current buffer mask.
    dmabuf_container_get_mask,
    DMABUF_CONTAINER_BASE,
    4,
    DmaBufMask
);

/// Convert a `u64` mask to `[u32; 2]` (little-endian: `[0]` = low, `[1]` = high).
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub const fn mask_to_array(mask: u64) -> [u32; 2] {
    [mask as u32, (mask >> 32) as u32]
}

/// Convert `[u32; 2]` back to a `u64` mask.
#[must_use]
pub const fn mask_from_array(arr: [u32; 2]) -> u64 {
    (arr[1] as u64) << 32 | arr[0] as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    #[test]
    fn dma_buf_merge_layout() {
        assert_eq!(mem::size_of::<DmaBufMerge>(), 24);
        assert_eq!(mem::align_of::<DmaBufMerge>(), 8);
        assert_eq!(mem::offset_of!(DmaBufMerge, dma_bufs), 0);
        assert_eq!(mem::offset_of!(DmaBufMerge, count), 8);
        assert_eq!(mem::offset_of!(DmaBufMerge, dmabuf_container), 12);
        assert_eq!(mem::offset_of!(DmaBufMerge, reserved), 16);
    }

    #[test]
    fn dma_buf_mask_layout() {
        assert_eq!(mem::size_of::<DmaBufMask>(), 16);
        assert_eq!(mem::align_of::<DmaBufMask>(), 4);
        assert_eq!(mem::offset_of!(DmaBufMask, dmabuf_container), 0);
        assert_eq!(mem::offset_of!(DmaBufMask, reserved), 4);
        assert_eq!(mem::offset_of!(DmaBufMask, mask), 8);
    }

    #[test]
    fn merge_ioctl_request_code() {
        let expected =
            nix::request_code_readwrite!(DMABUF_CONTAINER_BASE, 1, mem::size_of::<DmaBufMerge>());
        assert_eq!(expected, nix::request_code_readwrite!(b'D', 1, 24));
    }

    #[test]
    fn set_mask_ioctl_request_code() {
        let expected =
            nix::request_code_write!(DMABUF_CONTAINER_BASE, 3, mem::size_of::<DmaBufMask>());
        assert_eq!(expected, nix::request_code_write!(b'D', 3, 16));
    }

    #[test]
    fn get_mask_ioctl_request_code() {
        let expected =
            nix::request_code_read!(DMABUF_CONTAINER_BASE, 4, mem::size_of::<DmaBufMask>());
        assert_eq!(expected, nix::request_code_read!(b'D', 4, 16));
    }

    #[test]
    fn constants() {
        assert_eq!(MAX_BUFCON_BUFS, 64);
        assert_eq!(MAX_BUFCON_SRC_BUFS, 63);
    }

    #[test]
    fn mask_roundtrip() {
        let mask: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let arr = mask_to_array(mask);
        assert_eq!(arr[0], 0xCAFE_BABE);
        assert_eq!(arr[1], 0xDEAD_BEEF);
        assert_eq!(mask_from_array(arr), mask);
    }

    #[test]
    fn mask_zero() {
        assert_eq!(mask_to_array(0), [0, 0]);
        assert_eq!(mask_from_array([0, 0]), 0);
    }

    #[test]
    fn mask_max() {
        assert_eq!(mask_to_array(u64::MAX), [u32::MAX, u32::MAX]);
        assert_eq!(mask_from_array([u32::MAX, u32::MAX]), u64::MAX);
    }
}
