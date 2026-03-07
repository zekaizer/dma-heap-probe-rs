// Fragmentation observation tests: buddyinfo tracking, interleave pattern,
// pagetypeinfo tracking.

use std::error::Error;

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::DMA_BUF_SYNC_WRITE;
use crate::ioctl::dma_heap::{DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};
use crate::procfs::{self, BuddyInfoEntry, PageTypeInfoEntry};

/// Interleave pattern buffer count.
const INTERLEAVE_COUNT: usize = 100;

/// Mixed sizes for interleave pattern.
const MIXED_SIZES: &[u64] = &[4096, 65536, 1_048_576];

/// Run all fragmentation observation tests.
pub fn run<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    pattern: &str,
) -> Result<(), Box<dyn Error>> {
    let tests: Vec<(&str, nix::Result<()>)> = vec![
        (
            "buddyinfo_track",
            test_buddyinfo_track(backend, heap_name),
        ),
        (
            "interleave_pattern",
            test_interleave_pattern(backend, heap_name, pattern),
        ),
        (
            "pagetypeinfo_track",
            test_pagetypeinfo_track(backend, heap_name),
        ),
    ];

    let mut first_error: Option<(&str, nix::Error)> = None;

    for (name, result) in &tests {
        match result {
            Ok(()) => tracing::info!(name, "PASS"),
            Err(e) => {
                tracing::error!(name, error = %e, "FAIL");
                if first_error.is_none() {
                    first_error = Some((name, *e));
                }
            }
        }
    }

    match first_error {
        None => Ok(()),
        Some((name, e)) => Err(format!("fragmentation test '{name}' failed: {e}").into()),
    }
}

/// Try to read `/proc/buddyinfo`. Returns empty on non-Linux hosts.
fn try_buddyinfo() -> Vec<BuddyInfoEntry> {
    procfs::read_buddyinfo().unwrap_or_default()
}

/// Try to read `/proc/pagetypeinfo`. Returns empty on non-Linux hosts.
fn try_pagetypeinfo() -> Vec<PageTypeInfoEntry> {
    procfs::read_pagetypeinfo().unwrap_or_default()
}

/// Log high-order free chunk summary from buddyinfo.
fn log_buddyinfo_summary(label: &str, entries: &[BuddyInfoEntry]) {
    for entry in entries {
        // Sum order 4+ (64KB+) free chunks as fragmentation indicator.
        let high_order_sum: u64 = entry.free_counts.iter().skip(4).sum();
        tracing::info!(
            label,
            node = entry.node,
            zone = %entry.zone,
            high_order_free = high_order_sum,
            "buddyinfo"
        );
    }
}

/// Track buddyinfo before/after a large alloc/free cycle.
#[allow(clippy::cast_possible_truncation)]
fn test_buddyinfo_track<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Snapshot before.
    let before = try_buddyinfo();
    log_buddyinfo_summary("before", &before);

    // Alloc/free cycle: 100x 1MB.
    let mut buffers = Vec::with_capacity(100);
    for _ in 0..100 {
        let fd = heap.alloc(1_048_576, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        buffers.push(DmaBuf::new(backend, fd, 1_048_576));
    }
    drop(buffers);

    // Snapshot after.
    let after = try_buddyinfo();
    log_buddyinfo_summary("after", &after);

    Ok(())
}

