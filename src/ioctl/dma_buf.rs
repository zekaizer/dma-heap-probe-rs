// DMA-Buf ioctl definitions
// Based on linux/uapi/linux/dma-buf.h

/// Synchronize CPU access to a dma-buf.
///
/// Matches kernel `struct dma_buf_sync` exactly.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct DmaBufSync {
    /// Combination of `DMA_BUF_SYNC_*` flags.
    pub flags: u64,
}

/// Get a `sync_file` from a dma-buf.
///
/// Matches kernel `struct dma_buf_export_sync_file` exactly.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct DmaBufExportSyncFile {
    /// `DMA_BUF_SYNC_READ`, `DMA_BUF_SYNC_WRITE`, or both (input).
    pub flags: u32,
    /// Returned sync file descriptor (output).
    pub fd: i32,
}

/// Insert a `sync_file` into a dma-buf.
///
/// Matches kernel `struct dma_buf_import_sync_file` exactly.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct DmaBufImportSyncFile {
    /// `DMA_BUF_SYNC_READ`, `DMA_BUF_SYNC_WRITE`, or both.
    pub flags: u32,
    /// Sync file descriptor to import.
    pub fd: i32,
}

// Sync direction flags
pub const DMA_BUF_SYNC_READ: u64 = 1 << 0;
pub const DMA_BUF_SYNC_WRITE: u64 = 2;
pub const DMA_BUF_SYNC_RW: u64 = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE;

// Sync start/end flags
pub const DMA_BUF_SYNC_START: u64 = 0 << 2;
pub const DMA_BUF_SYNC_END: u64 = 1 << 2;

/// Bitmask of all valid sync flags.
pub const DMA_BUF_SYNC_VALID_FLAGS_MASK: u64 = DMA_BUF_SYNC_RW | DMA_BUF_SYNC_END;

/// Maximum length of a dma-buf name (excluding null terminator).
pub const DMA_BUF_NAME_LEN: usize = 32;

const DMA_BUF_BASE: u8 = b'b';

nix::ioctl_write_ptr!(
    /// `DMA_BUF_IOCTL_SYNC` — synchronize CPU access.
    dma_buf_ioctl_sync,
    DMA_BUF_BASE,
    0,
    DmaBufSync
);

// DMA_BUF_SET_NAME_B: _IOW('b', 1, __u64)
// The kernel defines this with sizeof(__u64)=8, but the actual data is a char*.
// We must use ioctl_write_ptr_bad! with a manually computed request code.
nix::ioctl_write_ptr_bad!(
    /// `DMA_BUF_SET_NAME_B` — set a name on a dma-buf for debugging.
    dma_buf_set_name,
    nix::request_code_write!(DMA_BUF_BASE, 1, std::mem::size_of::<u64>()),
    libc::c_char
);

nix::ioctl_readwrite!(
    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` — export fences as a sync_file.
    dma_buf_ioctl_export_sync_file,
    DMA_BUF_BASE,
    2,
    DmaBufExportSyncFile
);

nix::ioctl_write_ptr!(
    /// `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` — import a sync_file into a dma-buf.
    dma_buf_ioctl_import_sync_file,
    DMA_BUF_BASE,
    3,
    DmaBufImportSyncFile
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    #[test]
    fn dma_buf_sync_layout() {
        assert_eq!(mem::size_of::<DmaBufSync>(), 8);
        assert_eq!(mem::align_of::<DmaBufSync>(), 8);
        assert_eq!(mem::offset_of!(DmaBufSync, flags), 0);
    }

    #[test]
    fn export_sync_file_layout() {
        assert_eq!(mem::size_of::<DmaBufExportSyncFile>(), 8);
        assert_eq!(mem::align_of::<DmaBufExportSyncFile>(), 4);
        assert_eq!(mem::offset_of!(DmaBufExportSyncFile, flags), 0);
        assert_eq!(mem::offset_of!(DmaBufExportSyncFile, fd), 4);
    }

    #[test]
    fn import_sync_file_layout() {
        assert_eq!(mem::size_of::<DmaBufImportSyncFile>(), 8);
        assert_eq!(mem::align_of::<DmaBufImportSyncFile>(), 4);
        assert_eq!(mem::offset_of!(DmaBufImportSyncFile, flags), 0);
        assert_eq!(mem::offset_of!(DmaBufImportSyncFile, fd), 4);
    }

    #[test]
    fn sync_flag_constants() {
        assert_eq!(DMA_BUF_SYNC_READ, 0x1);
        assert_eq!(DMA_BUF_SYNC_WRITE, 0x2);
        assert_eq!(DMA_BUF_SYNC_RW, 0x3);
        assert_eq!(DMA_BUF_SYNC_START, 0x0);
        assert_eq!(DMA_BUF_SYNC_END, 0x4);
        assert_eq!(DMA_BUF_SYNC_VALID_FLAGS_MASK, 0x7);
    }

    #[test]
    fn name_len_constant() {
        assert_eq!(DMA_BUF_NAME_LEN, 32);
    }

    #[test]
    fn set_name_request_code() {
        // DMA_BUF_SET_NAME_B = _IOW('b', 1, __u64) => size field = 8
        let code = nix::request_code_write!(b'b', 1, mem::size_of::<u64>());
        assert_eq!(code, nix::request_code_write!(DMA_BUF_BASE, 1, 8));
    }

    #[test]
    fn sync_ioctl_request_code() {
        let expected = nix::request_code_write!(DMA_BUF_BASE, 0, mem::size_of::<DmaBufSync>());
        assert_eq!(expected, nix::request_code_write!(b'b', 0, 8));
    }

    #[test]
    fn export_sync_file_request_code() {
        let expected =
            nix::request_code_readwrite!(DMA_BUF_BASE, 2, mem::size_of::<DmaBufExportSyncFile>());
        assert_eq!(expected, nix::request_code_readwrite!(b'b', 2, 8));
    }

    #[test]
    fn import_sync_file_request_code() {
        let expected =
            nix::request_code_write!(DMA_BUF_BASE, 3, mem::size_of::<DmaBufImportSyncFile>());
        assert_eq!(expected, nix::request_code_write!(b'b', 3, 8));
    }
}
