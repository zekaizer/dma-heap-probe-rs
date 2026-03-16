// Heap discovery and capability probing.

use nix::errno::Errno;

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
    /// Detected allocation granularity in bytes (from llseek on 1-byte alloc).
    /// Falls back to 4096 if detection fails.
    pub alloc_granularity: u64,
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
            alloc_granularity: 4096,
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
    let Ok(fd) = heap.alloc(1, DMA_HEAP_ALLOC_FD_FLAGS, DMA_HEAP_VALID_HEAP_FLAGS) else {
        tracing::trace!(heap = heap_name, "probe: alloc failed");
        return caps;
    };
    caps.can_alloc = true;

    let mut buf = DmaBuf::new(backend, fd, 1);

    // Detect allocation granularity via llseek before mmap.
    if let Ok(actual_size) = buf.llseek_size() {
        caps.can_llseek = true;
        #[allow(clippy::cast_sign_loss)]
        {
            caps.alloc_granularity = actual_size as u64;
        }
        tracing::trace!(
            heap = heap_name,
            granularity = caps.alloc_granularity,
            "probe: llseek granularity detected"
        );
    } else {
        tracing::trace!(
            heap = heap_name,
            "probe: llseek failed, using default granularity 4096"
        );
    }

    // mmap — use detected granularity as the actual buffer size.
    #[allow(clippy::cast_possible_truncation)]
    let mapped_size = caps.alloc_granularity as usize;
    match buf.mmap() {
        Ok(ptr) => {
            caps.can_mmap = true;
            tracing::trace!(heap = heap_name, "probe: mmap ok");

            // sync + write
            if buf.sync_start(DMA_BUF_SYNC_WRITE).is_ok() {
                caps.can_sync = true;
                let write_ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // SAFETY: ptr is valid and mapped to `mapped_size` bytes.
                    unsafe {
                        std::ptr::write_bytes(ptr, 0xAA, mapped_size);
                    }
                }));
                caps.can_write = write_ok.is_ok();
                tracing::trace!(heap = heap_name, ok = caps.can_write, "probe: write");
                let _ = buf.sync_end(DMA_BUF_SYNC_WRITE);
            } else {
                tracing::trace!(heap = heap_name, "probe: sync failed");
            }
        }
        Err(Errno::EACCES) => {
            // Restricted heap: mmap denied by permission.
            tracing::trace!(heap = heap_name, "probe: mmap EACCES (restricted)");
        }
        Err(e) => {
            // mmap failed for a non-permission reason; mark as supported so
            // tests run and report the real failure.
            caps.can_mmap = true;
            tracing::trace!(heap = heap_name, err = %e, "probe: mmap failed (non-EACCES)");
        }
    }

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
        granularity = caps.alloc_granularity,
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
        assert_eq!(caps.alloc_granularity, 4096);
    }

    #[test]
    fn probe_heap_bad_heap() {
        let b = crate::backend::mock::MockBackend::new();
        let caps = probe_heap(&b, "");
        assert!(!caps.can_alloc);
        assert_eq!(caps.alloc_granularity, 4096); // fallback
    }

    #[test]
    fn probe_detects_64k_granularity() {
        use crate::backend::mock::{HeapProfile, MockBackend};
        use std::collections::HashMap;

        let mut configs = HashMap::new();
        configs.insert(
            "vframe".to_string(),
            HeapProfile {
                alloc_granularity: 65536,
                ..HeapProfile::default()
            },
        );
        let b = MockBackend::with_heaps(configs);
        let caps = probe_heap(&b, "vframe");
        assert!(caps.can_alloc);
        assert!(caps.can_llseek);
        assert_eq!(caps.alloc_granularity, 65536);
    }
}