/// 4-phase interleave allocation/free pattern to induce fragmentation.
#[allow(clippy::cast_possible_truncation)]
fn test_interleave_pattern<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
    pattern: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    // Phase 1: Allocate mixed-size buffers.
    let mut buffers: Vec<Option<DmaBuf<'_, B>>> = Vec::with_capacity(INTERLEAVE_COUNT);
    for i in 0..INTERLEAVE_COUNT {
        let size = MIXED_SIZES[i % MIXED_SIZES.len()];
        let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        let mut buf = DmaBuf::new(backend, fd, size as usize);
        // Touch the buffer to ensure pages are faulted in.
        let ptr = buf.mmap()?;
        buf.sync_start(DMA_BUF_SYNC_WRITE)?;
        unsafe { std::ptr::write_bytes(ptr, 0xAA, size as usize) };
        buf.sync_end(DMA_BUF_SYNC_WRITE)?;
        buffers.push(Some(buf));
    }
    tracing::info!(phase = 1, count = INTERLEAVE_COUNT, "allocated");
    let snap1 = try_buddyinfo();
    log_buddyinfo_summary("phase1", &snap1);

    // Phase 2: Close based on pattern.
    let released = if pattern == "sequential" {
        // Close first half.
        let mut count = 0;
        for slot in buffers.iter_mut().take(INTERLEAVE_COUNT / 2) {
            if slot.take().is_some() {
                count += 1;
            }
        }
        count
    } else {
        // "interleave": close even indices.
        let mut count = 0;
        for (i, slot) in buffers.iter_mut().enumerate() {
            if i % 2 == 0 && slot.take().is_some() {
                count += 1;
            }
        }
        count
    };
    tracing::info!(phase = 2, released, pattern, "freed");
    let snap2 = try_buddyinfo();
    log_buddyinfo_summary("phase2", &snap2);

    // Phase 3: Reallocate into freed slots with different sizes.
    let mut realloc_count = 0;
    for slot in &mut buffers {
        if slot.is_none() {
            // Use a different size than original.
            let size = 32768u64; // 32KB — different from all MIXED_SIZES.
            let fd = heap.alloc(size, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
            *slot = Some(DmaBuf::new(backend, fd, size as usize));
            realloc_count += 1;
        }
    }
    tracing::info!(phase = 3, reallocated = realloc_count, "realloc");
    let snap3 = try_buddyinfo();
    log_buddyinfo_summary("phase3", &snap3);

    // Phase 4: Clean up all.
    drop(buffers);
    tracing::info!(phase = 4, "all freed");
    let snap4 = try_buddyinfo();
    log_buddyinfo_summary("phase4", &snap4);

    Ok(())
}

/// Track pagetypeinfo (Movable/Unmovable/CMA) before/after alloc cycle.
#[allow(clippy::cast_possible_truncation)]
fn test_pagetypeinfo_track<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    heap_name: &str,
) -> nix::Result<()> {
    let heap = DmaHeap::open(backend, heap_name)?;

    let before = try_pagetypeinfo();
    log_pagetypeinfo_summary("before", &before);

    // Alloc cycle: 50x 1MB.
    let mut buffers = Vec::with_capacity(50);
    for _ in 0..50 {
        let fd = heap.alloc(1_048_576, DMA_HEAP_VALID_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS)?;
        buffers.push(DmaBuf::new(backend, fd, 1_048_576));
    }

    let during = try_pagetypeinfo();
    log_pagetypeinfo_summary("during", &during);

    drop(buffers);

    let after = try_pagetypeinfo();
    log_pagetypeinfo_summary("after", &after);

    Ok(())
}

/// Log pagetypeinfo summary for key migration types.
fn log_pagetypeinfo_summary(label: &str, entries: &[PageTypeInfoEntry]) {
    for entry in entries {
        // Focus on CMA, Movable, Unmovable types.
        match entry.page_type.as_str() {
            "CMA" | "Movable" | "Unmovable" => {
                let high_order_sum: u64 = entry.free_counts.iter().skip(4).sum();
                tracing::info!(
                    label,
                    node = entry.node,
                    zone = %entry.zone,
                    page_type = %entry.page_type,
                    high_order_free = high_order_sum,
                    "pagetypeinfo"
                );
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[test]
    fn buddyinfo_track_runs() {
        let b = MockBackend::new();
        test_buddyinfo_track(&b, "system").unwrap();
    }

    #[test]
    fn interleave_pattern_runs() {
        let b = MockBackend::new();
        test_interleave_pattern(&b, "system", "interleave").unwrap();
    }

    #[test]
    fn sequential_pattern_runs() {
        let b = MockBackend::new();
        test_interleave_pattern(&b, "system", "sequential").unwrap();
    }

    #[test]
    fn pagetypeinfo_track_runs() {
        let b = MockBackend::new();
        test_pagetypeinfo_track(&b, "system").unwrap();
    }

    #[test]
    fn run_passes() {
        let b = MockBackend::new();
        run(&b, "system", "interleave").unwrap();
    }

    #[test]
    fn interleave_no_leak() {
        let b = MockBackend::new();
        test_interleave_pattern(&b, "system", "interleave").unwrap();
        assert_eq!(b.buffer_count(), 0, "all buffers should be freed");
    }

    #[test]
    fn sequential_no_leak() {
        let b = MockBackend::new();
        test_interleave_pattern(&b, "system", "sequential").unwrap();
        assert_eq!(b.buffer_count(), 0);
    }
}
