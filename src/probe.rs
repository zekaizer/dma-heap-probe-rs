// Heap discovery and capability probing.

use crate::backend::{DmaBufBackend, HeapBackend};
use crate::dmabuf::DmaBuf;
use crate::heap::DmaHeap;
use crate::ioctl::dma_buf::DMA_BUF_SYNC_WRITE;
use crate::ioctl::dma_heap::{DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS};

// ── Heap discovery ──────────────────────────────────────────────────────────

/// Discover available heap names. Uses overrides if provided, else scans
/// `/dev/dma_heap/`, falling back to `["system"]`.
pub(crate) fn discover_heaps(override_heaps: Option<&[String]>) -> Vec<String> {
    if let Some(heaps) = override_heaps {
        tracing::debug!(count = heaps.len(), "using override heaps");
        return heaps.to_vec();
    }
    if let Ok(entries) = std::fs::read_dir("/dev/dma_heap") {
        let mut heaps: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        heaps.sort();
        if !heaps.is_empty() {
            tracing::debug!(count = heaps.len(), "discovered heaps from /dev/dma_heap");
            return heaps;
        }
    }
    tracing::debug!("no heaps found, falling back to system");
    vec!["system".to_string()]
}

// ── Heap capability probing ─────────────────────────────────────────────────

/// Per-heap capability flags determined by probing.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct HeapCaps {
    pub name: String,
    pub can_alloc: bool,
    pub can_mmap: bool,
    pub can_sync: bool,
    pub can_write: bool,
    pub can_llseek: bool,
    pub can_sync_file: bool,
    pub can_dup: bool,
}

impl HeapCaps {
    fn new_false(name: &str) -> Self {
        Self {
            name: name.to_string(),
            can_alloc: false,
            can_mmap: false,
            can_sync: false,
            can_write: false,
            can_llseek: false,
            can_sync_file: false,
            can_dup: false,
        }
    }
}

/// Probe a single heap to determine which operations are supported.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn probe_heap<B: HeapBackend + DmaBufBackend>(backend: &B, heap_name: &str) -> HeapCaps {
    let mut caps = HeapCaps::new_false(heap_name);

    let Ok(heap) = DmaHeap::open(backend, heap_name) else {
        tracing::trace!(heap = heap_name, "probe: open failed");
        return caps;
    };
    let Ok(fd) = heap.alloc(4096, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS) else {
        tracing::trace!(heap = heap_name, "probe: alloc failed");
        return caps;
    };
    caps.can_alloc = true;

    let mut buf = DmaBuf::new(backend, fd, 4096);

    // mmap
    if let Ok(ptr) = buf.mmap() {
        caps.can_mmap = true;
        tracing::trace!(heap = heap_name, "probe: mmap ok");

        // sync + write
        if buf.sync_start(DMA_BUF_SYNC_WRITE).is_ok() {
            caps.can_sync = true;
            let write_ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: ptr is valid and mapped to 4096 bytes.
                unsafe {
                    std::ptr::write_bytes(ptr, 0xAA, 4096);
                }
            }));
            caps.can_write = write_ok.is_ok();
            tracing::trace!(heap = heap_name, ok = caps.can_write, "probe: write");
            let _ = buf.sync_end(DMA_BUF_SYNC_WRITE);
        } else {
            tracing::trace!(heap = heap_name, "probe: sync failed");
        }
    } else {
        tracing::trace!(heap = heap_name, "probe: mmap failed");
    }

    caps.can_llseek = buf.llseek_size().is_ok();
    tracing::trace!(heap = heap_name, ok = caps.can_llseek, "probe: llseek");

    #[allow(clippy::cast_possible_truncation)]
    {
        caps.can_sync_file = buf.export_sync_file(DMA_BUF_SYNC_WRITE as u32).is_ok();
    }
    tracing::trace!(
        heap = heap_name,
        ok = caps.can_sync_file,
        "probe: sync_file"
    );

    if let Ok(dup_buf) = buf.dup() {
        caps.can_dup = true;
        drop(dup_buf);
    }
    tracing::trace!(heap = heap_name, ok = caps.can_dup, "probe: dup");

    drop(buf);

    tracing::info!(
        heap = heap_name,
        alloc = caps.can_alloc,
        mmap = caps.can_mmap,
        sync = caps.can_sync,
        write = caps.can_write,
        llseek = caps.can_llseek,
        sync_file = caps.can_sync_file,
        dup = caps.can_dup,
        "heap capabilities probed"
    );

    caps
}

/// Discover heaps and probe each one, returning only those that can alloc.
pub(crate) fn discover_and_probe<B: HeapBackend + DmaBufBackend>(
    backend: &B,
    override_heaps: Option<&[String]>,
) -> Vec<HeapCaps> {
    let names = discover_heaps(override_heaps);
    let caps: Vec<HeapCaps> = names
        .iter()
        .map(|name| probe_heap(backend, name))
        .filter(|c| c.can_alloc)
        .collect();

    if caps.is_empty() {
        tracing::error!("no usable heaps found");
    } else {
        tracing::info!(
            count = caps.len(),
            heaps = ?caps.iter().map(|c| &c.name).collect::<Vec<_>>(),
            "usable heaps"
        );
    }
    caps
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_heaps_with_override() {
        let heaps = vec!["a".to_string(), "b".to_string()];
        let result = discover_heaps(Some(&heaps));
        assert_eq!(result, heaps);
    }

    #[test]
    fn discover_heaps_fallback() {
        // On non-Android hosts, /dev/dma_heap doesn't exist.
        let result = discover_heaps(None);
        assert_eq!(result, vec!["system".to_string()]);
    }

    #[test]
    fn probe_heap_mock_all_caps() {
        let b = crate::backend::mock::MockBackend::new();
        let caps = probe_heap(&b, "system");
        assert!(caps.can_alloc);
        assert!(caps.can_mmap);
        assert!(caps.can_sync);
        assert!(caps.can_write);
        assert!(caps.can_llseek);
        assert!(caps.can_sync_file);
        assert!(caps.can_dup);
    }

    #[test]
    fn probe_heap_bad_heap() {
        let b = crate::backend::mock::MockBackend::new();
        let caps = probe_heap(&b, "");
        assert!(!caps.can_alloc);
    }
}
