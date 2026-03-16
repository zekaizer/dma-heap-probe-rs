// Parser for /sys/kernel/dmabuf/buffers/ directory.

use std::path::Path;

use anyhow::Context;

use serde::{Deserialize, Serialize};

/// Information about a single dma-buf buffer from sysfs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DmaBufInfo {
    /// Buffer inode number (directory name under `/sys/kernel/dmabuf/buffers/`).
    pub ino: u64,
    /// Buffer size in bytes.
    pub size: u64,
    /// Exporter name (e.g. "system").
    pub exporter_name: String,
}

/// Snapshot of all active dma-buf buffers visible in sysfs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SysfsSnapshot {
    pub buffers: Vec<DmaBufInfo>,
}

/// Parse a buffer entry from raw sysfs file contents.
///
/// `ino` is the directory name (buffer inode number).
/// `size_content` is the content of the `size` file.
/// `exporter_content` is the content of the `exporter_name` file.
pub fn parse_buffer_entry(
    ino: u64,
    size_content: &str,
    exporter_content: &str,
) -> anyhow::Result<DmaBufInfo> {
    let size: u64 = size_content
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("sysfs: invalid size for ino {ino}: {e}"))?;
    let exporter_name = exporter_content.trim().to_string();
    if exporter_name.is_empty() {
        anyhow::bail!("sysfs: empty exporter_name for ino {ino}");
    }
    Ok(DmaBufInfo {
        ino,
        size,
        exporter_name,
    })
}

const SYSFS_DMABUF_PATH: &str = "/sys/kernel/dmabuf/buffers";

/// Read all buffer entries from `/sys/kernel/dmabuf/buffers/`.
///
/// Returns an empty snapshot if the path does not exist (e.g. on host).
pub fn snapshot() -> anyhow::Result<SysfsSnapshot> {
    let base = Path::new(SYSFS_DMABUF_PATH);
    if !base.exists() {
        return Ok(SysfsSnapshot {
            buffers: Vec::new(),
        });
    }

    let mut buffers = Vec::new();
    for entry in std::fs::read_dir(base).context("failed to read sysfs dmabuf directory")? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let ino: u64 = match name_str.parse() {
            Ok(v) => v,
            Err(_) => continue, // skip non-numeric entries
        };

        let dir = entry.path();
        let size_content = std::fs::read_to_string(dir.join("size"))
            .with_context(|| format!("failed to read size for buffer ino {ino}"))?;
        let exporter_content = std::fs::read_to_string(dir.join("exporter_name"))
            .with_context(|| format!("failed to read exporter_name for buffer ino {ino}"))?;

        buffers.push(parse_buffer_entry(ino, &size_content, &exporter_content)?);
    }

    buffers.sort_by_key(|b| b.ino);
    Ok(SysfsSnapshot { buffers })
}

// ---------------------------------------------------------------------------
// CMA area stats — /sys/kernel/mm/cma/<name>/
// ---------------------------------------------------------------------------

const CMA_SYSFS_PATH: &str = "/sys/kernel/mm/cma";

/// Per-CMA-area allocation statistics from sysfs (`CONFIG_CMA_SYSFS`, kernel 5.13+).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CmaAreaStats {
    /// CMA area name (e.g. `"reserved"`, `"default_cma_region"`).
    pub name: String,
    /// Cumulative pages successfully allocated.
    pub alloc_pages_success: u64,
    /// Cumulative pages failed to allocate.
    pub alloc_pages_fail: u64,
    /// Cumulative pages released (active = `alloc_success` - `release_success`).
    pub release_pages_success: u64,
}

/// Read all CMA area stats from `/sys/kernel/mm/cma/`.
///
/// Returns an empty vec if the directory does not exist (`CONFIG_CMA_SYSFS` not enabled).
pub fn read_cma_areas() -> Vec<CmaAreaStats> {
    read_cma_areas_from(CMA_SYSFS_PATH)
}

/// Testable implementation that accepts an arbitrary base path.
fn read_cma_areas_from(base_path: &str) -> Vec<CmaAreaStats> {
    let base = Path::new(base_path);
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };

    let mut areas: Vec<CmaAreaStats> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let dir = entry.path();
            if !dir.is_dir() {
                return None;
            }
            let read_counter = |file: &str| -> u64 {
                std::fs::read_to_string(dir.join(file))
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0)
            };
            Some(CmaAreaStats {
                name,
                alloc_pages_success: read_counter("alloc_pages_success"),
                alloc_pages_fail: read_counter("alloc_pages_fail"),
                release_pages_success: read_counter("release_pages_success"),
            })
        })
        .collect();

    areas.sort_by(|a, b| a.name.cmp(&b.name));
    areas
}

// ---------------------------------------------------------------------------
// DMA heap page pool size — /sys/kernel/dma_heap/total_pools_kb
// ---------------------------------------------------------------------------

const DMA_HEAP_POOLS_PATH: &str = "/sys/kernel/dma_heap/total_pools_kb";

/// Read the total DMA heap page pool size in kB.
///
/// This is an Android-specific sysfs entry exposing the amount of memory held
/// in DMA-BUF system heap page pools (pre-allocated pages for fast allocation).
/// Returns `None` if the file does not exist (non-Android or older kernel).
pub fn read_dma_heap_pool_kb() -> Option<u64> {
    read_dma_heap_pool_kb_from(DMA_HEAP_POOLS_PATH)
}

fn read_dma_heap_pool_kb_from(path: &str) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Count total active buffers in a snapshot.
pub fn buffer_count(snap: &SysfsSnapshot) -> usize {
    snap.buffers.len()
}

