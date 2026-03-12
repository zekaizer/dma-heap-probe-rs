// Stage 2 sync_file tests: export/import sync_file operations.

use std::error::Error;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::{DMA_BUF_SYNC_READ, DMA_BUF_SYNC_RW, DMA_BUF_SYNC_WRITE};
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::runner::{self, SubTestResult};

/// Run all stage 2 `sync_file` tests. Executes all tests even if some fail;
/// returns sub-test results (and the first error, if any).
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> (Vec<SubTestResult>, Option<Box<dyn Error>>) {
    println!("sync_file sequence:");
    println!("  heap: {heap_name}");
    println!();
    println!("  1. export           alloc -> sync_file_create -> verify fd >= 0");
    println!("  2. import           export -> sync_file_import -> verify fd >= 0");
    println!();
    println!("sync_file result legend:");
    println!("  sync_fd    exported sync_file descriptor");
    println!("  import_fd  imported sync_file descriptor");
    println!();

    let tests: [(&str, nix::Result<()>); 2] = [
        (
            "export_sync_file",
            test_export_sync_file(backend, heap_name),
        ),
        (
            "import_sync_file",
            test_import_sync_file(backend, heap_name),
        ),
    ];

    runner::collect_test_results("sync_file", &tests)
}

/// Export `sync_file` with each valid flag combination and verify returned fd.
#[allow(clippy::cast_possible_truncation)]
fn test_export_sync_file<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let size: u64 = 4096;
    let buf_fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, buf_fd, size as usize);

    for &flags in &[DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE, DMA_BUF_SYNC_RW] {
        let sync_fd = buf.export_sync_file(flags as u32)?;
        tracing::debug!(flags, sync_fd, "exported sync_file");
        if sync_fd < 0 {
            tracing::error!(flags, sync_fd, "invalid sync_file fd");
            return Err(nix::errno::Errno::EIO);
        }
    }

    Ok(())
}

/// Export then import a `sync_file` to verify the full roundtrip.
#[allow(clippy::cast_possible_truncation)]
fn test_import_sync_file<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;
    let size: u64 = 4096;
    let buf_fd = heap.alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
    let buf = DmaBuf::new(backend, buf_fd, size as usize);

    // Export a sync_file, then import it back.
    let sync_fd = buf.export_sync_file(DMA_BUF_SYNC_READ as u32)?;
    tracing::debug!(sync_fd, "exported for import test");

    buf.import_sync_file(DMA_BUF_SYNC_READ as u32, sync_fd)?;
    tracing::debug!(sync_fd, "imported sync_file");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn export_returns_valid_fd() {
        let backend = MockBackend::new();
        test_export_sync_file(&backend, "system").unwrap();
    }

    #[test]
    fn import_after_export() {
        let backend = MockBackend::new();
        test_import_sync_file(&backend, "system").unwrap();
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn export_multiple_sizes() {
        let backend = MockBackend::new();
        let heap = DmaHeap::open(&backend, "system").unwrap();

        for size in [4096_u64, 65536, 1_048_576] {
            let fd = heap
                .alloc(size, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)
                .unwrap();
            let buf = DmaBuf::new(&backend, fd, size as usize);
            let sync_fd = buf.export_sync_file(DMA_BUF_SYNC_RW as u32).unwrap();
            assert!(sync_fd >= 0);
        }
    }

    #[test]
    fn run_passes() {
        let backend = MockBackend::new();
        let (results, err) = run(&backend, "system");
        assert!(err.is_none());
        assert!(results.iter().all(|t| t.passed));
    }
}