/// Total size of all active buffers in bytes.
pub fn total_size(snap: &SysfsSnapshot) -> u64 {
    snap.buffers.iter().map(|b| b.size).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_buffer_entry_valid() {
        let info = parse_buffer_entry(1234, "65536\n", "system\n").unwrap();
        assert_eq!(info.ino, 1234);
        assert_eq!(info.size, 65536);
        assert_eq!(info.exporter_name, "system");
    }

    #[test]
    fn parse_buffer_entry_no_trailing_newline() {
        let info = parse_buffer_entry(42, "4096", "my_heap").unwrap();
        assert_eq!(info.size, 4096);
        assert_eq!(info.exporter_name, "my_heap");
    }

    #[test]
    fn parse_buffer_entry_invalid_size() {
        assert!(parse_buffer_entry(1, "not_a_number\n", "system\n").is_err());
    }

    #[test]
    fn parse_buffer_entry_empty_exporter() {
        assert!(parse_buffer_entry(1, "4096\n", "\n").is_err());
    }

    #[test]
    fn buffer_count_and_total_size() {
        let snap = SysfsSnapshot {
            buffers: vec![
                DmaBufInfo {
                    ino: 1,
                    size: 4096,
                    exporter_name: "system".into(),
                },
                DmaBufInfo {
                    ino: 2,
                    size: 65536,
                    exporter_name: "system".into(),
                },
                DmaBufInfo {
                    ino: 3,
                    size: 1_048_576,
                    exporter_name: "custom".into(),
                },
            ],
        };
        assert_eq!(buffer_count(&snap), 3);
        assert_eq!(total_size(&snap), 4096 + 65536 + 1_048_576);
    }

    #[test]
    fn empty_snapshot() {
        let snap = SysfsSnapshot {
            buffers: Vec::new(),
        };
        assert_eq!(buffer_count(&snap), 0);
        assert_eq!(total_size(&snap), 0);
    }

    #[test]
    fn sysfs_snapshot_serde_roundtrip() {
        let snap = SysfsSnapshot {
            buffers: vec![DmaBufInfo {
                ino: 99,
                size: 8192,
                exporter_name: "test_heap".into(),
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: SysfsSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, deserialized);
    }

    #[test]
    fn read_cma_areas_from_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let area = dir.path().join("default_cma_region");
        std::fs::create_dir(&area).unwrap();
        std::fs::write(area.join("alloc_pages_success"), "500\n").unwrap();
        std::fs::write(area.join("alloc_pages_fail"), "3\n").unwrap();
        std::fs::write(area.join("release_pages_success"), "200\n").unwrap();

        let areas = read_cma_areas_from(dir.path().to_str().unwrap());
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].name, "default_cma_region");
        assert_eq!(areas[0].alloc_pages_success, 500);
        assert_eq!(areas[0].alloc_pages_fail, 3);
        assert_eq!(areas[0].release_pages_success, 200);
    }

    #[test]
    fn read_cma_areas_multiple_sorted() {
        let dir = tempfile::tempdir().unwrap();
        for name in &["reserved", "default"] {
            let area = dir.path().join(name);
            std::fs::create_dir(&area).unwrap();
            std::fs::write(area.join("alloc_pages_success"), "100\n").unwrap();
            std::fs::write(area.join("alloc_pages_fail"), "0\n").unwrap();
            std::fs::write(area.join("release_pages_success"), "50\n").unwrap();
        }

        let areas = read_cma_areas_from(dir.path().to_str().unwrap());
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0].name, "default");
        assert_eq!(areas[1].name, "reserved");
    }

    #[test]
    fn read_cma_areas_nonexistent() {
        let areas = read_cma_areas_from("/nonexistent/cma/path");
        assert!(areas.is_empty());
    }

    #[test]
    fn read_cma_areas_missing_counter_defaults_zero() {
        let dir = tempfile::tempdir().unwrap();
        let area = dir.path().join("partial");
        std::fs::create_dir(&area).unwrap();
        std::fs::write(area.join("alloc_pages_success"), "42\n").unwrap();
        // alloc_pages_fail and release_pages_success are missing

        let areas = read_cma_areas_from(dir.path().to_str().unwrap());
        assert_eq!(areas.len(), 1);
        assert_eq!(areas[0].alloc_pages_success, 42);
        assert_eq!(areas[0].alloc_pages_fail, 0);
        assert_eq!(areas[0].release_pages_success, 0);
    }

    #[test]
    fn read_dma_heap_pool_kb_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("total_pools_kb");
        std::fs::write(&path, "8192\n").unwrap();
        assert_eq!(
            read_dma_heap_pool_kb_from(path.to_str().unwrap()),
            Some(8192)
        );
    }

    #[test]
    fn read_dma_heap_pool_kb_missing() {
        assert_eq!(
            read_dma_heap_pool_kb_from("/nonexistent/total_pools_kb"),
            None
        );
    }

    #[test]
    fn read_dma_heap_pool_kb_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("total_pools_kb");
        std::fs::write(&path, "0\n").unwrap();
        assert_eq!(read_dma_heap_pool_kb_from(path.to_str().unwrap()), Some(0));
    }

    #[test]
    fn cma_area_stats_serde_roundtrip() {
        let stats = CmaAreaStats {
            name: "test_cma".into(),
            alloc_pages_success: 1000,
            alloc_pages_fail: 5,
            release_pages_success: 800,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let deserialized: CmaAreaStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, deserialized);
    }
}
